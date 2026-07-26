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
use crate::ui::menu::Menu;
use crate::ui::status;

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
    // with no server behind it.
    let menu = Menu::new(args.prefix_char());
    let mut show_help = false;
    let mut clear_help = false;
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
                    let was_showing = show_help;
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c')
                            if key.modifiers.contains(event::KeyModifiers::CONTROL) =>
                        {
                            return Ok(());
                        }
                        KeyCode::Char('h') | KeyCode::Char('?') => show_help = !show_help,
                        _ => show_help = false,
                    }
                    // The overlay leaves text and a backdrop image behind it, and
                    // neither is undone by drawing the pattern again.
                    if was_showing && !show_help {
                        clear_help = true;
                        renderer.mark_all();
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
                    if !cleanup.is_empty() {
                        let _ = writer.submit_blocking(cleanup);
                    }
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
        if !renderer.has_work() && !show_help && !clear_help {
            continue;
        }

        let mut buf = writer.take_buffer();
        if caps.sync_output {
            kitty::begin_sync(&mut buf);
        }
        if clear_help {
            menu.clear(&mut buf, &metrics);
            clear_help = false;
        }
        let stats = renderer.compose(&fb, &mut buf);
        if stats.tiles > 0 {
            last_stats = stats;
        }

        let layout = renderer.layout();
        let left = format!(
            " test-pattern  {}x{} {}  {} tiles",
            layout.dst_w,
            layout.dst_h,
            describe(layout),
            renderer.tile_count(),
        );
        let right = format!(
            "{:>5.1} fps  {:>3} tiles/f  {:>6}/f  {} dropped  q quit  h help ",
            fps.fps(),
            last_stats.tiles,
            human_bytes(last_stats.bytes),
            dropped,
        );
        status::draw(&mut buf, &metrics, &left, &right);
        if show_help {
            menu.draw(&mut buf, &metrics, args.scale);
        }
        if caps.sync_output {
            kitty::end_sync(&mut buf);
        }

        match writer.submit(buf) {
            Ok(()) => {
                renderer.commit();
                fps.tick();
            }
            // Terminal is still busy: keep the damage and try again next tick.
            Err(Busy::Full(buf)) => {
                dropped += 1;
                writer.recycle(buf);
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
