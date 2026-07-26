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

pub mod chrome;
pub mod menu;
mod paint;
pub mod status;
pub mod theme;
pub mod toast;

/// The button that takes a piece of chrome off the screen: the menu's title carries one
/// and so does the notification popup. One string for both, because they are the same
/// control and two copies of it could come to disagree about what it looks like.
pub const CLOSE: &str = "x";
