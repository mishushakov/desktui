//! The notification popup.
//!
//! Notes -- a resize refused, a scaling mode adopted, the clipboard picked up -- used
//! to be tacked onto the status line, where they shared a row with the figures and
//! were the first thing a narrow terminal cut off. They land in the top-right corner
//! instead, held off it by a margin, and take themselves off again after [`LINGER`].
//!
//! Everything the popup needs the menu needed first, and for the same reasons: a
//! backdrop that is an image, because a cell colour under the remote screen is never
//! seen (see `menu`), and an explicit blanking of its cells when it goes, because the
//! message is text and no repaint of the tiles below it can erase that.
//!
//! It can also be dismissed before its time, by the `x` at the right of the message: the
//! same button the menu's title carries, and a click acts on exactly the cell it is drawn
//! on, with no slack around it. Under the pointer it lifts exactly as the menu's does, the
//! palette defining that for both of them (see `theme`): by colour, which is also the one
//! kind of highlight that needs no image of its own out here.
//!
//! The rest of the box is not a target, so the pointer goes on reaching the remote screen
//! underneath as it crosses a note it has already read. The session offers the pointer
//! here before the menu, the popup being drawn over the menu -- see `session`.

use std::io::Write as _;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Widget};

use super::paint::write_cells;
use super::theme::{Palette, colour};
// The button is the menu's, not one of the popup's own: the way to close a thing should
// look the same wherever it is.
use super::CLOSE;
use crate::term::{Metrics, kitty};

/// How long a note stays on screen.
pub const LINGER: Duration = Duration::from_secs(4);

/// Breathing room inside the popup, and between the popup and the corner it sits in.
/// Two columns to the row in both, because a cell is about twice as tall as it is
/// wide, so the gap reads as the same gap in both directions.
const PAD_X: u16 = 2;
const PAD_Y: u16 = 1;
const MARGIN_X: u16 = 2;
const MARGIN_Y: u16 = 1;

/// Space between the message and the button that closes it, so the two do not read as
/// one word.
const GAP: u16 = 3;

/// Least of a message worth truncating to. Below this the box would be a couple of
/// letters over the remote screen, which says less than no box at all.
const MIN_TEXT: u16 = 12;

/// What the box holds beside the message: the gap, the button, and the padding either
/// side of the lot.
const CHROME: u16 = GAP + BUTTON + PAD_X * 2;

/// Cells the button covers, which the layout needs as a number. A `const` cannot count
/// them, so this is the one thing that could come to disagree with `CLOSE`, and the
/// assertion that it does not sits next to it.
const BUTTON: u16 = 1;
const _: () = assert!(CLOSE.len() == BUTTON as usize);

/// Width of the message as it will be drawn.
///
/// Measured rather than counted, as the status line measures its own: a note can carry
/// a server's name, and a name in something wider than Latin-1 would be measured short.
fn width_of(text: &str) -> u16 {
    u16::try_from(Line::raw(text).width()).unwrap_or(u16::MAX)
}

/// Where the popup goes, in zero-based screen cells, or `None` when there is not
/// enough screen to put it on.
///
/// The width follows the message, so drawing and erasing both come through here
/// rather than each working it out for itself.
fn area(metrics: &Metrics, text: &str) -> Option<Rect> {
    let height = 1 + PAD_Y * 2;
    // Measured against the image rows: the bar owns the bottom one, and a popup over
    // it would be one piece of chrome on top of another.
    if MARGIN_Y + height > metrics.image_rows() {
        return None;
    }
    // A margin either side, so a long message is truncated at the left edge of the box
    // rather than run off the left edge of the screen.
    let room = metrics.cols.checked_sub(MARGIN_X * 2)?;
    let wanted = width_of(text).saturating_add(CHROME);
    let width = wanted.min(room);
    // The button is not what gets truncated away: a note that cannot be dismissed is
    // worse than one that says a little less.
    if width < wanted.min(MIN_TEXT + CHROME) {
        return None;
    }
    Some(Rect {
        x: metrics.cols - MARGIN_X - width,
        y: MARGIN_Y,
        width,
        height,
    })
}

/// The cells the button is drawn on: the last of the padded inside, on the message's own
/// row. Drawing and hit testing both come through here, so a click cannot land beside
/// what it looks like it is on.
fn close_at(area: Rect) -> Rect {
    Rect {
        x: area.x + area.width - PAD_X - BUTTON,
        y: area.y + PAD_Y,
        width: BUTTON,
        height: 1,
    }
}

/// The note on screen, and whatever the last one left behind.
#[derive(Default)]
pub struct Toast {
    /// The message and when it went up.
    showing: Option<(String, Instant)>,
    /// The cells the popup was last drawn on. Recorded rather than worked out again
    /// when it comes off: the box is as wide as its message, and a message replaced or
    /// a terminal resized in between would put it somewhere else.
    drawn: Option<Rect>,
    /// Cells a popup has left behind, which stay written until they are blanked.
    stale: Option<Rect>,
    /// The pointer is on the button, which is drawn lit while it is.
    hover: bool,
}

impl Toast {
    /// Put a note up, replacing whatever was there.
    ///
    /// True when the popup that went has left cells behind, which is when the remote
    /// screen under them has to be drawn back.
    pub fn show(&mut self, text: String) -> bool {
        // The outgoing box is not simply drawn over: a shorter message makes a
        // narrower one, and the cells past it would keep the old backdrop.
        let left_behind = self.take_down();
        self.showing = Some((text, Instant::now()));
        left_behind
    }

    /// Take a note that has run out off the screen. True on the frame it goes, with
    /// the same meaning as [`Toast::show`]'s.
    pub fn expire(&mut self) -> bool {
        match &self.showing {
            Some((_, at)) if at.elapsed() >= LINGER => self.take_down(),
            _ => false,
        }
    }

    /// Take a note off before its linger is up, the button having been clicked. True
    /// with the same meaning as [`Toast::show`]'s.
    pub fn dismiss(&mut self) -> bool {
        self.take_down()
    }

    /// Is the pointer at zero-based cell `(col, row)` on the button?
    ///
    /// False whenever there is no popup, or none was drawn: a note too big for the
    /// screen has no button to click on either.
    pub fn on_close(&self, metrics: &Metrics, col: u16, row: u16) -> bool {
        let Some((text, _)) = &self.showing else {
            return false;
        };
        let Some(area) = area(metrics, text) else {
            return false;
        };
        close_at(area).contains(Position::new(col, row))
    }

    /// Light the button, or put it out, as the pointer arrives on it or leaves.
    pub fn set_hover(&mut self, on_close: bool) {
        self.hover = on_close;
    }

    /// The cells a click has to land on to close the popup, for the log. A click that
    /// missed by a cell and one the popup never heard about look the same from the
    /// outside, and this is what tells them apart.
    pub fn close_cells(&self, metrics: &Metrics) -> Option<Rect> {
        let (text, _) = self.showing.as_ref()?;
        Some(close_at(area(metrics, text)?))
    }

    /// Is there a note up? A session with one has something to redraw even when no
    /// frame has arrived.
    pub fn is_live(&self) -> bool {
        self.showing.is_some()
    }

    /// The geometry the box was drawn with has changed, so the cells it is on are not the
    /// cells it belongs on: the popup is anchored to the top right, which a window of a
    /// different width puts somewhere else. Whatever it drew becomes stale and is blanked
    /// on the next frame, which then draws it where it now belongs.
    ///
    /// True with the same meaning as [`Toast::show`]'s: cells were left behind, so the
    /// remote screen under them has to be drawn again. A relayout marks everything
    /// anyway, but saying so is not this type's business to assume.
    pub fn moved(&mut self) -> bool {
        match self.drawn.take() {
            Some(area) => {
                self.stale = Some(area);
                true
            }
            None => false,
        }
    }

    fn take_down(&mut self) -> bool {
        self.showing = None;
        // Or the next note would arrive with its button already lit, before the pointer
        // has moved to say so.
        self.hover = false;
        match self.drawn.take() {
            Some(area) => {
                self.stale = Some(area);
                true
            }
            // Never drawn -- a note replaced inside a single frame -- so there is
            // nothing on the screen to take off.
            None => false,
        }
    }

    /// Blank the cells the last popup left, if any.
    ///
    /// Goes out before the frame's tiles, as the menu's erase does: a terminal may
    /// treat clearing a cell as dropping the placement under it.
    pub fn clear(&self, out: &mut Vec<u8>) {
        let Some(area) = self.stale else {
            return;
        };
        out.extend_from_slice(b"\x1b[0m");
        for y in area.top()..area.bottom() {
            let _ = write!(out, "\x1b[{};{}H", y + 1, area.x + 1);
            out.extend(std::iter::repeat_n(b' ', usize::from(area.width)));
        }
        kitty::delete_image(out, kitty::TOAST_IMAGE_ID);
    }

    /// The frame carrying the erase reached the terminal, so the cells are clean.
    /// A dropped frame leaves the record standing and the next one blanks them again.
    pub fn commit(&mut self) {
        self.stale = None;
    }

    /// Draw the note, if there is one, over the top-right of the remote screen.
    pub fn draw(&mut self, out: &mut Vec<u8>, metrics: &Metrics, ink: &Palette) {
        let Some((text, _)) = &self.showing else {
            return;
        };
        let Some(area) = area(metrics, text) else {
            return;
        };

        // The backdrop first and the message on it. Above the menu's two ids, so a
        // note that lands while the menu is open is composited over it rather than
        // buried under it.
        kitty::place_solid(
            out,
            kitty::TOAST_IMAGE_ID,
            usize::from(area.x) + 1,
            usize::from(area.y) + 1,
            usize::from(area.width),
            usize::from(area.height),
            ink.paper,
        );
        let mut buf = Buffer::empty(area);
        let block = Block::new()
            .padding(Padding::symmetric(PAD_X, PAD_Y))
            .style(Style::new().bg(colour(ink.paper)).fg(colour(ink.ink)));
        let inner = block.inner(area);
        block.render(area, &mut buf);
        // Given the room the button and the gap leave rather than the whole inside, so a
        // long message is clipped short of the button instead of up against it. One line
        // however long it is: a wrapped note is a box that changes height as it is read.
        let said = Rect {
            width: inner.width.saturating_sub(GAP + BUTTON),
            ..inner
        };
        Line::from(Span::styled(
            text.as_str(),
            Style::new().fg(colour(ink.ink)),
        ))
        .render(said, &mut buf);
        // The palette's own styling for the button, quiet or lit, which is the styling the
        // menu's title gives the same button: one definition, so pointing at either does
        // the same thing. Colour and nothing else, which is also the one kind of highlight
        // that needs no image of its own -- a background here would be painted under the
        // remote screen and never seen.
        Line::from(Span::styled(CLOSE, ink.close_button(self.hover)))
            .render(close_at(area), &mut buf);
        write_cells(out, &buf);

        self.drawn = Some(area);
    }

    /// Push the note back in time, so a test can reach its expiry without waiting out
    /// the linger.
    #[cfg(test)]
    fn age(&mut self, by: Duration) {
        if let Some((_, at)) = &mut self.showing
            && let Some(earlier) = at.checked_sub(by)
        {
            *at = earlier;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::probe::{bg, fg};
    use crate::ui::theme::{Rgb, Theme};

    fn ink() -> &'static Palette {
        Theme::Dark.palette()
    }

    /// Was the image itself dropped? `d=I` frees the image; the lower-case `d=i` before
    /// every placement only releases the placement on it.
    fn dropped(text: &str, id: u32) -> bool {
        text.contains(&format!("a=d,d=I,i={id},"))
    }

    /// What was written in `colour`: everything from where it was set up to the reset
    /// that ends the run. Nothing is assumed about the escapes in between -- a run
    /// names its background too, and in whichever order `paint` happens to emit them.
    fn inked(text: &str, colour: Rgb) -> String {
        let run = text
            .split(&fg(colour))
            .nth(1)
            .unwrap_or_else(|| panic!("{colour:?} was never used: {text:?}"));
        run.split("\x1b[0m").next().unwrap_or("").to_string()
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

    /// A popup on screen, and the bytes that put it there.
    fn shown(m: &Metrics, text: &str) -> (Toast, Vec<u8>) {
        let mut toast = Toast::default();
        toast.show(text.to_string());
        let mut out = Vec::new();
        toast.draw(&mut out, m, ink());
        (toast, out)
    }

    /// The rows the popup wrote, as `(row, column, text)`, with the escapes and the
    /// image payloads taken out. One-based, as the cursor is.
    fn rows(buf: &[u8]) -> Vec<(u16, u16, String)> {
        let text = String::from_utf8(buf.to_vec()).unwrap();
        let mut out: Vec<(u16, u16, String)> = Vec::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => match chars.next() {
                    // CSI: parameters then a letter, and `H` starts a fresh row.
                    Some('[') => {
                        let mut params = String::new();
                        for c in chars.by_ref() {
                            if c.is_ascii_alphabetic() {
                                if c == 'H'
                                    && let Some((r, col)) = params.split_once(';')
                                    && let (Ok(r), Ok(col)) = (r.parse::<u16>(), col.parse::<u16>())
                                {
                                    out.push((r, col, String::new()));
                                }
                                break;
                            }
                            params.push(c);
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
                    if let Some(row) = out.last_mut() {
                        row.2.push(c);
                    }
                }
            }
        }
        out.retain(|(.., text)| !text.is_empty());
        out
    }

    #[test]
    fn the_popup_sits_in_the_top_right_with_room_around_it() {
        let m = metrics(80, 24);
        let (_, out) = shown(&m, "full refresh requested");
        let rows = rows(&out);

        // Three rows: a padding row, the message, another padding row. All at the same
        // column, and none of them on row one or against the right edge.
        assert_eq!(rows.len(), 3, "expected a padded box: {rows:?}");
        let width = rows[0].2.chars().count();
        assert!(
            rows.iter().all(|(.., text)| text.chars().count() == width),
            "every row must fill the box, or the backdrop shows through: {rows:?}"
        );
        let left = rows[0].1;
        assert!(rows.iter().all(|(_, col, _)| *col == left));
        assert_eq!(rows[0].0, MARGIN_Y + 1, "not held off the top");
        assert_eq!(rows[1].0, MARGIN_Y + 2);
        assert_eq!(
            usize::from(left) + width - 1,
            usize::from(80 - MARGIN_X),
            "not held off the right edge"
        );

        // The message is the middle row, indented by the padding on both sides, with the
        // button out at the end of it.
        assert!(rows[0].2.trim().is_empty(), "no padding above the message");
        assert!(rows[2].2.trim().is_empty(), "no padding below the message");
        assert_eq!(rows[1].2, "  full refresh requested   x  ");
    }

    #[test]
    fn the_popup_carries_its_own_backdrop() {
        // Tiles sit at z=-1: below the text but above the cell background, so no SGR
        // fill is ever seen behind the glyphs. The backdrop has to be an image of our
        // own, outranking every tile id -- and the menu's, so a note that arrives with
        // the menu open is not drawn underneath it.
        let (_, out) = shown(&metrics(80, 24), "remote clipboard copied");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(&format!("i={},", kitty::TOAST_IMAGE_ID)),
            "the popup must place a backdrop image, not colour its cells"
        );
        const { assert!(kitty::TOAST_IMAGE_ID > kitty::MENU_HIGHLIGHT_IMAGE_ID) };
        assert!(
            text.contains("z=-1"),
            "the backdrop shares the tiles' z-index; above it would cover the message"
        );
        assert!(
            text.contains(&bg(ink().paper)) && text.contains(&fg(ink().ink)),
            "the cells are coloured too, for a terminal with no graphics protocol"
        );
        assert!(
            !text.contains("\x1b[7m"),
            "reverse video would follow the theme instead of the palette"
        );
        // The backdrop goes down before the message that sits on it.
        let backdrop = text.find("\x1b_Ga=T").expect("no backdrop");
        assert!(backdrop < text.find("clipboard").expect("no message"));
    }

    #[test]
    fn the_popup_wears_whichever_palette_is_in_force() {
        let m = metrics(80, 24);
        let mut toast = Toast::default();
        toast.show("scaling: scaled".into());
        let mut out = Vec::new();
        toast.draw(&mut out, &m, Theme::Light.palette());
        let text = String::from_utf8(out).unwrap();
        let light = Theme::Light.palette();
        assert!(text.contains(&bg(light.paper)) && text.contains(&fg(light.ink)));
        assert!(
            !text.contains(&bg(Theme::Dark.palette().paper)),
            "the dark paper has no business in a light session: {text:?}"
        );
    }

    #[test]
    fn a_long_message_is_truncated_rather_than_wrapped() {
        let m = metrics(30, 24);
        let (_, out) = shown(&m, "a message far longer than thirty columns of terminal");
        let rows = rows(&out);
        assert_eq!(rows.len(), 3, "one line, whatever the length: {rows:?}");
        for (.., text) in &rows {
            assert_eq!(
                text.chars().count(),
                usize::from(30 - MARGIN_X * 2),
                "the box must stop at the margin: {rows:?}"
            );
        }
        assert_eq!(
            rows[1].2, "  a message far long   x  ",
            "the message is what gives way, never the button"
        );
    }

    #[test]
    fn the_button_is_where_the_click_it_answers_lands() {
        // 80 columns, a margin of two and a box as wide as its message and chrome: the
        // brackets end two columns in from the right edge of the screen.
        let m = metrics(80, 24);
        let (toast, _) = shown(&m, "full refresh requested");
        let area = area(&m, "full refresh requested").expect("no box");
        let button = close_at(area);
        assert_eq!(
            button.y,
            area.y + PAD_Y,
            "the button is on the message's row"
        );
        assert_eq!(
            button.x + button.width,
            80 - MARGIN_X - PAD_X,
            "the button is inside the padding, not against the edge"
        );

        // Every cell of it, and nothing more: what a click acts on is exactly what the
        // brackets are drawn around, which is what the brackets are for.
        for x in button.x..button.right() {
            assert!(toast.on_close(&m, x, button.y), "cell {x} of the button");
        }
        assert!(
            !toast.on_close(&m, button.x - 1, button.y),
            "the gap in front of it: whitespace, and not the button"
        );
        assert!(
            !toast.on_close(&m, button.right(), button.y),
            "the padding behind it"
        );
        assert!(!toast.on_close(&m, area.x + PAD_X, button.y), "the message");
        assert!(
            !toast.on_close(&m, button.x, area.y) && !toast.on_close(&m, button.x, area.y + 2),
            "the padding rows above and below it"
        );
        assert!(
            !toast.on_close(&m, button.right() + PAD_X, button.y),
            "the remote screen past the right edge of the box"
        );

        // Nothing showing and nothing that fits are both nothing to click on.
        assert!(!Toast::default().on_close(&m, button.x, button.y));
        let (narrow, _) = shown(&metrics(12, 24), "full refresh requested");
        assert!(!narrow.on_close(&metrics(12, 24), 7, 2));
    }

    #[test]
    fn the_button_lights_up_under_the_pointer() {
        // Grey to the accent, and that is the whole of it: the same lift the menu's title
        // gives the same button, which the palette defines for both of them.
        let m = metrics(80, 24);
        let ink = ink();
        let (mut toast, out) = shown(&m, "remote clipboard copied");
        let quiet = String::from_utf8(out).unwrap();
        assert!(
            inked(&quiet, ink.muted).contains(CLOSE),
            "the button should be quiet until it is pointed at: {quiet:?}"
        );

        toast.set_hover(true);
        let mut out = Vec::new();
        toast.draw(&mut out, &m, ink);
        let lit = String::from_utf8(out).unwrap();
        assert!(
            inked(&lit, ink.accent).contains(CLOSE),
            "expected the accent on the button: {lit:?}"
        );
        assert!(
            !lit.contains(&fg(ink.muted)),
            "and the quiet ink was the button's alone: {lit:?}"
        );

        // Nothing else changes. A ground behind it would have to be an image out here,
        // being invisible as a cell colour, and a rule under it would be a second mark
        // for one thing; the row is the same shape lit or not.
        assert!(
            !lit.contains(&bg(ink.hover)) && !lit.contains("\x1b[4m"),
            "the colour is the whole of the lift: {lit:?}"
        );
        let images = |text: &str| text.matches("\x1b_G").count();
        assert_eq!(
            images(&lit),
            images(&quiet),
            "the lit button placed an image of its own: {lit:?}"
        );
        assert_eq!(
            rows(lit.as_bytes()),
            rows(quiet.as_bytes()),
            "only the ink should have changed"
        );
    }

    #[test]
    fn the_button_takes_the_note_off_before_its_linger_is_up() {
        let m = metrics(80, 24);
        let (mut toast, _) = shown(&m, "remote clipboard copied");
        toast.set_hover(true);
        assert!(toast.dismiss(), "the box was left standing");
        assert!(!toast.is_live());

        let mut out = Vec::new();
        toast.clear(&mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(
            rows(text.as_bytes())
                .iter()
                .all(|(.., t)| t.trim().is_empty())
        );
        // The backdrop outranks every tile and would stay on top of them for ever.
        assert!(dropped(&text, kitty::TOAST_IMAGE_ID));

        // The next note is not lit before the pointer has said so.
        toast.show("full refresh requested".into());
        let mut out = Vec::new();
        toast.draw(&mut out, &m, ink());
        let text = String::from_utf8(out).unwrap();
        assert!(inked(&text, ink().muted).contains(CLOSE));
    }

    #[test]
    fn a_screen_with_no_room_for_it_gets_no_popup() {
        // Narrow enough that nothing worth reading would fit, and short enough that
        // the box would sit on the bar.
        let (_, out) = shown(&metrics(12, 24), "full refresh requested");
        assert!(out.is_empty(), "drew a box too narrow to say anything");

        let (_, out) = shown(&metrics(80, 4), "full refresh requested");
        assert!(out.is_empty(), "drew a box over the status line");
    }

    #[test]
    fn a_note_stays_for_its_linger_and_then_comes_off() {
        let m = metrics(80, 24);
        let (mut toast, out) = shown(&m, "renegotiating the remote size");
        assert!(!out.is_empty());
        assert!(!toast.expire(), "took the note off before its time");
        assert!(toast.is_live());

        toast.age(LINGER);
        assert!(toast.expire(), "the note outstayed its linger");
        assert!(!toast.is_live());

        // Blanked cells and a dropped backdrop: the message is text, which no repaint
        // of the tiles under it would erase.
        let mut out = Vec::new();
        toast.clear(&mut out);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            rows(text.as_bytes()).len(),
            3,
            "every row of the box has to be blanked: {text:?}"
        );
        assert!(
            rows(text.as_bytes())
                .iter()
                .all(|(.., t)| t.trim().is_empty())
        );
        assert!(
            text.contains(&format!("i={}", kitty::TOAST_IMAGE_ID)),
            "the backdrop outranks every tile and would stay on top of them: {text:?}"
        );
        assert!(!text.contains("renegotiating"), "redrew what it took off");

        // Nothing more to do once the frame carrying it has gone out.
        toast.commit();
        let mut out = Vec::new();
        toast.clear(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn a_dropped_frame_leaves_the_erase_to_the_next_one() {
        let m = metrics(80, 24);
        let (mut toast, _) = shown(&m, "renegotiating the remote size");
        toast.age(LINGER);
        toast.expire();

        // Written but never submitted, so the cells are still on screen and the second
        // frame has to carry the same erase.
        let mut first = Vec::new();
        toast.clear(&mut first);
        let mut second = Vec::new();
        toast.clear(&mut second);
        assert!(!first.is_empty());
        assert_eq!(first, second);
    }

    #[test]
    fn a_replaced_note_takes_the_wider_box_off_with_it() {
        // A shorter message makes a narrower box, so the cells past it would keep the
        // old backdrop unless they are blanked.
        let m = metrics(80, 24);
        let (mut toast, _) = shown(&m, "a good deal longer than the next one");
        assert!(toast.show("brief".into()), "the old box was left standing");

        let mut out = Vec::new();
        toast.clear(&mut out);
        let blanked = rows(&out);
        assert_eq!(blanked.len(), 3);
        let width = area(&m, "a good deal longer than the next one")
            .expect("the first box did not fit")
            .width;
        assert!(
            blanked
                .iter()
                .all(|(.., text)| text.chars().count() == usize::from(width)),
            "the erase has to cover the box that was drawn, not the one replacing it"
        );
    }

    #[test]
    fn a_note_that_moved_blanks_the_cells_it_was_on() {
        // The box is anchored to the top right, so a window of another width puts it
        // somewhere else -- and the note stays up across a resize. Nothing else would take
        // the old one off: a relayout no longer erases the screen, and the box that is
        // drawn where it now belongs does not reach where it was.
        let wide = metrics(80, 24);
        let (mut toast, _) = shown(&wide, "still up");
        assert!(toast.moved(), "the box that was drawn was not taken off");

        let mut out = Vec::new();
        toast.clear(&mut out);
        let blanked = rows(&out);
        let was = area(&wide, "still up").expect("the box did not fit");
        assert_eq!(blanked.len(), usize::from(was.height));
        assert!(
            blanked
                .iter()
                .all(|(.., text)| text.chars().count() == usize::from(was.width)),
            "the erase has to cover the box as it was drawn"
        );
        assert!(toast.is_live(), "the note itself is still up");
    }

    #[test]
    fn a_note_that_never_reached_the_screen_has_not_moved() {
        let mut toast = Toast::default();
        toast.show("never drawn".into());
        assert!(
            !toast.moved(),
            "nothing reached the screen, so nothing has to be taken off it"
        );
    }

    #[test]
    fn a_note_replaced_before_it_was_drawn_leaves_nothing_behind() {
        let mut toast = Toast::default();
        toast.show("never drawn".into());
        assert!(
            !toast.show("nor this".into()),
            "nothing reached the screen, so nothing has to be taken off it"
        );
        let mut out = Vec::new();
        toast.clear(&mut out);
        assert!(out.is_empty());
    }
}
