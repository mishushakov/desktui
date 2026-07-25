//! The status line and the help overlay.

use std::io::Write as _;

use crate::term::{Metrics, kitty};

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

/// The overlay is black on white, said twice over because one way is not enough.
///
/// `HELP_SGR` colours the cells. That is the whole story in a terminal with no
/// graphics protocol, which `--force` allows, and it is why the box looks right in
/// Terminal.app or Alacritty.
///
/// Where the protocol does work the cell colour is invisible. Tiles are placed at
/// `z=-1` (see `term::kitty`): below the text, but *above* the cell background, so
/// a colour set there is painted under the remote screen and only the glyphs come
/// out on top -- dark text adrift on the wallpaper. Probing Ghostty settled it:
/// reverse video, an explicit `48;` pair and the basic pairs were every one of them
/// buried, and only an image of our own -- same z-index, higher id than any tile --
/// was composited over the desktop. So `draw_help` places one and writes on that.
///
/// Both agree on the colours, so the box looks the same whichever is doing the work.
const HELP_SGR: &str = "\x1b[48;2;255;255;255m\x1b[38;2;0;0;0m";
const HELP_BACKDROP: (u8, u8, u8) = (0xff, 0xff, 0xff);

/// The overlay's contents, without the border around them.
fn help_lines(prefix: char) -> Vec<String> {
    let p = format!("Ctrl+{}", prefix.to_ascii_uppercase());
    vec![
        "desktui".to_string(),
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
        format!(
            "  Ctrl+{}     send a literal Ctrl+{}",
            prefix.to_ascii_uppercase(),
            prefix.to_ascii_uppercase()
        ),
        String::new(),
        "any other key dismisses this".to_string(),
    ]
}

/// Where the overlay goes: one-based `(row, col, width, height)`, or `None` when
/// the image area is too small to hold it.
fn help_box(metrics: &Metrics, lines: &[String]) -> Option<(usize, usize, usize, usize)> {
    let inner = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let box_w = inner + 4;
    let box_h = lines.len() + 2;
    if box_w > usize::from(metrics.cols) || box_h > usize::from(metrics.image_rows()) {
        return None;
    }
    let col = (usize::from(metrics.cols) - box_w) / 2 + 1;
    let row = (usize::from(metrics.image_rows()) - box_h) / 2 + 1;
    Some((row, col, box_w, box_h))
}

/// Draw the help overlay, centred, listing the prefix-key commands.
pub fn draw_help(out: &mut Vec<u8>, metrics: &Metrics, prefix: char) {
    let lines = help_lines(prefix);
    let Some((row, col, box_w, box_h)) = help_box(metrics, &lines) else {
        return;
    };
    let inner = box_w - 4;

    // The backdrop first, then the text on top of it. It goes out after the tiles
    // for the same frame, which is what puts it above them.
    kitty::place_solid(
        out,
        kitty::OVERLAY_IMAGE_ID,
        col,
        row,
        box_w,
        box_h,
        HELP_BACKDROP,
    );

    out.extend_from_slice(HELP_SGR.as_bytes());
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

/// Take the overlay off the screen: blank its cells and drop its backdrop.
///
/// Both halves are needed, and redrawing the remote screen is neither of them.
/// The glyphs are text, which no repaint of an image below them can erase, and the
/// backdrop is an image of ours that outranks every tile and would otherwise stay
/// on top of them for ever. The cells go back to default attributes, which is what
/// makes them transparent to the tiles again.
pub fn clear_help(out: &mut Vec<u8>, metrics: &Metrics, prefix: char) {
    let lines = help_lines(prefix);
    let Some((row, col, box_w, box_h)) = help_box(metrics, &lines) else {
        return;
    };

    // Text first, then images, as a relayout does it: an erase may take the
    // placements under it along too.
    out.extend_from_slice(b"\x1b[0m");
    for i in 0..box_h {
        let _ = write!(out, "\x1b[{};{}H", row + i, col);
        for _ in 0..box_w {
            out.push(b' ');
        }
    }
    kitty::delete_image(out, kitty::OVERLAY_IMAGE_ID);
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
    fn help_overlay_carries_its_own_background() {
        // Tiles sit at z=-1: below the text, but above the cell background, so no
        // SGR fill can be seen behind the glyphs. The backdrop has to be an image
        // of our own, outranking every tile id so it is composited over them.
        let mut out = Vec::new();
        draw_help(&mut out, &metrics(100, 40), 'a');
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(&format!("i={},", kitty::OVERLAY_IMAGE_ID)),
            "the overlay must place a backdrop image, not colour its cells"
        );
        assert!(
            text.contains("z=-1"),
            "the backdrop shares the tiles' z-index; above it would cover the text"
        );
        assert!(
            text.contains(HELP_SGR),
            "the cells are coloured too, for a terminal with no graphics protocol"
        );
        assert!(
            !text.contains("\x1b[7m"),
            "reverse video would follow the theme, and is buried by the image anyway"
        );
        // The backdrop goes down before the text that sits on it.
        let backdrop = text.find("\x1b_Ga=T").expect("no backdrop");
        assert!(backdrop < text.find('┌').expect("no border"));
    }

    /// The rows and columns a buffer moved the cursor to, in order.
    fn positions(buf: &[u8]) -> Vec<(u16, u16)> {
        let text = String::from_utf8(buf.to_vec()).unwrap();
        let mut out = Vec::new();
        for part in text.split("\x1b[").skip(1) {
            if let Some((coords, _)) = part.split_once('H')
                && let Some((r, c)) = coords.split_once(';')
                && let (Ok(r), Ok(c)) = (r.parse::<u16>(), c.parse::<u16>())
            {
                out.push((r, c));
            }
        }
        out
    }

    #[test]
    fn clearing_the_help_blanks_every_cell_it_drew() {
        // Repainting the image cannot undraw the overlay, because the image is
        // composited below the text. The cells have to be erased, and the erase has
        // to cover exactly the rectangle that was drawn or a frame of it survives.
        let m = metrics(100, 40);
        let mut drawn = Vec::new();
        draw_help(&mut drawn, &m, 'a');
        let mut cleared = Vec::new();
        clear_help(&mut cleared, &m, 'a');

        assert!(!cleared.is_empty(), "nothing was erased");
        let cells = |buf: &[u8]| {
            let mut v = positions(buf);
            v.sort_unstable();
            v.dedup();
            v
        };
        assert_eq!(
            cells(&drawn),
            cells(&cleared),
            "the erase must cover the same cells the overlay drew"
        );
        let text = String::from_utf8(cleared).unwrap();
        assert!(
            text.starts_with("\x1b[0m"),
            "the cells must go back to default attributes to be transparent again"
        );
        assert!(
            text.contains(&format!("a=d,d=I,i={}", kitty::OVERLAY_IMAGE_ID)),
            "the backdrop image outranks every tile and has to be deleted too"
        );
        // Nothing but blanks between the cursor moves: a leftover border would
        // still be on screen.
        for chunk in text.split("\x1b[").skip(2) {
            let body = chunk.split_once('H').map(|(_, b)| b).unwrap_or("");
            // The backdrop delete rides along after the last row's blanks.
            let body = body.split('\x1b').next().unwrap_or("");
            assert!(
                body.chars().all(|c| c == ' '),
                "the erase wrote something other than blanks: {body:?}"
            );
        }
    }

    #[test]
    fn a_help_overlay_that_does_not_fit_is_not_cleared_either() {
        // Otherwise the erase would blank a rectangle that was never drawn.
        let mut out = Vec::new();
        clear_help(&mut out, &metrics(20, 6), 'a');
        assert!(out.is_empty());
    }

    #[test]
    fn help_overlay_never_touches_the_status_row() {
        let m = metrics(100, 20);
        let mut out = Vec::new();
        draw_help(&mut out, &m, 'a');
        let text = String::from_utf8(out).unwrap();
        // Every cursor move must stay above the last row. Only sequences that
        // actually terminate in `H` are positions: the colour set-up ends in `m`,
        // and its parameters would otherwise be read as a row number.
        for part in text.split("\x1b[").skip(1) {
            if let Some((coords, _)) = part.split_once('H')
                && let Some((r, _)) = coords.split_once(';')
                && let Ok(row) = r.parse::<u16>()
            {
                assert!(row < m.rows, "overlay wrote to row {row}");
            }
        }
    }
}
