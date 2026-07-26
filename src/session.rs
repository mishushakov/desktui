//! A live remote desktop session.
//!
//! One loop selects over three sources: updates from the remote, input from the
//! terminal, and a render tick. Everything on screen is composed in the tick and
//! handed to the writer thread as a single frame.
//!
//! Nothing here names a protocol. What arrives comes through [`crate::remote`], and
//! the two capabilities this loop changes shape around -- whether frames arrive
//! unasked, and whether a round trip can be measured -- are announced by the backend
//! rather than assumed.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use futures::StreamExt;
use tokio::time::{MissedTickBehavior, interval};

use crate::app::{FpsMeter, describe, human_bytes};
use crate::cli::{Args, ScaleMode};
use crate::remote::{
    Backend, Connect, ConnectError, Input, Key, ResizeStatus, Screen, ScreenInfo, ScreenLayout,
    Update,
};
use crate::render::framebuffer::Framebuffer;
use crate::render::{FrameStats, Layout, Rect, Renderer};
use crate::term::caps::Caps;
use crate::term::input::{Command, InputMapper, KeyOutcome, LockState};
use crate::term::writer::{Busy, FrameWriter};
use crate::term::{Metrics, TerminalGuard, kitty};
use crate::ui::chrome::Chrome;
use crate::ui::menu::{self, Hit, Menu};
use crate::ui::status;
use crate::ui::theme::Theme;
use crate::ui::toast::Toast;

/// A server that has gone quiet for this long gets its update request repeated.
/// Some servers drop one under load, and without this the session would simply
/// stop redrawing.
const UPDATE_WATCHDOG: Duration = Duration::from_secs(1);

/// Shortest gap between two resizes being acted on.
///
/// Terminal resizes arrive in a stream while a window is dragged, and each one adopted
/// is a relayout and a request to the server. A rate limit rather than a quiet period:
/// the first resize after a lull is acted on at once and the stream behind it is thinned
/// to this, so a drag is answered as it happens instead of after it.
///
/// The same hundred milliseconds TigerVNC and noVNC settled on -- and TigerVNC
/// deliberately moved *away* from a 500ms idle period, because waiting for a drag to
/// finish makes maximising or going full screen feel like it did not take.
const RESIZE_INTERVAL: Duration = Duration::from_millis(100);

/// Floor on the gap between two update requests, so a server that answers an
/// incremental request instantly cannot spin us at full speed.
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(2);

/// How long a frame will wait for the update it would be drawing to finish arriving,
/// measured from the arrival of that update's first rectangle.
///
/// Past this it draws anyway. An update whose rectangles take longer than this to
/// arrive cannot be drawn whole *and* often, and a screen that stands still is worse
/// than one that shows a seam: the choice only comes up when the link or the server is
/// already too slow to keep up.
const MAX_PARTIAL_WAIT: Duration = Duration::from_millis(250);

/// Overrides [`MAX_PARTIAL_WAIT`], in milliseconds. A seam for the tests, not an option:
/// a test that means to prove a frame *waits* has to be able to take the deadline off the
/// table, because nothing on the wire distinguishes a seam the client was entitled to draw
/// from the bug of drawing one it was not -- and a shared machine under load will cross any
/// wall-clock deadline eventually. Read once, at the start of a session.
const PARTIAL_WAIT_ENV: &str = "DESKTUI_MAX_PARTIAL_WAIT_MS";

fn max_partial_wait() -> Duration {
    std::env::var(PARTIAL_WAIT_ENV)
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map_or(MAX_PARTIAL_WAIT, Duration::from_millis)
}

/// How often to measure the round trip, once frames stop being requested.
const RTT_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// A probe this old was dropped by the server; stop waiting for it.
const RTT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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

pub async fn run<C: Connect>(
    args: &Args,
    caps: &Caps,
    guard: &TerminalGuard,
    remote: &mut C,
) -> Result<()> {
    // The first connection is made before the alternate screen, so that a
    // password prompt is somewhere the user can see it. Later attempts reuse the
    // password and need no prompt.
    let backend = connect(remote).await?;
    guard.begin_full_screen()?;

    let addr = remote.address().to_string();
    let mut next = Some(backend);
    let mut backoff = RECONNECT_BACKOFF;

    loop {
        let backend = match next.take() {
            Some(backend) => {
                backoff = RECONNECT_BACKOFF;
                backend
            }
            None => match remote.connect().await {
                Ok(backend) => {
                    backoff = RECONNECT_BACKOFF;
                    backend
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

        let mut session = Session::new(args, caps, backend).await?;
        let result = session.run().await;
        // Let go of anything held on the remote before leaving, so a modifier does
        // not stay stuck in whatever had focus over there.
        session.release_input().await;
        session.backend.close().await;
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

/// Open the connection, prompting for a password only if the remote asks for one
/// and none was supplied.
///
/// The password is handed back to the connector rather than kept here, so that a
/// reconnect an hour later needs no prompt.
async fn connect<C: Connect>(remote: &mut C) -> Result<C::Backend> {
    match remote.connect().await {
        Ok(backend) => Ok(backend),
        Err(ConnectError::NeedsPassword) => {
            let password = crate::prompt_password(remote.address())?;
            remote.use_password(password);
            remote
                .connect()
                .await
                .map_err(|err| anyhow::anyhow!("{err}"))
        }
        Err(ConnectError::Failed(err)) => Err(err),
    }
}

struct Session<B: Backend> {
    backend: B,
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
    /// The backend has said it can measure a round trip.
    can_probe_latency: bool,
    /// When the outstanding latency probe went out.
    rtt_probe_at: Option<Instant>,
    /// When the last measurement completed, to space the probes out.
    rtt_measured_at: Option<Instant>,
    /// Frames arrive without being asked for, so no requests are sent. How the
    /// backend arranged that is its own business.
    pushed_frames: bool,
    /// The remote lock-key state, when the server tells us. `None` means unknown,
    /// which is also what it becomes right after a correction is sent: the answer
    /// takes a moment to arrive and acting twice would toggle it back.
    remote_caps_lock: Option<bool>,
    remote_num_lock: Option<bool>,

    view_only: bool,
    no_clipboard: bool,
    /// Text put on the remote clipboard will not survive intact, so anything outside
    /// Latin-1 has to be warned about. True until a backend says otherwise: the
    /// answer takes a negotiation, and the pessimistic assumption is the safe one to
    /// start on.
    clipboard_lossy: bool,
    /// Which palette the chrome wears. Dark to start, the bar having been that colour
    /// before there was a choice.
    theme: Theme,
    /// The cells the client owns, diffed frame to frame so taking chrome off the screen is
    /// the same operation as putting it on.
    chrome: Chrome,
    /// The command menu, and where the pointer is on it.
    menu: Menu,
    show_menu: bool,
    /// The wipe a relayout asks for, waiting for the frame that fills the screen
    /// back in. Written on its own it is a blank screen that lasts until the next
    /// frame composes, which is what a resize looked like; carried into that frame's
    /// synchronised block, the old picture stands until the new one replaces it.
    pending_cleanup: Vec<u8>,
    show_stats: bool,
    /// The notification popup in the top-right corner, and the note it is showing.
    toast: Toast,

    fps: FpsMeter,
    /// How long a frame is allowed, from `--fps`. What the render tick is paced by, and
    /// what says whether a frame is owed when an update finishes arriving.
    frame_time: Duration,
    /// Where the pointer is, in terminal pixels, so it can be put back after a resize.
    pointer_px: Option<(u32, u32)>,
    /// A rectangle of the update being received has been applied and the update is not
    /// over. Drawing now would put half of one picture on the screen and half of
    /// another -- which on a page being scrolled is half of it at each scroll position.
    mid_update: bool,
    /// How long that wait is allowed to last: [`MAX_PARTIAL_WAIT`], or whatever
    /// [`PARTIAL_WAIT_ENV`] said when the session started.
    max_partial_wait: Duration,
    /// When the update being received started arriving, and how much of the picture it
    /// has carried so far.
    ///
    /// The other end's half of the statistics. Everything else in the bar is this side --
    /// our frame rate, our tiles, our bytes -- so a session that crawls because the
    /// server is re-encoding a whole screen looks exactly like one that crawls because we
    /// are. These say which: an update rate well under the frame rate is a server that
    /// cannot keep up, whatever this end manages.
    update_started: Option<Instant>,
    update_pixels: u64,
    /// Updates that carried picture, as a rate, and what the last one cost.
    updates: FpsMeter,
    delivery: Option<Duration>,
    last_update_pixels: u64,
    last_stats: FrameStats,
    dropped: u64,
    pending_metrics: Option<Instant>,
    /// When a resize was last acted on, which is what [`RESIZE_INTERVAL`] is measured
    /// from: the first after a lull goes through at once, and a stream is thinned.
    metrics_applied_at: Option<Instant>,
    quit: bool,
}

impl<B: Backend> Session<B> {
    async fn new(args: &Args, caps: &Caps, backend: B) -> Result<Self> {
        let metrics = Metrics::query()?;
        let remote = backend.resolution().await;
        let server_name = backend.name().await;

        let fb = Framebuffer::new(u32::from(remote.0), u32::from(remote.1));
        let layout = Layout::compute(
            &metrics,
            args.scale,
            u32::from(remote.0),
            u32::from(remote.1),
            (0, 0),
        );

        Ok(Self {
            backend,
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
            can_probe_latency: false,
            rtt_probe_at: None,
            rtt_measured_at: None,
            pushed_frames: false,
            remote_caps_lock: None,
            remote_num_lock: None,
            view_only: args.view_only,
            no_clipboard: args.no_clipboard,
            clipboard_lossy: true,
            theme: Theme::Dark,
            chrome: Chrome::new(),
            menu: Menu::new(args.prefix_char()),
            show_menu: false,
            pending_cleanup: Vec::new(),
            show_stats: false,
            toast: Toast::default(),
            fps: FpsMeter::new(),
            frame_time: Duration::from_micros(1_000_000 / u64::from(args.fps)),
            mid_update: false,
            max_partial_wait: max_partial_wait(),
            pointer_px: None,
            update_started: None,
            update_pixels: 0,
            updates: FpsMeter::new(),
            delivery: None,
            last_update_pixels: 0,
            last_stats: FrameStats::default(),
            dropped: 0,
            pending_metrics: None,
            metrics_applied_at: None,
            quit: false,
        })
    }

    async fn run(&mut self) -> Result<()> {
        let mut terminal = EventStream::new();
        let mut ticker = interval(self.frame_time);
        // Falling behind should drop frames, not queue them up to be raced
        // through later.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        while !self.quit {
            tokio::select! {
                update = self.backend.recv() => match update {
                    Ok(update) => self.on_update(update).await?,
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

    async fn on_update(&mut self, update: Update) -> Result<()> {
        // Anything that changes the picture opens an update, and `FrameEnd` closes it. The
        // rectangles between the two are one picture and belong on the screen together.
        //
        // An update of nothing but pseudo-rectangles -- a cursor shape, a layout, the lock
        // keys -- is not a picture and does not open one, which is also what keeps it out
        // of the delivery figures.
        if matches!(
            update,
            Update::Bgra(..) | Update::Jpeg(..) | Update::Copy { .. }
        ) {
            self.mid_update = true;
            self.update_started.get_or_insert_with(Instant::now);
            self.update_pixels += match &update {
                Update::Bgra(rect, _) | Update::Jpeg(rect, _) => {
                    u64::from(rect.width) * u64::from(rect.height)
                }
                Update::Copy { dst, .. } => u64::from(dst.width) * u64::from(dst.height),
                _ => 0,
            };
        }
        match update {
            Update::Bgra(rect, data) => {
                if let Some(damage) = self.fb.apply_bgra(convert(rect), &data) {
                    self.renderer.mark(damage);
                }
            }
            Update::Jpeg(rect, data) => match decode_jpeg(&data) {
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
            Update::Copy { dst, from } => {
                if let Some(damage) =
                    self.fb
                        .copy_rect(convert(dst), u32::from(from.0), u32::from(from.1))
                {
                    self.renderer.mark(damage);
                }
            }
            Update::Resolution(screen) => self.on_remote_size(screen, None).await?,
            Update::Layout(layout) => self.on_layout(layout).await?,
            Update::FrameEnd => {
                // Only meaningful when this frame answers a request of ours. With
                // frames arriving unbidden there is no pair, and the latency probe
                // measures it instead.
                if self.awaiting_update && !self.pushed_frames {
                    self.rtt = Some(self.requested_at.elapsed());
                }
                self.awaiting_update = false;
                // The first frame is also the answer to our capability probe: a
                // remote that supports resizing must have reported a layout with it.
                if self.resize == Resize::Probing {
                    self.resize = Resize::Unsupported;
                    if let Some(note) = self.resize.note() {
                        self.set_note(note);
                    }
                    self.fall_back_from_native();
                }
                // Ask for the next frame now, not on the next render tick. The
                // remote can then encode the following one while we are still
                // drawing this one; waiting for the tick leaves it idle for up to a
                // whole frame interval, which roughly halves the rate a moving
                // picture can reach.
                self.request_update().await?;

                // And now the picture is whole, so draw it -- rather than leaving it to a
                // tick, which would add latency for nothing. The render tick's job is to
                // pace this, not to choose the moment: a frame is owed only if one has not
                // gone out inside the last frame interval.
                self.mid_update = false;
                // What the other end just took to deliver one picture, before drawing it:
                // the delivery is the server's, the frame that follows is ours, and telling
                // them apart is the whole point of measuring here.
                if let Some(at) = self.update_started.take() {
                    self.delivery = Some(at.elapsed());
                    self.last_update_pixels = std::mem::take(&mut self.update_pixels);
                    self.updates.tick();
                }
                if self.fps.since_last() >= self.frame_time {
                    self.draw()?;
                }
            }
            Update::Clipboard(text) => {
                if !self.no_clipboard {
                    self.copy_to_local_clipboard(&text);
                }
            }
            Update::ClipboardLossy(lossy) => {
                // Whether text put on the remote clipboard survives intact. A session
                // assumes it does not until told otherwise, because the answer takes a
                // negotiation and the pessimistic path is the safe one to start on.
                tracing::debug!(
                    "remote clipboard is {}",
                    if lossy { "Latin-1" } else { "UTF-8" }
                );
                self.clipboard_lossy = lossy;
            }
            Update::Bell => {
                // A bell is the one thing worth passing straight through.
                let mut buf = self.writer.take_buffer();
                buf.push(0x07);
                let _ = self.writer.submit_blocking(buf);
            }
            Update::LockKeys { num, caps } => {
                // Only useful as something to compare the local keyboard against, so
                // scroll lock never crosses the seam: no terminal reports it.
                tracing::debug!("remote lock keys: caps={caps} num={num}");
                self.remote_caps_lock = Some(caps);
                self.remote_num_lock = Some(num);
            }
            Update::Pushing(pushing) => {
                if pushing && !self.pushed_frames {
                    tracing::info!("frames now arrive without being asked for");
                    self.set_note("the remote is pushing frames".into());
                }
                self.pushed_frames = pushing;
                // Nothing is outstanding any more: frames arrive unbidden from here.
                if pushing {
                    self.awaiting_update = false;
                }
            }
            Update::LatencyAvailable => {
                tracing::debug!("the remote can measure a round trip");
                self.can_probe_latency = true;
            }
            Update::LatencyProbe => {
                // Our own probe, back again: the gap is the round trip.
                if let Some(sent) = self.rtt_probe_at.take() {
                    self.rtt = Some(sent.elapsed());
                    self.rtt_measured_at = Some(Instant::now());
                }
            }
            Update::Cursor {
                size,
                hotspot,
                bgra,
            } => {
                let cursor = crate::render::Cursor {
                    w: u32::from(size.0),
                    h: u32::from(size.1),
                    hot_x: u32::from(hotspot.0),
                    hot_y: u32::from(hotspot.1),
                    pixels: bgra,
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
            Update::Error(err) => anyhow::bail!("the remote connection failed: {err}"),
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
        self.backend
            .send(Input::Resize {
                width: want.0,
                height: want.1,
                screens,
            })
            .await?;
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
                                    self.send(Input::Key(key)).await?;
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
                                self.send(Input::Key(key)).await?;
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
                // Kept in terminal pixels, not the destination pixels the renderer wants:
                // a resize changes what a destination pixel means, and the pointer has to
                // be re-derived from where it actually is rather than left where it was.
                self.pointer_px = Some((tx, ty));
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
                    self.toast.dismiss();
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
                        self.send(Input::Pointer(event)).await?;
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
                        self.send(Input::Key(key)).await?;
                    }
                }
            }
            Command::FullRefresh => {
                self.renderer.mark_all();
                self.send(Input::Refresh { incremental: false }).await?;
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

    /// Hide the menu.
    ///
    /// Nothing else to do: a menu absent from the next frame's plane is a menu whose cells
    /// the diff blanks and whose images the chrome drops, and the cells it gives up come
    /// back as damage so the picture under them is drawn again.
    fn dismiss_menu(&mut self) {
        self.show_menu = false;
        // Or the row the pointer happened to be on would be lit the next time the
        // menu opens, before the pointer has moved to say so.
        self.menu.clear_hover();
    }

    async fn on_tick(&mut self) -> Result<()> {
        // A resize is waiting: adopt the new geometry, and ask the server to match it if
        // it is willing to.
        //
        // Rate limited, not deferred. The first resize after a lull goes through at once
        // and a continuing stream is thinned to one every `RESIZE_INTERVAL`, so a drag is
        // answered while it happens. The second arm is the tail of a stream that stopped
        // before the interval was up, which nothing else would come back for.
        let due = self
            .metrics_applied_at
            .is_none_or(|last| last.elapsed() >= RESIZE_INTERVAL);
        if let Some(at) = self.pending_metrics
            && (due || at.elapsed() >= RESIZE_INTERVAL)
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

        // With frames arriving unbidden there is no request to time, so ask for a
        // round trip and measure that instead.
        self.probe_round_trip().await?;

        // A pointer that stopped moving mid-rate-limit still has to arrive.
        if !self.view_only
            && let Some(pointer) = self.input.flush_motion()
        {
            self.send(Input::Pointer(pointer)).await?;
        }

        // Normally the request went out the moment the last update finished, so
        // this only covers the gaps: the very first frame, and a server that has
        // gone quiet.
        if !self.awaiting_update {
            self.request_update().await?;
        } else if !self.pushed_frames && self.requested_at.elapsed() > UPDATE_WATCHDOG {
            tracing::debug!(
                "no update for {:?}; asking again",
                self.requested_at.elapsed()
            );
            self.requested_at = Instant::now();
            self.send(Input::Refresh { incremental: true }).await?;
        }

        // A frame drawn in the middle of an update is half of one picture and half of
        // another. The rectangles still arriving are the rest of this one, and the end of
        // it draws -- so there is nothing to do here but wait, up to the point where
        // waiting is worse than the seam.
        //
        // Measured from when this update started arriving, not from the last frame: the
        // budget is for *this update's* rectangles to turn up in, and time spent idle
        // before it began is not the server being slow with it. Timing it from the last
        // frame charged the update for the gap ahead of it, so a machine that had been
        // busy elsewhere drew the seam on an update that arrived perfectly promptly.
        let waited = self
            .update_started
            .map_or(Duration::ZERO, |at| at.elapsed());
        if self.mid_update && waited < self.max_partial_wait {
            return Ok(());
        }
        self.draw()
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
                self.send(Input::Key(Key { keysym, down })).await?;
            }
        }
        Ok(())
    }

    /// Measure the round trip, when there are no requests to measure.
    ///
    /// Frames that arrive unbidden carry no timing information: they answer nothing.
    /// A probe does -- the remote sends it straight back -- and is the only honest
    /// latency figure available once requests stop. A backend that cannot measure one
    /// leaves the figure unknown, which the status line shows as dashes rather than
    /// inventing a number.
    async fn probe_round_trip(&mut self) -> Result<()> {
        if !self.pushed_frames || !self.can_probe_latency {
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
        self.send(Input::ProbeLatency).await
    }

    /// Ask for the next incremental update, keeping one in flight at a time.
    ///
    /// A floor on the interval guards against a server that answers an incremental
    /// request immediately with nothing: the protocol says it should hold the
    /// request until something changes, and a server that does not would otherwise
    /// have us both spinning at full tilt.
    async fn request_update(&mut self) -> Result<()> {
        // Frames are being pushed; asking as well would undo the point of it.
        if self.pushed_frames {
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
        self.send(Input::Refresh { incremental: true }).await
    }

    fn draw(&mut self) -> Result<()> {
        // A note that has run out has to come off the screen: work of its own, whether or
        // not a frame arrived.
        let expired = self.toast.expire();
        if !self.renderer.has_work() && !expired && self.pending_cleanup.is_empty() {
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
        // A relayout's wipe, if one is owed: erase and delete inside the same
        // synchronised block that puts the screen back, so the terminal only ever shows
        // one of the two layouts and never the gap between them.
        let cleanup = std::mem::take(&mut self.pending_cleanup);
        buf.extend_from_slice(&cleanup);

        // The chrome, all of it, before the tiles. Its own cells are diffed against what
        // is on screen, so a menu that closed or a bar whose row moved is blanked by being
        // absent rather than by anyone remembering to erase it -- and the cells it gives up
        // come back as damage, which is what the tiles under them need. Before the tiles
        // because a terminal may treat clearing a cell as dropping the placement under it;
        // the text still lands above them, z-index deciding that rather than write order.
        let resized = self.chrome.begin(&self.metrics);
        if resized {
            // The geometry changed, so what is on screen is a guess and the guess has been
            // wrong: erase the text and say all of it again. Inside the frame that redraws,
            // so nothing is ever seen blank -- which is the whole difference between this
            // and the wipe that used to go out on its own.
            buf.extend_from_slice(b"\x1b[2J");
        }
        self.render_chrome(&mut buf);
        for cells in self.chrome.flush(&mut buf) {
            self.renderer
                .mark_cells(cells.x, cells.y, cells.width, cells.height);
        }
        if resized {
            // And put the picture back on its cells, in case the erase took the placements
            // with it. Pixels, not placements, are what a resize is expensive in.
            self.renderer.replace_all(&mut buf);
        }

        let stats = self.renderer.compose(&self.fb, &mut buf);
        if stats.tiles > 0 {
            self.last_stats = stats;
        }
        if self.caps.sync_output {
            kitty::end_sync(&mut buf);
        }

        match self.writer.submit(buf) {
            Ok(()) => {
                self.renderer.commit();
                self.chrome.commit();
                self.fps.tick();
            }
            Err(Busy::Full(buf)) => {
                self.dropped += 1;
                self.writer.recycle(buf);
                // The wipe went out with the frame or not at all: dropping it here would
                // leave the old layout's chrome and tiles on screen for good, since the
                // damage that would have painted over them was never composed.
                if !cleanup.is_empty() {
                    self.pending_cleanup = cleanup;
                }
            }
            Err(Busy::Closed) => anyhow::bail!("the terminal stopped accepting output"),
        }
        Ok(())
    }

    /// Render every piece of chrome into the plane, and place the images they sit on.
    ///
    /// In the order they stack: the bar, then the menu over the picture, then a note over
    /// the menu -- a note that arrives while the menu is open belongs on top of it.
    fn render_chrome(&mut self, out: &mut Vec<u8>) {
        self.render_status();
        if self.show_menu {
            let state = self.menu_state();
            self.menu
                .render(&mut self.chrome, out, &self.metrics, state);
        }
        self.toast
            .render(&mut self.chrome, out, &self.metrics, self.theme.palette());
    }

    fn render_status(&mut self) {
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
            // Ours, then theirs. An update rate far below the frame rate is a server that
            // cannot keep up, and no amount of work on this side will move it.
            (
                format!(
                    "{:>5.1} fps  {:>3} tiles  {:>6}/f  {:>5.1} up/s  {:>6} /up  \
                     {:>5.2} MP  {:>6} rtt  {} dropped  ",
                    self.fps.fps(),
                    self.last_stats.tiles,
                    human_bytes(self.last_stats.bytes),
                    self.updates.fps(),
                    format_rtt(self.delivery),
                    self.last_update_pixels as f64 / 1e6,
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

        status::render(
            self.chrome.buffer(),
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
        // The pointer is placed against the terminal's grid, which has just changed under
        // it. Its screen position has not, so put it back where it actually is.
        if let Some((tx, ty)) = self.pointer_px {
            let at = self.renderer.layout().terminal_px_to_dst(tx, ty);
            self.renderer.move_cursor(at);
        }
        // Held for the next frame rather than written now, and appended: a relayout
        // names the tiles it has dropped, and a second one before that frame goes out
        // names different ones. Each lowers what the renderer believes is placed, so no
        // id is named twice.
        self.pending_cleanup.extend_from_slice(&cleanup);
    }

    async fn send(&self, input: Input) -> Result<()> {
        self.backend
            .send(input)
            .await
            .context("failed to send input")
    }

    /// Let go of every key and button we told the server about.
    async fn release_input(&mut self) {
        for key in self.input.release_all() {
            let _ = self.backend.send(Input::Key(key)).await;
        }
        if let Some(pointer) = self.input.release_buttons() {
            let _ = self.backend.send(Input::Pointer(pointer)).await;
        }
    }

    /// Put locally pasted text on the remote clipboard.
    ///
    /// How it gets there is the backend's business -- which message, whether the text
    /// goes now or is announced and fetched later. The one thing that belongs here is
    /// telling the user when the remote cannot carry what they pasted, because a
    /// substitution they were not warned about is a substitution they will find later
    /// in whatever they pasted into.
    async fn paste_to_remote(&mut self, text: String) -> Result<()> {
        let dropped = if self.clipboard_lossy {
            text.chars().filter(|c| (*c as u32) > 0xff).count()
        } else {
            0
        };
        self.send(Input::Clipboard(text)).await?;
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
        self.toast.show(note);
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

fn convert(rect: crate::remote::Rect) -> Rect {
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
        let rect = convert(crate::remote::Rect {
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
