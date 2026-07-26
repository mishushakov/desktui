//! The two palettes the chrome can wear.
//!
//! One palette covers both pieces, because they are one piece of chrome as far as
//! the eye is concerned: the bar names what you are connected to and the menu lists
//! what you can do about it, and a light box over a dark bar would read as two
//! programs.
//!
//! Every colour is kept as bytes rather than as a `ratatui::Color`. The menu needs
//! both forms -- its backdrop and its highlight are images, because a cell colour
//! under the remote screen is never seen (see `menu`) -- and one definition that
//! converts is a colour that cannot drift from itself.

use ratatui::style::{Color, Style};
use ratatui::text::Span;

/// A colour as the graphics protocol wants it.
pub type Rgb = (u8, u8, u8);

/// The same colour as the cell attributes want it.
pub fn colour(rgb: Rgb) -> Color {
    Color::Rgb(rgb.0, rgb.1, rgb.2)
}

/// Which palette is in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

impl Theme {
    pub fn palette(self) -> &'static Palette {
        match self {
            Theme::Dark => &DARK,
            Theme::Light => &LIGHT,
        }
    }
}

/// Everything either piece of chrome is allowed to paint with.
pub struct Palette {
    /// The menu's background, and the ink on it: labels, then the shortcuts beside
    /// them, which are quieter because you read them second.
    pub paper: Rgb,
    pub ink: Rgb,
    pub muted: Rgb,
    /// What the pointer is on.
    pub hover: Rgb,
    /// The status bar, its ordinary figures, and the two things on it worth reading
    /// without looking for them.
    pub bar: Rgb,
    pub text: Rgb,
    pub bright: Rgb,
    /// Shared by both: the menu's headings and whatever the pointer is on, and the
    /// bar's light for a prefix waiting on its key. Being the same in both places is
    /// the whole of its job.
    pub accent: Rgb,
}

impl Palette {
    /// Ordinary status text.
    pub fn text<'a>(&self, text: &'a str) -> Span<'a> {
        Span::styled(text, Style::new().fg(colour(self.text)))
    }

    /// Status text worth reading at a glance.
    pub fn bright<'a>(&self, text: &'a str) -> Span<'a> {
        Span::styled(text, Style::new().fg(colour(self.bright)))
    }

    /// A mark in the menu's own colour. Used once: the light that comes on while the
    /// prefix waits for the key that follows it.
    pub fn accent<'a>(&self, text: &'a str) -> Span<'a> {
        Span::styled(text, Style::new().fg(colour(self.accent)))
    }
}

/// Near-black, which is the bar's own colour: the menu is the same surface, so the
/// box reads as the bar opening up rather than as something landing on top of it.
const DARK: Palette = Palette {
    paper: (0x0a, 0x0a, 0x0a),
    ink: (0xee, 0xee, 0xee),
    muted: (0x80, 0x80, 0x80),
    // Violet at the bottom of its range, so a row lifts off the black without
    // becoming a second colour of its own.
    hover: (0x2e, 0x10, 0x65),
    bar: (0x0a, 0x0a, 0x0a),
    text: (0x80, 0x80, 0x80),
    bright: (0xee, 0xee, 0xee),
    // Lighter than the light theme's, because the same violet on black is a bruise.
    accent: (0xa7, 0x8b, 0xfa),
};

/// The same arrangement inverted, for a terminal being read in daylight.
const LIGHT: Palette = Palette {
    paper: (0xee, 0xee, 0xee),
    ink: (0x11, 0x11, 0x11),
    // Darker than the dark theme's grey: the same grey that reads as quiet on black
    // reads as illegible on white.
    muted: (0x6b, 0x6b, 0x6b),
    hover: (0xdd, 0xd6, 0xfe),
    bar: (0xee, 0xee, 0xee),
    text: (0x6b, 0x6b, 0x6b),
    bright: (0x0a, 0x0a, 0x0a),
    accent: (0x7c, 0x3a, 0xed),
};

/// The escapes a colour comes out as, for tests that check what was inked with what.
/// Deriving the needle from the palette is the point: a literal here would be a second
/// copy of a colour, and the drift it allowed is exactly what the test is for.
#[cfg(test)]
pub(super) mod probe {
    use super::Rgb;

    pub fn fg(rgb: Rgb) -> String {
        format!("\x1b[38;2;{};{};{}m", rgb.0, rgb.1, rgb.2)
    }

    pub fn bg(rgb: Rgb) -> String {
        format!("\x1b[48;2;{};{};{}m", rgb.0, rgb.1, rgb.2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_and_the_bar_share_a_surface() {
        // The two are one piece of chrome to look at, so the box is the bar's colour
        // rather than one of its own.
        for theme in [Theme::Dark, Theme::Light] {
            let p = theme.palette();
            assert_eq!(
                p.paper, p.bar,
                "{theme:?}: the menu is not the bar's colour"
            );
        }
    }

    #[test]
    fn each_theme_is_legible_the_way_round_it_claims_to_be() {
        // Not a contrast measurement, just the sanity check that would catch a palette
        // half-edited: dark ink on dark paper, or the reverse.
        let luma =
            |(r, g, b): Rgb| 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b);
        let dark = Theme::Dark.palette();
        assert!(
            luma(dark.paper) < 64.0,
            "the dark theme's paper is not dark"
        );
        for ink in [dark.ink, dark.muted, dark.accent, dark.text, dark.bright] {
            assert!(
                luma(ink) > luma(dark.paper) + 48.0,
                "{ink:?} is lost on the dark paper"
            );
        }

        let light = Theme::Light.palette();
        assert!(
            luma(light.paper) > 192.0,
            "the light theme's paper is not light"
        );
        for ink in [light.ink, light.muted, light.accent] {
            assert!(
                luma(ink) + 48.0 < luma(light.paper),
                "{ink:?} is lost on the light paper"
            );
        }
        for ink in [light.text, light.bright] {
            assert!(
                luma(ink) + 48.0 < luma(light.bar),
                "{ink:?} is lost on the light bar"
            );
        }
    }
}
