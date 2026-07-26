//! The command menu: the overlay listing the prefix-key commands, and the one
//! piece of chrome you can point at.
//!
//! Laid out with ratatui, but not driven by it. A ratatui `Terminal` owns the
//! screen and diffs its way to the next one, which would put it in charge of the
//! cells our graphics placements live in -- the one thing this renderer cannot
//! give away. So the widgets render into a `Buffer` the exact size of the box and
//! that buffer is serialised to escapes here, leaving every other cell alone.
//!
//! What ratatui is actually for is the part that was hand-rolled before: padding,
//! alignment, styled spans, and a hit test that comes out of the same geometry the
//! drawing used, so a click cannot land on a row the box is not showing.

use std::io::Write as _;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Widget};

use super::paint::write_cells;
use crate::cli::ScaleMode;
use crate::term::input::Command;
use crate::term::{Metrics, kitty};

/// The box is white with dark ink, whichever way it ends up being painted.
///
/// Twice over, because one way is not enough. The cell colours are the whole
/// story in a terminal with no graphics protocol, which `--force` allows, and are
/// why the box looks right in Terminal.app or Alacritty.
///
/// Where the protocol does work the cell colour is invisible. Tiles are placed at
/// `z=-1` (see `term::kitty`): below the text, but *above* the cell background, so
/// a colour set there is painted under the remote screen and only the glyphs come
/// out on top -- dark text adrift on the wallpaper. Probing Ghostty settled it:
/// reverse video, an explicit `48;` pair and the basic pairs were every one of them
/// buried, and only an image of our own -- same z-index, higher id than any tile --
/// was composited over the desktop. So the backdrop and the highlight bar are
/// images, and the text is written on them.
const PAPER: Color = Color::Rgb(0xff, 0xff, 0xff);
const PAPER_RGB: (u8, u8, u8) = (0xff, 0xff, 0xff);

/// The row under the pointer. Light enough to read dark ink on, and the same
/// colour whether it arrives as an image or as a cell background.
const HOVER: Color = Color::Rgb(0xed, 0xe9, 0xfe);
const HOVER_RGB: (u8, u8, u8) = (0xed, 0xe9, 0xfe);

/// Inks. The background is set once by the block and left alone, so these only
/// ever change the glyph colour.
const INK: Color = Color::Rgb(0x11, 0x11, 0x11);
const ACCENT: Color = Color::Rgb(0x7c, 0x3a, 0xed);
const MUTED: Color = Color::Rgb(0x88, 0x88, 0x88);

/// Breathing room inside the box, in cells. There is no border to hold the text
/// off the edge, so the padding is the only thing that does.
const PAD_X: u16 = 3;
const PAD_Y: u16 = 1;

/// Least space between a label and the shortcut sitting against the right edge.
const GAP: u16 = 6;

/// Space between two options of a choice row.
const CHOICE_GAP: u16 = 2;

/// One line of the menu.
enum Entry {
    /// Heading of the whole box: name on the left, how to leave on the right.
    Title(String, String),
    /// A group of commands.
    Section(String),
    /// A command, the keys that reach it, and what a click on it runs.
    Item {
        label: String,
        keys: String,
        command: Command,
    },
    /// A row of alternatives, exactly one of which is in force. Which one that is
    /// arrives at draw time; the row itself only knows what is on offer.
    Choice {
        options: Vec<Option_>,
        keys: String,
    },
    Blank,
}

/// One alternative on a choice row.
///
/// Named awkwardly because `Option` is taken. It is drawn `[name]` when in force
/// and ` name ` when not, so the two are the same width and the geometry does not
/// shift when the selection moves -- which is what lets the hit test agree with the
/// drawing without being told which one is selected.
struct Option_ {
    name: &'static str,
    mode: ScaleMode,
}

impl Option_ {
    fn width(&self) -> u16 {
        cells(self.name) + 2
    }

    fn command(&self) -> Command {
        Command::Mode(self.mode)
    }
}

/// Every option with the columns it covers, relative to the left of the row.
///
/// The gap after an option belongs to it, so there is no dead strip between two
/// targets; the last one ends where its text does, because everything past it is
/// the shortcut's.
fn option_spans(options: &[Option_]) -> Vec<(&Option_, u16, u16)> {
    let mut out = Vec::with_capacity(options.len());
    let mut x = 0;
    for (i, option) in options.iter().enumerate() {
        let last = i + 1 == options.len();
        let end = x + option.width() + if last { 0 } else { CHOICE_GAP };
        out.push((option, x, end));
        x = end;
    }
    out
}

/// Columns the options of a choice row occupy together.
fn choice_width(options: &[Option_]) -> u16 {
    match option_spans(options).last() {
        Some((option, start, _)) => start + option.width(),
        None => 0,
    }
}

impl Entry {
    /// Columns this line needs, before padding.
    fn width(&self) -> u16 {
        match self {
            Entry::Blank => 0,
            Entry::Section(text) => cells(text),
            Entry::Title(left, right) => cells(left) + GAP + cells(right),
            Entry::Item { label, keys, .. } => cells(label) + GAP + cells(keys),
            Entry::Choice { options, keys } => choice_width(options) + GAP + cells(keys),
        }
    }

    /// What clicking this line does, `offset` cells in from where its text starts.
    ///
    /// Signed, because the padding to the left of the text comes out negative: it is
    /// part of a command's row, which is a target all the way across, but part of no
    /// single option on a row of them.
    fn command_at(&self, offset: i32) -> Option<Command> {
        match self {
            Entry::Item { command, .. } => Some(*command),
            Entry::Choice { options, .. } => option_spans(options)
                .into_iter()
                .find(|(_, start, end)| offset >= i32::from(*start) && offset < i32::from(*end))
                .map(|(option, ..)| option.command()),
            _ => None,
        }
    }
}

/// Width of a label in cells. Every one is a short ASCII literal from this file,
/// so the cast cannot lose anything.
fn cells(text: &str) -> u16 {
    text.chars().count() as u16
}

/// What sits under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// Somewhere else entirely: the remote screen, or the status line.
    Outside,
    /// On the menu, but not on anything a click would act on.
    Inside,
    /// On a command.
    Item { index: usize, command: Command },
}

/// The menu and what the pointer is on.
pub struct Menu {
    entries: Vec<Entry>,
    /// The entry under the pointer and the command it would run, which is only ever
    /// one a click would act on. The command is kept because a row of options has
    /// several, and the highlight has to know which.
    hover: Option<(usize, Command)>,
}

impl Menu {
    pub fn new(prefix: char) -> Self {
        let p = format!("ctrl+{}", prefix.to_ascii_lowercase());
        let item = |label: &str, keys: String, command| Entry::Item {
            label: label.to_string(),
            keys,
            command,
        };
        let entries = vec![
            Entry::Title("Commands".into(), "esc".into()),
            Entry::Blank,
            Entry::Section("Session".into()),
            item("Quit", format!("{p} q"), Command::Quit),
            Entry::Blank,
            Entry::Section("Screen".into()),
            item("Refresh in full", format!("{p} f"), Command::FullRefresh),
            item(
                "Renegotiate the remote size",
                format!("{p} r"),
                Command::Renegotiate,
            ),
            // One line each rather than one that says "arrows". A click can only mean
            // one direction, and the menu closing after it is what the keyboard does
            // too: the prefix disarms after a single command, so panning twice is two
            // chords there as it is two clicks here.
            item("Pan left", format!("{p} left"), Command::Pan(-1, 0)),
            item("Pan right", format!("{p} right"), Command::Pan(1, 0)),
            item("Pan up", format!("{p} up"), Command::Pan(0, -1)),
            item("Pan down", format!("{p} down"), Command::Pan(0, 1)),
            Entry::Blank,
            Entry::Section("Scaling".into()),
            // One row rather than four commands: the key steps through the modes
            // because a single binding cannot name one, but a click can, so the
            // options are offered side by side with the one in force marked.
            Entry::Choice {
                options: vec![
                    Option_ {
                        name: "Native",
                        mode: ScaleMode::Native,
                    },
                    Option_ {
                        name: "Fit",
                        mode: ScaleMode::Fit,
                    },
                    Option_ {
                        name: "Integer",
                        mode: ScaleMode::Integer,
                    },
                    Option_ {
                        name: "1:1",
                        mode: ScaleMode::OneToOne,
                    },
                ],
                keys: format!("{p} m"),
            },
            Entry::Blank,
            Entry::Section("View".into()),
            item(
                "Toggle view-only",
                format!("{p} v"),
                Command::ToggleViewOnly,
            ),
            item("Toggle statistics", format!("{p} c"), Command::ToggleStats),
        ];
        Self {
            entries,
            hover: None,
        }
    }

    /// Where the box goes, in zero-based screen cells, or `None` when the image
    /// area is too small to hold it.
    ///
    /// Everything -- drawing, erasing and hit testing -- comes through here, so
    /// the three cannot disagree about which cells the menu owns.
    fn area(&self, metrics: &Metrics) -> Option<Rect> {
        let inner = self.entries.iter().map(Entry::width).max().unwrap_or(0);
        let width = inner + PAD_X * 2;
        let height = u16::try_from(self.entries.len()).ok()? + PAD_Y * 2;
        let rows = metrics.image_rows();
        if width > metrics.cols || height > rows {
            return None;
        }
        Some(Rect {
            x: (metrics.cols - width) / 2,
            y: (rows - height) / 2,
            width,
            height,
        })
    }

    /// The cells the highlight covers, which is what the bar is placed over and
    /// what the cells behind it are coloured -- one function, so the image and the
    /// colour cannot land on different cells.
    ///
    /// A command's target is its whole row. An option's is the option itself: the
    /// row holds four of them, and a bar across all of it would say the pointer is
    /// on something it is not.
    fn hover_bar(&self, area: Rect) -> Option<Rect> {
        let (index, command) = self.hover?;
        let y = area.y + PAD_Y + u16::try_from(index).ok()?;
        match self.entries.get(index)? {
            Entry::Choice { options, .. } => {
                let (option, start, _) = option_spans(options)
                    .into_iter()
                    .find(|(option, ..)| option.command() == command)?;
                Some(Rect {
                    x: area.x + PAD_X + start,
                    y,
                    width: option.width(),
                    height: 1,
                })
            }
            _ => Some(Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            }),
        }
    }

    /// Move the highlight to whatever the pointer is on.
    pub fn set_hover(&mut self, hit: Hit) {
        self.hover = match hit {
            Hit::Item { index, command } => Some((index, command)),
            Hit::Outside | Hit::Inside => None,
        };
    }

    /// Forget the highlight, for a menu that is being put away.
    pub fn clear_hover(&mut self) {
        self.hover = None;
    }

    /// What a pointer at zero-based cell `(col, row)` is on.
    pub fn hit(&self, metrics: &Metrics, col: u16, row: u16) -> Hit {
        let Some(area) = self.area(metrics) else {
            return Hit::Outside;
        };
        if !area.contains(Position::new(col, row)) {
            return Hit::Outside;
        }
        // Above the first entry the subtraction underflows, and below the last one
        // there is no entry to find.
        let Some(index) = row.checked_sub(area.y + PAD_Y).map(usize::from) else {
            return Hit::Inside;
        };
        let Some(entry) = self.entries.get(index) else {
            return Hit::Inside;
        };
        // Counted from where the text starts, so the padding comes out negative: it
        // belongs to a command's row, but to no single option on a row of them.
        let offset = i32::from(col) - i32::from(area.x + PAD_X);
        match entry.command_at(offset) {
            Some(command) => Hit::Item { index, command },
            None => Hit::Inside,
        }
    }

    /// Draw the menu, centred over the remote screen.
    ///
    /// `mode` is the scaling in force, which the row of scaling options marks. It is
    /// passed in rather than remembered: the session owns it, and a second copy here
    /// could only ever drift from the first.
    ///
    /// No border: the box is held off the remote screen by its padding and its own
    /// backdrop, which is a good deal quieter than a rule around the outside.
    pub fn draw(&self, out: &mut Vec<u8>, metrics: &Metrics, mode: ScaleMode) {
        let Some(area) = self.area(metrics) else {
            return;
        };

        // The backdrop and the highlight first, then the text on top of them. All
        // three go out after the frame's tiles, but it is the image id rather than
        // the order that keeps them above: at equal z-index the higher id wins.
        kitty::place_solid(
            out,
            kitty::OVERLAY_IMAGE_ID,
            usize::from(area.x) + 1,
            usize::from(area.y) + 1,
            usize::from(area.width),
            usize::from(area.height),
            PAPER_RGB,
        );
        // The bar is deleted before it is placed, every frame, and the delete is not
        // conditional on there being one. Re-sending the id is all a tile needs,
        // because a tile is placed on the same cells every time -- but this one
        // follows the pointer, and the placement it had a row up outlives the
        // retransmit. Without the delete, every row the pointer touched keeps its
        // stripe: only the rows that are not commands were ever clearing them,
        // which is why it showed up between two items of one section and nowhere
        // else. The backdrop needs no such thing, moving only on a resize, and a
        // relayout drops every image before redrawing.
        kitty::delete_image(out, kitty::MENU_HIGHLIGHT_IMAGE_ID);
        if let Some(bar) = self.hover_bar(area) {
            kitty::place_solid(
                out,
                kitty::MENU_HIGHLIGHT_IMAGE_ID,
                usize::from(bar.x) + 1,
                usize::from(bar.y) + 1,
                usize::from(bar.width),
                usize::from(bar.height),
                HOVER_RGB,
            );
        }

        let mut buf = Buffer::empty(area);
        MenuView { menu: self, mode }.render(area, &mut buf);
        write_cells(out, &buf);
    }

    /// Take the menu off the screen: blank its cells and drop its images.
    ///
    /// Both halves are needed, and redrawing the remote screen is neither of them.
    /// The glyphs are text, which no repaint of an image below them can erase, and
    /// the backdrop is an image of ours that outranks every tile and would otherwise
    /// stay on top of them for ever. The cells go back to default attributes, which
    /// is what makes them transparent to the tiles again.
    pub fn clear(&self, out: &mut Vec<u8>, metrics: &Metrics) {
        let Some(area) = self.area(metrics) else {
            return;
        };

        // Text first, then images, as a relayout does it: an erase may take the
        // placements under it along too.
        out.extend_from_slice(b"\x1b[0m");
        for y in area.top()..area.bottom() {
            let _ = write!(out, "\x1b[{};{}H", y + 1, area.x + 1);
            out.extend(std::iter::repeat_n(b' ', usize::from(area.width)));
        }
        kitty::delete_image(out, kitty::OVERLAY_IMAGE_ID);
        kitty::delete_image(out, kitty::MENU_HIGHLIGHT_IMAGE_ID);
    }
}

/// The menu together with the scaling mode it has to show as in force. A `Widget`
/// takes no arguments beyond the area, and this is the one thing the entries cannot
/// know for themselves.
struct MenuView<'a> {
    menu: &'a Menu,
    mode: ScaleMode,
}

impl Widget for MenuView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::new()
            .padding(Padding::symmetric(PAD_X, PAD_Y))
            .style(Style::new().bg(PAPER).fg(INK));
        let inner = block.inner(area);
        block.render(area, buf);

        // The cells under the highlight, coloured for a terminal with no graphics
        // protocol. The same rectangle the bar image is placed over.
        if let Some(bar) = self.menu.hover_bar(area) {
            buf.set_style(bar, Style::new().bg(HOVER));
        }

        for (index, entry) in self.menu.entries.iter().enumerate() {
            let Some(y) = u16::try_from(index).ok().map(|i| inner.y + i) else {
                break;
            };
            if y >= inner.bottom() {
                break;
            }
            let row = Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            };
            let hovered = |command| self.menu.hover == Some((index, command));

            match entry {
                Entry::Blank => {}
                Entry::Section(text) => {
                    Line::from(Span::styled(text.as_str(), Style::new().fg(ACCENT)))
                        .render(row, buf);
                }
                Entry::Title(left, right) => split(
                    buf,
                    row,
                    left,
                    right,
                    Style::new().fg(INK).add_modifier(Modifier::BOLD),
                ),
                Entry::Item {
                    label,
                    keys,
                    command,
                } => {
                    let ink = if hovered(*command) {
                        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().fg(INK)
                    };
                    split(buf, row, label, keys, ink);
                }
                Entry::Choice { options, keys } => {
                    for (option, start, _) in option_spans(options) {
                        // Brackets on the one in force, blanks on the rest, so every
                        // option is the same width whichever is selected and the
                        // options do not shuffle sideways as it changes.
                        let selected = option.mode == self.mode;
                        let mut style = if selected {
                            Style::new().fg(INK).add_modifier(Modifier::BOLD)
                        } else {
                            Style::new().fg(MUTED)
                        };
                        if hovered(option.command()) {
                            style = style.fg(ACCENT).add_modifier(Modifier::UNDERLINED);
                        }
                        let text = if selected {
                            format!("[{}]", option.name)
                        } else {
                            format!(" {} ", option.name)
                        };
                        let at = Rect {
                            x: row.x + start,
                            width: option.width(),
                            ..row
                        };
                        Line::from(Span::styled(text, style)).render(at, buf);
                    }
                    Line::from(Span::styled(keys.as_str(), Style::new().fg(MUTED)))
                        .right_aligned()
                        .render(row, buf);
                }
            }
        }
    }
}

/// A label on the left, its keys pushed against the right edge.
///
/// The colour goes on the spans rather than on the lines, because a `Line` styles
/// the whole row it is given and the second one would repaint the first.
fn split(buf: &mut Buffer, row: Rect, left: &str, right: &str, ink: Style) {
    Line::from(Span::styled(left, ink)).render(row, buf);
    if !right.is_empty() {
        Line::from(Span::styled(right, Style::new().fg(MUTED)))
            .right_aligned()
            .render(row, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ScaleMode;

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

    /// The menu as it lands on screen: one string per positioned row, with the
    /// escapes and the image payloads taken out.
    fn rendered(buf: &[u8]) -> Vec<String> {
        let text = String::from_utf8(buf.to_vec()).unwrap();
        let mut lines: Vec<String> = Vec::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => match chars.next() {
                    // CSI: parameters then a letter. `H` starts a fresh row.
                    Some('[') => {
                        for c in chars.by_ref() {
                            if c.is_ascii_alphabetic() {
                                if c == 'H' {
                                    lines.push(String::new());
                                }
                                break;
                            }
                        }
                    }
                    // APC: an image, terminated by ESC backslash.
                    Some('_') => {
                        while let Some(c) = chars.next() {
                            if c == '\x1b' {
                                chars.next();
                                break;
                            }
                        }
                    }
                    _ => {}
                },
                c => {
                    if let Some(line) = lines.last_mut() {
                        line.push(c);
                    }
                }
            }
        }
        while lines.first().is_some_and(String::is_empty) {
            lines.remove(0);
        }
        lines
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
    fn menu_fits_or_is_skipped() {
        let menu = Menu::new('a');
        let mut out = Vec::new();
        menu.draw(&mut out, &metrics(100, 40), ScaleMode::Fit);
        assert!(!out.is_empty());

        out.clear();
        menu.draw(&mut out, &metrics(20, 6), ScaleMode::Fit);
        assert!(out.is_empty(), "must not draw a menu that does not fit");
    }

    #[test]
    fn menu_carries_its_own_background() {
        // Tiles sit at z=-1: below the text, but above the cell background, so no
        // SGR fill can be seen behind the glyphs. The backdrop has to be an image
        // of our own, outranking every tile id so it is composited over them.
        let menu = Menu::new('a');
        let mut out = Vec::new();
        menu.draw(&mut out, &metrics(100, 40), ScaleMode::Fit);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(&format!("i={},", kitty::OVERLAY_IMAGE_ID)),
            "the menu must place a backdrop image, not colour its cells"
        );
        assert!(
            text.contains("z=-1"),
            "the backdrop shares the tiles' z-index; above it would cover the text"
        );
        assert!(
            text.contains("\x1b[48;2;255;255;255m"),
            "the cells are coloured too, for a terminal with no graphics protocol"
        );
        assert!(
            !text.contains("\x1b[7m"),
            "reverse video would follow the theme, and is buried by the image anyway"
        );
        // The backdrop goes down before the text that sits on it.
        let backdrop = text.find("\x1b_Ga=T").expect("no backdrop");
        assert!(backdrop < text.find("Commands").expect("no title"));
    }

    #[test]
    fn menu_is_padded_and_right_aligns_its_shortcuts() {
        let menu = Menu::new('a');
        let mut out = Vec::new();
        menu.draw(&mut out, &metrics(100, 40), ScaleMode::Fit);
        let lines = rendered(&out);

        assert!(!lines.is_empty(), "nothing was drawn");
        let width = lines[0].chars().count();
        assert!(
            lines.iter().all(|l| l.chars().count() == width),
            "every row must fill the box, or the backdrop shows through the gaps"
        );
        assert!(
            !lines.iter().any(|l| l.contains(['┌', '─', '│', '└'])),
            "the box is borderless"
        );
        // Padded top and bottom, and held off both edges.
        assert!(lines[0].trim().is_empty(), "no padding above");
        assert!(lines[lines.len() - 1].trim().is_empty(), "no padding below");
        for line in lines.iter().filter(|l| !l.trim().is_empty()) {
            let left = line.len() - line.trim_start().len();
            let right = line.len() - line.trim_end().len();
            assert!(left >= usize::from(PAD_X), "row is not indented: {line:?}");
            assert!(
                right >= usize::from(PAD_X),
                "row runs into the right edge: {line:?}"
            );
        }
        // Every shortcut ends in the same column.
        let ends: Vec<usize> = lines
            .iter()
            .filter(|l| l.contains("ctrl+a"))
            .map(|l| l.trim_end().chars().count())
            .collect();
        assert!(ends.len() > 5, "expected the shortcut list");
        assert!(
            ends.iter().all(|e| *e == ends[0]),
            "shortcuts are not flush right: {ends:?}"
        );
    }

    #[test]
    fn clearing_the_menu_blanks_every_cell_it_drew() {
        // Repainting the image cannot undraw the menu, because the image is
        // composited below the text. The cells have to be erased, and the erase has
        // to cover exactly the rectangle that was drawn or a frame of it survives.
        let menu = Menu::new('a');
        let m = metrics(100, 40);
        let mut drawn = Vec::new();
        menu.draw(&mut drawn, &m, ScaleMode::Fit);
        let mut cleared = Vec::new();
        menu.clear(&mut cleared, &m);

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
            "the erase must cover the same cells the menu drew"
        );
        let text = String::from_utf8(cleared).unwrap();
        assert!(
            text.starts_with("\x1b[0m"),
            "the cells must go back to default attributes to be transparent again"
        );
        for id in [kitty::OVERLAY_IMAGE_ID, kitty::MENU_HIGHLIGHT_IMAGE_ID] {
            assert!(
                text.contains(&format!("a=d,d=I,i={id}")),
                "image {id} outranks every tile and has to be deleted too"
            );
        }
        // Nothing but blanks between the cursor moves: a leftover border would
        // still be on screen.
        for chunk in text.split("\x1b[").skip(2) {
            let body = chunk.split_once('H').map(|(_, b)| b).unwrap_or("");
            // The image deletes ride along after the last row's blanks.
            let body = body.split('\x1b').next().unwrap_or("");
            assert!(
                body.chars().all(|c| c == ' '),
                "the erase wrote something other than blanks: {body:?}"
            );
        }
    }

    #[test]
    fn a_menu_that_does_not_fit_is_not_cleared_either() {
        // Otherwise the erase would blank a rectangle that was never drawn.
        let mut out = Vec::new();
        Menu::new('a').clear(&mut out, &metrics(20, 6));
        assert!(out.is_empty());
    }

    #[test]
    fn menu_never_touches_the_status_row() {
        // Big enough that the menu actually draws: at 20 rows it does not fit and
        // the check below would pass by having nothing to look at.
        let m = metrics(100, 40);
        let mut out = Vec::new();
        Menu::new('a').draw(&mut out, &m, ScaleMode::Fit);
        let text = String::from_utf8(out).unwrap();
        // Every cursor move must stay above the last row. Only sequences that
        // actually terminate in `H` are positions: the colour set-up ends in `m`,
        // and its parameters would otherwise be read as a row number.
        for part in text.split("\x1b[").skip(1) {
            if let Some((coords, _)) = part.split_once('H')
                && let Some((r, _)) = coords.split_once(';')
                && let Ok(row) = r.parse::<u16>()
            {
                assert!(row < m.rows, "menu wrote to row {row}");
            }
        }
    }

    /// The cell a label's row sits on, found by drawing and looking.
    fn row_of(menu: &Menu, m: &Metrics, label: &str) -> u16 {
        let area = menu.area(m).expect("the menu must fit");
        let index = menu
            .entries
            .iter()
            .position(|e| matches!(e, Entry::Item { label: l, .. } if l == label))
            .expect("no such item");
        area.y + PAD_Y + u16::try_from(index).unwrap()
    }

    /// The row of scaling options, and the column its first option starts at.
    fn choice_row(menu: &Menu, m: &Metrics) -> (u16, u16) {
        let area = menu.area(m).expect("the menu must fit");
        let index = menu
            .entries
            .iter()
            .position(|e| matches!(e, Entry::Choice { .. }))
            .expect("no row of options");
        (
            area.y + PAD_Y + u16::try_from(index).unwrap(),
            area.x + PAD_X,
        )
    }

    #[test]
    fn a_click_on_a_row_finds_that_rows_command() {
        let menu = Menu::new('a');
        let m = metrics(100, 40);
        let area = menu.area(&m).unwrap();

        for (label, want) in [
            ("Quit", Command::Quit),
            ("Toggle view-only", Command::ToggleViewOnly),
            ("Refresh in full", Command::FullRefresh),
            // One direction each: a click can only mean one of the four.
            ("Pan left", Command::Pan(-1, 0)),
            ("Pan down", Command::Pan(0, 1)),
            ("Toggle statistics", Command::ToggleStats),
        ] {
            let row = row_of(&menu, &m, label);
            // The whole width of the box is the target, padding included.
            for col in [area.x, area.x + area.width / 2, area.x + area.width - 1] {
                match menu.hit(&m, col, row) {
                    Hit::Item { command, .. } => assert_eq!(command, want, "{label} at {col}"),
                    other => panic!("{label} at column {col} hit {other:?}"),
                }
            }
        }
    }

    #[test]
    fn the_padding_and_the_blank_rows_are_not_commands() {
        let menu = Menu::new('a');
        let m = metrics(100, 40);
        let area = menu.area(&m).unwrap();
        let col = area.x + area.width / 2;

        // The title, the section headings and the blank lines are inside the box
        // but nothing to click.
        assert_eq!(menu.hit(&m, col, area.y), Hit::Inside, "top padding");
        assert_eq!(
            menu.hit(&m, col, area.bottom() - 1),
            Hit::Inside,
            "bottom padding"
        );
        assert_eq!(menu.hit(&m, col, area.y + PAD_Y), Hit::Inside, "the title");

        // A blank line and a section heading, each a row of its own among the
        // commands, and neither of them anything to click.
        for (kind, is_kind) in [
            (
                "blank",
                (|e| matches!(e, Entry::Blank)) as fn(&Entry) -> bool,
            ),
            ("section", |e| matches!(e, Entry::Section(_))),
        ] {
            let index = menu
                .entries
                .iter()
                .position(is_kind)
                .unwrap_or_else(|| panic!("the menu has no {kind} row"));
            let row = area.y + PAD_Y + u16::try_from(index).unwrap();
            assert_eq!(menu.hit(&m, col, row), Hit::Inside, "the {kind} row");
        }
    }

    #[test]
    fn everything_off_the_box_is_outside() {
        let menu = Menu::new('a');
        let m = metrics(100, 40);
        let area = menu.area(&m).unwrap();
        for (col, row) in [
            (0, 0),
            (area.x - 1, area.y + PAD_Y + 3),
            (area.right(), area.y + PAD_Y + 3),
            (area.x, area.y - 1),
            (area.x, area.bottom()),
            (area.x, m.rows - 1), // the status line
        ] {
            assert_eq!(menu.hit(&m, col, row), Hit::Outside, "{col},{row}");
        }
        // A menu that does not fit was never drawn, so no cell belongs to it.
        assert_eq!(menu.hit(&metrics(20, 6), 10, 3), Hit::Outside);
    }

    #[test]
    fn the_hovered_row_is_highlighted_and_the_others_are_not() {
        let m = metrics(100, 40);
        let mut menu = Menu::new('a');
        let row = row_of(&menu, &m, "Quit");
        menu.set_hover(menu.hit(&m, m.cols / 2, row));

        let mut out = Vec::new();
        menu.draw(&mut out, &m, ScaleMode::Fit);
        let text = String::from_utf8(out).unwrap();
        // A bar the pointer's row wide, drawn as an image with an id above the
        // backdrop's: a cell background would be buried, and a lower id would put
        // the bar under the very thing it has to be seen on.
        assert!(
            text.contains(&format!("i={},s=2,v=2,c=", kitty::MENU_HIGHLIGHT_IMAGE_ID)),
            "the highlight must be an image; a cell background is buried"
        );
        assert!(
            text.contains("\x1b[48;2;237;233;254m"),
            "the cells are coloured too, for a terminal with no graphics protocol"
        );
        // And on the row that was pointed at, not merely somewhere on the box: the
        // hit test and the drawing have to agree about which entry is which.
        let area = menu.area(&m).unwrap();
        let cup = positions(&text.as_bytes()[..placement(&text)])
            .pop()
            .expect("no cursor move");
        assert_eq!(
            cup,
            (row + 1, area.x + 1),
            "the bar is not on the row the pointer hit"
        );
        assert!(
            text.contains(&format!(",c={},r=1;", area.width)),
            "the bar must be one row tall and the width of the box"
        );
        // The old bar goes before the new one is placed. Re-sending the id is not
        // enough for a placement that moves: the row above would keep its stripe,
        // which is the bug this guards.
        assert!(
            deletion(&text) < placement(&text),
            "the bar must be deleted before it is placed again"
        );

        // Pointing at nothing takes it away and puts nothing back.
        let mut out = Vec::new();
        menu.set_hover(Hit::Outside);
        menu.draw(&mut out, &m, ScaleMode::Fit);
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("\x1b[48;2;237;233;254m"));
        assert!(
            text.contains(&format!("a=d,d=I,i={}", kitty::MENU_HIGHLIGHT_IMAGE_ID)),
            "a bar that is no longer wanted has to be deleted, not just left off"
        );
        assert!(
            !text.contains(&format!("i={},s=2", kitty::MENU_HIGHLIGHT_IMAGE_ID)),
            "nothing to highlight, so nothing to place"
        );
    }

    /// Where the highlight bar is placed, and where the one before it is deleted.
    /// Both commands name the same id, so the transmit keys tell them apart.
    fn placement(text: &str) -> usize {
        text.find(&format!("i={},s=2", kitty::MENU_HIGHLIGHT_IMAGE_ID))
            .expect("the bar was never placed")
    }

    fn deletion(text: &str) -> usize {
        text.find(&format!("a=d,d=I,i={}", kitty::MENU_HIGHLIGHT_IMAGE_ID))
            .expect("the bar was never deleted")
    }

    #[test]
    fn moving_the_highlight_leaves_no_stripe_behind() {
        // Two entries of one section, which is where this went wrong: the pointer
        // crosses no heading between them, so nothing but the delete below clears the
        // row it came from.
        let m = metrics(100, 40);
        let mut menu = Menu::new('a');
        let col = m.cols / 2;
        let left = menu.area(&m).unwrap().x + 1;
        let mut rows = Vec::new();
        for label in ["Pan left", "Pan right", "Pan up"] {
            let row = row_of(&menu, &m, label);
            menu.set_hover(menu.hit(&m, col, row));
            let mut out = Vec::new();
            menu.draw(&mut out, &m, ScaleMode::Fit);
            let text = String::from_utf8(out).unwrap();
            assert!(
                deletion(&text) < placement(&text),
                "{label}: the bar has to be deleted before it is placed again"
            );
            // One bar per frame, on this row and no other.
            assert_eq!(
                text.matches(&format!("i={},s=2", kitty::MENU_HIGHLIGHT_IMAGE_ID))
                    .count(),
                1
            );
            let cup = positions(&text.as_bytes()[..placement(&text)])
                .pop()
                .unwrap();
            assert_eq!(cup, (row + 1, left), "{label}: the bar is on the wrong row");
            rows.push(row);
        }
        // Adjacent rows, or the test is not exercising the case it claims to.
        assert_eq!(rows[1], rows[0] + 1);
        assert_eq!(rows[2], rows[1] + 1);
    }

    /// Every option of the scaling row with the column its text starts at.
    fn options(menu: &Menu, m: &Metrics) -> Vec<(ScaleMode, u16, u16)> {
        let (_, left) = choice_row(menu, m);
        let Some(Entry::Choice { options, .. }) = menu
            .entries
            .iter()
            .find(|e| matches!(e, Entry::Choice { .. }))
        else {
            panic!("no row of options");
        };
        option_spans(options)
            .into_iter()
            .map(|(option, start, end)| (option.mode, left + start, left + end))
            .collect()
    }

    #[test]
    fn each_scaling_option_is_its_own_target() {
        let menu = Menu::new('a');
        let m = metrics(100, 40);
        let (row, left) = choice_row(&menu, &m);

        for (mode, start, end) in options(&menu, &m) {
            for col in [start, end - 1] {
                match menu.hit(&m, col, row) {
                    Hit::Item { command, .. } => assert_eq!(
                        command,
                        Command::Mode(mode),
                        "column {col} of the scaling row"
                    ),
                    other => panic!("column {col} of the scaling row hit {other:?}"),
                }
            }
        }

        // The padding to the left of the first option belongs to no option, and
        // neither does the stretch on the right where the shortcut lives. A row of
        // choices is not a target all the way across the way a command's row is.
        assert_eq!(menu.hit(&m, left - 1, row), Hit::Inside, "the left padding");
        let last = options(&menu, &m).last().copied().unwrap();
        assert_eq!(
            menu.hit(&m, last.2, row),
            Hit::Inside,
            "past the last option"
        );
    }

    #[test]
    fn the_option_in_force_is_the_marked_one() {
        let m = metrics(100, 40);
        let menu = Menu::new('a');
        let (row, _) = choice_row(&menu, &m);

        // Whichever mode is in force wears the brackets, and only that one.
        for (mode, name) in [
            (ScaleMode::Native, "Native"),
            (ScaleMode::Fit, "Fit"),
            (ScaleMode::Integer, "Integer"),
            (ScaleMode::OneToOne, "1:1"),
        ] {
            let mut out = Vec::new();
            menu.draw(&mut out, &m, mode);
            let lines = rendered(&out);
            let drawn = &lines[usize::from(row - menu.area(&m).unwrap().y)];
            assert!(drawn.contains(&format!("[{name}]")), "{mode:?}: {drawn:?}");
            assert_eq!(drawn.matches('[').count(), 1, "{mode:?}: {drawn:?}");
            // And the row is the same width whichever it is, or the options would
            // shuffle sideways as the mode changed and the hit test would be looking
            // in the wrong place.
            assert_eq!(
                drawn.chars().count(),
                usize::from(menu.area(&m).unwrap().width)
            );
            assert!(
                drawn.contains("ctrl+a m"),
                "the key still cycles: {drawn:?}"
            );
        }
    }

    #[test]
    fn the_highlight_covers_one_option_not_the_whole_row() {
        // A row of four targets cannot take a bar across all of it: that would say
        // the pointer is on things it is not.
        let m = metrics(100, 40);
        let mut menu = Menu::new('a');
        let (row, _) = choice_row(&menu, &m);
        let area = menu.area(&m).unwrap();
        let (mode, start, _) = options(&menu, &m)[2];

        menu.set_hover(menu.hit(&m, start, row));
        let mut out = Vec::new();
        menu.draw(&mut out, &m, ScaleMode::Fit);
        let text = String::from_utf8(out).unwrap();

        let bar = menu.hover_bar(area).expect("nothing highlighted");
        assert_eq!(bar.x, start, "the bar does not start at the option");
        assert_eq!(bar.width, cells("Integer") + 2);
        assert!(bar.width < area.width, "the bar spans the whole row");
        assert_eq!(mode, ScaleMode::Integer);
        // And the image is placed on exactly those cells.
        let cup = positions(&text.as_bytes()[..placement(&text)])
            .pop()
            .unwrap();
        assert_eq!(cup, (bar.y + 1, bar.x + 1));
        assert!(text.contains(&format!(",c={},r=1;", bar.width)));

        // The option is underlined as well as barred. Nothing else in the box
        // underlines anything, so this is the one cue that survives a terminal with
        // no graphics protocol *and* no colour.
        assert!(text.contains("\x1b[4m"), "the option is not underlined");
        let mut plain = Vec::new();
        menu.clear_hover();
        menu.draw(&mut plain, &m, ScaleMode::Fit);
        assert!(!String::from_utf8(plain).unwrap().contains("\x1b[4m"));
    }

    #[test]
    fn only_commands_can_be_hovered() {
        let m = metrics(100, 40);
        let mut menu = Menu::new('a');
        let col = m.cols / 2;
        menu.set_hover(menu.hit(&m, col, row_of(&menu, &m, "Quit")));
        assert!(menu.hover.is_some());
        // A heading under the pointer clears it rather than highlighting a row no
        // click would act on.
        menu.set_hover(menu.hit(&m, col, menu.area(&m).unwrap().y + PAD_Y));
        assert_eq!(menu.hover, None);
    }
}
