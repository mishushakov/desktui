//! RFB (VNC) client protocol.
//!
//! Vendored from `vnc-rs` 0.5.3 (MIT OR Apache-2.0, see `LICENSE-MIT` and
//! `LICENSE-APACHE` in this directory), because this client needs three things
//! the published crate does not offer, and one of them is the headline feature:
//!
//! 1. **`SetDesktopSize` and the `ExtendedDesktopSize` pseudo-encoding.** Asking
//!    the server to make its desktop exactly the size of the terminal's pixel
//!    area is what makes a pixel-exact view possible without resampling.
//! 2. **A signal that a framebuffer update finished.** Without it a client can
//!    only fire update requests on a timer and hope; with it there is never more
//!    than one request in flight.
//! 3. **A screen size that tracks resizes.** Upstream caches the size from
//!    `ServerInit` and never updates it, so every refresh after a resize asks
//!    for the wrong rectangle.
//!
//! While vendoring, the following were fixed. Each was reachable from the
//! network, which is to say from a server that is hostile or merely buggy:
//!
//! * Palette indices were used unchecked in the Tight and ZRLE decoders. An
//!   index past the end of the palette panicked the process.
//! * Three `unreachable!()`s and one `unimplemented!()` sat on paths chosen by
//!   values off the wire (an unexpected pixel format, a colour-map message).
//! * The cursor decoder indexed four bytes per pixel regardless of the
//!   negotiated depth.
//! * ZRLE trusted a 32-bit length as an allocation size, as did `ServerCutText`:
//!   four gigabytes on request.
//! * `uninit_vec` handed out `Vec<u8>` whose contents were uninitialised memory.
//! * A decoder that failed mid-rectangle left its zlib stream taken out of its
//!   slot, so the next rectangle unwrapped a `None`. Tight was fixed where it takes
//!   the stream. ZRLE needed its tile loop moved out of line as well: every `?` in
//!   the loop returned past the restore at the end of `decode`, so one malformed
//!   rectangle left the decoder with no stream and every later one in the session
//!   failed. A palette index off the end or an undefined subencoding is a single
//!   byte the server picks, so that was reachable from the network too.
//! * Two `std::mem::transmute` calls turned arbitrary integers into enums.
//!
//! The TRLE decoder was dropped rather than fixed: it read a 32-bit length
//! prefix that TRLE does not have (RFC 6143 section 7.7.5), so it desynchronised
//! the stream on first use. Upstream's own README notes TRLE was never verified
//! against a server. Tight and ZRLE cover everything real servers negotiate.

mod client;
mod codec;
mod config;
mod error;
mod event;

pub use client::clipboard::Caps as ClipboardCaps;
pub use client::{VncClient, VncConnector};
pub use config::{PixelFormat, VncEncoding, VncVersion, hint};
pub use error::VncError;
pub use event::{VncEvent, X11Event, fence};

/// Largest allocation we will make on a server's word alone.
///
/// Well above any legitimate compressed rectangle or clipboard payload, and far
/// below the point where a hostile server can exhaust memory.
pub(crate) const MAX_PAYLOAD: usize = 64 << 20;

/// Clipboard transfers are capped much lower: this is text.
pub(crate) const MAX_CUT_TEXT: usize = 1 << 20;
