//! Turning a laid-out ratatui buffer into positioned text.
//!
//! This is the part a backend would otherwise do, minus the diffing. There is no
//! previous screen to diff against: the chrome is drawn whole or not at all, and
//! everything else on the cells it covers is an image the text is composited over
//! rather than cells we wrote. A backend that believed otherwise would repaint
//! them, and the graphics placements are not ours to repaint.

use std::io::Write as _;

#[cfg(test)]
use ratatui::buffer::Buffer;
use ratatui::buffer::{Cell, CellWidth};
use ratatui::style::{Color, Modifier, Style};

/// Write out the cells a diff says have changed, and nothing else.
///
/// `updates` is what `Buffer::diff` produces: the cells that differ, in reading order,
/// with the tails of wide glyphs already left out. Positioning follows the same rule as
/// [`write_cells`] -- one cursor move per run of adjacent cells, a fresh one after any gap
/// -- because a diff is mostly runs and paying for a move per cell would double it.
pub(super) fn write_diff(out: &mut Vec<u8>, updates: &[(u16, u16, &Cell)]) {
    let mut current: Option<Style> = None;
    let mut after: Option<(u16, u16)> = None;
    for (x, y, cell) in updates {
        if after != Some((*x, *y)) {
            let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
        }
        if current != Some(cell.style()) {
            write_style(out, cell.style());
            current = Some(cell.style());
        }
        out.extend_from_slice(cell.symbol().as_bytes());
        after = Some((x + cell.cell_width().max(1), *y));
    }
    if !updates.is_empty() {
        out.extend_from_slice(b"\x1b[0m");
    }
}

/// Write every cell of `buf` out at the position it was laid out for.
#[cfg(test)]
pub(super) fn write_cells(out: &mut Vec<u8>, buf: &Buffer) {
    let area = buf.area;
    let before = out.len();
    let mut current: Option<Style> = None;
    for y in area.top()..area.bottom() {
        // One cursor move per run of cells, and a fresh one after any gap.
        let mut position = true;
        // Cells the glyph just written already covers.
        let mut covered = 0;
        for x in area.left()..area.right() {
            let Some(cell) = buf.cell((x, y)) else {
                position = true;
                continue;
            };
            if covered > 0 {
                // The tail of a wide glyph. The cursor is already past it, so writing
                // this cell would land beyond the glyph and shove the rest of the row
                // along -- which is why a backend's diff skips these too.
                covered -= 1;
                continue;
            }
            let width = cell.cell_width();
            if width == 0 {
                // Nothing to write, and the cursor stays where it is, so whatever
                // comes next has to be positioned again.
                position = true;
                continue;
            }
            if position {
                let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
                position = false;
            }
            if current != Some(cell.style()) {
                write_style(out, cell.style());
                current = Some(cell.style());
            }
            out.extend_from_slice(cell.symbol().as_bytes());
            covered = width - 1;
        }
    }
    if out.len() > before {
        out.extend_from_slice(b"\x1b[0m");
    }
}

/// Emit a style, from a clean slate each time.
///
/// A reset rather than a diff attribute by attribute: the runs are long -- a whole
/// status row is one of them -- so the bytes are free, and a reset cannot leave an
/// attribute behind that the next run never asked for.
fn write_style(out: &mut Vec<u8>, style: Style) {
    out.extend_from_slice(b"\x1b[0m");
    if let Some(fg) = style.fg {
        write_colour(out, fg, 38);
    }
    if let Some(bg) = style.bg {
        write_colour(out, bg, 48);
    }
    for (modifier, code) in [
        (Modifier::BOLD, 1),
        (Modifier::DIM, 2),
        (Modifier::ITALIC, 3),
        (Modifier::UNDERLINED, 4),
        (Modifier::REVERSED, 7),
    ] {
        if style.add_modifier.contains(modifier) {
            let _ = write!(out, "\x1b[{code}m");
        }
    }
}

/// One SGR colour. `base` is 38 for a foreground, 48 for a background.
fn write_colour(out: &mut Vec<u8>, colour: Color, base: u8) {
    match colour {
        // A cell always names a colour, `Reset` being the default one, and the reset
        // that opens every style has already said as much. Saying it again is eight
        // bytes on every run of a row that mostly has no colour of its own.
        Color::Reset => {}
        Color::Rgb(r, g, b) => {
            let _ = write!(out, "\x1b[{base};2;{r};{g};{b}m");
        }
        // Named colours included: they are the first sixteen palette entries, and
        // `5;n` reaches them exactly as the 30-37 codes do.
        other => {
            let _ = write!(out, "\x1b[{base};5;{}m", palette_index(other));
        }
    }
}

/// A named or indexed colour's place in the 256-colour palette.
fn palette_index(colour: Color) -> u8 {
    match colour {
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        Color::Indexed(index) => index,
        // Both handled by the caller.
        Color::Reset | Color::Rgb(..) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn buffer(area: Rect) -> Buffer {
        Buffer::empty(area)
    }

    #[test]
    fn a_run_of_one_style_is_positioned_and_set_once() {
        let mut buf = buffer(Rect::new(4, 2, 3, 1));
        buf.set_string(4, 2, "abc", Style::new().add_modifier(Modifier::REVERSED));
        let mut out = Vec::new();
        write_cells(&mut out, &buf);
        let text = String::from_utf8(out).unwrap();
        // One-based cursor move, then the style, then the cells, then a reset.
        assert_eq!(text, "\x1b[3;5H\x1b[0m\x1b[7mabc\x1b[0m");
    }

    #[test]
    fn a_change_of_style_starts_a_new_run_but_not_a_new_position() {
        let mut buf = buffer(Rect::new(0, 0, 4, 1));
        buf.set_string(0, 0, "ab", Style::new().fg(Color::Rgb(1, 2, 3)));
        buf.set_string(2, 0, "cd", Style::new().bg(Color::Indexed(9)));
        let mut out = Vec::new();
        write_cells(&mut out, &buf);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "\x1b[1;1H\x1b[0m\x1b[38;2;1;2;3mab\x1b[0m\x1b[48;5;9mcd\x1b[0m"
        );
        assert_eq!(text.matches('H').count(), 1, "one cursor move for the row");
    }

    #[test]
    fn the_tail_of_a_wide_glyph_is_not_written() {
        // A double-width glyph occupies two cells and the buffer blanks the second.
        // Writing that blank would land *after* the glyph, because the cursor is
        // already past it, and shove the rest of the row one cell along.
        let mut buf = buffer(Rect::new(0, 0, 4, 1));
        buf.set_string(0, 0, "漢x", Style::new());
        let mut out = Vec::new();
        write_cells(&mut out, &buf);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[1;1H\x1b[0m漢x \x1b[0m",
            "the glyph, then the cells after the one it covers"
        );
    }

    #[test]
    fn a_cell_with_nothing_in_it_is_stepped_over() {
        // Width zero moves no cursor, so the cell after it has to be positioned.
        let mut buf = buffer(Rect::new(0, 0, 3, 1));
        buf.set_string(0, 0, "abc", Style::new());
        buf.cell_mut((1, 0)).unwrap().set_symbol("");
        let mut out = Vec::new();
        write_cells(&mut out, &buf);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[1;1H\x1b[0ma\x1b[1;3Hc\x1b[0m"
        );
    }

    #[test]
    fn colours_become_sgr_and_the_default_is_left_to_the_reset() {
        let mut out = Vec::new();
        write_colour(&mut out, Color::Reset, 38);
        write_colour(&mut out, Color::Reset, 48);
        write_colour(&mut out, Color::Rgb(0x11, 0x22, 0x33), 38);
        write_colour(&mut out, Color::LightCyan, 48);
        write_colour(&mut out, Color::Indexed(200), 38);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[38;2;17;34;51m\x1b[48;5;14m\x1b[38;5;200m"
        );
    }
}
