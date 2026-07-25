//! Chrome drawn around the remote screen.
//!
//! Hand-written escapes rather than a TUI framework: a cell-diffing renderer
//! would have opinions about the cells our graphics placements live in, and this
//! is only ever a status line and an overlay.
//!
//! Text is legible over the image because placements use `z=-1`, which puts them
//! below text and above the cell background.

pub mod status;
