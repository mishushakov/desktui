//! The status line.
//!
//! Reverse video, which the menu cannot use: the row is the one below the image
//! area, so no graphics placement is ever put on it and a colour set on its cells is
//! the colour that shows. Following the terminal's own theme beats picking a pair,
//! and it is only available here.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Widget;

use super::paint::write_cells;
use crate::term::Metrics;

/// Draw the status line on the bottom row.
///
/// `left` is truncated before `right` is dropped: the right-hand side carries the
/// frame statistics, which are the first thing to go when space runs out.
pub fn draw(out: &mut Vec<u8>, metrics: &Metrics, left: &str, right: &str) {
    if metrics.cols == 0 || metrics.rows == 0 {
        return;
    }
    let area = Rect {
        x: 0,
        y: metrics.rows - 1,
        width: metrics.cols,
        height: 1,
    };

    // Reversed before anything is written on it, so the whole row carries it whether
    // or not the text reaches the end. Every cell is written every time, which is
    // what a `CSI K` used to be there for.
    let mut buf = Buffer::empty(area);
    buf.set_style(area, Style::new().add_modifier(Modifier::REVERSED));

    let left = Line::from(left);
    let right = Line::from(right).right_aligned();
    // Widths as they will be drawn rather than counts of characters: a server that
    // names itself in something wider than Latin-1 would otherwise be measured short
    // and push the statistics off the end.
    let both = left.width() + right.width() < usize::from(metrics.cols);

    // Rendered into the same row, one aligned each way. Whatever does not fit is
    // clipped at the edge, so the row is exactly its own width however long the text.
    left.render(area, &mut buf);
    if both {
        right.render(area, &mut buf);
    }

    write_cells(out, &buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(cols: u16, rows: u16) -> Metrics {
        Metrics {
            cols,
            rows,
            px_w: u32::from(cols) * 8,
            px_h: u32::from(rows) * 16,
            cell_w: 8,
            cell_h: 16,
        }
    }

    /// The row as it lands on screen, with the escapes taken out.
    fn body(buf: &[u8], row: u16) -> String {
        let text = String::from_utf8(buf.to_vec()).unwrap();
        let head = format!("\x1b[{row};1H");
        assert!(
            text.starts_with(&head),
            "the line must start at column one of row {row}: {text:?}"
        );
        assert!(
            text.contains("\x1b[7m"),
            "the row is reverse video, so it follows the terminal's theme: {text:?}"
        );
        assert!(
            text.ends_with("\x1b[0m"),
            "the attributes must not outlive the row: {text:?}"
        );
        // Everything that is not an escape sequence. They all end in a letter here:
        // a cursor move in `H`, a colour or an attribute in `m`.
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn status_line_fills_the_row_exactly() {
        let m = metrics(40, 10);
        let mut out = Vec::new();
        draw(&mut out, &m, "left", "right");
        let body = body(&out, 10);
        assert_eq!(body.chars().count(), 40);
        assert!(body.starts_with("left"));
        assert!(body.ends_with("right"));
    }

    #[test]
    fn status_line_truncates_rather_than_wrapping() {
        let m = metrics(10, 5);
        let mut out = Vec::new();
        draw(&mut out, &m, "a very long left side indeed", "stats");
        let body = body(&out, 5);
        assert_eq!(body.chars().count(), 10, "must never exceed the row width");
        assert_eq!(body, "a very lon", "the left side is what survives");
    }

    #[test]
    fn the_statistics_go_before_the_left_side_does() {
        // Both sides fit at forty columns and neither does at twenty, where the left
        // is still short enough to be drawn whole.
        let mut out = Vec::new();
        draw(&mut out, &metrics(40, 10), " a-server  1600x832", "60 fps ");
        assert!(body(&out, 10).ends_with("60 fps "));

        out.clear();
        draw(&mut out, &metrics(20, 10), " a-server  1600x832", "60 fps ");
        let body = body(&out, 10);
        assert_eq!(body, " a-server  1600x832 ");
        assert!(
            !body.contains("fps"),
            "the statistics should have gone first"
        );
    }

    #[test]
    fn a_wide_glyph_is_measured_by_what_it_covers() {
        // Two cells each, so this name is twelve columns wide and not six. Counting
        // characters would leave room for a right-hand side that cannot fit.
        let m = metrics(20, 10);
        let mut out = Vec::new();
        draw(&mut out, &m, "日本語のデス", "12345678");
        let body = body(&out, 10);
        assert!(
            !body.contains("12345678"),
            "twelve columns and eight leave nothing between them: {body:?}"
        );
    }

    #[test]
    fn degenerate_geometry_draws_nothing() {
        let mut out = Vec::new();
        draw(&mut out, &metrics(0, 0), "x", "y");
        assert!(out.is_empty());
    }
}
