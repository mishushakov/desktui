//! Terminal keys to X11 keysyms.
//!
//! RFB carries X11 keysyms, so every key the terminal reports has to be named the
//! way an X server would name it. Two rules cover most of it: a character in
//! Latin-1 is its own keysym, and any other character is `0x0100_0000` plus its
//! code point. Everything else is a table.

use crossterm::event::{KeyCode, MediaKeyCode, ModifierKeyCode};

/// Keysyms for keys that are not characters.
mod sym {
    pub const BACKSPACE: u32 = 0xff08;
    pub const TAB: u32 = 0xff09;
    pub const RETURN: u32 = 0xff0d;
    pub const ESCAPE: u32 = 0xff1b;
    pub const HOME: u32 = 0xff50;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
    pub const PAGE_UP: u32 = 0xff55;
    pub const PAGE_DOWN: u32 = 0xff56;
    pub const END: u32 = 0xff57;
    pub const PRINT: u32 = 0xff61;
    pub const INSERT: u32 = 0xff63;
    pub const MENU: u32 = 0xff67;
    pub const PAUSE: u32 = 0xff13;
    pub const SCROLL_LOCK: u32 = 0xff14;
    pub const NUM_LOCK: u32 = 0xff7f;
    pub const KP_BEGIN: u32 = 0xff9d;
    pub const F1: u32 = 0xffbe;
    pub const SHIFT_L: u32 = 0xffe1;
    pub const SHIFT_R: u32 = 0xffe2;
    pub const CONTROL_L: u32 = 0xffe3;
    pub const CONTROL_R: u32 = 0xffe4;
    pub const CAPS_LOCK: u32 = 0xffe5;
    pub const META_L: u32 = 0xffe7;
    pub const META_R: u32 = 0xffe8;
    pub const ALT_L: u32 = 0xffe9;
    pub const ALT_R: u32 = 0xffea;
    pub const SUPER_L: u32 = 0xffeb;
    pub const SUPER_R: u32 = 0xffec;
    pub const HYPER_L: u32 = 0xffed;
    pub const HYPER_R: u32 = 0xffee;
    pub const DELETE: u32 = 0xffff;
    /// Multimedia keys live in their own page.
    pub const AUDIO_LOWER_VOLUME: u32 = 0x1008ff11;
    pub const AUDIO_MUTE: u32 = 0x1008ff12;
    pub const AUDIO_RAISE_VOLUME: u32 = 0x1008ff13;
    pub const AUDIO_PLAY: u32 = 0x1008ff14;
    pub const AUDIO_STOP: u32 = 0x1008ff15;
    pub const AUDIO_PREV: u32 = 0x1008ff16;
    pub const AUDIO_NEXT: u32 = 0x1008ff17;
    pub const AUDIO_RECORD: u32 = 0x1008ff1c;
    pub const AUDIO_REWIND: u32 = 0x1008ff3e;
    pub const AUDIO_PAUSE: u32 = 0x1008ff31;
    pub const AUDIO_FORWARD: u32 = 0x1008ff97;
}

/// A character's keysym.
///
/// Latin-1 maps one to one; anything else gets the Unicode offset. Both rules are
/// from the X11 protocol's keysym encoding.
pub fn keysym_for_char(c: char) -> u32 {
    let cp = c as u32;
    match cp {
        // The C0 controls are not keysyms; a terminal that reports them at all
        // means a key that has its own name below.
        0x20..=0xff => cp,
        _ => 0x0100_0000 + cp,
    }
}

/// The keysym for a terminal key, or `None` for something unrepresentable.
pub fn keysym(code: KeyCode) -> Option<u32> {
    Some(match code {
        KeyCode::Char(c) => keysym_for_char(c),
        KeyCode::Backspace => sym::BACKSPACE,
        KeyCode::Enter => sym::RETURN,
        KeyCode::Left => sym::LEFT,
        KeyCode::Right => sym::RIGHT,
        KeyCode::Up => sym::UP,
        KeyCode::Down => sym::DOWN,
        KeyCode::Home => sym::HOME,
        KeyCode::End => sym::END,
        KeyCode::PageUp => sym::PAGE_UP,
        KeyCode::PageDown => sym::PAGE_DOWN,
        KeyCode::Tab => sym::TAB,
        // Shift+Tab. The shift is reported separately, so the key itself is Tab.
        KeyCode::BackTab => sym::TAB,
        KeyCode::Delete => sym::DELETE,
        KeyCode::Insert => sym::INSERT,
        KeyCode::Esc => sym::ESCAPE,
        KeyCode::CapsLock => sym::CAPS_LOCK,
        KeyCode::ScrollLock => sym::SCROLL_LOCK,
        KeyCode::NumLock => sym::NUM_LOCK,
        KeyCode::PrintScreen => sym::PRINT,
        KeyCode::Pause => sym::PAUSE,
        KeyCode::Menu => sym::MENU,
        KeyCode::KeypadBegin => sym::KP_BEGIN,
        // F1 through F35 are contiguous from 0xffbe.
        KeyCode::F(n) if (1..=35).contains(&n) => sym::F1 + u32::from(n) - 1,
        KeyCode::F(_) => return None,
        KeyCode::Modifier(m) => modifier_keysym(m),
        KeyCode::Media(m) => match m {
            MediaKeyCode::Play => sym::AUDIO_PLAY,
            MediaKeyCode::Pause | MediaKeyCode::PlayPause => sym::AUDIO_PAUSE,
            MediaKeyCode::Stop => sym::AUDIO_STOP,
            MediaKeyCode::Reverse | MediaKeyCode::Rewind => sym::AUDIO_REWIND,
            MediaKeyCode::FastForward => sym::AUDIO_FORWARD,
            MediaKeyCode::TrackNext => sym::AUDIO_NEXT,
            MediaKeyCode::TrackPrevious => sym::AUDIO_PREV,
            MediaKeyCode::Record => sym::AUDIO_RECORD,
            MediaKeyCode::LowerVolume => sym::AUDIO_LOWER_VOLUME,
            MediaKeyCode::RaiseVolume => sym::AUDIO_RAISE_VOLUME,
            MediaKeyCode::MuteVolume => sym::AUDIO_MUTE,
        },
        KeyCode::Null => return None,
    })
}

pub fn modifier_keysym(m: ModifierKeyCode) -> u32 {
    match m {
        ModifierKeyCode::LeftShift => sym::SHIFT_L,
        ModifierKeyCode::RightShift => sym::SHIFT_R,
        ModifierKeyCode::LeftControl => sym::CONTROL_L,
        ModifierKeyCode::RightControl => sym::CONTROL_R,
        ModifierKeyCode::LeftAlt => sym::ALT_L,
        ModifierKeyCode::RightAlt => sym::ALT_R,
        ModifierKeyCode::LeftSuper => sym::SUPER_L,
        ModifierKeyCode::RightSuper => sym::SUPER_R,
        ModifierKeyCode::LeftHyper => sym::HYPER_L,
        ModifierKeyCode::RightHyper => sym::HYPER_R,
        ModifierKeyCode::LeftMeta => sym::META_L,
        ModifierKeyCode::RightMeta => sym::META_R,
        ModifierKeyCode::IsoLevel3Shift | ModifierKeyCode::IsoLevel5Shift => sym::ALT_R,
    }
}

/// Keysyms for the modifiers a terminal reports as a bitmask rather than as keys.
///
/// Needed only when the terminal cannot report modifier keys in their own right,
/// in which case they have to be inferred from each event's modifier set.
pub mod bitmask {
    use super::sym;

    pub const SHIFT: u32 = sym::SHIFT_L;
    pub const CONTROL: u32 = sym::CONTROL_L;
    pub const ALT: u32 = sym::ALT_L;
    pub const SUPER: u32 = sym::SUPER_L;
    pub const HYPER: u32 = sym::HYPER_L;
    pub const META: u32 = sym::META_L;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin1_characters_are_their_own_keysyms() {
        assert_eq!(keysym_for_char('a'), 0x61);
        assert_eq!(keysym_for_char('A'), 0x41);
        assert_eq!(keysym_for_char(' '), 0x20);
        assert_eq!(keysym_for_char('~'), 0x7e);
        // The top of Latin-1: y with diaeresis.
        assert_eq!(keysym_for_char('\u{ff}'), 0xff);
    }

    #[test]
    fn other_characters_get_the_unicode_offset() {
        // Euro sign, the canonical example from the X11 keysym rules.
        assert_eq!(keysym_for_char('€'), 0x0100_20ac);
        assert_eq!(keysym_for_char('あ'), 0x0100_3042);
        assert_eq!(keysym_for_char('🦀'), 0x0101_f980);
    }

    #[test]
    fn function_keys_are_contiguous_from_f1() {
        assert_eq!(keysym(KeyCode::F(1)), Some(0xffbe));
        assert_eq!(keysym(KeyCode::F(12)), Some(0xffc9));
        assert_eq!(keysym(KeyCode::F(13)), Some(0xffca));
        assert_eq!(keysym(KeyCode::F(35)), Some(0xffe0));
        assert_eq!(keysym(KeyCode::F(36)), None);
        assert_eq!(keysym(KeyCode::F(0)), None);
    }

    #[test]
    fn the_editing_keys_match_x11() {
        assert_eq!(keysym(KeyCode::Enter), Some(0xff0d));
        assert_eq!(keysym(KeyCode::Backspace), Some(0xff08));
        assert_eq!(keysym(KeyCode::Delete), Some(0xffff));
        assert_eq!(keysym(KeyCode::Esc), Some(0xff1b));
        assert_eq!(keysym(KeyCode::Home), Some(0xff50));
        assert_eq!(keysym(KeyCode::End), Some(0xff57));
        assert_eq!(keysym(KeyCode::Up), Some(0xff52));
    }

    #[test]
    fn backtab_is_tab_because_shift_travels_separately() {
        assert_eq!(keysym(KeyCode::BackTab), keysym(KeyCode::Tab));
    }

    #[test]
    fn left_and_right_modifiers_are_distinct() {
        assert_eq!(modifier_keysym(ModifierKeyCode::LeftShift), 0xffe1);
        assert_eq!(modifier_keysym(ModifierKeyCode::RightShift), 0xffe2);
        assert_eq!(modifier_keysym(ModifierKeyCode::LeftControl), 0xffe3);
        assert_eq!(modifier_keysym(ModifierKeyCode::RightControl), 0xffe4);
        assert_ne!(
            modifier_keysym(ModifierKeyCode::LeftSuper),
            modifier_keysym(ModifierKeyCode::LeftAlt)
        );
    }

    #[test]
    fn control_characters_are_not_keysyms_in_their_own_right() {
        // A terminal reporting a raw control byte would otherwise send a keysym
        // an X server does not recognise.
        assert_eq!(keysym_for_char('\u{3}'), 0x0100_0003);
        assert_eq!(keysym(KeyCode::Null), None);
    }
}
