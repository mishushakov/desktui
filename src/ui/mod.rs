//! Chrome drawn around the remote screen.
//!
//! Laid out by ratatui, but the screen is never handed to it: a cell-diffing
//! renderer that owned the terminal would have opinions about the cells our graphics
//! placements live in. Each piece renders into a buffer of exactly its own size and
//! `paint` writes that out, so the widgets do the padding, alignment and styling
//! without reaching a single cell they were not given.
//!
//! Text is legible over the image because placements use `z=-1`, which puts them
//! below text and above the cell background.

pub mod menu;
mod paint;
pub mod status;

/// The colour both pieces of chrome pick things out with: the menu's headings and
/// whatever the pointer is on, and the bar's mark for a prefix waiting on its key.
///
/// One definition rather than one each, because the whole of its job is being the
/// same colour in both places.
const ACCENT: ratatui::style::Color = ratatui::style::Color::Rgb(0x7c, 0x3a, 0xed);
