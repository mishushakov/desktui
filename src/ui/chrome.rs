//! The cells we own, and the difference between one frame's worth and the last.
//!
//! Everything the client draws in text -- the bar, the command menu, a notification --
//! renders into one buffer the size of the terminal, which is then compared against what
//! is already on screen. Only the cells that changed are written.
//!
//! The rule that makes this safe is the one `ui/mod.rs` states: never touch a cell we did
//! not write. A renderer that owned the whole screen would be wrong here, because it would
//! repaint the cells our graphics placements live in, and those are not its to repaint.
//! Owning one plane and diffing that is the same idea with the boundary in the right place:
//! a cell nothing has drawn in stays blank in both buffers, so it never appears in a diff.
//!
//! Two things fall out of it rather than being arranged:
//!
//! * **Taking chrome off the screen is not a separate operation.** A menu that closed, a
//!   bar whose row moved, a note that expired: each is simply absent from the next frame's
//!   buffer, and the cells it occupied come back as blanks in the diff. There is nothing to
//!   remember having drawn.
//! * **The cells it vacates are damage.** They are handed back from [`Chrome::flush`] so
//!   the tiles underneath can be redrawn, which is what a terminal that treats clearing a
//!   cell as dropping the placement under it needs -- and it is impossible to forget,
//!   because the flush returns them whether the caller wants them or not.
//!
//! Overlay *images* -- the menu's backdrop, its hover bar, a note's paper -- are placements
//! rather than cells, so they cannot be diffed. They are declared each frame instead, and
//! whatever was placed last frame and is not wanted this one is deleted.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::paint::write_diff;
use crate::term::{Metrics, kitty};

/// The chrome's own plane: what is on screen, and what should be.
pub struct Chrome {
    shown: Buffer,
    next: Buffer,
    /// Overlay image ids placed last frame, and the ones wanted this frame.
    placed: Vec<u32>,
    wanted: Vec<u32>,
}

impl Chrome {
    pub fn new() -> Self {
        let empty = Rect::new(0, 0, 0, 0);
        Self {
            shown: Buffer::empty(empty),
            next: Buffer::empty(empty),
            placed: Vec::new(),
            wanted: Vec::new(),
        }
    }

    /// Start a frame: blank everything, and adopt the terminal's size.
    ///
    /// `shown` keeps its contents cell by cell, because that is what is actually on the
    /// screen -- a window that grew still has the old bar on the row it was drawn on, and
    /// the diff has to see it there to blank it. `next` is reset, so anything not drawn this
    /// frame is absent by default rather than by being taken down.
    ///
    /// Cell by cell rather than `Buffer::resize`, which truncates the flat vector without
    /// reflowing it: change the *width* and every row after the first is reinterpreted at
    /// the wrong offset, so the diff compares against a screen that never existed and the
    /// old bar is never blanked.
    pub fn begin(&mut self, metrics: &Metrics) {
        let area = Rect::new(0, 0, metrics.cols, metrics.rows);
        if self.shown.area != area {
            let mut moved = Buffer::empty(area);
            let rows = self.shown.area.height.min(area.height);
            let cols = self.shown.area.width.min(area.width);
            for y in 0..rows {
                for x in 0..cols {
                    if let Some(was) = self.shown.cell((x, y)).cloned()
                        && let Some(cell) = moved.cell_mut((x, y))
                    {
                        *cell = was;
                    }
                }
            }
            self.shown = moved;
            self.next = Buffer::empty(area);
        }
        self.next.reset();
        self.wanted.clear();
    }

    /// The buffer this frame's chrome renders into.
    pub fn buffer(&mut self) -> &mut Buffer {
        &mut self.next
    }

    /// Note that an overlay image belongs on screen this frame. The caller places it; this
    /// is what stops it being deleted at the end of the frame.
    pub fn keep(&mut self, id: u32) {
        self.wanted.push(id);
    }

    /// Write what changed, drop the overlays that are no longer wanted, and say which cells
    /// were vacated so the picture under them can be drawn back.
    ///
    /// Nothing is committed here: [`Chrome::commit`] is called once the frame has actually
    /// reached the terminal, because a frame the writer was too busy for did not happen and
    /// the next one has to say all of it again.
    pub fn flush(&mut self, out: &mut Vec<u8>) -> Vec<Rect> {
        for id in &self.placed {
            if !self.wanted.contains(id) {
                kitty::delete_image(out, *id);
            }
        }

        let updates = self.shown.diff(&self.next);
        write_diff(out, &updates);

        // A cell that has gone blank is a cell something was drawn on and is not any more,
        // which is exactly the damage the tiles underneath need.
        updates
            .iter()
            .filter(|(x, y, cell)| {
                cell.symbol() == " "
                    && self
                        .shown
                        .cell((*x, *y))
                        .is_some_and(|was| was.symbol() != " ")
            })
            .map(|(x, y, _)| Rect::new(*x, *y, 1, 1))
            .collect()
    }

    /// The frame reached the terminal, so what it drew is what is on screen.
    pub fn commit(&mut self) {
        std::mem::swap(&mut self.shown, &mut self.next);
        std::mem::swap(&mut self.placed, &mut self.wanted);
    }
}

impl Default for Chrome {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    fn metrics(cols: u16, rows: u16) -> Metrics {
        Metrics {
            cols,
            rows,
            px_w: u32::from(cols) * 8,
            px_h: u32::from(rows) * 17,
            cell_w: 8,
            cell_h: 17,
        }
    }

    /// One frame: render `text` at `(x, y)` if given, flush, commit.
    fn frame(
        chrome: &mut Chrome,
        m: &Metrics,
        at: Option<(u16, u16, &str)>,
    ) -> (String, Vec<Rect>) {
        chrome.begin(m);
        if let Some((x, y, text)) = at {
            chrome.buffer().set_string(x, y, text, Style::new());
        }
        let mut out = Vec::new();
        let vacated = chrome.flush(&mut out);
        chrome.commit();
        (String::from_utf8(out).unwrap(), vacated)
    }

    #[test]
    fn only_what_changed_is_written() {
        let m = metrics(20, 5);
        let mut chrome = Chrome::new();

        let (first, vacated) = frame(&mut chrome, &m, Some((2, 1, "hello")));
        assert!(first.contains("hello"), "{first:?}");
        assert!(vacated.is_empty(), "nothing was on screen before");

        // The same content again writes nothing at all.
        let (again, vacated) = frame(&mut chrome, &m, Some((2, 1, "hello")));
        assert_eq!(again, "", "an unchanged plane is no bytes");
        assert!(vacated.is_empty());

        // One letter different is one cell.
        let (edit, _) = frame(&mut chrome, &m, Some((2, 1, "hellp")));
        assert!(edit.contains('p'), "{edit:?}");
        assert!(!edit.contains("hell"), "only the last cell moved: {edit:?}");
    }

    #[test]
    fn text_that_goes_away_blanks_its_cells_and_is_reported_as_damage() {
        // What used to be `Menu::clear`, `status::clear` and the popup's own record of
        // where it had drawn: absent from the next frame is all it takes.
        let m = metrics(20, 5);
        let mut chrome = Chrome::new();
        frame(&mut chrome, &m, Some((2, 1, "hi")));

        let (gone, vacated) = frame(&mut chrome, &m, None);
        assert!(
            gone.contains("\x1b[2;3H"),
            "positioned at the cells: {gone:?}"
        );
        assert_eq!(
            vacated,
            vec![Rect::new(2, 1, 1, 1), Rect::new(3, 1, 1, 1)],
            "both cells have to be handed back as damage"
        );
    }

    #[test]
    fn a_window_that_changed_width_still_blanks_the_row_the_chrome_left() {
        // The trap: `Buffer::resize` truncates the flat vector without reflowing it, so a
        // change of width reinterprets every row after the first at the wrong offset and the
        // diff compares against a screen that never existed.
        let narrow = metrics(20, 5);
        let mut chrome = Chrome::new();
        frame(&mut chrome, &narrow, Some((0, 4, "bar")));

        let wide = metrics(40, 8);
        chrome.begin(&wide);
        chrome.buffer().set_string(0, 7, "bar", Style::new());
        let mut out = Vec::new();
        let vacated = chrome.flush(&mut out);
        chrome.commit();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("\x1b[8;1Hbar"),
            "drawn on the new row: {text:?}"
        );
        assert!(
            text.contains("\x1b[5;1H"),
            "and the row it left blanked: {text:?}"
        );
        assert_eq!(vacated.len(), 3, "three cells of it");
    }

    #[test]
    fn a_window_that_grew_still_blanks_the_row_the_chrome_left() {
        // The resize case, which used to need the old metrics kept and the widgets asked
        // to erase themselves at them.
        let small = metrics(20, 5);
        let mut chrome = Chrome::new();
        frame(&mut chrome, &small, Some((0, 4, "bar")));

        // Grown: the bar belongs on the new last row, so row 4 has to be blanked.
        let big = metrics(20, 8);
        chrome.begin(&big);
        chrome.buffer().set_string(0, 7, "bar", Style::new());
        let mut out = Vec::new();
        let vacated = chrome.flush(&mut out);
        chrome.commit();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("\x1b[8;1Hbar"),
            "drawn on the new row: {text:?}"
        );
        assert!(
            text.contains("\x1b[5;1H"),
            "and the old row blanked: {text:?}"
        );
        assert_eq!(vacated.len(), 3, "three cells of it");
    }

    #[test]
    fn a_dropped_frame_is_said_again() {
        // No commit, so nothing reached the terminal and the next frame owes all of it.
        let m = metrics(20, 5);
        let mut chrome = Chrome::new();
        chrome.begin(&m);
        chrome.buffer().set_string(0, 0, "x", Style::new());
        let mut out = Vec::new();
        chrome.flush(&mut out);
        // ... and no commit.

        chrome.begin(&m);
        chrome.buffer().set_string(0, 0, "x", Style::new());
        let mut again = Vec::new();
        chrome.flush(&mut again);
        assert_eq!(
            again, out,
            "the frame that was thrown away has to be redone"
        );
    }

    #[test]
    fn an_overlay_nobody_wants_this_frame_is_deleted() {
        let m = metrics(20, 5);
        let mut chrome = Chrome::new();
        chrome.begin(&m);
        chrome.keep(kitty::OVERLAY_IMAGE_ID);
        let mut out = Vec::new();
        chrome.flush(&mut out);
        chrome.commit();
        assert!(
            !String::from_utf8_lossy(&out).contains("a=d"),
            "a wanted overlay must not be deleted"
        );

        chrome.begin(&m);
        let mut out = Vec::new();
        chrome.flush(&mut out);
        chrome.commit();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(&format!("i={},", kitty::OVERLAY_IMAGE_ID)),
            "the overlay should have been dropped: {text:?}"
        );
    }
}
