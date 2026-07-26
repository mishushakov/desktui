//! The RFB backend.
//!
//! Everything RFB-shaped that the session used to carry is here: which encodings to
//! negotiate, the fence flags behind a latency probe, and the continuous-updates
//! dance. The session is left with the two facts it acts on -- whether frames arrive
//! unasked, and whether a round trip can be measured -- which arrive as
//! [`Update::Pushing`] and [`Update::LatencyAvailable`].

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::TcpStream;

use super::{Connect, ConnectError, Input, Rect, ResizeStatus, Update};
use crate::rfb::{
    ClipboardCaps, PixelFormat, VncClient, VncConnector, VncEncoding, VncError, VncEvent, X11Event,
};

/// How long to wait for the TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Marks a fence as our own latency probe rather than a synchronisation request.
const RTT_PROBE_MARKER: &[u8] = b"desktui-rtt";

/// What to ask the server for, once connected.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub quality: Option<u8>,
    pub compression: Option<u8>,
    /// Ask the server for the cursor shape, so the pointer can be drawn locally
    /// instead of waiting for a round trip.
    pub local_cursor: bool,
    /// Offer to exchange clipboards at all.
    pub clipboard: bool,
}

/// An RFB server, and what it takes to connect to it.
pub struct Vnc {
    addr: String,
    password: Option<String>,
    opts: Options,
}

impl Vnc {
    pub fn new(addr: String, password: Option<String>, opts: Options) -> Self {
        Self {
            addr,
            password,
            opts,
        }
    }
}

impl Connect for Vnc {
    type Backend = VncBackend;

    fn address(&self) -> &str {
        &self.addr
    }

    fn use_password(&mut self, password: String) {
        self.password = Some(password);
    }

    async fn connect(&self) -> Result<Self::Backend, ConnectError> {
        match self.try_connect().await {
            Ok(client) => {
                // Seeded from ServerInit rather than left at zero: the server may
                // answer SetEncodings before it sends a single rectangle, and
                // continuous updates enabled for a 0x0 rectangle is a black screen.
                let size = client.resolution().await;
                Ok(VncBackend::new(client, size, self.opts))
            }
            Err(VncError::NoPassword) => Err(ConnectError::NeedsPassword),
            Err(err) => Err(ConnectError::Failed(
                anyhow::anyhow!("{err}").context(format!("connecting to {}", self.addr)),
            )),
        }
    }
}

impl Vnc {
    async fn try_connect(&self) -> Result<VncClient, VncError> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&self.addr))
            .await
            .map_err(|_| VncError::General(format!("timed out connecting to {}", self.addr)))?
            .map_err(VncError::IoError)?;
        // Input latency is the whole point; do not let Nagle sit on a click.
        stream.set_nodelay(true).map_err(VncError::IoError)?;

        let password = self.password.clone();
        VncConnector::new(stream)
            .set_auth_method(async move { password.ok_or(VncError::NoPassword) })
            // Order is preference order. Tight first: it is what every modern server
            // does best, and its JPEG rectangles are the cheapest way to move a busy
            // screen.
            .add_encoding(VncEncoding::Tight)
            .add_encoding(VncEncoding::Zrle)
            .add_encoding(VncEncoding::CopyRect)
            .add_encoding(VncEncoding::Raw)
            .add_encoding(VncEncoding::ExtendedDesktopSizePseudo)
            .add_encoding(VncEncoding::DesktopSizePseudo)
            .add_encoding(VncEncoding::LastRectPseudo)
            // Ask the server to push frames rather than answer a request each time, which
            // saves a round trip per frame; Fence is what makes that safe to negotiate.
            .add_encoding(VncEncoding::ContinuousUpdatesPseudo)
            .add_encoding(VncEncoding::FencePseudo)
            // And to tell us its lock-key state, so a caps lock that disagrees with the
            // local keyboard can be corrected instead of shouting.
            .add_encoding(VncEncoding::QemuLedStatePseudo)
            // Asking for the cursor shape stops the server drawing the pointer into the
            // framebuffer, which is what lets it move at local speed instead of waiting for
            // a round trip. Not asked for in a view-only session: there is no local pointer
            // worth drawing there, and letting the server composite its own is the only way
            // to see where the real one is.
            .add_encodings(if self.opts.local_cursor {
                &[VncEncoding::CursorPseudo][..]
            } else {
                &[]
            })
            // The clipboard in UTF-8 rather than Latin-1, and announced rather than
            // pushed. Left out entirely with --no-clipboard: the encoding is a standing
            // offer to exchange clipboards, and a session that wants none should not
            // make it.
            .add_encodings(if self.opts.clipboard {
                &[VncEncoding::ExtendedClipboardPseudo][..]
            } else {
                &[]
            })
            .set_quality(self.opts.quality)
            .set_compression(self.opts.compression)
            .allow_shared(true)
            // BGRA is what an x86 server produces natively, so this is the format
            // that costs neither side a swizzle on the wire. The pack to RGB happens
            // once per tile at the end of the pipeline.
            .set_pixel_format(PixelFormat::bgra())
            .build()?
            .try_start()
            .await?
            .finish()
    }
}

/// What the backend has to remember between events.
#[derive(Default)]
struct State {
    /// Replies the protocol owes the server, waiting to go out.
    ///
    /// Queued rather than sent on the spot because [`Backend::recv`] is cancelled
    /// every time a keystroke or a render tick beats it, and awaiting a send after
    /// an update has been taken off the wire would lose that update. Nothing here
    /// waits long: the session is back in `recv` within microseconds, and the queue
    /// is flushed before anything else is read.
    outbox: VecDeque<X11Event>,
    /// A fence has been seen, so the server understands them.
    fence_seen: bool,
    /// Continuous updates are on, and for which framebuffer size.
    ///
    /// The server remembers the rectangle it was told to push and a resize does not
    /// change its mind, so this has to be said again after one -- the difference
    /// between a desktop that grows and one that grows a black band down two sides.
    pushing_for: Option<(u16, u16)>,
    /// The framebuffer size, tracked so a resize can re-arm the above.
    ///
    /// A second copy of what the engine already knows, because deciding to re-arm
    /// happens inside a lock with nothing to await on. Both are derived the same
    /// way: a reported resolution, or a layout the server stands behind.
    size: (u16, u16),
    /// What the server accepts on the extended clipboard, once its `caps` message has
    /// arrived. `None` means the extension is not in play and the Latin-1 `CutText`
    /// messages are all there is.
    clipboard: Option<ClipboardCaps>,
    /// Text we have told the server we hold and have not been asked for yet.
    ///
    /// The extension moves ownership before it moves data: a local paste announces
    /// itself, and the bytes go over when something on the remote side pastes. Kept
    /// rather than taken when that happens, because it can be pasted more than once.
    announced: Option<String>,
    /// Whether the session asked for clipboards to be exchanged at all.
    clipboard_wanted: bool,
}

/// A live RFB session.
pub struct VncBackend {
    client: VncClient,
    state: Mutex<State>,
}

impl VncBackend {
    fn new(client: VncClient, size: (u16, u16), opts: Options) -> Self {
        Self {
            client,
            state: Mutex::new(State {
                size,
                clipboard_wanted: opts.clipboard,
                ..State::default()
            }),
        }
    }

    /// Send whatever the protocol owes the server.
    ///
    /// Called before reading, never after: cancellation here costs nothing, because
    /// an event that fails to go out is still in the queue.
    async fn flush(&self) -> Result<()> {
        loop {
            let Some(event) = self.state.lock().unwrap().outbox.front().cloned() else {
                return Ok(());
            };
            self.client
                .input(event)
                .await
                .map_err(|err| anyhow::anyhow!("{err}"))
                .context("failed to answer the server")?;
            self.state.lock().unwrap().outbox.pop_front();
        }
    }

    /// Turn one RFB event into an update, queueing whatever reply it obliges.
    fn translate(&self, event: VncEvent) -> Option<Update> {
        self.state.lock().unwrap().translate(event)
    }
}

impl State {
    /// Turn one RFB event into an update, queueing whatever reply it obliges.
    ///
    /// `None` means the event was pure protocol and the session has no business
    /// hearing about it.
    fn translate(&mut self, event: VncEvent) -> Option<Update> {
        match event {
            VncEvent::RawImage(rect, data) => Some(Update::Bgra(rect, data)),
            VncEvent::JpegImage(rect, data) => Some(Update::Jpeg(rect, data)),
            VncEvent::Copy(dst, src) => Some(Update::Copy {
                dst,
                from: (src.x, src.y),
            }),
            VncEvent::SetResolution(screen) => {
                self.adopt_size((screen.width, screen.height));
                Some(Update::Resolution(screen))
            }
            VncEvent::DesktopLayout(layout) => {
                // A refused reply leaves the dimensions undefined, so only adopt a
                // size the server actually stands behind.
                if !layout.is_reply_to_us() || layout.status == ResizeStatus::Success {
                    self.adopt_size((layout.screen.width, layout.screen.height));
                }
                Some(Update::Layout(layout))
            }
            VncEvent::FramebufferUpdateEnd => Some(Update::FrameEnd),
            VncEvent::SetCursor(rect, pixels) => Some(Update::Cursor {
                size: (rect.width, rect.height),
                // The rectangle's x and y are the hotspot rather than a position,
                // which is a trap worth leaving behind the seam.
                hotspot: (rect.x, rect.y),
                bgra: pixels,
            }),
            VncEvent::Text(text) => Some(Update::Clipboard(text)),
            VncEvent::ClipboardCaps(caps) => {
                // The extension is live from here: the clipboard is UTF-8 in both
                // directions, and a remote selection arrives as an announcement rather
                // than as a copy of itself. Only worth telling the session if the caps
                // actually cover text going the other way -- a server that takes none
                // leaves us on the lossy path regardless.
                tracing::debug!("extended clipboard negotiated: {caps:?}");
                self.clipboard = Some(caps);
                Some(Update::ClipboardLossy(!self.clipboard_is_usable()))
            }
            VncEvent::ClipboardNotify { text } => {
                // Nothing has been transferred yet -- that is the point of a notify --
                // so ask for it. With the extension never advertised this cannot
                // arrive; the check is belt and braces around a server that sends one
                // anyway.
                if text && self.clipboard_wanted {
                    self.outbox.push_back(X11Event::ClipboardRequest);
                }
                None
            }
            VncEvent::ClipboardRequest => {
                // Something on the remote side pasted, so the text we announced is
                // wanted now.
                match self.announced.clone() {
                    Some(text) if self.clipboard_wanted => {
                        tracing::debug!("sending the announced clipboard, {} bytes", text.len());
                        self.outbox.push_back(X11Event::ClipboardProvide(text));
                    }
                    _ => tracing::debug!("server asked for a clipboard we never announced"),
                }
                None
            }
            VncEvent::Bell => Some(Update::Bell),
            VncEvent::LedState { num, caps, .. } => Some(Update::LockKeys { num, caps }),
            VncEvent::SetPixelFormat(pf) => {
                // Nothing above acts on this: the connector pinned the format.
                tracing::debug!("server pixel format: {pf:?}");
                None
            }
            VncEvent::EndOfContinuousUpdates => {
                // The first one of these is the server saying the extension exists.
                // Any later one means it stopped, and asking again is how it restarts.
                self.pushing_for = None;
                self.enable_pushing();
                Some(Update::Pushing(true))
            }
            VncEvent::Fence { flags, payload } => {
                // A fence at all means the server understands them, which is what
                // makes a latency probe possible.
                let first = !std::mem::replace(&mut self.fence_seen, true);

                if flags & crate::rfb::fence::REQUEST != 0 {
                    // A fence with the request bit set has to come back with that bit
                    // cleared, along with any flag we do not understand. Ours arrive
                    // in order and are handled one at a time, which is what
                    // BlockBefore and BlockAfter ask for, so echoing is all there is
                    // to do.
                    let echo =
                        flags & (crate::rfb::fence::BLOCK_BEFORE | crate::rfb::fence::BLOCK_AFTER);
                    self.outbox.push_back(X11Event::Fence {
                        flags: echo,
                        payload,
                    });
                    // The session hears about the capability, not the fence.
                    return first.then_some(Update::LatencyAvailable);
                }

                if payload == RTT_PROBE_MARKER {
                    return Some(Update::LatencyProbe);
                }
                tracing::debug!("unsolicited fence response, flags {flags:#x}");
                first.then_some(Update::LatencyAvailable)
            }
            VncEvent::Error(err) => Some(Update::Error(err)),
        }
    }

    /// Note a new framebuffer size, re-arming continuous updates if they were on.
    fn adopt_size(&mut self, size: (u16, u16)) {
        if size == self.size || size.0 == 0 || size.1 == 0 {
            return;
        }
        self.size = size;
        if self.pushing_for.is_some() {
            self.pushing_for = None;
            self.enable_pushing();
        }
    }

    /// Does the extension cover text in both directions?
    ///
    /// A server may answer the `SetEncodings` and still decline text, or decline to be
    /// given any, which leaves the legacy Latin-1 message as the only way across.
    fn clipboard_is_usable(&self) -> bool {
        self.clipboard
            .is_some_and(|caps| caps.takes_text() && caps.takes_provide())
    }

    /// Which message carries locally pasted text, and whether it goes now.
    ///
    /// Over the extension the text goes as UTF-8 and arrives whole. Without it the
    /// payload is Latin-1 and everything outside it has to be substituted, which is
    /// why a server that negotiated the extension is worth the extra round trip.
    fn paste(&mut self, text: String) -> X11Event {
        if let Some(caps) = self.clipboard.filter(|_| self.clipboard_is_usable()) {
            // The size the server's limit applies to is the text as it goes on the
            // wire: CRLF line endings, and the terminating null that the length counts.
            let wire_len = text.len() + text.matches('\n').count() + 1;
            if wire_len as u64 <= u64::from(caps.unsolicited_text()) || !caps.takes_notify() {
                // Either small enough to push unasked, or a server that offered no way
                // to announce it, which leaves pushing it the only thing left to try.
                return X11Event::ClipboardProvide(text);
            }
            // Announce it and keep it. The server asks when something over there
            // pastes, which is the point: a clipboard nobody reads costs one small
            // message instead of the whole text.
            self.announced = Some(text);
            return X11Event::ClipboardNotify;
        }

        // Legacy `ClientCutText` is Latin-1 only. Substitute rather than drop:
        // deleting characters silently shortens the text and moves everything after
        // them, where a question mark leaves the shape intact and is visibly a
        // substitution. noVNC does the same. The session has already warned about it,
        // having been told the clipboard is lossy.
        X11Event::CopyText(
            text.chars()
                .map(|c| if (c as u32) > 0xff { '?' } else { c })
                .collect(),
        )
    }

    /// Ask the server to push updates for the whole framebuffer.
    fn enable_pushing(&mut self) {
        if self.pushing_for == Some(self.size) {
            return;
        }
        let (width, height) = self.size;
        self.outbox.push_back(X11Event::EnableContinuousUpdates {
            enable: true,
            rect: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        });
        self.pushing_for = Some(self.size);
        tracing::debug!("continuous updates enabled for {width}x{height}");
    }
}

impl super::Backend for VncBackend {
    async fn recv(&self) -> Result<Update> {
        loop {
            // Before reading, so that a cancellation costs at most a repeated flush.
            self.flush().await?;
            let event = self
                .client
                .recv_event()
                .await
                .map_err(|err| anyhow::anyhow!("{err}"))?;
            // No await between taking the event and returning it, or a keystroke
            // winning the race in `select!` would drop a frame.
            if let Some(update) = self.translate(event) {
                return Ok(update);
            }
        }
    }

    async fn send(&self, input: Input) -> Result<()> {
        // Keep our own replies ahead of the session's traffic.
        self.flush().await?;
        let event = match input {
            Input::Refresh { incremental: true } => X11Event::Refresh,
            Input::Refresh { incremental: false } => X11Event::FullRefresh,
            Input::Key(key) => X11Event::KeyEvent(key),
            Input::Pointer(pointer) => X11Event::PointerEvent(pointer),
            Input::Clipboard(text) => self.state.lock().unwrap().paste(text),
            // No block flags: the server can answer immediately, which is the point.
            Input::ProbeLatency => X11Event::Fence {
                flags: crate::rfb::fence::REQUEST,
                payload: RTT_PROBE_MARKER.to_vec(),
            },
            Input::Resize {
                width,
                height,
                screens,
            } => X11Event::SetDesktopSize {
                width,
                height,
                screens,
            },
        };
        self.client
            .input(event)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    async fn resolution(&self) -> (u16, u16) {
        self.client.resolution().await
    }

    async fn name(&self) -> String {
        self.client.name().await
    }

    async fn close(&self) {
        self.client.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{Screen, ScreenLayout};

    /// Every test here drives the state machine, which is the whole of the
    /// translation and needs no socket behind it.
    fn state() -> State {
        State::default()
    }

    fn request(flags: u32, payload: &[u8]) -> VncEvent {
        VncEvent::Fence {
            flags,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn a_server_fence_is_echoed_without_the_request_bit() {
        let mut state = state();
        let update = state.translate(request(
            crate::rfb::fence::REQUEST
                | crate::rfb::fence::BLOCK_BEFORE
                | crate::rfb::fence::SYNC_NEXT,
            b"server's own",
        ));
        // The session hears that latency can be measured, not about the fence.
        assert!(matches!(update, Some(Update::LatencyAvailable)));

        match state.outbox.pop_front().unwrap() {
            X11Event::Fence { flags, payload } => {
                // Request cleared, BlockBefore kept, SyncNext dropped: we do not
                // implement it and must not claim to.
                assert_eq!(flags, crate::rfb::fence::BLOCK_BEFORE);
                assert_eq!(payload, b"server's own");
            }
            other => panic!("expected a fence, got {other:?}"),
        }
    }

    #[test]
    fn only_the_first_fence_announces_the_capability() {
        let mut state = state();
        assert!(matches!(
            state.translate(request(crate::rfb::fence::REQUEST, b"")),
            Some(Update::LatencyAvailable)
        ));
        assert!(
            state
                .translate(request(crate::rfb::fence::REQUEST, b""))
                .is_none()
        );
        // Both were still echoed.
        assert_eq!(state.outbox.len(), 2);
    }

    #[test]
    fn our_own_probe_coming_back_is_a_measurement() {
        let mut state = state();
        // The server proves fences work first, the way a real one does.
        state.translate(request(crate::rfb::fence::REQUEST, b""));
        state.outbox.clear();

        let update = state.translate(request(0, RTT_PROBE_MARKER));
        assert!(matches!(update, Some(Update::LatencyProbe)));
        // A reply is answered with nothing.
        assert!(state.outbox.is_empty());
    }

    #[test]
    fn end_of_continuous_updates_asks_for_them_again() {
        let mut state = state();
        state.size = (800, 600);

        let update = state.translate(VncEvent::EndOfContinuousUpdates);
        assert!(matches!(update, Some(Update::Pushing(true))));
        match state.outbox.pop_front().unwrap() {
            X11Event::EnableContinuousUpdates { enable, rect } => {
                assert!(enable);
                assert_eq!((rect.width, rect.height), (800, 600));
            }
            other => panic!("expected an enable, got {other:?}"),
        }
    }

    #[test]
    fn a_resize_re_arms_continuous_updates_for_the_new_size() {
        // The bug this guards: the server remembers the rectangle it was told to
        // push, so without saying it again a desktop that grows gains a black band
        // down two sides.
        let mut state = state();
        state.size = (800, 600);
        state.translate(VncEvent::EndOfContinuousUpdates);
        state.outbox.clear();

        state.translate(VncEvent::SetResolution(Screen {
            width: 1600,
            height: 900,
        }));
        match state.outbox.pop_front().unwrap() {
            X11Event::EnableContinuousUpdates { rect, .. } => {
                assert_eq!((rect.width, rect.height), (1600, 900));
            }
            other => panic!("expected an enable, got {other:?}"),
        }
    }

    #[test]
    fn pushing_is_enabled_for_the_size_known_at_connect_time() {
        // The regression this guards: a server may answer SetEncodings before it
        // sends a single rectangle, so waiting for a resolution event to learn the
        // size means enabling continuous updates for a 0x0 rectangle -- which is a
        // black screen. The size comes from ServerInit instead.
        let mut state = State {
            size: (1024, 768),
            ..State::default()
        };
        state.translate(VncEvent::EndOfContinuousUpdates);
        match state.outbox.pop_front().unwrap() {
            X11Event::EnableContinuousUpdates { rect, .. } => {
                assert_eq!((rect.width, rect.height), (1024, 768));
            }
            other => panic!("expected an enable, got {other:?}"),
        }
    }

    #[test]
    fn a_resize_says_nothing_when_updates_were_never_being_pushed() {
        let mut state = state();
        state.translate(VncEvent::SetResolution(Screen {
            width: 1600,
            height: 900,
        }));
        assert!(state.outbox.is_empty());
    }

    #[test]
    fn a_refused_resize_does_not_move_the_remembered_size() {
        let mut state = state();
        state.size = (800, 600);
        state.translate(VncEvent::EndOfContinuousUpdates);
        state.outbox.clear();

        // The spec leaves the dimensions of a refusal undefined, so adopting them
        // would re-arm pushing for a rectangle that does not exist.
        state.translate(VncEvent::DesktopLayout(ScreenLayout {
            screen: (4096, 4096).into(),
            screens: vec![],
            reason: 1,
            status: ResizeStatus::Prohibited,
        }));
        assert_eq!(state.size, (800, 600));
        assert!(state.outbox.is_empty());
    }

    fn caps_taking(limit: u32) -> ClipboardCaps {
        ClipboardCaps::taking_text_up_to(limit)
    }

    #[test]
    fn negotiated_caps_take_the_clipboard_off_the_lossy_path() {
        let mut state = State {
            clipboard_wanted: true,
            ..State::default()
        };
        let update = state.translate(VncEvent::ClipboardCaps(ClipboardCaps::taking_text_up_to(
            1024,
        )));
        assert!(matches!(update, Some(Update::ClipboardLossy(false))));
    }

    #[test]
    fn a_paste_the_server_will_take_unasked_goes_straight_over() {
        let mut state = State {
            clipboard: Some(caps_taking(1024)),
            clipboard_wanted: true,
            ..State::default()
        };
        match state.paste("small".into()) {
            X11Event::ClipboardProvide(text) => assert_eq!(text, "small"),
            other => panic!("expected a provide, got {other:?}"),
        }
        // Nothing is being held for later: it has already gone.
        assert!(state.announced.is_none());
    }

    #[test]
    fn a_paste_over_the_servers_limit_is_announced_and_kept() {
        // The point of the extension: a clipboard nobody reads costs one small message
        // instead of the whole text.
        let mut state = State {
            clipboard: Some(caps_taking(8)),
            clipboard_wanted: true,
            ..State::default()
        };
        let long = "x".repeat(100);
        assert!(matches!(
            state.paste(long.clone()),
            X11Event::ClipboardNotify
        ));
        assert_eq!(state.announced.as_deref(), Some(long.as_str()));

        // And it goes over when the remote side pastes -- kept, not taken, because it
        // can be pasted more than once.
        assert!(state.translate(VncEvent::ClipboardRequest).is_none());
        match state.outbox.pop_front().unwrap() {
            X11Event::ClipboardProvide(text) => assert_eq!(text, long),
            other => panic!("expected a provide, got {other:?}"),
        }
        assert!(state.announced.is_some());
    }

    #[test]
    fn the_wire_length_counts_line_endings_and_the_terminating_null() {
        // Ten bytes on the wire: eight characters, one of which becomes CRLF and so
        // costs an extra byte, plus the null the length counts. A limit of ten takes it
        // and nine does not.
        let text = "abc\ndefg";
        assert_eq!(text.len() + text.matches('\n').count() + 1, 10);
        let paste = |limit| {
            State {
                clipboard: Some(caps_taking(limit)),
                clipboard_wanted: true,
                ..State::default()
            }
            .paste(text.into())
        };
        assert!(matches!(paste(10), X11Event::ClipboardProvide(_)));
        assert!(matches!(paste(9), X11Event::ClipboardNotify));
    }

    #[test]
    fn without_the_extension_a_paste_is_coerced_to_latin1() {
        // The session has already warned about this, having been told the clipboard is
        // lossy; coercing here is what stops UTF-8 bytes going out under a Latin-1
        // message.
        let mut state = state();
        match state.paste("piñata — ok".into()) {
            X11Event::CopyText(text) => {
                assert_eq!(text, "piñata ? ok");
                assert!(text.chars().all(|c| (c as u32) <= 0xff));
            }
            other => panic!("expected a legacy cut text, got {other:?}"),
        }
    }

    #[test]
    fn a_remote_selection_is_fetched_rather_than_waited_for() {
        let mut state = State {
            clipboard_wanted: true,
            ..State::default()
        };
        // A notify carries nothing, so the text has to be asked for.
        assert!(
            state
                .translate(VncEvent::ClipboardNotify { text: true })
                .is_none()
        );
        assert!(matches!(
            state.outbox.pop_front().unwrap(),
            X11Event::ClipboardRequest
        ));

        // An empty remote clipboard is not worth a round trip.
        state.translate(VncEvent::ClipboardNotify { text: false });
        assert!(state.outbox.is_empty());
    }

    #[test]
    fn no_clipboard_ignores_a_notify_the_server_sent_anyway() {
        let mut state = state();
        state.translate(VncEvent::ClipboardNotify { text: true });
        assert!(state.outbox.is_empty());
    }

    #[test]
    fn a_cursor_rectangle_becomes_a_size_and_a_hotspot() {
        let mut state = state();
        let update = state.translate(VncEvent::SetCursor(
            Rect {
                x: 3,
                y: 4,
                width: 16,
                height: 16,
            },
            vec![0; 16 * 16 * 4],
        ));
        match update {
            Some(Update::Cursor { size, hotspot, .. }) => {
                assert_eq!(size, (16, 16));
                assert_eq!(hotspot, (3, 4));
            }
            other => panic!("expected a cursor, got {other:?}"),
        }
    }
}
