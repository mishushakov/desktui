//! Terminal input to RFB input.
//!
//! Two things make this more than a lookup table.
//!
//! **Releases.** RFB wants a down and an up for every key. The Kitty keyboard
//! protocol provides both; a terminal without it reports only presses, so the
//! release has to be synthesised straight after. Which of the two we are dealing
//! with is settled by the capability probe rather than guessed at from traffic.
//!
//! **Never release what was not pressed.** Every release is checked against the
//! set of keys we actually sent a press for. Without that, swallowing a key
//! locally -- which the prefix does -- would leak a release for a key the server
//! never saw pressed, and a stuck modifier on the remote is a miserable thing to
//! debug.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};

use super::Metrics;
use super::keysym::{bitmask, keysym};
use crate::cli::ScaleMode;
use crate::render::Layout;
use crate::rfb::{ClientKeyEvent, ClientMouseEvent};

/// Shortest gap between two motion reports, about 60 a second.
///
/// A pixel-reporting terminal can produce motion far faster than any desktop needs,
/// and every report is a packet. noVNC uses the same 17ms. Button and wheel events
/// are never rate limited, and the last position is always flushed, so nothing is
/// lost -- only intermediate positions the remote would not have noticed.
const MOTION_INTERVAL: Duration = Duration::from_millis(17);

/// VNC pointer button bits, from RFC 6143 section 7.5.5.
mod button {
    pub const LEFT: u8 = 1 << 0;
    pub const MIDDLE: u8 = 1 << 1;
    pub const RIGHT: u8 = 1 << 2;
    pub const WHEEL_UP: u8 = 1 << 3;
    pub const WHEEL_DOWN: u8 = 1 << 4;
    pub const WHEEL_LEFT: u8 = 1 << 5;
    pub const WHEEL_RIGHT: u8 = 1 << 6;
}

/// A local command, reached through the prefix key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Quit,
    FullRefresh,
    Renegotiate,
    Mode(ScaleMode),
    Pan(i32, i32),
    ToggleViewOnly,
    ToggleStats,
    Help,
    /// The prefix was pressed twice: send it through to the server.
    SendPrefix,
}

/// The local lock-key state, as far as the terminal will say.
///
/// `None` means "no idea": a terminal without the Kitty keyboard protocol never
/// mentions lock keys, and an unset bit is then indistinguishable from the key being
/// off. Only when the protocol is in use does absence mean off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LockState {
    pub caps: Option<bool>,
    pub num: Option<bool>,
}

/// What a key event turned into.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyOutcome {
    /// Nothing to do: swallowed, or unmapped.
    Ignored,
    /// Send these to the server, in order.
    Keys(Vec<ClientKeyEvent>),
    /// Handle this locally.
    Local(Command),
}

pub struct InputMapper {
    prefix: char,
    /// The terminal reports key releases, so we must not invent them.
    expect_releases: bool,
    /// Pointer positions arrive in pixels rather than cell coordinates.
    pixel_mouse: bool,
    armed: bool,
    /// Keysyms currently pressed as far as the server is concerned.
    held: HashSet<u32>,
    /// Modifier state we have synthesised, for terminals that only report a
    /// bitmask.
    synthesised: KeyModifiers,
    /// Pointer buttons currently down.
    buttons: u8,
    /// Last position sent, to drop duplicate motion.
    last_position: Option<(u16, u16)>,
    /// When the last motion report went out.
    last_motion: Option<Instant>,
    /// A position held back by the rate limit, waiting to be flushed.
    pending_motion: Option<(u16, u16)>,
}

impl InputMapper {
    pub fn new(prefix: char, expect_releases: bool, pixel_mouse: bool) -> Self {
        Self {
            prefix: prefix.to_ascii_lowercase(),
            expect_releases,
            pixel_mouse,
            armed: false,
            held: HashSet::new(),
            synthesised: KeyModifiers::NONE,
            buttons: 0,
            last_position: None,
            last_motion: None,
            pending_motion: None,
        }
    }

    pub fn prefix(&self) -> char {
        self.prefix
    }

    /// The lock-key state this event reports, if the terminal reports any.
    pub fn lock_state(&self, ev: &KeyEvent) -> LockState {
        if !self.expect_releases {
            // No Kitty keyboard protocol, so no lock reporting either, and an absent
            // bit would be read as "off" when it only means "unsaid".
            return LockState::default();
        }
        LockState {
            caps: Some(ev.state.contains(KeyEventState::CAPS_LOCK)),
            num: Some(ev.state.contains(KeyEventState::NUM_LOCK)),
        }
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Is this the prefix key itself?
    fn is_prefix(&self, ev: &KeyEvent) -> bool {
        matches!(ev.code, KeyCode::Char(c)
            if c.to_ascii_lowercase() == self.prefix
                && ev.modifiers.contains(KeyModifiers::CONTROL))
    }

    pub fn on_key(&mut self, ev: KeyEvent) -> KeyOutcome {
        // While the prefix is armed, nothing reaches the server: the next press
        // is a command, and any release in between belongs to the prefix chord.
        if self.armed {
            if ev.kind != KeyEventKind::Press {
                return KeyOutcome::Ignored;
            }
            self.armed = false;
            if self.is_prefix(&ev) {
                return KeyOutcome::Local(Command::SendPrefix);
            }
            return match command_for(ev.code) {
                Some(cmd) => KeyOutcome::Local(cmd),
                None => KeyOutcome::Ignored,
            };
        }

        if self.is_prefix(&ev) {
            if ev.kind == KeyEventKind::Press {
                self.armed = true;
            }
            return KeyOutcome::Ignored;
        }

        self.translate(ev)
    }

    /// Send the prefix chord through to the server, as if it had not been caught.
    pub fn literal_prefix(&mut self) -> Vec<ClientKeyEvent> {
        let mut out = Vec::new();
        let ctrl = bitmask::CONTROL;
        let key = super::keysym::keysym_for_char(self.prefix);
        self.press(&mut out, ctrl);
        self.press(&mut out, key);
        self.release(&mut out, key);
        self.release(&mut out, ctrl);
        out
    }

    fn translate(&mut self, ev: KeyEvent) -> KeyOutcome {
        let Some(sym) = keysym(ev.code) else {
            return KeyOutcome::Ignored;
        };
        let mut out = Vec::new();

        // A terminal that cannot report modifier keys in their own right leaves us
        // to infer them from each event's modifier set.
        if !self.expect_releases {
            self.sync_modifiers(&mut out, ev.modifiers);
        }

        match ev.kind {
            KeyEventKind::Press | KeyEventKind::Repeat => {
                self.press(&mut out, sym);
                if !self.expect_releases {
                    // No release is coming, so make one.
                    self.release(&mut out, sym);
                }
            }
            KeyEventKind::Release => self.release(&mut out, sym),
        }

        if out.is_empty() {
            KeyOutcome::Ignored
        } else {
            KeyOutcome::Keys(out)
        }
    }

    /// Bring the remote's modifier state in line with the reported bitmask.
    fn sync_modifiers(&mut self, out: &mut Vec<ClientKeyEvent>, want: KeyModifiers) {
        for (flag, sym) in [
            (KeyModifiers::SHIFT, bitmask::SHIFT),
            (KeyModifiers::CONTROL, bitmask::CONTROL),
            (KeyModifiers::ALT, bitmask::ALT),
            (KeyModifiers::SUPER, bitmask::SUPER),
            (KeyModifiers::HYPER, bitmask::HYPER),
            (KeyModifiers::META, bitmask::META),
        ] {
            let wanted = want.contains(flag);
            if wanted != self.synthesised.contains(flag) {
                if wanted {
                    self.press(out, sym);
                } else {
                    self.release(out, sym);
                }
                self.synthesised.set(flag, wanted);
            }
        }
    }

    fn press(&mut self, out: &mut Vec<ClientKeyEvent>, sym: u32) {
        self.held.insert(sym);
        out.push(ClientKeyEvent {
            keycode: sym,
            down: true,
        });
    }

    /// Release a key, but only if we told the server it was down.
    fn release(&mut self, out: &mut Vec<ClientKeyEvent>, sym: u32) {
        if self.held.remove(&sym) {
            out.push(ClientKeyEvent {
                keycode: sym,
                down: false,
            });
        }
    }

    /// Release everything still held. Called when leaving, losing focus, or
    /// reconnecting: a modifier left down on the remote outlives us.
    pub fn release_all(&mut self) -> Vec<ClientKeyEvent> {
        let mut out: Vec<_> = self
            .held
            .drain()
            .map(|sym| ClientKeyEvent {
                keycode: sym,
                down: false,
            })
            .collect();
        // Deterministic order makes the traffic reproducible in tests and logs.
        out.sort_by_key(|e| e.keycode);
        self.synthesised = KeyModifiers::NONE;
        out
    }

    /// The terminal pixel this event happened at.
    ///
    /// Mode 1016 reports pixels in the same framing as 1006, so crossterm's column and
    /// row *are* pixels. Without it, the middle of the cell is as close as cell
    /// resolution can get.
    pub fn terminal_pixel(&self, ev: &MouseEvent, metrics: &Metrics) -> (u32, u32) {
        if self.pixel_mouse {
            (u32::from(ev.column), u32::from(ev.row))
        } else {
            (
                u32::from(ev.column) * metrics.cell_w + metrics.cell_w / 2,
                u32::from(ev.row) * metrics.cell_h + metrics.cell_h / 2,
            )
        }
    }

    /// Translate a mouse event, returning the pointer events to send.
    ///
    /// Returns nothing when the pointer is outside the drawn image, so a click on
    /// the letterbox is not reported as a click on the nearest edge.
    pub fn on_mouse(
        &mut self,
        ev: MouseEvent,
        layout: &Layout,
        metrics: &Metrics,
    ) -> Vec<ClientMouseEvent> {
        let (tx, ty) = self.terminal_pixel(&ev, metrics);
        let Some((x, y)) = layout.terminal_px_to_src(tx, ty) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        match ev.kind {
            // A button transition must never be held back, and must not land at a
            // stale position, so anything the rate limit was holding goes first.
            MouseEventKind::Down(b) => {
                self.flush_pending(&mut out);
                self.buttons |= button_bit(b);
            }
            MouseEventKind::Up(b) => {
                self.flush_pending(&mut out);
                self.buttons &= !button_bit(b);
            }
            MouseEventKind::Drag(_) | MouseEventKind::Moved => {
                // Nothing but the position changed; drop it if it did not.
                if self.last_position == Some((x, y)) {
                    return Vec::new();
                }
                // Too soon since the last report: remember where the pointer got
                // to and let the next tick send it.
                if let Some(last) = self.last_motion
                    && last.elapsed() < MOTION_INTERVAL
                {
                    self.pending_motion = Some((x, y));
                    return Vec::new();
                }
                self.last_motion = Some(Instant::now());
            }
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {
                self.flush_pending(&mut out);
                // A wheel notch is a click: press the wheel bit and let go.
                let bit = match ev.kind {
                    MouseEventKind::ScrollUp => button::WHEEL_UP,
                    MouseEventKind::ScrollDown => button::WHEEL_DOWN,
                    MouseEventKind::ScrollLeft => button::WHEEL_LEFT,
                    _ => button::WHEEL_RIGHT,
                };
                self.last_position = Some((x, y));
                out.push(ClientMouseEvent {
                    position_x: x,
                    position_y: y,
                    buttons: self.buttons | bit,
                });
                out.push(ClientMouseEvent {
                    position_x: x,
                    position_y: y,
                    buttons: self.buttons,
                });
                return out;
            }
        }

        self.last_position = Some((x, y));
        out.push(ClientMouseEvent {
            position_x: x,
            position_y: y,
            buttons: self.buttons,
        });
        out
    }

    /// Send a position the rate limit held back, if enough time has passed.
    ///
    /// Called from the render tick, so a pointer that stops moving still ends up
    /// where the user left it.
    pub fn flush_motion(&mut self) -> Option<ClientMouseEvent> {
        let (x, y) = self.pending_motion?;
        if let Some(last) = self.last_motion
            && last.elapsed() < MOTION_INTERVAL
        {
            return None;
        }
        self.pending_motion = None;
        self.last_motion = Some(Instant::now());
        self.last_position = Some((x, y));
        Some(ClientMouseEvent {
            position_x: x,
            position_y: y,
            buttons: self.buttons,
        })
    }

    /// Emit a held-back position immediately, before an event that must not be
    /// reordered behind it.
    fn flush_pending(&mut self, out: &mut Vec<ClientMouseEvent>) {
        if let Some((x, y)) = self.pending_motion.take() {
            self.last_motion = Some(Instant::now());
            self.last_position = Some((x, y));
            out.push(ClientMouseEvent {
                position_x: x,
                position_y: y,
                buttons: self.buttons,
            });
        }
    }

    /// Let go of every pointer button, for the same reason as `release_all`.
    pub fn release_buttons(&mut self) -> Option<ClientMouseEvent> {
        if self.buttons == 0 {
            return None;
        }
        self.buttons = 0;
        self.pending_motion = None;
        let (x, y) = self.last_position.unwrap_or((0, 0));
        Some(ClientMouseEvent {
            position_x: x,
            position_y: y,
            buttons: 0,
        })
    }
}

fn button_bit(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => button::LEFT,
        MouseButton::Middle => button::MIDDLE,
        MouseButton::Right => button::RIGHT,
    }
}

/// The command bound to a key in the prefix window.
fn command_for(code: KeyCode) -> Option<Command> {
    Some(match code {
        KeyCode::Char('q') => Command::Quit,
        KeyCode::Char('f') => Command::FullRefresh,
        KeyCode::Char('r') => Command::Renegotiate,
        KeyCode::Char('n') => Command::Mode(ScaleMode::Native),
        KeyCode::Char('s') => Command::Mode(ScaleMode::Fit),
        KeyCode::Char('i') => Command::Mode(ScaleMode::Integer),
        KeyCode::Char('1') => Command::Mode(ScaleMode::OneToOne),
        KeyCode::Char('v') => Command::ToggleViewOnly,
        KeyCode::Char('c') => Command::ToggleStats,
        KeyCode::Char('h') | KeyCode::Char('?') => Command::Help,
        KeyCode::Left => Command::Pan(-1, 0),
        KeyCode::Right => Command::Pan(1, 0),
        KeyCode::Up => Command::Pan(0, -1),
        KeyCode::Down => Command::Pan(0, 1),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Layout;

    fn key(code: KeyCode, kind: KeyEventKind, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        key(code, KeyEventKind::Press, KeyModifiers::NONE)
    }

    fn release(code: KeyCode) -> KeyEvent {
        key(code, KeyEventKind::Release, KeyModifiers::NONE)
    }

    fn ctrl(c: char, kind: KeyEventKind) -> KeyEvent {
        key(KeyCode::Char(c), kind, KeyModifiers::CONTROL)
    }

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

    fn native_layout() -> Layout {
        let m = metrics();
        let (w, h) = m.image_area();
        Layout::compute(&m, ScaleMode::Native, w, h, (0, 0))
    }

    #[test]
    fn a_key_press_and_release_pass_straight_through() {
        let mut input = InputMapper::new('a', true, true);
        assert_eq!(
            input.on_key(press(KeyCode::Char('x'))),
            KeyOutcome::Keys(vec![ClientKeyEvent {
                keycode: 0x78,
                down: true
            }])
        );
        assert_eq!(
            input.on_key(release(KeyCode::Char('x'))),
            KeyOutcome::Keys(vec![ClientKeyEvent {
                keycode: 0x78,
                down: false
            }])
        );
    }

    #[test]
    fn a_terminal_without_releases_gets_a_synthesised_one() {
        let mut input = InputMapper::new('a', false, true);
        let KeyOutcome::Keys(events) = input.on_key(press(KeyCode::Char('x'))) else {
            panic!("expected key events");
        };
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events[0].down);
        assert!(!events[1].down);
        assert_eq!(events[0].keycode, events[1].keycode);
    }

    #[test]
    fn modifiers_are_synthesised_only_when_the_terminal_cannot_report_them() {
        let mut input = InputMapper::new('a', false, true);
        let KeyOutcome::Keys(events) = input.on_key(key(
            KeyCode::Char('c'),
            KeyEventKind::Press,
            KeyModifiers::SHIFT,
        )) else {
            panic!("expected key events");
        };
        // Shift down, c down, c up. Shift stays down until it is reported gone.
        assert_eq!(
            events[0],
            ClientKeyEvent {
                keycode: bitmask::SHIFT,
                down: true
            }
        );
        assert_eq!(events[1].keycode, 0x63);
        assert!(!events[2].down);

        // Now without shift: it has to be released.
        let KeyOutcome::Keys(events) = input.on_key(press(KeyCode::Char('c'))) else {
            panic!("expected key events");
        };
        assert_eq!(
            events[0],
            ClientKeyEvent {
                keycode: bitmask::SHIFT,
                down: false
            }
        );
    }

    #[test]
    fn a_terminal_with_the_keyboard_protocol_sends_its_own_modifier_keys() {
        // The modifier arrives as a key in its own right, so inferring it from the
        // bitmask as well would press it twice.
        let mut input = InputMapper::new('a', true, true);
        let KeyOutcome::Keys(events) = input.on_key(key(
            KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftControl),
            KeyEventKind::Press,
            KeyModifiers::CONTROL,
        )) else {
            panic!("expected key events");
        };
        assert_eq!(
            events,
            vec![ClientKeyEvent {
                keycode: 0xffe3,
                down: true
            }]
        );

        let KeyOutcome::Keys(events) = input.on_key(key(
            KeyCode::Char('c'),
            KeyEventKind::Press,
            KeyModifiers::CONTROL,
        )) else {
            panic!("expected key events");
        };
        assert_eq!(
            events,
            vec![ClientKeyEvent {
                keycode: 0x63,
                down: true
            }],
            "the modifier must not be pressed a second time"
        );
    }

    #[test]
    fn a_release_is_never_sent_for_a_key_that_was_not_pressed() {
        // This is what keeps a swallowed prefix chord from leaking a stray release
        // and leaving a modifier stuck on the remote.
        let mut input = InputMapper::new('a', true, true);
        assert_eq!(
            input.on_key(release(KeyCode::Char('z'))),
            KeyOutcome::Ignored
        );
        assert_eq!(
            input.on_key(release(KeyCode::Modifier(
                crossterm::event::ModifierKeyCode::LeftControl
            ))),
            KeyOutcome::Ignored
        );
    }

    #[test]
    fn the_prefix_arms_and_the_next_key_is_a_command() {
        let mut input = InputMapper::new('a', true, true);
        assert_eq!(
            input.on_key(ctrl('a', KeyEventKind::Press)),
            KeyOutcome::Ignored
        );
        assert!(input.is_armed());
        // The release of the chord is swallowed too.
        assert_eq!(
            input.on_key(ctrl('a', KeyEventKind::Release)),
            KeyOutcome::Ignored
        );
        assert!(input.is_armed());
        assert_eq!(
            input.on_key(press(KeyCode::Char('q'))),
            KeyOutcome::Local(Command::Quit)
        );
        assert!(!input.is_armed());
    }

    #[test]
    fn the_prefix_twice_sends_it_through() {
        let mut input = InputMapper::new('a', true, true);
        input.on_key(ctrl('a', KeyEventKind::Press));
        assert_eq!(
            input.on_key(ctrl('a', KeyEventKind::Press)),
            KeyOutcome::Local(Command::SendPrefix)
        );
        let events = input.literal_prefix();
        assert_eq!(events.len(), 4);
        assert_eq!(
            events[0],
            ClientKeyEvent {
                keycode: bitmask::CONTROL,
                down: true
            }
        );
        assert_eq!(events[1].keycode, 0x61);
        assert!(!events[3].down);
        // And nothing is left held afterwards.
        assert!(input.release_all().is_empty());
    }

    #[test]
    fn an_unbound_key_in_the_prefix_window_does_nothing() {
        let mut input = InputMapper::new('a', true, true);
        input.on_key(ctrl('a', KeyEventKind::Press));
        assert_eq!(input.on_key(press(KeyCode::Char('%'))), KeyOutcome::Ignored);
        assert!(!input.is_armed(), "the window closes either way");
    }

    #[test]
    fn prefix_commands_cover_the_documented_bindings() {
        assert_eq!(command_for(KeyCode::Char('q')), Some(Command::Quit));
        assert_eq!(command_for(KeyCode::Char('f')), Some(Command::FullRefresh));
        assert_eq!(command_for(KeyCode::Char('r')), Some(Command::Renegotiate));
        assert_eq!(
            command_for(KeyCode::Char('n')),
            Some(Command::Mode(ScaleMode::Native))
        );
        assert_eq!(
            command_for(KeyCode::Char('1')),
            Some(Command::Mode(ScaleMode::OneToOne))
        );
        assert_eq!(command_for(KeyCode::Left), Some(Command::Pan(-1, 0)));
        assert_eq!(
            command_for(KeyCode::Char('v')),
            Some(Command::ToggleViewOnly)
        );
    }

    #[test]
    fn release_all_lets_go_of_everything_still_down() {
        let mut input = InputMapper::new('a', true, true);
        input.on_key(press(KeyCode::Char('a')));
        input.on_key(press(KeyCode::Char('b')));
        input.on_key(key(
            KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftControl),
            KeyEventKind::Press,
            KeyModifiers::CONTROL,
        ));
        let released = input.release_all();
        assert_eq!(released.len(), 3);
        assert!(released.iter().all(|e| !e.down));
        assert_eq!(released.last().unwrap().keycode, 0xffe3);
        assert!(input.release_all().is_empty(), "and only once");
    }

    #[test]
    fn pointer_position_is_pixel_exact_with_mode_1016() {
        let mut input = InputMapper::new('a', true, true);
        let layout = native_layout();
        let events = input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 37,
                row: 91,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &metrics(),
        );
        assert_eq!(events.len(), 1);
        assert_eq!((events[0].position_x, events[0].position_y), (37, 91));
    }

    #[test]
    fn without_mode_1016_the_pointer_aims_at_the_cell_centre() {
        let mut input = InputMapper::new('a', true, false);
        let layout = native_layout();
        let events = input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 4,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &metrics(),
        );
        assert_eq!(
            (events[0].position_x, events[0].position_y),
            (4 * 8 + 4, 5 * 17 + 8)
        );
    }

    #[test]
    fn buttons_accumulate_and_clear() {
        let mut input = InputMapper::new('a', true, true);
        let layout = native_layout();
        let m = metrics();
        let at = |kind| MouseEvent {
            kind,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };

        let e = input.on_mouse(at(MouseEventKind::Down(MouseButton::Left)), &layout, &m);
        assert_eq!(e[0].buttons, button::LEFT);
        let e = input.on_mouse(at(MouseEventKind::Down(MouseButton::Right)), &layout, &m);
        assert_eq!(e[0].buttons, button::LEFT | button::RIGHT);
        let e = input.on_mouse(at(MouseEventKind::Up(MouseButton::Left)), &layout, &m);
        assert_eq!(e[0].buttons, button::RIGHT);

        let stop = input.release_buttons().unwrap();
        assert_eq!(stop.buttons, 0);
        assert!(input.release_buttons().is_none());
    }

    #[test]
    fn a_wheel_notch_is_a_press_and_a_release() {
        let mut input = InputMapper::new('a', true, true);
        let layout = native_layout();
        let events = input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &metrics(),
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].buttons, button::WHEEL_UP);
        assert_eq!(events[1].buttons, 0);
    }

    #[test]
    fn a_wheel_notch_while_dragging_keeps_the_held_button() {
        let mut input = InputMapper::new('a', true, true);
        let layout = native_layout();
        let m = metrics();
        input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &m,
        );
        let events = input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &m,
        );
        assert_eq!(events[0].buttons, button::LEFT | button::WHEEL_DOWN);
        assert_eq!(events[1].buttons, button::LEFT);
    }

    #[test]
    fn motion_is_rate_limited_and_the_last_position_still_arrives() {
        // A pixel-reporting terminal can emit motion far faster than any desktop
        // needs. Intermediate positions may be dropped; the final one may not.
        let mut input = InputMapper::new('a', true, true);
        let layout = native_layout();
        let m = metrics();
        let at = |col| MouseEvent {
            kind: MouseEventKind::Moved,
            column: col,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            input.on_mouse(at(10), &layout, &m).len(),
            1,
            "first move goes"
        );
        assert!(
            input.on_mouse(at(11), &layout, &m).is_empty(),
            "a move straight after must be held back"
        );
        assert!(input.on_mouse(at(12), &layout, &m).is_empty());

        // Nothing to flush until the interval has passed, and then exactly the
        // last position.
        assert!(input.flush_motion().is_none());
        std::thread::sleep(MOTION_INTERVAL + Duration::from_millis(2));
        let flushed = input.flush_motion().expect("the last position must arrive");
        assert_eq!((flushed.position_x, flushed.position_y), (12, 10));
        assert!(input.flush_motion().is_none(), "and only once");
    }

    #[test]
    fn a_click_flushes_held_back_motion_first() {
        // Otherwise the button would land wherever the last report happened to be,
        // which is the classic off-by-a-few-pixels click bug.
        let mut input = InputMapper::new('a', true, true);
        let layout = native_layout();
        let m = metrics();
        let ev = |kind, col| MouseEvent {
            kind,
            column: col,
            row: 20,
            modifiers: KeyModifiers::NONE,
        };

        input.on_mouse(ev(MouseEventKind::Moved, 30), &layout, &m);
        assert!(
            input
                .on_mouse(ev(MouseEventKind::Moved, 44), &layout, &m)
                .is_empty()
        );

        let events = input.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), 44), &layout, &m);
        assert_eq!(
            events.len(),
            2,
            "the held position, then the click: {events:?}"
        );
        assert_eq!((events[0].position_x, events[0].buttons), (44, 0));
        assert_eq!(
            (events[1].position_x, events[1].buttons),
            (44, button::LEFT)
        );
    }

    #[test]
    fn a_wheel_notch_is_never_held_back() {
        let mut input = InputMapper::new('a', true, true);
        let layout = native_layout();
        let m = metrics();
        input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &m,
        );
        let events = input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 6,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &m,
        );
        assert!(
            events.iter().any(|e| e.buttons == button::WHEEL_UP),
            "the notch must go out immediately: {events:?}"
        );
    }

    #[test]
    fn releasing_buttons_drops_any_held_back_motion() {
        let mut input = InputMapper::new('a', true, true);
        let layout = native_layout();
        let m = metrics();
        input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &m,
        );
        input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &m,
        );
        assert!(input.release_buttons().is_some());
        std::thread::sleep(MOTION_INTERVAL + Duration::from_millis(2));
        assert!(
            input.flush_motion().is_none(),
            "a stale position must not follow the button release"
        );
    }

    #[test]
    fn repeated_motion_at_the_same_pixel_is_dropped() {
        let mut input = InputMapper::new('a', true, true);
        let layout = native_layout();
        let m = metrics();
        let ev = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 20,
            row: 20,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(input.on_mouse(ev, &layout, &m).len(), 1);
        assert!(input.on_mouse(ev, &layout, &m).is_empty());
    }

    #[test]
    fn a_pointer_outside_the_image_is_not_reported() {
        let m = metrics();
        // A letterboxed layout: the image does not fill the area.
        let layout = Layout::compute(&m, ScaleMode::Fit, 1920, 1080, (0, 0));
        let mut input = InputMapper::new('a', true, true);
        assert!(
            input
                .on_mouse(
                    MouseEvent {
                        kind: MouseEventKind::Moved,
                        column: 0,
                        row: 0,
                        modifiers: KeyModifiers::NONE,
                    },
                    &layout,
                    &m,
                )
                .is_empty(),
            "the top-left corner is letterbox, not desktop"
        );
    }
}
