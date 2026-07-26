//! The command menu: the overlay listing the prefix-key commands, and the piece of
//! chrome with more than one thing on it you can point at.
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
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Widget};

use super::paint::write_cells;
use super::theme::{Palette, Theme, colour};
use crate::cli::ScaleMode;
use crate::term::input::Command;
use crate::term::{Metrics, kitty};

// Every colour comes from the palette (see `ui::theme`), and each one is needed
// twice over, because one way is not enough.
//
// The cell colours are the whole story in a terminal with no graphics protocol,
// which `--force` allows, and are why the box looks right in Terminal.app or
// Alacritty.
//
// Where the protocol does work the cell colour is invisible. Tiles are placed at
// `z=-1` (see `term::kitty`): below the text, but *above* the cell background, so
// a colour set there is painted under the remote screen and only the glyphs come
// out on top -- dark text adrift on the wallpaper. Probing Ghostty settled it:
// reverse video, an explicit `48;` pair and the basic pairs were every one of them
// buried, and only an image of our own -- same z-index, higher id than any tile --
// was composited over the desktop. So the backdrop and the highlight bar are
// images, and the text is written on them.

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
    /// Heading of the whole box: its name, and how to leave. The second is a
    /// target as well as a label, being the one thing in here that always ends the
    /// menu rather than doing something and leaving it up.
    Title {
        name: String,
        close: String,
    },
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
    command: Command,
}

impl Option_ {
    fn width(&self) -> u16 {
        cells(self.name) + 2
    }

    /// Is this the one in force?
    ///
    /// Read off the command rather than stored beside it: an option's command is
    /// precisely the request to be the one in force, so it already says which state
    /// would make it so, and a second copy of that could only disagree.
    fn in_force(&self, state: State) -> bool {
        match self.command {
            Command::Mode(mode) => state.mode == mode,
            Command::Theme(theme) => state.theme == theme,
            _ => false,
        }
    }
}

/// What the menu has to show as being in force.
///
/// Passed in at draw time rather than remembered: the session owns both of these, and
/// a second copy here could only ever drift from the first.
#[derive(Debug, Clone, Copy)]
pub struct State {
    pub mode: ScaleMode,
    pub theme: Theme,
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

/// Columns the title's dismissal covers, relative to the left of the row.
///
/// Right-aligned, like every shortcut, so it starts where it ends up rather than at
/// a fixed offset. Drawing and hit testing both come through here, or a click would
/// land next to the button rather than on it.
fn close_span(close: &str, width: u16) -> (u16, u16) {
    (width.saturating_sub(cells(close)), width)
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
            Entry::Title { name, close } => cells(name) + GAP + cells(close),
            Entry::Item { label, keys, .. } => cells(label) + GAP + cells(keys),
            Entry::Choice { options, keys } => choice_width(options) + GAP + cells(keys),
        }
    }

    /// What clicking this line does, `offset` cells in from where its text starts,
    /// on a row `width` cells wide.
    ///
    /// The offset is signed because the padding to the left of the text comes out
    /// negative: it is part of a command's row, which is a target all the way across,
    /// but part of no single option on a row of them. The width is needed because the
    /// title's dismissal is pushed against the right edge, so where it starts depends
    /// on how wide the box turned out.
    fn command_at(&self, offset: i32, width: u16) -> Option<Command> {
        match self {
            Entry::Item { command, .. } => Some(*command),
            Entry::Choice { options, .. } => option_spans(options)
                .into_iter()
                .find(|(_, start, end)| offset >= i32::from(*start) && offset < i32::from(*end))
                .map(|(option, ..)| option.command),
            Entry::Title { close, .. } => {
                let (start, end) = close_span(close, width);
                (offset >= i32::from(start) && offset < i32::from(end)).then_some(Command::Menu)
            }
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
            Entry::Title {
                name: "Command menu".into(),
                close: super::CLOSE.into(),
            },
            Entry::Blank,
            Entry::Section("Session".into()),
            // First, and the only line that names the binding that opened the box.
            // Clicking it closes the box, which is what toggling something already
            // showing means.
            item("Toggle command menu", format!("{p} p"), Command::Menu),
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
                        command: Command::Mode(ScaleMode::Native),
                    },
                    Option_ {
                        name: "Fit",
                        command: Command::Mode(ScaleMode::Fit),
                    },
                    Option_ {
                        name: "Integer",
                        command: Command::Mode(ScaleMode::Integer),
                    },
                    Option_ {
                        name: "1:1",
                        command: Command::Mode(ScaleMode::OneToOne),
                    },
                ],
                keys: format!("{p} m"),
            },
            Entry::Blank,
            Entry::Section("Theme".into()),
            // No keys: two of anything is a choice rather than a cycle, and a click
            // names one where a single binding could only alternate.
            Entry::Choice {
                options: vec![
                    Option_ {
                        name: "Dark",
                        command: Command::Theme(Theme::Dark),
                    },
                    Option_ {
                        name: "Light",
                        command: Command::Theme(Theme::Light),
                    },
                ],
                keys: String::new(),
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
    /// A command's target is its whole row, and a bar across the row is the answer. An
    /// option's is the option itself, that row holding several, and a bar across all of it
    /// would say the pointer is on things it is not. The title's button gets no bar at all.
    fn hover_bar(&self, area: Rect) -> Option<Rect> {
        let (index, command) = self.hover?;
        let y = area.y + PAD_Y + u16::try_from(index).ok()?;
        let span = |start: u16, width: u16| {
            Some(Rect {
                x: area.x + PAD_X + start,
                y,
                width,
                height: 1,
            })
        };
        match self.entries.get(index)? {
            Entry::Choice { options, .. } => {
                let (option, start, _) = option_spans(options)
                    .into_iter()
                    .find(|(option, ..)| option.command == command)?;
                span(start, option.width())
            }
            // The one target with no bar: it is a button in the palette's own styling,
            // which lifts by colour alone, and a bar behind it would be a second mark for
            // one thing -- as well as the one difference between this button and the
            // popup's, which has no bar to give it (see `theme::Palette::close_button`).
            Entry::Title { .. } => None,
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
        match entry.command_at(offset, area.width.saturating_sub(PAD_X * 2)) {
            Some(command) => Hit::Item { index, command },
            None => Hit::Inside,
        }
    }

    /// Draw the menu, centred over the remote screen.
    ///
    /// `state` says which option each row of them should mark as in force.
    ///
    /// No border: the box is held off the remote screen by its padding and its own
    /// backdrop, which is a good deal quieter than a rule around the outside.
    pub fn draw(&self, out: &mut Vec<u8>, metrics: &Metrics, state: State) {
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
            state.theme.palette().paper,
        );
        // The bar moves with the pointer, and every placement releases the one it
        // replaces (see `term::kitty`), so a row the pointer has left keeps nothing.
        // Only the case with no bar at all needs saying here: nothing is placed, so
        // nothing releases the last one, and it would sit there until the menu closed.
        match self.hover_bar(area) {
            Some(bar) => kitty::place_solid(
                out,
                kitty::MENU_HIGHLIGHT_IMAGE_ID,
                usize::from(bar.x) + 1,
                usize::from(bar.y) + 1,
                usize::from(bar.width),
                usize::from(bar.height),
                state.theme.palette().hover,
            ),
            None => kitty::delete_image(out, kitty::MENU_HIGHLIGHT_IMAGE_ID),
        }

        let mut buf = Buffer::empty(area);
        MenuView { menu: self, state }.render(area, &mut buf);
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

/// The menu together with what it has to show as in force. A `Widget` takes no
/// arguments beyond the area, and this is the one thing the entries cannot know for
/// themselves.
struct MenuView<'a> {
    menu: &'a Menu,
    state: State,
}

impl Widget for MenuView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let ink = self.state.theme.palette();
        let block = Block::new()
            .padding(Padding::symmetric(PAD_X, PAD_Y))
            .style(Style::new().bg(colour(ink.paper)).fg(colour(ink.ink)));
        let inner = block.inner(area);
        block.render(area, buf);

        // The cells under the highlight, coloured for a terminal with no graphics
        // protocol. The same rectangle the bar image is placed over.
        if let Some(bar) = self.menu.hover_bar(area) {
            buf.set_style(bar, Style::new().bg(colour(ink.hover)));
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
                    Line::from(Span::styled(
                        text.as_str(),
                        Style::new().fg(colour(ink.accent)),
                    ))
                    .render(row, buf);
                }
                Entry::Title { name, close } => {
                    Line::from(Span::styled(
                        name.as_str(),
                        Style::new()
                            .fg(colour(ink.ink))
                            .add_modifier(Modifier::BOLD),
                    ))
                    .render(row, buf);
                    // A target, so it lifts under the pointer like everything else
                    // rather than sitting there as a label that happens to work. The
                    // styling is the palette's rather than this file's, the popup
                    // carrying the same button (see `toast`).
                    let style = ink.close_button(hovered(Command::Menu));
                    Line::from(Span::styled(close.as_str(), style))
                        .right_aligned()
                        .render(row, buf);
                }
                Entry::Item {
                    label,
                    keys,
                    command,
                } => {
                    let style = if hovered(*command) {
                        Style::new()
                            .fg(colour(ink.accent))
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().fg(colour(ink.ink))
                    };
                    split(buf, row, label, keys, style, ink);
                }
                Entry::Choice { options, keys } => {
                    for (option, start, _) in option_spans(options) {
                        // Brackets on the one in force, blanks on the rest, so every
                        // option is the same width whichever is selected and the
                        // options do not shuffle sideways as it changes.
                        let selected = option.in_force(self.state);
                        let mut style = if selected {
                            Style::new()
                                .fg(colour(ink.ink))
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::new().fg(colour(ink.muted))
                        };
                        if hovered(option.command) {
                            style = style
                                .fg(colour(ink.accent))
                                .add_modifier(Modifier::UNDERLINED);
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
                    Line::from(Span::styled(
                        keys.as_str(),
                        Style::new().fg(colour(ink.muted)),
                    ))
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
fn split(buf: &mut Buffer, row: Rect, left: &str, right: &str, style: Style, ink: &Palette) {
    Line::from(Span::styled(left, style)).render(row, buf);
    if !right.is_empty() {
        Line::from(Span::styled(right, Style::new().fg(colour(ink.muted))))
            .right_aligned()
            .render(row, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ScaleMode;
    use crate::ui::CLOSE;
    use crate::ui::theme::probe::{bg, fg};

    /// A settled state, for the tests that reason about geometry rather than marks.
    fn state() -> State {
        State {
            mode: ScaleMode::Fit,
            theme: Theme::Dark,
        }
    }

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
        menu.draw(&mut out, &metrics(100, 40), state());
        assert!(!out.is_empty());

        out.clear();
        menu.draw(&mut out, &metrics(20, 6), state());
        assert!(out.is_empty(), "must not draw a menu that does not fit");
    }

    #[test]
    fn menu_carries_its_own_background() {
        // Tiles sit at z=-1: below the text, but above the cell background, so no
        // SGR fill can be seen behind the glyphs. The backdrop has to be an image
        // of our own, outranking every tile id so it is composited over them.
        let menu = Menu::new('a');
        let mut out = Vec::new();
        menu.draw(&mut out, &metrics(100, 40), state());
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
            text.contains(&bg(state().theme.palette().paper)),
            "the cells are coloured too, for a terminal with no graphics protocol"
        );
        assert!(
            !text.contains("\x1b[7m"),
            "reverse video would follow the theme, and is buried by the image anyway"
        );
        // The backdrop goes down before the text that sits on it.
        let backdrop = text.find("\x1b_Ga=T").expect("no backdrop");
        assert!(backdrop < text.find("Command menu").expect("no title"));
    }

    #[test]
    fn menu_is_padded_and_right_aligns_its_shortcuts() {
        let menu = Menu::new('a');
        let mut out = Vec::new();
        menu.draw(&mut out, &metrics(100, 40), state());
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
        menu.draw(&mut drawn, &m, state());
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
        Menu::new('a').draw(&mut out, &m, state());
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
        // The menu's own toggle comes first, so the binding that opened the box is
        // the first thing in it.
        assert_eq!(
            menu.hit(&m, area.x, area.y + PAD_Y + 3),
            Hit::Item {
                index: 3,
                command: Command::Menu
            },
            "the first item is no longer the menu's own toggle"
        );

        for (label, want) in [
            // The first row, and the only one whose command acts on the menu itself.
            ("Toggle command menu", Command::Menu),
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
    fn the_button_in_the_title_is_a_target_and_the_name_beside_it_is_not() {
        let mut menu = Menu::new('a');
        let m = metrics(100, 40);
        let area = menu.area(&m).unwrap();
        let row = area.y + PAD_Y;
        // The button is pushed against the right edge of the padded area, so it ends
        // there.
        let end = area.x + area.width - PAD_X;
        let dismiss = Hit::Item {
            index: 0,
            command: Command::Menu,
        };
        // One cell of mark and one cell of target: what a click acts on is the cell the
        // button is drawn on, with nothing either side of it standing in.
        assert_eq!(menu.hit(&m, end - 1, row), dismiss, "the button");
        // Neither the name at the other end nor the space between them.
        assert_eq!(menu.hit(&m, area.x + PAD_X, row), Hit::Inside, "the name");
        assert_eq!(menu.hit(&m, end - 2, row), Hit::Inside, "the gap");

        // And it lifts by colour alone: no bar behind it, which would be a second mark for
        // one thing and the one way this button and the popup's -- which has no bar to be
        // given -- could look unlike each other.
        menu.set_hover(menu.hit(&m, end - 1, row));
        assert_eq!(
            menu.hover_bar(area),
            None,
            "the title's button should have no bar"
        );

        let mut out = Vec::new();
        menu.draw(&mut out, &m, state());
        let text = String::from_utf8(out).unwrap();
        let ink = state().theme.palette();
        assert!(
            !text.contains(&bg(ink.hover)),
            "nothing else is hovered, so nothing should carry the lift: {text:?}"
        );
        // The accent is the sections' colour too, so it is the run carrying the button
        // that has to be found rather than the first run in that ink.
        let lit = text.split(&fg(ink.accent)).any(|run| {
            run.split("\x1b[0m")
                .next()
                .is_some_and(|run| run.contains(CLOSE))
        });
        assert!(lit, "the button should be in the accent: {text:?}");
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
        menu.draw(&mut out, &m, state());
        let text = String::from_utf8(out).unwrap();
        // A bar the pointer's row wide, drawn as an image with an id above the
        // backdrop's: a cell background would be buried, and a lower id would put
        // the bar under the very thing it has to be seen on.
        assert!(
            bar_command(&text, "a=T").is_some(),
            "the highlight must be an image; a cell background is buried"
        );
        assert!(
            text.contains(&bg(state().theme.palette().hover)),
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
        // The placement releases the one it replaces, which is what stops the row the
        // pointer came from keeping its stripe. `term::kitty` emits it and tests it;
        // this only asks that the bar go through the path that does.
        assert!(
            bar_command(&text, "a=d").is_some_and(|release| release < placement(&text)),
            "the bar must release its last placement before making another"
        );

        // Pointing at nothing takes it away and puts nothing back. Nothing is placed,
        // so nothing releases the last placement either, and only this delete does.
        let mut out = Vec::new();
        menu.set_hover(Hit::Outside);
        menu.draw(&mut out, &m, state());
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains(&bg(state().theme.palette().hover)));
        assert!(
            text.contains(&format!("a=d,d=I,i={}", kitty::MENU_HIGHLIGHT_IMAGE_ID)),
            "a bar that is no longer wanted has to be deleted, not just left off"
        );
        assert!(
            bar_command(&text, "a=T").is_none(),
            "nothing to highlight, so nothing to place"
        );
    }

    /// Where the graphics command that does `action` to the highlight bar starts.
    ///
    /// Matched on the keys rather than by substring, because the placement and the
    /// release that precedes it both name the same image id, and the keys between
    /// them are `term::kitty`'s business to order.
    fn bar_command(text: &str, action: &str) -> Option<usize> {
        text.match_indices("\x1b_G").find_map(|(at, _)| {
            let keys = text[at..].split_once(';')?.0;
            let names_the_bar = keys.contains(&format!("i={},", kitty::MENU_HIGHLIGHT_IMAGE_ID));
            (keys.contains(action) && names_the_bar).then_some(at)
        })
    }

    fn placement(text: &str) -> usize {
        bar_command(text, "a=T").expect("the bar was never placed")
    }

    #[test]
    fn moving_the_highlight_leaves_no_stripe_behind() {
        // Three entries of one section, which is where this went wrong: the pointer
        // crosses no heading between them, so nothing clears the row it came from
        // except the release the new placement carries.
        let m = metrics(100, 40);
        let mut menu = Menu::new('a');
        let col = m.cols / 2;
        let left = menu.area(&m).unwrap().x + 1;
        let mut rows = Vec::new();
        for label in ["Pan left", "Pan right", "Pan up"] {
            let row = row_of(&menu, &m, label);
            menu.set_hover(menu.hit(&m, col, row));
            let mut out = Vec::new();
            menu.draw(&mut out, &m, state());
            let text = String::from_utf8(out).unwrap();
            assert!(
                bar_command(&text, "a=d").is_some_and(|release| release < placement(&text)),
                "{label}: the last placement has to be released before the next is made"
            );
            // One bar per frame, on this row and no other.
            assert_eq!(
                text.matches(&format!("i={},p=", kitty::MENU_HIGHLIGHT_IMAGE_ID))
                    .count(),
                2,
                "{label}: expected one release and one placement"
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
    fn options(menu: &Menu, m: &Metrics) -> Vec<(Command, u16, u16)> {
        let index = menu
            .entries
            .iter()
            .position(|e| matches!(e, Entry::Choice { .. }))
            .expect("no row of options");
        options_of(menu, m, index)
    }

    /// The same for whichever row of options is asked for.
    fn options_of(menu: &Menu, m: &Metrics, index: usize) -> Vec<(Command, u16, u16)> {
        let left = menu.area(m).expect("the menu must fit").x + PAD_X;
        let Some(Entry::Choice { options, .. }) = menu.entries.get(index) else {
            panic!("entry {index} is not a row of options");
        };
        option_spans(options)
            .into_iter()
            .map(|(option, start, end)| (option.command, left + start, left + end))
            .collect()
    }

    #[test]
    fn each_scaling_option_is_its_own_target() {
        let menu = Menu::new('a');
        let m = metrics(100, 40);
        let (row, left) = choice_row(&menu, &m);

        for (want, start, end) in options(&menu, &m) {
            for col in [start, end - 1] {
                match menu.hit(&m, col, row) {
                    Hit::Item { command, .. } => {
                        assert_eq!(command, want, "column {col} of the scaling row");
                    }
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
            menu.draw(&mut out, &m, State { mode, ..state() });
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
    fn the_theme_row_marks_the_one_in_force_and_a_click_names_the_other() {
        let menu = Menu::new('a');
        let m = metrics(100, 40);
        let index = menu
            .entries
            .iter()
            .rposition(|e| matches!(e, Entry::Choice { .. }))
            .expect("no theme row");
        let area = menu.area(&m).unwrap();
        let row = area.y + PAD_Y + u16::try_from(index).unwrap();

        // Two options, each its own target, and each naming the theme it would put on.
        for (want, start, end) in options_of(&menu, &m, index) {
            for col in [start, end - 1] {
                match menu.hit(&m, col, row) {
                    Hit::Item { command, .. } => {
                        assert_eq!(command, want, "column {col} of the theme row");
                    }
                    other => panic!("column {col} of the theme row hit {other:?}"),
                }
            }
        }

        // The brackets follow whichever is in force, and the row is drawn in it too:
        // choosing a theme shows you the theme.
        for (theme, name) in [(Theme::Dark, "Dark"), (Theme::Light, "Light")] {
            let mut out = Vec::new();
            menu.draw(&mut out, &m, State { theme, ..state() });
            let text = String::from_utf8(out).unwrap();
            let drawn = &rendered(text.as_bytes())[usize::from(row - area.y)];
            assert!(drawn.contains(&format!("[{name}]")), "{theme:?}: {drawn:?}");
            assert_eq!(drawn.matches('[').count(), 1, "{theme:?}: {drawn:?}");
            assert!(
                text.contains(&bg(theme.palette().paper)),
                "{theme:?}: the box is not in the theme it says is in force"
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
        let (command, start, _) = options(&menu, &m)[2];

        menu.set_hover(menu.hit(&m, start, row));
        let mut out = Vec::new();
        menu.draw(&mut out, &m, state());
        let text = String::from_utf8(out).unwrap();

        let bar = menu.hover_bar(area).expect("nothing highlighted");
        assert_eq!(bar.x, start, "the bar does not start at the option");
        assert_eq!(bar.width, cells("Integer") + 2);
        assert!(bar.width < area.width, "the bar spans the whole row");
        assert_eq!(command, Command::Mode(ScaleMode::Integer));
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
        menu.draw(&mut plain, &m, state());
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
