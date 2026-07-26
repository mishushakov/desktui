//! A live VNC session.
//!
//! One loop selects over three sources: events decoded from the server, input
//! from the terminal, and a render tick. Everything on screen is composed in the
//! tick and handed to the writer thread as a single frame.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use futures::StreamExt;
use tokio::net::TcpStream;
use tokio::time::{MissedTickBehavior, interval};

use crate::app::{FpsMeter, describe, human_bytes};
use crate::cli::{Args, ScaleMode};
use crate::render::framebuffer::Framebuffer;
use crate::render::{FrameStats, Layout, Rect, Renderer};
use crate::rfb::{
    ClipboardCaps, PixelFormat, ResizeStatus, Screen, ScreenInfo, ScreenLayout, VncClient,
    VncConnector, VncEncoding, VncError, VncEvent, X11Event,
};
use crate::term::caps::Caps;
use crate::term::input::{Command, InputMapper, KeyOutcome, LockState};
use crate::term::writer::{Busy, FrameWriter};
use crate::term::{Metrics, TerminalGuard, kitty};
use crate::ui::menu::{self, Hit, Menu};
use crate::ui::status;
use crate::ui::theme::Theme;
use crate::ui::toast::Toast;

/// How long to wait for the TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A server that has gone quiet for this long gets its update request repeated.
/// Some servers drop one under load, and without this the session would simply
/// stop redrawing.
const UPDATE_WATCHDOG: Duration = Duration::from_secs(1);

/// Terminal resizes arrive in a stream while a window is dragged. Waiting this
/// long after the last one keeps us from asking the server to resize dozens of
/// times.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(250);

/// Floor on the gap between two update requests, so a server that answers an
/// incremental request instantly cannot spin us at full speed.
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(2);

/// How often to measure the round trip with a fence, once frames stop being requested.
const RTT_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// A probe this old was dropped by the server; stop waiting for it.
const RTT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Marks a fence as our own latency probe rather than a synchronisation request.
const RTT_PROBE_MARKER: &[u8] = b"desktui-rtt";

/// First pause before a reconnect attempt, doubling up to the cap.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(10);

/// The largest remote framebuffer we will allocate for, in bytes.
///
/// 512 MB is four bytes per pixel across roughly 11k by 11k, far past any real
/// desktop and far below the 17 GB the protocol's 16-bit dimensions permit.
const MAX_FRAMEBUFFER_BYTES: u64 = 512 << 20;

/// Would this size fit in a framebuffer we are willing to allocate?
fn framebuffer_size_is_plausible(size: (u16, u16)) -> bool {
    u64::from(size.0) * u64::from(size.1) * 4 <= MAX_FRAMEBUFFER_BYTES
}

/// Where the negotiation over the remote desktop's size has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resize {
    /// No layout rectangle has arrived yet, so we do not know whether the server
    /// can resize.
    Probing,
    /// The server never sent a layout rectangle in reply to a non-incremental
    /// request, which per the spec means it does not support resizing.
    Unsupported,
    /// A request is outstanding.
    Waiting { want: (u16, u16) },
    /// The remote desktop is exactly the size we asked for.
    Native,
    /// The server said no.
    Refused(ResizeStatus),
}

impl Resize {
    fn note(&self) -> Option<String> {
        match self {
            Resize::Unsupported => Some("server cannot resize; scaling instead".into()),
            Resize::Refused(status) => Some(format!("{}; scaling instead", status.describe())),
            _ => None,
        }
    }
}

pub async fn run(
    args: &Args,
    caps: &Caps,
    guard: &TerminalGuard,
    addr: &str,
    password: Option<String>,
) -> Result<()> {
    // The first connection is made before the alternate screen, so that a
    // password prompt is somewhere the user can see it. Later attempts reuse the
    // password and need no prompt.
    let (client, password) = connect(addr, password, args).await?;
    guard.begin_full_screen()?;

    let mut next = Some(client);
    let mut backoff = RECONNECT_BACKOFF;

    loop {
        let client = match next.take() {
            Some(client) => {
                backoff = RECONNECT_BACKOFF;
                client
            }
            None => match try_connect(addr, password.clone(), args).await {
                Ok(client) => {
                    backoff = RECONNECT_BACKOFF;
                    client
                }
                Err(err) => {
                    notice(&format!(
                        " reconnecting to {addr} in {}s -- {err}",
                        backoff.as_secs().max(1)
                    ));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
                    continue;
                }
            },
        };

        let mut session = Session::new(args, caps, client).await?;
        let result = session.run(args).await;
        // Let go of anything held on the remote before leaving, so a modifier does
        // not stay stuck in whatever had focus over there.
        session.release_input().await;
        session.client.close().await;
        drop(session);

        match result {
            // A clean exit is the user quitting.
            Ok(()) => return Ok(()),
            Err(err) if args.reconnect => {
                notice(&format!(" lost the session ({err}); reconnecting"));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
            }
            Err(err) => return Err(err),
        }
    }
}

/// Write a line straight to the terminal, between sessions.
///
/// Safe only here: the session and its writer thread are gone by this point, so
/// there is nothing to interleave with.
fn notice(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    // Bottom of the screen, without needing to know how tall it is.
    let _ = write!(out, "\x1b[999;1H\x1b[7m\x1b[K{text}\x1b[0m");
    let _ = out.flush();
}

/// Open the connection, prompting for a password only if the server asks for one
/// and none was supplied.
async fn connect(
    addr: &str,
    password: Option<String>,
    args: &Args,
) -> Result<(VncClient, Option<String>)> {
    match try_connect(addr, password.clone(), args).await {
        Ok(client) => Ok((client, password)),
        Err(VncError::NoPassword) => {
            let password = crate::prompt_password(addr)?;
            let client = try_connect(addr, Some(password.clone()), args)
                .await
                .map_err(|err| anyhow::anyhow!("{err}"))?;
            Ok((client, Some(password)))
        }
        Err(err) => Err(anyhow::anyhow!("{err}")).with_context(|| format!("connecting to {addr}")),
    }
}

async fn try_connect(
    addr: &str,
    password: Option<String>,
    args: &Args,
) -> Result<VncClient, VncError> {
    // Not in a view-only session: there is no local pointer worth drawing there, and
    // letting the server composite its own is the only way to see where the real one
    // is.
    let local_cursor = !args.view_only;
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| VncError::General(format!("timed out connecting to {addr}")))?
        .map_err(VncError::IoError)?;
    // Input latency is the whole point; do not let Nagle sit on a click.
    stream.set_nodelay(true).map_err(VncError::IoError)?;

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
        // a round trip.
        .add_encodings(if local_cursor {
            &[VncEncoding::CursorPseudo][..]
        } else {
            &[]
        })
        // The clipboard in UTF-8 rather than Latin-1, and announced rather than pushed.
        // Left out entirely with --no-clipboard: the encoding is a standing offer to
        // exchange clipboards, and a session that wants none should not make it.
        .add_encodings(if args.no_clipboard {
            &[]
        } else {
            &[VncEncoding::ExtendedClipboardPseudo][..]
        })
        .set_quality(args.quality)
        .set_compression(args.compression)
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

struct Session {
    client: VncClient,
    fb: Framebuffer,
    renderer: Renderer,
    writer: FrameWriter,
    metrics: Metrics,
    caps: Caps,
    input: InputMapper,
    server_name: String,

    mode: ScaleMode,
    pan: (u32, u32),
    remote: (u16, u16),
    screens: Vec<ScreenInfo>,
    resize: Resize,
    /// The size of the last request actually sent.
    ///
    /// Servers may accept a request and then snap to a mode of their own. Asking
    /// again whenever the granted size differs from the wanted one would loop for
    /// ever, so a repeat only follows a change in what *we* want.
    requested_size: Option<(u16, u16)>,

    awaiting_update: bool,
    requested_at: Instant,
    /// Round trip to the server, or `None` while it is unknown.
    ///
    /// Optional on purpose: with frames arriving unbidden there is no request to
    /// measure against, and showing the age of the last request we happened to send
    /// would be a number that only ever grows.
    rtt: Option<Duration>,
    /// The server has sent us a fence, so it understands them and can be asked to
    /// bounce one back.
    fence_supported: bool,
    /// When the outstanding latency probe went out.
    rtt_probe_at: Option<Instant>,
    /// When the last measurement completed, to space the probes out.
    rtt_measured_at: Option<Instant>,
    /// The server pushes updates without being asked, so no requests are sent.
    continuous_updates: bool,
    /// The rectangle continuous updates was last enabled for, so a resize can tell
    /// whether it needs to say so again.
    continuous_rect: Option<(u16, u16)>,
    /// The remote lock-key state, when the server tells us. `None` means unknown,
    /// which is also what it becomes right after a correction is sent: the answer
    /// takes a moment to arrive and acting twice would toggle it back.
    remote_caps_lock: Option<bool>,
    remote_num_lock: Option<bool>,

    view_only: bool,
    no_clipboard: bool,
    /// What the server accepts on the extended clipboard, once its `caps` message has
    /// arrived. `None` means the extension is not in play, and the Latin-1 `CutText`
    /// messages are all there is.
    clipboard_caps: Option<ClipboardCaps>,
    /// Text we have told the server we hold and have not been asked for yet.
    ///
    /// The extension moves ownership before it moves data: a local paste announces
    /// itself, and the bytes go over when something on the remote side pastes. Kept
    /// rather than taken when that happens, because it can be pasted more than once.
    announced_clipboard: Option<String>,
    /// Which palette the chrome wears. Dark to start, the bar having been that colour
    /// before there was a choice.
    theme: Theme,
    /// The command menu, and where the pointer is on it.
    menu: Menu,
    show_menu: bool,
    /// The menu was dismissed and its cells still have to be blanked. Drawing
    /// the image over them does not do it: the image sits below the text.
    clear_menu: bool,
    show_stats: bool,
    /// The notification popup in the top-right corner, and the note it is showing.
    toast: Toast,

    fps: FpsMeter,
    last_stats: FrameStats,
    dropped: u64,
    pending_metrics: Option<Instant>,
    /// When a resize was last acted on, so the first one after a quiet spell can be
    /// acted on at once rather than waiting out the debounce.
    metrics_applied_at: Option<Instant>,
    quit: bool,
}

impl Session {
    async fn new(args: &Args, caps: &Caps, client: VncClient) -> Result<Self> {
        let metrics = Metrics::query()?;
        let remote = client.resolution().await;
        let server_name = client.name().await;

        let fb = Framebuffer::new(u32::from(remote.0), u32::from(remote.1));
        let layout = Layout::compute(
            &metrics,
            args.scale,
            u32::from(remote.0),
            u32::from(remote.1),
            (0, 0),
        );

        Ok(Self {
            client,
            fb,
            renderer: Renderer::new(layout, true, args.transfer.resolve(caps)),
            writer: FrameWriter::spawn(),
            metrics,
            caps: caps.clone(),
            input: InputMapper::new(args.prefix_char(), caps.kitty_keyboard, caps.pixel_mouse),
            server_name,
            mode: args.scale,
            pan: (0, 0),
            remote,
            screens: Vec::new(),
            resize: Resize::Probing,
            requested_size: None,
            awaiting_update: true, // the engine asks for the first frame itself
            requested_at: Instant::now(),
            rtt: None,
            fence_supported: false,
            rtt_probe_at: None,
            rtt_measured_at: None,
            continuous_updates: false,
            continuous_rect: None,
            remote_caps_lock: None,
            remote_num_lock: None,
            view_only: args.view_only,
            no_clipboard: args.no_clipboard,
            clipboard_caps: None,
            announced_clipboard: None,
            theme: Theme::Dark,
            menu: Menu::new(args.prefix_char()),
            show_menu: false,
            clear_menu: false,
            show_stats: false,
            toast: Toast::default(),
            fps: FpsMeter::new(),
            last_stats: FrameStats::default(),
            dropped: 0,
            pending_metrics: None,
            metrics_applied_at: None,
            quit: false,
        })
    }

    async fn run(&mut self, args: &Args) -> Result<()> {
        let mut terminal = EventStream::new();
        let mut ticker = interval(Duration::from_micros(1_000_000 / u64::from(args.fps)));
        // Falling behind should drop frames, not queue them up to be raced
        // through later.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        while !self.quit {
            tokio::select! {
                event = self.client.recv_event() => match event {
                    Ok(event) => self.on_vnc(event).await?,
                    Err(err) => {
                        anyhow::bail!("the session ended: {err}");
                    }
                },
                Some(event) = terminal.next() => {
                    let event = event.context("failed to read terminal input")?;
                    self.on_terminal(event).await?;
                }
                _ = ticker.tick() => self.on_tick().await?,
            }
        }
        Ok(())
    }

    async fn on_vnc(&mut self, event: VncEvent) -> Result<()> {
        match event {
            VncEvent::RawImage(rect, data) => {
                if let Some(damage) = self.fb.apply_bgra(convert(rect), &data) {
                    self.renderer.mark(damage);
                }
            }
            VncEvent::JpegImage(rect, data) => match decode_jpeg(&data) {
                Ok((w, h, rgb)) => {
                    // Trust the rectangle header for placement, but the JPEG for
                    // its own dimensions.
                    let rect = Rect::new(u32::from(rect.x), u32::from(rect.y), w, h);
                    if let Some(damage) = self.fb.apply_rgb(rect, &rgb) {
                        self.renderer.mark(damage);
                    }
                }
                Err(err) => tracing::warn!("dropping an undecodable JPEG rectangle: {err}"),
            },
            VncEvent::Copy(dst, src) => {
                if let Some(damage) =
                    self.fb
                        .copy_rect(convert(dst), u32::from(src.x), u32::from(src.y))
                {
                    self.renderer.mark(damage);
                }
            }
            VncEvent::SetResolution(screen) => self.on_remote_size(screen, None).await?,
            VncEvent::DesktopLayout(layout) => self.on_layout(layout).await?,
            VncEvent::FramebufferUpdateEnd => {
                // Only meaningful when this update answers a request of ours. With
                // continuous updates there is no pair, and the fence probe measures it
                // instead.
                if self.awaiting_update && !self.continuous_updates {
                    self.rtt = Some(self.requested_at.elapsed());
                }
                self.awaiting_update = false;
                // The first update is also the answer to our capability probe: a
                // server that supports resizing must have included a layout
                // rectangle in it.
                if self.resize == Resize::Probing {
                    self.resize = Resize::Unsupported;
                    if let Some(note) = self.resize.note() {
                        self.set_note(note);
                    }
                    self.fall_back_from_native();
                }
                // Ask for the next update now, not on the next render tick. The
                // server can then encode the following frame while we are still
                // drawing this one; waiting for the tick leaves it idle for up to a
                // whole frame interval, which roughly halves the rate a moving
                // picture can reach.
                self.request_update().await?;
            }
            VncEvent::Text(text) => {
                if !self.no_clipboard {
                    self.copy_to_local_clipboard(&text);
                }
            }
            VncEvent::ClipboardCaps(caps) => {
                // The extension is live from here: the clipboard is UTF-8 in both
                // directions, and a remote selection arrives as an announcement rather
                // than as a copy of itself.
                tracing::debug!("extended clipboard negotiated: {caps:?}");
                self.clipboard_caps = Some(caps);
            }
            VncEvent::ClipboardNotify { text } => {
                // Nothing has been transferred yet -- that is the point of a notify --
                // so ask for it. With --no-clipboard the extension was never
                // advertised, so this cannot arrive; the check is belt and braces
                // around a server that sends one anyway.
                if text && !self.no_clipboard {
                    self.send(X11Event::ClipboardRequest).await?;
                }
            }
            VncEvent::ClipboardRequest => {
                // Something on the remote side pasted, so the text we announced is
                // wanted now.
                match self.announced_clipboard.clone() {
                    Some(text) if !self.view_only && !self.no_clipboard => {
                        tracing::debug!("sending the announced clipboard, {} bytes", text.len());
                        self.send(X11Event::ClipboardProvide(text)).await?;
                    }
                    _ => tracing::debug!("server asked for a clipboard we never announced"),
                }
            }
            VncEvent::Bell => {
                // A bell is the one thing worth passing straight through.
                let mut buf = self.writer.take_buffer();
                buf.push(0x07);
                let _ = self.writer.submit_blocking(buf);
            }
            VncEvent::SetPixelFormat(pf) => {
                tracing::debug!("server pixel format: {pf:?}");
            }
            VncEvent::LedState { num, caps, .. } => {
                // Only useful as something to compare the local keyboard against, so
                // scroll lock is ignored: no terminal reports it.
                tracing::debug!("remote lock keys: caps={caps} num={num}");
                self.remote_caps_lock = Some(caps);
                self.remote_num_lock = Some(num);
            }
            VncEvent::EndOfContinuousUpdates => {
                // The first one of these is the server saying the extension exists.
                // Any later one means it stopped, and asking again is how it restarts.
                if !self.continuous_updates {
                    tracing::info!("server supports continuous updates; enabling");
                    self.set_note("continuous updates: the server pushes frames".into());
                }
                self.continuous_updates = false;
                self.continuous_rect = None;
                self.enable_continuous_updates().await?;
            }
            VncEvent::Fence { flags, payload } => {
                tracing::debug!(
                    "fence from server, flags {flags:#x}, {} bytes",
                    payload.len()
                );
                // A fence at all means the server understands them, which is what makes
                // the latency probe possible.
                self.fence_supported = true;
                // A fence with the request bit set has to come back with that bit
                // cleared, along with any flag we do not understand. Ours arrive in
                // order and are handled one at a time, which is what BlockBefore and
                // BlockAfter ask for, so echoing is all there is to do.
                if flags & crate::rfb::fence::REQUEST != 0 {
                    let echo =
                        flags & (crate::rfb::fence::BLOCK_BEFORE | crate::rfb::fence::BLOCK_AFTER);
                    self.send(X11Event::Fence {
                        flags: echo,
                        payload,
                    })
                    .await?;
                } else if payload == RTT_PROBE_MARKER {
                    // Our own probe, back again: the gap is the round trip.
                    if let Some(sent) = self.rtt_probe_at.take() {
                        self.rtt = Some(sent.elapsed());
                        self.rtt_measured_at = Some(Instant::now());
                    }
                } else {
                    tracing::debug!("unsolicited fence response, flags {flags:#x}");
                }
            }
            VncEvent::SetCursor(rect, pixels) => {
                // The rectangle's x and y are the hotspot rather than a position.
                let cursor = crate::render::Cursor {
                    w: u32::from(rect.width),
                    h: u32::from(rect.height),
                    hot_x: u32::from(rect.x),
                    hot_y: u32::from(rect.y),
                    pixels,
                };
                tracing::debug!(
                    "cursor shape {}x{} hotspot {},{}",
                    cursor.w,
                    cursor.h,
                    cursor.hot_x,
                    cursor.hot_y
                );
                self.renderer.set_cursor(Some(cursor));
            }
            VncEvent::Error(err) => anyhow::bail!("the server connection failed: {err}"),
        }
        Ok(())
    }

    /// Adopt a new remote framebuffer size.
    async fn on_remote_size(&mut self, screen: Screen, note: Option<String>) -> Result<()> {
        let size = (screen.width, screen.height);
        if size == self.remote || size.0 == 0 || size.1 == 0 {
            return Ok(());
        }
        // A size is four bytes of allocation per pixel, and the protocol allows
        // 65535 in each direction -- seventeen gigabytes. Rust aborts the process
        // on a failed allocation, so an absurd size has to be refused here rather
        // than discovered by the allocator.
        if !framebuffer_size_is_plausible(size) {
            tracing::warn!(
                "ignoring an implausible framebuffer size of {}x{}",
                size.0,
                size.1
            );
            self.set_note(format!(
                "server reported an implausible size of {}x{}; ignored",
                size.0, size.1
            ));
            return Ok(());
        }
        tracing::info!("remote framebuffer is now {}x{}", size.0, size.1);
        self.remote = size;
        self.fb.resize(u32::from(size.0), u32::from(size.1));
        self.pan = (0, 0);
        self.relayout();
        if let Some(note) = note {
            self.set_note(note);
        }

        // The server remembers which rectangle it was told to push, and a resize does
        // not change its mind. Saying so again is the whole difference between a
        // desktop that grows and one that grows a black band down two sides: the region
        // beyond the old rectangle is never sent otherwise.
        if self.continuous_updates {
            self.continuous_rect = None;
            self.enable_continuous_updates().await?;
        }
        Ok(())
    }

    async fn on_layout(&mut self, layout: ScreenLayout) -> Result<()> {
        // Keep the screen ids: the server uses them to tell a moved screen from a
        // new one, and a request that invents its own would be rejected.
        if !layout.screens.is_empty() {
            self.screens = layout.screens.clone();
        }

        let was_probing = self.resize == Resize::Probing;
        if layout.is_reply_to_us() {
            match layout.status {
                ResizeStatus::Success => {
                    self.resize = Resize::Native;
                    self.on_remote_size(layout.screen, None).await?;
                    // The terminal may have been resized again while the request
                    // was in flight, so check we are still in sync. A no-op unless
                    // our own target actually moved.
                    self.request_native_size(false).await?;
                }
                // The server passed the request on and cannot say yet, so keep
                // waiting: a later layout may report success.
                ResizeStatus::Forwarded => {
                    self.set_note("resize request forwarded".into());
                }
                status => {
                    self.resize = Resize::Refused(status);
                    if let Some(note) = self.resize.note() {
                        self.set_note(note);
                    }
                    self.fall_back_from_native();
                }
            }
        } else {
            // Someone else changed it, or this is the initial report.
            self.on_remote_size(layout.screen, None).await?;
            if was_probing {
                self.resize = Resize::Unsupported; // provisional, refined below
            }
        }

        if was_probing {
            // A layout rectangle at all means SetDesktopSize is available.
            self.resize = match self.resize {
                Resize::Native => Resize::Native,
                _ => Resize::Unsupported,
            };
            if self.mode == ScaleMode::Native {
                self.resize = Resize::Probing;
                self.request_native_size(false).await?;
            }
        }
        Ok(())
    }

    /// The pixel size we want the remote desktop to be.
    ///
    /// Rounded down to even numbers: some servers snap to modes with even
    /// dimensions, and asking for something they will only approximate wastes a
    /// round trip.
    fn target_size(&self) -> (u16, u16) {
        let (w, h) = self.metrics.image_area();
        (
            (w.min(u32::from(u16::MAX)) as u16) & !1,
            (h.min(u32::from(u16::MAX)) as u16) & !1,
        )
    }

    /// Ask the server to match the terminal exactly.
    async fn request_native_size(&mut self, force: bool) -> Result<()> {
        let want = self.target_size();
        if want.0 == 0 || want.1 == 0 {
            return Ok(());
        }
        if want == self.remote {
            self.resize = Resize::Native;
            self.relayout();
            return Ok(());
        }
        if !force {
            // One request at a time. A terminal resize arriving while one is
            // outstanding gets picked up by the re-check after the reply lands.
            if matches!(self.resize, Resize::Waiting { .. }) {
                return Ok(());
            }
            // We already asked for exactly this and were given something else:
            // the server snapped to a mode of its own, and asking again would
            // loop for ever.
            if self.requested_size == Some(want) {
                return Ok(());
            }
        }

        // Carry the server's own screen ids and flags over. With no layout to go
        // by there is nothing legal to send, so wait for one.
        let screens = if self.screens.is_empty() {
            return Ok(());
        } else {
            let mut screens = self.screens.clone();
            let first = &mut screens[0];
            first.x = 0;
            first.y = 0;
            first.width = want.0;
            first.height = want.1;
            // A multi-head remote cannot be squeezed into one terminal window, so
            // ask for a single screen and let the others go.
            screens.truncate(1);
            screens
        };

        tracing::info!("asking the server for {}x{}", want.0, want.1);
        self.resize = Resize::Waiting { want };
        self.requested_size = Some(want);
        self.requested_at = Instant::now();
        self.client
            .input(X11Event::SetDesktopSize {
                width: want.0,
                height: want.1,
                screens,
            })
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        Ok(())
    }

    /// Native mapping is not available, so pick the best mapping that is.
    fn fall_back_from_native(&mut self) {
        if self.mode == ScaleMode::Native {
            let (area_w, area_h) = self.metrics.image_area();
            // A desktop that already fits needs no scaling at all; only a larger
            // one has to be resampled.
            self.mode = if u32::from(self.remote.0) <= area_w && u32::from(self.remote.1) <= area_h
            {
                ScaleMode::OneToOne
            } else {
                ScaleMode::Fit
            };
            self.relayout();
        }
    }

    async fn on_terminal(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key(key) => {
                // The menu holds the focus while it is up. Escape is what it says
                // puts it away, and the only key that does; everything else is a
                // local command or is swallowed. Presses only, or the release of the
                // chord that opened it would close it before it could be read.
                if self.show_menu {
                    if key.kind == KeyEventKind::Press && key.code == KeyCode::Esc {
                        self.dismiss_menu();
                        return Ok(());
                    }
                    // Local commands still work -- the menu is the list of them --
                    // and a release still reaches the remote, because a key held from
                    // before the menu opened is down over there until it does.
                    let locks = self.input.lock_state(&key);
                    match self.input.on_key_local(key) {
                        KeyOutcome::Ignored => {}
                        KeyOutcome::Keys(keys) => {
                            if !self.view_only {
                                self.sync_lock_keys(locks).await?;
                                for key in keys {
                                    self.send(X11Event::KeyEvent(key)).await?;
                                }
                            }
                        }
                        // Not `SendPrefix`: nothing typed at the menu belongs to the
                        // remote, and the menu no longer offers it.
                        KeyOutcome::Local(Command::SendPrefix) => {}
                        KeyOutcome::Local(cmd) => self.on_command(cmd).await?,
                    }
                    return Ok(());
                }
                let locks = self.input.lock_state(&key);
                match self.input.on_key(key) {
                    KeyOutcome::Ignored => {}
                    KeyOutcome::Keys(keys) => {
                        if !self.view_only {
                            // Before the keystroke, not after: the whole point is that the
                            // remote interprets *this* key with the right lock state.
                            self.sync_lock_keys(locks).await?;
                            for key in keys {
                                self.send(X11Event::KeyEvent(key)).await?;
                            }
                        }
                    }
                    KeyOutcome::Local(cmd) => self.on_command(cmd).await?,
                }
            }
            Event::Mouse(mouse) => {
                // Move the drawn cursor first and unconditionally: it is a local
                // overlay, so it should keep up with the hand even while a frame is in
                // flight.
                let (tx, ty) = self.input.terminal_pixel(&mouse, &self.metrics);
                let at = self.renderer.layout().terminal_px_to_dst(tx, ty);
                self.renderer.move_cursor(at);

                // The popup is offered the pointer before the menu is, because it is
                // drawn over the menu: half the notes there are arrive from a menu item,
                // and a click on one leaves the box up, so a cross that the menu could
                // swallow is a cross that does nothing most of the time it is on screen.
                //
                // Only a press on the cross is taken. The rest of the box is not a
                // target, so the pointer goes on reaching whatever is underneath as it
                // crosses a note that has already been read -- and a release is never
                // swallowed, which would leave a button held down over there.
                let (col, row) = self.input.terminal_cell(&mouse, &self.metrics);
                let on_close = self.toast.on_close(&self.metrics, col, row);
                self.toast.set_hover(on_close);
                let pressed = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
                if pressed && let Some(target) = self.toast.close_cells(&self.metrics) {
                    // A click that missed by a cell and one the popup never saw look the
                    // same from the outside; this is what tells them apart.
                    tracing::debug!("click at cell {col},{row}; the cross wants {target:?}");
                }
                if on_close && pressed {
                    if self.toast.dismiss() {
                        self.renderer.mark_all();
                    }
                    return Ok(());
                }

                // The menu takes the rest of the pointer while it is up. Nothing goes
                // through to the remote: a click meant for a menu item must not also land
                // on whatever is behind it.
                if self.show_menu {
                    // Except where the popup covers it: the box lights nothing under a
                    // pointer that is on the cross drawn over it.
                    if on_close {
                        self.menu.clear_hover();
                    } else {
                        self.on_menu_mouse(mouse).await?;
                    }
                    return Ok(());
                }

                if !self.view_only {
                    let events = {
                        let layout = *self.renderer.layout();
                        self.input.on_mouse(mouse, &layout, &self.metrics)
                    };
                    for event in events {
                        self.send(X11Event::PointerEvent(event)).await?;
                    }
                }
            }
            Event::Paste(text) => {
                if !self.view_only && !self.no_clipboard {
                    self.paste_to_remote(text).await?;
                }
            }
            Event::Resize(cols, rows) => {
                // Coalesce: a window drag produces a stream of these.
                tracing::debug!("resize event: {cols}x{rows} cells");
                self.pending_metrics = Some(Instant::now());
            }
            Event::FocusLost => {
                // Whatever is held here is not held there.
                self.release_input().await;
            }
            Event::FocusGained => {}
        }
        Ok(())
    }

    async fn on_command(&mut self, cmd: Command) -> Result<()> {
        match cmd {
            Command::Quit => self.quit = true,
            Command::SendPrefix => {
                if !self.view_only {
                    for key in self.input.literal_prefix() {
                        self.send(X11Event::KeyEvent(key)).await?;
                    }
                }
            }
            Command::FullRefresh => {
                self.renderer.mark_all();
                self.send(X11Event::FullRefresh).await?;
                self.awaiting_update = true;
                self.requested_at = Instant::now();
                self.set_note("full refresh requested".into());
            }
            Command::Renegotiate => {
                self.mode = ScaleMode::Native;
                self.resize = Resize::Probing;
                self.requested_size = None;
                self.request_native_size(true).await?;
                self.set_note("renegotiating the remote size".into());
            }
            Command::CycleMode => self.set_mode(self.mode.next()).await?,
            Command::Mode(mode) => self.set_mode(mode).await?,
            Command::Pan(dx, dy) => {
                let layout = *self.renderer.layout();
                let (max_x, max_y) = layout.pan_limits();
                if max_x == 0 && max_y == 0 {
                    self.set_note("nothing to pan: the whole screen is visible".into());
                } else {
                    // A screenful at a time, clamped to what exists.
                    let step_x = (layout.src.w / 2).max(1) as i64;
                    let step_y = (layout.src.h / 2).max(1) as i64;
                    let x = (i64::from(self.pan.0) + i64::from(dx) * step_x)
                        .clamp(0, i64::from(max_x)) as u32;
                    let y = (i64::from(self.pan.1) + i64::from(dy) * step_y)
                        .clamp(0, i64::from(max_y)) as u32;
                    self.pan = (x, y);
                    self.relayout();
                }
            }
            Command::ToggleViewOnly => {
                self.view_only = !self.view_only;
                if self.view_only {
                    self.release_input().await;
                }
                self.set_note(
                    if self.view_only {
                        "view-only"
                    } else {
                        "input enabled"
                    }
                    .into(),
                );
            }
            Command::ToggleStats => self.show_stats = !self.show_stats,
            Command::Theme(theme) => {
                self.theme = theme;
                // The menu is redrawn in the new palette on the next tick; the cells it
                // has already coloured are overwritten there rather than erased.
                self.set_note(format!(
                    "theme: {}",
                    if theme == Theme::Dark {
                        "dark"
                    } else {
                        "light"
                    }
                ));
            }
            Command::Menu => {
                if self.show_menu {
                    self.dismiss_menu();
                } else {
                    self.show_menu = true;
                }
            }
        }
        Ok(())
    }

    /// Adopt a scaling mode, however it was asked for: the key cycles, the menu
    /// names one outright.
    async fn set_mode(&mut self, mode: ScaleMode) -> Result<()> {
        self.mode = mode;
        self.pan = (0, 0);
        if mode == ScaleMode::Native {
            self.requested_size = None;
            self.request_native_size(true).await?;
        } else {
            self.relayout();
        }
        self.set_note(format!("scaling: {}", describe(self.renderer.layout())));
        Ok(())
    }

    /// Point at the menu, and run whatever is clicked on.
    ///
    /// A click runs its command and leaves the menu up. Only the dismissal takes it
    /// down -- the word in the title, the escape key, or the toggle at the top, which
    /// are three ways of asking for the same thing. So panning twice is two clicks
    /// rather than two trips through the menu, and picking a scaling mode shows the
    /// brackets move to it.
    ///
    /// A click that lands on nothing does nothing, off the box included: the menu has
    /// the focus, so there is nothing behind it to click on.
    async fn on_menu_mouse(&mut self, ev: MouseEvent) -> Result<()> {
        let (col, row) = self.input.terminal_cell(&ev, &self.metrics);
        let hit = self.menu.hit(&self.metrics, col, row);
        match ev.kind {
            MouseEventKind::Moved | MouseEventKind::Drag(_) => self.menu.set_hover(hit),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Hit::Item { command, .. } = hit {
                    self.on_command(command).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// What the menu should mark as being in force.
    fn menu_state(&self) -> menu::State {
        menu::State {
            mode: self.mode,
            theme: self.theme,
        }
    }

    /// Hide the menu and arrange for the cells it used to be blanked.
    ///
    /// Clearing the flag is not enough on its own, and neither is damaging the
    /// image: the menu is text, and tiles are placed below the text, so the box
    /// outlives any repaint until the cells themselves are erased. The tiles are
    /// marked too, for a terminal that treats an erase as dropping the placements
    /// underneath it.
    fn dismiss_menu(&mut self) {
        self.show_menu = false;
        self.clear_menu = true;
        // Or the row the pointer happened to be on would be lit the next time the
        // menu opens, before the pointer has moved to say so.
        self.menu.clear_hover();
        self.renderer.mark_all();
    }

    async fn on_tick(&mut self) -> Result<()> {
        // A resize has settled: adopt the new geometry, and ask the server to
        // match it if it is willing to.
        // Leading edge, then trailing: the first resize after a quiet spell is acted on
        // at once, and only a continuing stream of them is held back. Waiting out the
        // debounce for a single resize makes the window feel like it did not take, which
        // is the whole reason the delay was noticeable.
        let quiet_before = self
            .metrics_applied_at
            .is_none_or(|last| last.elapsed() >= RESIZE_DEBOUNCE);
        if let Some(at) = self.pending_metrics
            && (quiet_before || at.elapsed() >= RESIZE_DEBOUNCE)
        {
            self.pending_metrics = None;
            self.metrics_applied_at = Some(Instant::now());
            self.metrics = Metrics::query()?;
            tracing::debug!(
                "applying resize after {:?}: {}x{} cells, {}x{} px",
                at.elapsed(),
                self.metrics.cols,
                self.metrics.rows,
                self.metrics.px_w,
                self.metrics.px_h
            );
            self.relayout();
            if self.mode == ScaleMode::Native
                || matches!(self.resize, Resize::Native | Resize::Waiting { .. })
            {
                self.request_native_size(false).await?;
            }
        }

        // With frames arriving unbidden there is no request to time, so ask the server
        // to bounce a fence back and measure that instead.
        self.probe_round_trip().await?;

        // A pointer that stopped moving mid-rate-limit still has to arrive.
        if !self.view_only
            && let Some(pointer) = self.input.flush_motion()
        {
            self.send(X11Event::PointerEvent(pointer)).await?;
        }

        // Normally the request went out the moment the last update finished, so
        // this only covers the gaps: the very first frame, and a server that has
        // gone quiet.
        if !self.awaiting_update {
            self.request_update().await?;
        } else if !self.continuous_updates && self.requested_at.elapsed() > UPDATE_WATCHDOG {
            tracing::debug!(
                "no update for {:?}; asking again",
                self.requested_at.elapsed()
            );
            self.requested_at = Instant::now();
            self.send(X11Event::Refresh).await?;
        }

        self.draw()
    }

    /// Ask the server to push updates for the whole framebuffer.
    ///
    /// Re-sent after a resize: the rectangle is remembered by the server, and the
    /// spec says a second enable replaces the coordinates rather than adding to them.
    async fn enable_continuous_updates(&mut self) -> Result<()> {
        let size = self.remote;
        if self.continuous_updates && self.continuous_rect == Some(size) {
            return Ok(());
        }
        self.send(X11Event::EnableContinuousUpdates {
            enable: true,
            rect: crate::rfb::Rect {
                x: 0,
                y: 0,
                width: size.0,
                height: size.1,
            },
        })
        .await?;
        tracing::debug!("continuous updates enabled for {}x{}", size.0, size.1);
        self.continuous_updates = true;
        self.continuous_rect = Some(size);
        // Nothing is outstanding any more: frames arrive unbidden from here.
        self.awaiting_update = false;
        Ok(())
    }

    /// Bring the remote lock keys into line with the local keyboard.
    ///
    /// A remote caps lock that disagrees with the local one turns every keystroke
    /// into the wrong case, and nothing in the key events themselves reveals it --
    /// the server has to say so, which is what the LED-state encoding is for.
    async fn sync_lock_keys(&mut self, local: LockState) -> Result<()> {
        // Decide first, send second: holding a borrow on the state while awaiting a
        // send would mean borrowing self twice.
        let mut corrections = Vec::new();
        if let (Some(local_on), Some(remote_on)) = (local.caps, self.remote_caps_lock)
            && local_on != remote_on
        {
            // Forget the remembered state before sending: the correction takes a round
            // trip to come back, and acting on the stale value would toggle it again.
            self.remote_caps_lock = None;
            corrections.push(("caps lock", crate::term::keysym::CAPS_LOCK));
        }
        if let (Some(local_on), Some(remote_on)) = (local.num, self.remote_num_lock)
            && local_on != remote_on
        {
            self.remote_num_lock = None;
            corrections.push(("num lock", crate::term::keysym::NUM_LOCK));
        }

        for (name, keysym) in corrections {
            tracing::debug!("correcting remote {name}");
            for down in [true, false] {
                self.send(X11Event::KeyEvent(crate::rfb::ClientKeyEvent {
                    keycode: keysym,
                    down,
                }))
                .await?;
            }
        }
        Ok(())
    }

    /// Measure the round trip with a fence, when there are no requests to measure.
    ///
    /// Frames pushed by the server carry no timing information: they answer nothing.
    /// A fence does -- the server bounces it straight back, which is the only honest
    /// latency figure available once requests stop.
    async fn probe_round_trip(&mut self) -> Result<()> {
        if !self.continuous_updates || !self.fence_supported {
            return Ok(());
        }
        // A probe that never came back was dropped; stop waiting on it.
        if let Some(sent) = self.rtt_probe_at
            && sent.elapsed() > RTT_PROBE_TIMEOUT
        {
            self.rtt_probe_at = None;
            self.rtt = None;
        }
        if self.rtt_probe_at.is_some() {
            return Ok(());
        }
        if let Some(last) = self.rtt_measured_at
            && last.elapsed() < RTT_PROBE_INTERVAL
        {
            return Ok(());
        }

        self.rtt_probe_at = Some(Instant::now());
        // No block flags: the server can answer immediately, which is the point.
        self.send(X11Event::Fence {
            flags: crate::rfb::fence::REQUEST,
            payload: RTT_PROBE_MARKER.to_vec(),
        })
        .await
    }

    /// Ask for the next incremental update, keeping one in flight at a time.
    ///
    /// A floor on the interval guards against a server that answers an incremental
    /// request immediately with nothing: the protocol says it should hold the
    /// request until something changes, and a server that does not would otherwise
    /// have us both spinning at full tilt.
    async fn request_update(&mut self) -> Result<()> {
        // The server is pushing frames; asking as well would undo the point of it.
        if self.continuous_updates {
            return Ok(());
        }
        if self.awaiting_update {
            return Ok(());
        }
        if self.requested_at.elapsed() < MIN_REQUEST_INTERVAL {
            // Let the render tick pick it up shortly.
            return Ok(());
        }
        self.awaiting_update = true;
        self.requested_at = Instant::now();
        self.send(X11Event::Refresh).await
    }

    fn draw(&mut self) -> Result<()> {
        // A note that has run out has to come off the screen, and the screen it was
        // over has to be drawn back: work of its own, whether or not a frame arrived.
        if self.toast.expire() {
            self.renderer.mark_all();
        }
        let has_work = self.renderer.has_work();
        if !has_work && !self.toast.is_live() && !self.show_menu && !self.clear_menu {
            // Still repaint the status line often enough for the clock-like
            // fields to stay honest, but not every tick.
            if self.fps.since_last() < Duration::from_millis(500) {
                return Ok(());
            }
        }

        let mut buf = self.writer.take_buffer();
        if self.caps.sync_output {
            kitty::begin_sync(&mut buf);
        }
        // Text first, then images, exactly as a relayout does it: erasing cells may
        // take the placements under them with it, so the tiles have to go out after.
        if self.clear_menu {
            self.menu.clear(&mut buf, &self.metrics);
            self.clear_menu = false;
        }
        self.toast.clear(&mut buf);
        let stats = self.renderer.compose(&self.fb, &mut buf);
        if stats.tiles > 0 {
            self.last_stats = stats;
        }
        self.draw_status(&mut buf);
        if self.show_menu {
            self.menu.draw(&mut buf, &self.metrics, self.menu_state());
        }
        // Last of the chrome, so a note that arrives with the menu open lands on top
        // of it rather than under it.
        self.toast
            .draw(&mut buf, &self.metrics, self.theme.palette());
        if self.caps.sync_output {
            kitty::end_sync(&mut buf);
        }

        match self.writer.submit(buf) {
            Ok(()) => {
                self.renderer.commit();
                self.toast.commit();
                self.fps.tick();
            }
            Err(Busy::Full(buf)) => {
                self.dropped += 1;
                self.writer.recycle(buf);
            }
            Err(Busy::Closed) => anyhow::bail!("the terminal stopped accepting output"),
        }
        Ok(())
    }

    fn draw_status(&mut self, buf: &mut Vec<u8>) {
        let layout = *self.renderer.layout();
        // What this is connected to, which is the one thing on the left worth reading
        // without looking for it.
        let name = if self.server_name.is_empty() {
            " desktui".to_string()
        } else {
            format!(" {}", self.server_name)
        };

        let mut rest = format!("  {}x{}", self.remote.0, self.remote.1);
        if !layout.is_pixel_exact() {
            rest.push_str(&format!(" -> {}x{}", layout.dst_w, layout.dst_h));
        }
        rest.push_str(&format!("  {}", describe(&layout)));
        if self.view_only {
            rest.push_str("  view-only");
        }

        // The one binding worth naming, because it opens the menu the rest of them are
        // listed in. It keeps the ink when the statistics crowd the words out.
        // Lower case, as the menu writes its shortcuts: the two should read alike.
        let key = format!("ctrl+{} p", self.input.prefix());
        let (figures, label) = if self.show_stats {
            (
                format!(
                    "{:>5.1} fps  {:>3} tiles  {:>6}/f  {:>6} rtt  {} dropped  ",
                    self.fps.fps(),
                    self.last_stats.tiles,
                    human_bytes(self.last_stats.bytes),
                    format_rtt(self.rtt),
                    self.dropped,
                ),
                " ".to_string(),
            )
        } else {
            (
                format!("{:>6}  ", format_rtt(self.rtt)),
                " commands ".into(),
            )
        };

        // Lit while the prefix waits on its key. The dot is what catches the eye and
        // the word is what it means: the next key is a command, not a keystroke. In the
        // colour the menu picks things out with, so the light and the box it is about
        // read as the same idea.
        let ink = self.theme.palette();
        let mut left = vec![ink.bright(&name), ink.text(&rest)];
        if self.input.is_armed() {
            left.push(ink.accent("  ● CMD"));
        }

        status::draw(
            buf,
            &self.metrics,
            ink,
            left,
            vec![ink.text(&figures), ink.bright(&key), ink.text(&label)],
        );
    }

    fn relayout(&mut self) {
        let layout = Layout::compute(
            &self.metrics,
            self.mode,
            u32::from(self.remote.0),
            u32::from(self.remote.1),
            self.pan,
        );
        let cleanup = self.renderer.relayout(layout);
        if !cleanup.is_empty() {
            let _ = self.writer.submit_blocking(cleanup);
        }
    }

    async fn send(&self, event: X11Event) -> Result<()> {
        self.client
            .input(event)
            .await
            .map_err(|err| anyhow::anyhow!("failed to send input: {err}"))
    }

    /// Let go of every key and button we told the server about.
    async fn release_input(&mut self) {
        for key in self.input.release_all() {
            let _ = self.client.input(X11Event::KeyEvent(key)).await;
        }
        if let Some(pointer) = self.input.release_buttons() {
            let _ = self.client.input(X11Event::PointerEvent(pointer)).await;
        }
    }

    /// Put locally pasted text on the remote clipboard.
    ///
    /// Over the extension the text goes as UTF-8 and arrives whole. Without it the
    /// payload is Latin-1 and everything outside it has to be substituted, which is
    /// why a server that negotiated the extension is worth the extra round trip.
    async fn paste_to_remote(&mut self, text: String) -> Result<()> {
        if let Some(caps) = self
            .clipboard_caps
            .filter(|caps: &ClipboardCaps| caps.takes_text() && caps.takes_provide())
        {
            // The size the server's limit applies to is the text as it goes on the
            // wire: CRLF line endings, and the terminating null that the length counts.
            let wire_len = text.len() + text.matches('\n').count() + 1;
            if wire_len as u64 <= u64::from(caps.unsolicited_text()) || !caps.takes_notify() {
                // Either small enough to push unasked, or a server that offered no way
                // to announce it, which leaves pushing it the only thing left to try.
                self.send(X11Event::ClipboardProvide(text)).await?;
            } else {
                // Announce it and keep it. The server asks when something over there
                // pastes, which is the point: a clipboard nobody reads costs one small
                // message instead of the whole text.
                self.send(X11Event::ClipboardNotify).await?;
                self.announced_clipboard = Some(text);
            }
            self.set_note("pasted to the remote clipboard".into());
            return Ok(());
        }

        // Legacy `ClientCutText` is Latin-1 only. Substitute rather than drop:
        // deleting characters silently shortens the text and moves everything after
        // them, where a question mark leaves the shape intact and is visibly a
        // substitution. noVNC does the same.
        let dropped = text.chars().filter(|c| (*c as u32) > 0xff).count();
        let latin1: String = text
            .chars()
            .map(|c| if (c as u32) > 0xff { '?' } else { c })
            .collect();
        self.send(X11Event::CopyText(latin1)).await?;
        self.set_note(if dropped > 0 {
            format!("pasted; {dropped} character(s) are not Latin-1 and became '?'")
        } else {
            "pasted to the remote clipboard".into()
        });
        Ok(())
    }

    /// Put the server's clipboard on the local one, with OSC 52.
    fn copy_to_local_clipboard(&mut self, text: &str) {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        let mut buf = self.writer.take_buffer();
        buf.extend_from_slice(b"\x1b]52;c;");
        buf.extend_from_slice(encoded.as_bytes());
        buf.extend_from_slice(b"\x07");
        let _ = self.writer.submit_blocking(buf);
        self.set_note("remote clipboard copied".into());
    }

    /// Put a note up in the notification popup.
    fn set_note(&mut self, note: String) {
        tracing::debug!("{note}");
        // A note landing on top of one already up takes the old box off the screen with
        // it, and the remote screen under the cells it no longer covers has to come
        // back -- the same repair the menu asks for when it is dismissed.
        if self.toast.show(note) {
            self.renderer.mark_all();
        }
    }
}

/// The round trip for the status line, or dashes while it is unknown.
///
/// Dashes rather than a zero or a stale figure: an unmeasured latency is not a fast
/// one, and the old code showed the age of the last request it happened to send, which
/// only ever grew.
fn format_rtt(rtt: Option<Duration>) -> String {
    match rtt {
        Some(rtt) => format!("{}ms", rtt.as_millis()),
        None => "--".to_string(),
    }
}

fn convert(rect: crate::rfb::Rect) -> Rect {
    Rect::new(
        u32::from(rect.x),
        u32::from(rect.y),
        u32::from(rect.width),
        u32::from(rect.height),
    )
}

/// Decode a Tight JPEG rectangle to packed RGB.
///
/// The output colour space is pinned to RGB so a greyscale JPEG comes back with
/// three channels like everything else, rather than one.
fn decode_jpeg(data: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    // The decoder wants a seekable reader, which a bare slice is not.
    let mut decoder = JpegDecoder::new_with_options(std::io::Cursor::new(data), options);
    let pixels = decoder
        .decode()
        .map_err(|err| anyhow::anyhow!("{err:?}"))
        .context("JPEG decode failed")?;
    let info = decoder.info().context("JPEG carried no dimensions")?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    anyhow::ensure!(
        pixels.len() >= (w as usize) * (h as usize) * 3,
        "JPEG decoded to {} bytes, too few for {w}x{h} RGB",
        pixels.len()
    );
    Ok((w, h, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unsupported_or_refused_resize_explains_itself() {
        assert!(
            Resize::Unsupported
                .note()
                .unwrap()
                .contains("cannot resize")
        );
        let refused = Resize::Refused(ResizeStatus::Prohibited);
        assert!(refused.note().unwrap().contains("prohibited"));
        // States that are not a dead end say nothing.
        assert!(Resize::Probing.note().is_none());
        assert!(Resize::Native.note().is_none());
        assert!(Resize::Waiting { want: (1, 1) }.note().is_none());
    }

    #[test]
    fn an_unmeasured_round_trip_shows_dashes_rather_than_a_number() {
        // The bug this replaces: with frames arriving unbidden there was no request to
        // measure against, so the display showed the age of the last request ever sent
        // and climbed for ever.
        assert_eq!(format_rtt(None), "--");
        assert_eq!(format_rtt(Some(Duration::from_millis(0))), "0ms");
        assert_eq!(format_rtt(Some(Duration::from_millis(23))), "23ms");
        assert_eq!(format_rtt(Some(Duration::from_millis(1500))), "1500ms");
    }

    #[test]
    fn an_implausible_framebuffer_size_is_refused() {
        // The protocol's dimensions are 16-bit, so a server can ask for seventeen
        // gigabytes of allocation. Rust aborts on a failed allocation, so this has
        // to be caught before the allocator sees it.
        assert!(framebuffer_size_is_plausible((1600, 832)));
        assert!(framebuffer_size_is_plausible((3840, 2160)));
        assert!(framebuffer_size_is_plausible((8192, 8192)));
        assert!(!framebuffer_size_is_plausible((65535, 65535)));
        assert!(!framebuffer_size_is_plausible((65535, 4096)));
    }

    #[test]
    fn a_rectangle_widens_from_the_protocol_type() {
        let rect = convert(crate::rfb::Rect {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        });
        assert_eq!(rect, Rect::new(10, 20, 30, 40));
    }

    #[test]
    fn garbage_is_not_mistaken_for_a_jpeg() {
        assert!(decode_jpeg(&[]).is_err());
        assert!(decode_jpeg(b"not a jpeg at all").is_err());
    }

    /// An 8x8 single-component baseline JPEG.
    ///
    /// Greyscale on purpose: it is the case that would come back one byte per
    /// pixel if the output colour space were left to the decoder's judgement, and
    /// the framebuffer expects three.
    const GREY_8X8: &[u8] = include_bytes!("../tests/fixtures/grey8x8.jpg");

    #[test]
    fn a_greyscale_jpeg_still_decodes_to_three_channels() {
        let (w, h, rgb) = decode_jpeg(GREY_8X8).expect("should decode");
        assert_eq!((w, h), (8, 8));
        assert_eq!(
            rgb.len(),
            8 * 8 * 3,
            "expected RGB for every pixel, got {} bytes",
            rgb.len()
        );
        // Every channel of a pixel holds the same value in a greyscale image.
        assert_eq!(rgb[0], rgb[1]);
        assert_eq!(rgb[1], rgb[2]);
    }

    #[test]
    fn a_truncated_jpeg_is_an_error_not_a_panic() {
        // A rectangle cut short by a dropped connection must not take the session
        // with it.
        for cut in [4, 20, GREY_8X8.len() / 2, GREY_8X8.len() - 1] {
            let _ = decode_jpeg(&GREY_8X8[..cut]);
        }
    }
}
