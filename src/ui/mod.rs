//! Chrome drawn around the remote screen.
//!
//! Nothing here hands the screen to a TUI framework: a cell-diffing renderer that
//! owned the terminal would have opinions about the cells our graphics placements
//! live in. The status line is a single row of hand-written escapes. The menu is
//! laid out by ratatui into a buffer of its own size and serialised from there, so
//! the widgets do the padding and alignment without reaching any other cell.
//!
//! Text is legible over the image because placements use `z=-1`, which puts them
//! below text and above the cell background.

pub mod menu;
pub mod status;
