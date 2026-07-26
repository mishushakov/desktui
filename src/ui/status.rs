//! The status line.

use std::io::Write as _;

use crate::term::Metrics;

/// Draw the status line on the bottom row.
///
/// `left` is truncated before `right` is dropped: the right-hand side carries
/// the frame statistics, which are the first thing to go when space runs out.
pub fn draw(out: &mut Vec<u8>, metrics: &Metrics, left: &str, right: &str) {
    let cols = usize::from(metrics.cols);
    if cols == 0 || metrics.rows == 0 {
        return;
    }

    let mut line = String::with_capacity(cols + 16);
    let right_len = right.chars().count();
    let left_len = left.chars().count();

    if right_len + 1 < cols && left_len + right_len < cols {
        line.push_str(left);
        for _ in 0..cols - left_len - right_len {
            line.push(' ');
        }
        line.push_str(right);
    } else {
        // No room for both: keep the left side and pad.
        line.extend(left.chars().take(cols));
        for _ in line.chars().count()..cols {
            line.push(' ');
        }
    }

    // Reverse video, bottom row, then reset. `CSI K` first so a shorter line
    // than last time does not leave a tail behind.
    let _ = write!(out, "\x1b[{};1H\x1b[7m\x1b[K", metrics.rows);
    out.extend_from_slice(line.as_bytes());
    out.extend_from_slice(b"\x1b[0m");
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

    #[test]
    fn status_line_fills_the_row_exactly() {
        let m = metrics(40, 10);
        let mut out = Vec::new();
        draw(&mut out, &m, "left", "right");
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1b[10;1H\x1b[7m\x1b[K"));
        let body = text
            .trim_start_matches("\x1b[10;1H\x1b[7m\x1b[K")
            .trim_end_matches("\x1b[0m");
        assert_eq!(body.chars().count(), 40);
        assert!(body.starts_with("left"));
        assert!(body.ends_with("right"));
    }

    #[test]
    fn status_line_truncates_rather_than_wrapping() {
        let m = metrics(10, 5);
        let mut out = Vec::new();
        draw(&mut out, &m, "a very long left side indeed", "stats");
        let text = String::from_utf8(out).unwrap();
        let body = text
            .trim_start_matches("\x1b[5;1H\x1b[7m\x1b[K")
            .trim_end_matches("\x1b[0m");
        assert_eq!(body.chars().count(), 10, "must never exceed the row width");
    }

    #[test]
    fn degenerate_geometry_draws_nothing() {
        let mut out = Vec::new();
        draw(&mut out, &metrics(0, 0), "x", "y");
        assert!(out.is_empty());
    }
}
