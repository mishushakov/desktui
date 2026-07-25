# The RFB layer

`src/rfb/` is vendored from `vnc-rs` 0.5.3 (MIT OR Apache-2.0) rather than used as a
dependency, because this client needs `SetDesktopSize`, a signal that a framebuffer
update finished, and a screen size that survives a resize. While vendoring, four
remotely-triggerable panics, two unbounded allocations, one case of undefined
behaviour and two unsound `transmute`s were fixed, and the TRLE decoder was dropped
as unusable. [`mod.rs`](mod.rs) lists all of it.
