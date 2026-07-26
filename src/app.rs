//! Session loops.
//!
//! The test-pattern loop lives here alongside the real session so both go
//! through the same compose-and-submit path: if the pattern renders correctly,
//! the pixel pipeline is sound and anything wrong afterwards is the protocol's
//! fault, not the renderer's.

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseEventKind};

use crate::cli::{Args, ScaleMode};
use crate::render::framebuffer::Framebuffer;
use crate::render::testpattern::TestPattern;
use crate::render::{Layout, Rect, Renderer};
use crate::term::caps::Caps;
use crate::term::kitty;
use crate::term::writer::{Busy, FrameWriter};
use crate::term::{Metrics, TerminalGuard};
use crate::ui::chrome::Chrome;
use crate::ui::menu::{self, Menu};
use crate::ui::status;
use crate::ui::theme::Theme;

/// Rolling frame-rate estimate over a short window.
pub struct FpsMeter {
    samples: std::collections::VecDeque<Instant>,
}

impl FpsMeter {
    pub fn new() -> Self {
        Self {
            samples: std::collections::VecDeque::with_capacity(64),
        }
    }

    /// How long since the last frame was counted.
    pub fn since_last(&self) -> Duration {
        match self.samples.back() {
            Some(last) => last.elapsed(),
            None => Duration::MAX,
        }
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        self.samples.push_back(now);
        while let Some(&front) = self.samples.front() {
            if now.duration_since(front) > Duration::from_secs(1) {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn fps(&self) -> f64 {
        match (self.samples.front(), self.samples.back()) {
            (Some(&a), Some(&b)) if self.samples.len() > 1 => {
                let secs = b.duration_since(a).as_secs_f64();
                if secs > 0.0 {
                    (self.samples.len() - 1) as f64 / secs
                } else {
                    0.0
                }
            }
            _ => 0.0,
        }
    }
}

/// Render the synthetic test pattern until the user quits.
pub fn run_test_pattern(args: &Args, caps: &Caps, guard: &TerminalGuard) -> Result<()> {
    guard.begin_full_screen()?;

    let mut metrics = Metrics::query()?;
    let mut writer = FrameWriter::spawn();

    // The pattern is generated at exactly the size of the image area, so the
    // default path is the pixel-exact one.
    let (mut area_w, mut area_h) = metrics.image_area();
    let mut pattern = TestPattern::new(area_w, area_h);
    let mut fb = Framebuffer::new(area_w, area_h);
    pattern.paint_all(&mut fb);

    let mut renderer = Renderer::new(
        Layout::compute(&metrics, args.scale, area_w, area_h, (0, 0)),
        true,
        args.transfer.resolve(caps),
    );
    let mut damage: Vec<Rect> = Vec::new();
    let mut fps = FpsMeter::new();
    // Listed, not clickable: none of the prefix commands mean anything to a loop
    // with no server behind it, and nothing here can change what it shows either.
    let menu = Menu::new(args.prefix_char());
    let state = menu::State {
        mode: args.scale,
        theme: Theme::Dark,
    };
    let ink = state.theme.palette();
    let mut show_menu = false;
    let mut menu_shown = false;
    let mut chrome = Chrome::new();
    // The wipe a relayout asks for, waiting for the frame that fills the screen back
    // in. Written on its own it is a blank screen that lasts until the next frame
    // composes; carried into that frame's synchronised block, the old picture stands
    // until the new one replaces it.
    let mut pending_cleanup: Vec<u8> = Vec::new();
    let mut last_stats = crate::render::FrameStats::default();
    let mut dropped: u64 = 0;

    let frame_time = Duration::from_micros(1_000_000 / u64::from(args.fps));
    let mut next_frame = Instant::now();

    loop {
        // Drain input without blocking past the next frame deadline.
        loop {
            let now = Instant::now();
            let wait = next_frame.saturating_duration_since(now);
            if !event::poll(wait)? {
                break;
            }
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    match key.code {
                        // Escape belongs to the menu while it is up, which is what the
                        // menu's own title offers. Leaving it as the way out of the
                        // pattern would have the box lie about what it does.
                        KeyCode::Esc if show_menu => show_menu = false,
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c')
                            if key.modifiers.contains(event::KeyModifiers::CONTROL) =>
                        {
                            return Ok(());
                        }
                        KeyCode::Char('h') | KeyCode::Char('?') => show_menu = !show_menu,
                        // Nothing else dismisses it, here as in a session.
                        _ => {}
                    }
                }
                Event::Mouse(m) => {
                    // With mode 1016 in force these are pixel coordinates; when
                    // the terminal does not support it they are cells, so scale
                    // to the middle of the cell.
                    let (tx, ty) = if caps.pixel_mouse {
                        (u32::from(m.column), u32::from(m.row))
                    } else {
                        (
                            u32::from(m.column) * metrics.cell_w + metrics.cell_w / 2,
                            u32::from(m.row) * metrics.cell_h + metrics.cell_h / 2,
                        )
                    };
                    if matches!(
                        m.kind,
                        MouseEventKind::Moved
                            | MouseEventKind::Drag(_)
                            | MouseEventKind::Down(_)
                            | MouseEventKind::Up(_)
                    ) {
                        pattern.set_cursor(
                            renderer
                                .layout()
                                .terminal_px_to_src(tx, ty)
                                .map(|(x, y)| (u32::from(x), u32::from(y))),
                        );
                    }
                }
                Event::Resize(_, _) => {
                    metrics = Metrics::query()?;
                    let (w, h) = metrics.image_area();
                    area_w = w;
                    area_h = h;
                    pattern.resize(area_w, area_h);
                    fb.resize(area_w, area_h);
                    pattern.paint_all(&mut fb);

                    let layout = Layout::compute(&metrics, args.scale, area_w, area_h, (0, 0));
                    let cleanup = renderer.relayout(layout);
                    // Held for the next frame rather than written now, and appended: a
                    // relayout names the tiles it has dropped, and a second one before
                    // that frame goes out names different ones.
                    pending_cleanup.extend_from_slice(&cleanup);
                }
                _ => {}
            }
        }

        if Instant::now() < next_frame {
            continue;
        }
        next_frame += frame_time;
        // Never chase a deadline we have already missed by a whole frame.
        let now = Instant::now();
        if next_frame < now {
            next_frame = now + frame_time;
        }

        damage.clear();
        pattern.step(&mut fb, &mut damage);
        for r in &damage {
            renderer.mark(*r);
        }
        if !renderer.has_work() && pending_cleanup.is_empty() && !show_menu && !menu_shown {
            continue;
        }

        let mut buf = writer.take_buffer();
        if caps.sync_output {
            kitty::begin_sync(&mut buf);
        }
        // A relayout's wipe, if one is owed: erase and delete inside the same
        // synchronised block that puts the screen back, so the terminal only ever shows
        // one of the two layouts and never the gap between them.
        let cleanup = std::mem::take(&mut pending_cleanup);
        buf.extend_from_slice(&cleanup);

        // The chrome, diffed against what is on screen, before the tiles: see `session`.
        chrome.begin(&metrics);
        let layout = *renderer.layout();
        let rest = format!(
            "  {}x{} {}  {} tiles",
            layout.dst_w,
            layout.dst_h,
            describe(&layout),
            renderer.tile_count(),
        );
        let figures = format!(
            "{:>5.1} fps  {:>3} tiles/f  {:>6}/f  {} dropped  q quit  ",
            fps.fps(),
            last_stats.tiles,
            human_bytes(last_stats.bytes),
            dropped,
        );
        status::render(
            chrome.buffer(),
            &metrics,
            ink,
            vec![ink.bright(" test-pattern"), ink.text(&rest)],
            vec![ink.text(&figures), ink.bright("h"), ink.text(" menu ")],
        );
        if show_menu {
            menu.render(&mut chrome, &mut buf, &metrics, state);
        }
        menu_shown = show_menu;
        for cells in chrome.flush(&mut buf) {
            renderer.mark_cells(cells.x, cells.y, cells.width, cells.height);
        }

        let stats = renderer.compose(&fb, &mut buf);
        if stats.tiles > 0 {
            last_stats = stats;
        }

        if caps.sync_output {
            kitty::end_sync(&mut buf);
        }

        match writer.submit(buf) {
            Ok(()) => {
                renderer.commit();
                chrome.commit();
                fps.tick();
            }
            // Terminal is still busy: keep the damage and try again next tick.
            Err(Busy::Full(buf)) => {
                dropped += 1;
                writer.recycle(buf);
                // The wipe went out with the frame or not at all: dropping it here would
                // leave the old layout's status line and tiles on screen for good, since
                // the damage that would have painted over them was never composed.
                if !cleanup.is_empty() {
                    pending_cleanup = cleanup;
                }
            }
            Err(Busy::Closed) => break,
        }
    }

    writer.shutdown();
    Ok(())
}

/// A short description of the active mapping, for the status line.
pub fn describe(layout: &Layout) -> &'static str {
    match (layout.mode, layout.is_pixel_exact(), layout.is_cropped()) {
        (ScaleMode::Native, true, false) => "native 1:1",
        (_, true, true) => "1:1 cropped",
        (_, true, false) => "1:1",
        (ScaleMode::Integer, false, _) => "integer",
        _ => "scaled",
    }
}

pub fn human_bytes(n: usize) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1}M", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.0}K", n as f64 / 1024.0)
    } else {
        format!("{n}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Metrics;

    fn metrics() -> Metrics {
        Metrics {
            cols: 200,
            rows: 50,
            px_w: 1600,
            px_h: 850,
            cell_w: 8,
            cell_h: 17,
        }
    }

    #[test]
    fn describes_an_exact_match_as_native() {
        let m = metrics();
        let (w, h) = m.image_area();
        let l = Layout::compute(&m, ScaleMode::Native, w, h, (0, 0));
        assert_eq!(describe(&l), "native 1:1");
    }

    #[test]
    fn describes_a_cropped_view_and_a_scaled_one() {
        let m = metrics();
        let cropped = Layout::compute(&m, ScaleMode::OneToOne, 4000, 4000, (0, 0));
        assert_eq!(describe(&cropped), "1:1 cropped");
        let scaled = Layout::compute(&m, ScaleMode::Fit, 1920, 1080, (0, 0));
        assert_eq!(describe(&scaled), "scaled");
    }

    #[test]
    fn byte_formatting_is_compact() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(2048), "2K");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0M");
    }

    #[test]
    fn fps_meter_reports_zero_before_it_has_samples() {
        let mut meter = FpsMeter::new();
        assert_eq!(meter.fps(), 0.0);
        meter.tick();
        assert_eq!(meter.fps(), 0.0);
    }
}
