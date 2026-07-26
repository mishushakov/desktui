//! The seam between a remote desktop and the session that draws it.
//!
//! The session below this module knows about framebuffers, damage rectangles and
//! terminal input. It knows nothing about RFB, and should learn nothing about any
//! other protocol either: everything wire-shaped lives behind [`Backend`].
//!
//! Two rules keep the seam honest.
//!
//! **Capabilities are announced, not assumed.** A session starts out believing the
//! least: that it must ask for every frame, and that latency cannot be measured. A
//! backend says otherwise with [`Update::Pushing`] and [`Update::LatencyAvailable`]
//! when it knows -- which for RFB is only after the encodings have been negotiated,
//! and so cannot be a value read once at connect time.
//!
//! **Mechanics stay behind the seam.** Whether frames arrive because the server was
//! asked, because continuous updates were negotiated, or because the protocol only
//! ever pushes, is the backend's business. The session is told the one thing it acts
//! on: whether frames arrive unasked.
//!
//! Keys cross the seam as X11 keysyms. That is RFB's own representation, but it is
//! also the only one that names a *character* rather than a position on a keyboard,
//! which is what a terminal can actually report -- crossterm gives us `KeyCode`, not
//! a scancode. A protocol that wants scancodes (RDP does) translates on its own
//! side, the way FreeRDP and xrdp do.

pub mod vnc;

use anyhow::Result;

/// A rectangle of the remote framebuffer.
///
/// 16-bit throughout, which is what every remote desktop protocol carries on the
/// wire. The renderer widens to 32-bit at the framebuffer boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// The size of the remote desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    pub width: u16,
    pub height: u16,
}

impl From<(u16, u16)> for Screen {
    fn from(tuple: (u16, u16)) -> Self {
        Self {
            width: tuple.0,
            height: tuple.1,
        }
    }
}

/// One screen in the remote desktop's layout.
///
/// The `id` and any `flags` bits are opaque: they come from the backend and must be
/// handed back unchanged in an [`Input::Resize`], because the id is how a server
/// tells a moved screen from a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenInfo {
    pub id: u32,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub flags: u32,
}

/// The desktop layout, and why it was reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenLayout {
    pub screen: Screen,
    pub screens: Vec<ScreenInfo>,
    /// Why the layout was sent: 0 the server changed it itself, 1 this client
    /// asked, 2 another client asked. Unknown values are treated as 0.
    pub reason: u16,
    /// Only meaningful when `reason` is 1 or 2.
    pub status: ResizeStatus,
}

impl ScreenLayout {
    /// Did this arrive because we asked for it?
    pub fn is_reply_to_us(&self) -> bool {
        self.reason == 1
    }
}

/// How a backend answered an [`Input::Resize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeStatus {
    Success,
    Prohibited,
    OutOfResources,
    InvalidLayout,
    /// The server does not control the layout directly -- a hypervisor handing
    /// the request to a guest, say -- and cannot say yet whether it worked. A
    /// later layout may report success, possibly much later.
    Forwarded,
    Unknown(u16),
}

impl ResizeStatus {
    /// A short reason, for the status line.
    pub fn describe(self) -> &'static str {
        match self {
            ResizeStatus::Success => "accepted",
            ResizeStatus::Prohibited => "resize prohibited",
            ResizeStatus::OutOfResources => "server out of resources",
            ResizeStatus::InvalidLayout => "layout rejected",
            ResizeStatus::Forwarded => "request forwarded",
            ResizeStatus::Unknown(_) => "resize refused",
        }
    }
}

/// A key going down or coming up, named by X11 keysym.
///
/// See the module documentation for why a keysym and not a scancode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub keysym: u32,
    pub down: bool,
}

/// Where the pointer is and which buttons are held.
///
/// The button bits are RFB's, from RFC 6143 section 7.5.5: left, middle, right,
/// then the four wheel directions. A backend whose protocol numbers them
/// differently translates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pointer {
    pub x: u16,
    pub y: u16,
    pub buttons: u8,
}

/// Something that happened on the remote desktop.
#[derive(Debug, Clone)]
pub enum Update {
    /// The remote framebuffer is now this size.
    Resolution(Screen),
    /// Pixel data for a rectangle, four bytes per pixel in BGRA order.
    Bgra(Rect, Vec<u8>),
    /// Pixel data for a rectangle, as a JPEG. The rectangle's position is
    /// authoritative; its size is whatever the JPEG says.
    Jpeg(Rect, Vec<u8>),
    /// Copy a rectangle of the framebuffer from `from` to `dst`.
    Copy { dst: Rect, from: (u16, u16) },
    /// Every rectangle of one update has been consumed.
    ///
    /// This is what lets the session pace requests instead of firing them on a
    /// timer, and where it measures the round trip when it is the one asking.
    FrameEnd,
    /// The desktop layout. Receiving one at all means resizing can be asked for.
    Layout(ScreenLayout),
    /// A new pointer shape. `bgra` is `size.0 * size.1` pixels.
    Cursor {
        size: (u16, u16),
        hotspot: (u16, u16),
        bgra: Vec<u8>,
    },
    /// The remote clipboard changed.
    Clipboard(String),
    /// Whether text sent with [`Input::Clipboard`] survives intact.
    ///
    /// `true` means the remote can only carry Latin-1, so anything outside it will be
    /// substituted and the user should be told. A session assumes `true` until a
    /// backend says otherwise, because for RFB the answer takes a negotiation and
    /// being wrong the other way loses characters silently.
    ClipboardLossy(bool),
    /// The remote lock-key state, which is worth having because a remote caps lock
    /// that disagrees with the local one makes every keystroke come out in the
    /// wrong case.
    LockKeys { num: bool, caps: bool },
    /// Frames now arrive without being asked for (`true`), or have stopped doing so
    /// (`false`). A session starts out assuming `false`.
    ///
    /// A backend that only ever pushes says `true` once, before anything else.
    Pushing(bool),
    /// A round trip can be measured from now on, with [`Input::ProbeLatency`].
    LatencyAvailable,
    /// The round trip asked for by [`Input::ProbeLatency`] came back.
    LatencyProbe,
    /// Ring a bell.
    Bell,
    /// The connection failed. Terminal: no further updates will arrive.
    Error(String),
}

/// Something to ask the remote desktop for.
#[derive(Debug, Clone)]
pub enum Input {
    /// Ask for a frame. `incremental` asks only for what changed; a full request
    /// is also what makes a backend report its layout.
    ///
    /// Ignored by a backend that pushes frames unasked.
    Refresh {
        incremental: bool,
    },
    Key(Key),
    Pointer(Pointer),
    /// Put text on the remote clipboard.
    ///
    /// The backend chooses how: the text may go now, or be announced and sent when
    /// something on the remote side pastes. Text outside Latin-1 is coerced by a
    /// backend that cannot carry it, which is what [`Update::ClipboardLossy`] warns
    /// about beforehand.
    Clipboard(String),
    /// Ask for a round trip to be measured, answered with [`Update::LatencyProbe`].
    ///
    /// Only sent after [`Update::LatencyAvailable`].
    ProbeLatency,
    /// Ask the remote desktop to change size.
    ///
    /// Only sent after an [`Update::Layout`], and the screen ids and flag bits from
    /// that layout have to be carried over.
    Resize {
        width: u16,
        height: u16,
        screens: Vec<ScreenInfo>,
    },
}

/// A live connection to a remote desktop.
///
/// Every method takes `&self`: the session holds one backend and both reads from it
/// and writes to it, from the same task, without being able to split the borrow.
///
/// [`Backend::recv`] is called from a `select!` and so is dropped and re-entered
/// constantly. It must be cancel-safe: an implementation that has taken an update
/// off the wire must not await anything before returning it, or the update is lost
/// the next time a keystroke wins the race.
///
/// A backend either asks for the first frame itself while connecting, or announces
/// [`Update::Pushing(true)`](Update::Pushing) before anything else. Otherwise the
/// session waits out its watchdog before asking for one.
pub trait Backend {
    /// Wait for the next thing to happen on the remote desktop.
    async fn recv(&self) -> Result<Update>;

    /// Send something to the remote desktop.
    async fn send(&self, input: Input) -> Result<()>;

    /// The framebuffer size as last reported.
    async fn resolution(&self) -> (u16, u16);

    /// What to call this desktop in the status line.
    async fn name(&self) -> String;

    /// Stop, releasing whatever tasks the backend is running.
    async fn close(&self);
}

/// Somewhere to connect to, and the credentials for it.
///
/// Separate from [`Backend`] because the session reconnects: it needs to be able to
/// open the connection again, with the password it was given the first time.
pub trait Connect {
    type Backend: Backend;

    /// What to call this in messages to the user.
    fn address(&self) -> &str;

    /// Open a connection.
    async fn connect(&self) -> Result<Self::Backend, ConnectError>;

    /// Supply the password the remote asked for, for this attempt and every later
    /// one.
    fn use_password(&mut self, password: String);
}

/// Why a connection attempt did not produce a session.
#[derive(Debug)]
pub enum ConnectError {
    /// The remote wants a password and none was supplied. The caller is expected
    /// to prompt and try again through [`Connect::use_password`].
    NeedsPassword,
    Failed(anyhow::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NeedsPassword => write!(f, "a password is required"),
            ConnectError::Failed(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for ConnectError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_resize_names_its_reason() {
        assert_eq!(ResizeStatus::Prohibited.describe(), "resize prohibited");
        assert_eq!(ResizeStatus::Unknown(99).describe(), "resize refused");
    }

    #[test]
    fn only_reason_one_is_a_reply_to_this_client() {
        let layout = |reason| ScreenLayout {
            screen: (100, 100).into(),
            screens: vec![],
            reason,
            status: ResizeStatus::Success,
        };
        assert!(layout(1).is_reply_to_us());
        assert!(!layout(0).is_reply_to_us());
        assert!(!layout(2).is_reply_to_us());
    }
}
