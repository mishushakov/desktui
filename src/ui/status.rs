//! The status line and the help overlay.

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

/// Draw the help overlay, centred, listing the prefix-key commands.
pub fn draw_help(out: &mut Vec<u8>, metrics: &Metrics, prefix: char) {
    let p = format!("Ctrl+{}", prefix.to_ascii_uppercase());
    let lines: Vec<String> = vec![
        "vnctui".to_string(),
        String::new(),
        format!("{p} then:"),
        "  q          quit".to_string(),
        "  f          request a full screen refresh".to_string(),
        "  r          renegotiate the remote size".to_string(),
        "  n s i 1    native / fit / integer / 1:1 scaling".to_string(),
        "  arrows     pan, when the view is cropped".to_string(),
        "  v          toggle view-only".to_string(),
        "  c          toggle statistics".to_string(),
        "  h ?        this help".to_string(),
        format!("  Ctrl+{}     send a literal Ctrl+{}", prefix.to_ascii_uppercase(), prefix.to_ascii_uppercase()),
        String::new(),
        "any other key dismisses this".to_string(),
    ];

    let inner = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let box_w = inner + 4;
    let box_h = lines.len() + 2;
    if box_w > usize::from(metrics.cols) || box_h > usize::from(metrics.image_rows()) {
        return;
    }

    let col = (usize::from(metrics.cols) - box_w) / 2 + 1;
    let row = (usize::from(metrics.image_rows()) - box_h) / 2 + 1;

    let _ = write!(out, "\x1b[7m");
    let _ = write!(out, "\x1b[{row};{col}H");
    out.extend_from_slice("┌".as_bytes());
    for _ in 0..box_w - 2 {
        out.extend_from_slice("─".as_bytes());
    }
    out.extend_from_slice("┐".as_bytes());

    for (i, line) in lines.iter().enumerate() {
        let _ = write!(out, "\x1b[{};{}H", row + 1 + i, col);
        out.extend_from_slice("│ ".as_bytes());
        out.extend_from_slice(line.as_bytes());
        for _ in line.chars().count()..inner {
            out.push(b' ');
        }
        out.extend_from_slice(" │".as_bytes());
    }

    let _ = write!(out, "\x1b[{};{}H", row + box_h - 1, col);
    out.extend_from_slice("└".as_bytes());
    for _ in 0..box_w - 2 {
        out.extend_from_slice("─".as_bytes());
    }
    out.extend_from_slice("┘".as_bytes());
    let _ = write!(out, "\x1b[0m");
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

    #[test]
    fn help_overlay_fits_or_is_skipped() {
        let mut out = Vec::new();
        draw_help(&mut out, &metrics(100, 40), 'a');
        assert!(!out.is_empty());

        out.clear();
        draw_help(&mut out, &metrics(20, 6), 'a');
        assert!(out.is_empty(), "must not draw an overlay that does not fit");
    }

    #[test]
    fn help_overlay_never_touches_the_status_row() {
        let m = metrics(100, 20);
        let mut out = Vec::new();
        draw_help(&mut out, &m, 'a');
        let text = String::from_utf8(out).unwrap();
        // Every cursor move must stay above the last row.
        for part in text.split("\x1b[").skip(1) {
            if let Some(coords) = part.strip_suffix('H').or_else(|| part.split('H').next())
                && let Some((r, _)) = coords.split_once(';')
                    && let Ok(row) = r.parse::<u16>() {
                        assert!(row < m.rows, "overlay wrote to row {row}");
                    }
        }
    }
}
