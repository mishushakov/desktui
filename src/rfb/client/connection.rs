//! The connected client: one task decoding the server's stream, one writing ours.
//!
//! Rewritten from upstream, which bridged the socket into the decoder through a
//! 4096-deep channel of `Vec<u8>` chunks so the same code could run on wasm.
//! Every byte was copied twice on the way in, which on multi-megabyte frames is
//! the difference between comfortable and not. Here the reader decodes straight
//! from a buffered read half.

use std::future::Future;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio::task::JoinHandle;
use tracing::*;

use super::clipboard;
use super::messages::{ClientMsg, ServerMsg};
use crate::rfb::{
    PixelFormat, Rect, ResizeStatus, ScreenInfo, ScreenLayout, VncEncoding, VncError, VncEvent,
    X11Event, codec,
};

const CHANNEL_SIZE: usize = 4096;

/// Read buffer for the decode side, large enough that a rectangle's worth of
/// pixels rarely needs more than a couple of syscalls.
const READ_BUFFER: usize = 256 * 1024;

/// A desktop name longer than this is not a desktop name.
const MAX_NAME: usize = 4096;

struct ImageRect {
    rect: Rect,
    encoding: VncEncoding,
}

impl ImageRect {
    async fn read<S>(reader: &mut S) -> Result<Self, VncError>
    where
        S: AsyncRead + Unpin,
    {
        let mut buf = [0_u8; 12];
        reader.read_exact(&mut buf).await?;
        let encoding_id = u32::from_be_bytes(buf[8..12].try_into().unwrap());
        // An encoding we never asked for cannot be skipped: without knowing its
        // length there is no way back to a rectangle boundary. Upstream turned
        // unknown ids into `Raw` and then read a rectangle's worth of pixels out
        // of a stream holding something else entirely.
        let encoding = VncEncoding::try_from(encoding_id).map_err(|unknown| {
            VncError::General(format!(
                "server used encoding {} which was never requested",
                unknown.0
            ))
        })?;
        Ok(Self {
            rect: Rect {
                x: u16::from_be_bytes(buf[0..2].try_into().unwrap()),
                y: u16::from_be_bytes(buf[2..4].try_into().unwrap()),
                width: u16::from_be_bytes(buf[4..6].try_into().unwrap()),
                height: u16::from_be_bytes(buf[6..8].try_into().unwrap()),
            },
            encoding,
        })
    }
}

/// The framebuffer size implied by an event, or the current one if it says nothing.
///
/// A refused resize request is the case that matters: the spec leaves the
/// dimensions of such a reply undefined, so adopting them would poison every
/// later refresh request.
fn adopt_size(current: (u16, u16), event: &VncEvent) -> (u16, u16) {
    match event {
        VncEvent::SetResolution(screen) => (screen.width, screen.height),
        VncEvent::DesktopLayout(layout)
            if !layout.is_reply_to_us() || layout.status == ResizeStatus::Success =>
        {
            (layout.screen.width, layout.screen.height)
        }
        _ => current,
    }
}

struct VncInner {
    name: String,
    /// Current framebuffer size.
    ///
    /// Upstream took this from `ServerInit` and never touched it again, so every
    /// refresh after a resize asked for the wrong rectangle. It is kept in step
    /// here as resolution events pass through to the caller.
    screen: (u16, u16),
    input_ch: Sender<ClientMsg>,
    output_ch: Receiver<VncEvent>,
    tasks: Vec<JoinHandle<()>>,
    closed: bool,
}

impl VncInner {
    async fn new<S>(
        mut stream: S,
        shared: bool,
        mut pixel_format: Option<PixelFormat>,
        encodings: Vec<VncEncoding>,
        quality: Option<u8>,
        compression: Option<u8>,
    ) -> Result<Self, VncError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (output_tx, output_rx) = channel(CHANNEL_SIZE);
        let (input_tx, input_rx) = channel(CHANNEL_SIZE);

        trace!("client init msg");
        send_client_init(&mut stream, shared).await?;

        trace!("server init msg");
        let adopted_server_format = pixel_format.is_none();
        let (name, (width, height)) =
            read_server_init(&mut stream, &mut pixel_format, &|e| async {
                output_tx.send(e).await?;
                Ok(())
            })
            .await?;

        trace!("client encodings: {:?}", encodings);
        let extended_clipboard = encodings.contains(&VncEncoding::ExtendedClipboardPseudo);
        send_client_encoding(&mut stream, encodings, quality, compression).await?;

        // A server that understands the extension answers the `SetEncodings` above with
        // a `caps` message. Ours goes the other way now: text is the only format a
        // terminal can hold, and the size of zero says we would rather be told the
        // clipboard changed than handed a copy of every remote selection.
        if extended_clipboard {
            ClientMsg::ExtendedCutText(clipboard::caps())
                .write(&mut stream)
                .await?;
        }

        // Start with a non-incremental request. Besides fetching the first frame,
        // this is the only way to learn the screen layout: a server supporting
        // ExtendedDesktopSize must answer a non-incremental request with a layout
        // rectangle, and its absence is how we discover it does not.
        input_tx
            .send(ClientMsg::FramebufferUpdateRequest(
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                0,
            ))
            .await?;

        let pf = pixel_format.expect("read_server_init always leaves a pixel format");
        if adopted_server_format {
            debug!("using the server's pixel format: {pf:?}");
        }

        let (read_half, write_half) = tokio::io::split(stream);

        let reader = tokio::spawn(async move {
            let mut reader = BufReader::with_capacity(READ_BUFFER, read_half);
            let output_func = |e| {
                let tx = output_tx.clone();
                async move {
                    tx.send(e).await?;
                    Ok(())
                }
            };
            if let Err(err) = read_loop(&mut reader, &pf, &output_func).await {
                match &err {
                    // The peer going away is how a session normally ends.
                    VncError::IoError(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => {
                        trace!("server closed the connection");
                    }
                    _ => error!("error while decoding: {err:?}"),
                }
                let _ = output_func(VncEvent::Error(err.to_string())).await;
            }
            trace!("decode task stops");
        });

        let writer = tokio::spawn(async move {
            write_loop(write_half, input_rx).await;
            trace!("write task stops");
        });

        info!("VNC client {name} starts");
        Ok(Self {
            name,
            screen: (width, height),
            input_ch: input_tx,
            output_ch: output_rx,
            tasks: vec![reader, writer],
            closed: false,
        })
    }

    async fn input(&mut self, event: X11Event) -> Result<(), VncError> {
        if self.closed {
            return Err(VncError::ClientNotRunning);
        }
        let msg = match event {
            X11Event::Refresh => ClientMsg::FramebufferUpdateRequest(self.full_rect(), 1),
            X11Event::FullRefresh => ClientMsg::FramebufferUpdateRequest(self.full_rect(), 0),
            X11Event::KeyEvent(key) => ClientMsg::KeyEvent(key.keycode, key.down),
            X11Event::PointerEvent(mouse) => {
                ClientMsg::PointerEvent(mouse.position_x, mouse.position_y, mouse.buttons)
            }
            X11Event::CopyText(text) => ClientMsg::ClientCutText(text),
            X11Event::ClipboardRequest => ClientMsg::ExtendedCutText(clipboard::request()),
            X11Event::ClipboardNotify => ClientMsg::ExtendedCutText(clipboard::notify()),
            X11Event::ClipboardProvide(text) => {
                ClientMsg::ExtendedCutText(clipboard::provide(&text))
            }
            X11Event::SetDesktopSize {
                width,
                height,
                screens,
            } => ClientMsg::SetDesktopSize {
                width,
                height,
                screens,
            },
            X11Event::EnableContinuousUpdates { enable, rect } => {
                ClientMsg::EnableContinuousUpdates { enable, rect }
            }
            X11Event::Fence { flags, payload } => ClientMsg::Fence { flags, payload },
        };
        self.input_ch.send(msg).await?;
        Ok(())
    }

    fn full_rect(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.screen.0,
            height: self.screen.1,
        }
    }

    /// Keep the cached size in step with what the caller is being told.
    fn observe(&mut self, event: &VncEvent) {
        self.screen = adopt_size(self.screen, event);
    }

    async fn recv_event(&mut self) -> Result<VncEvent, VncError> {
        if self.closed {
            return Err(VncError::ClientNotRunning);
        }
        match self.output_ch.recv().await {
            Some(e) => {
                self.observe(&e);
                Ok(e)
            }
            None => {
                self.closed = true;
                Err(VncError::ClientNotRunning)
            }
        }
    }

    fn close(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        self.closed = true;
    }
}

impl Drop for VncInner {
    fn drop(&mut self) {
        info!("VNC client {} stops", self.name);
        self.close();
    }
}

pub struct VncClient {
    inner: Arc<Mutex<VncInner>>,
}

impl VncClient {
    pub(super) async fn new<S>(
        stream: S,
        shared: bool,
        pixel_format: Option<PixelFormat>,
        encodings: Vec<VncEncoding>,
        quality: Option<u8>,
        compression: Option<u8>,
    ) -> Result<Self, VncError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Ok(Self {
            inner: Arc::new(Mutex::new(
                VncInner::new(
                    stream,
                    shared,
                    pixel_format,
                    encodings,
                    quality,
                    compression,
                )
                .await?,
            )),
        })
    }

    /// Send an event to the server.
    pub async fn input(&self, event: X11Event) -> Result<(), VncError> {
        self.inner.lock().await.input(event).await
    }

    /// Wait for the next event from the server.
    pub async fn recv_event(&self) -> Result<VncEvent, VncError> {
        self.inner.lock().await.recv_event().await
    }

    /// The framebuffer size as last reported by the server.
    pub async fn resolution(&self) -> (u16, u16) {
        self.inner.lock().await.screen
    }

    pub async fn name(&self) -> String {
        self.inner.lock().await.name.clone()
    }

    /// Stop the engine and release its tasks.
    pub async fn close(&self) {
        self.inner.lock().await.close();
    }
}

impl Clone for VncClient {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

async fn send_client_init<S>(stream: &mut S, shared: bool) -> Result<(), VncError>
where
    S: AsyncWrite + Unpin,
{
    trace!("Send shared flag: {}", shared);
    stream.write_u8(shared as u8).await?;
    Ok(())
}

async fn read_server_init<S, F, Fut>(
    stream: &mut S,
    pf: &mut Option<PixelFormat>,
    output_func: &F,
) -> Result<(String, (u16, u16)), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: Fn(VncEvent) -> Fut,
    Fut: Future<Output = Result<(), VncError>>,
{
    // +--------------+--------------+------------------------------+
    // | No. of bytes | Type [Value] | Description                  |
    // +--------------+--------------+------------------------------+
    // | 2            | U16          | framebuffer-width in pixels  |
    // | 2            | U16          | framebuffer-height in pixels |
    // | 16           | PIXEL_FORMAT | server-pixel-format          |
    // | 4            | U32          | name-length                  |
    // | name-length  | U8 array     | name-string                  |
    // +--------------+--------------+------------------------------+
    let screen_width = stream.read_u16().await?;
    let screen_height = stream.read_u16().await?;
    let mut send_our_pf = false;

    output_func(VncEvent::SetResolution(
        (screen_width, screen_height).into(),
    ))
    .await?;

    let pixel_format = PixelFormat::read(stream).await?;
    if pf.is_none() {
        output_func(VncEvent::SetPixelFormat(pixel_format)).await?;
        let _ = pf.insert(pixel_format);
    } else {
        send_our_pf = true;
    }

    // A 32-bit length is not permission to allocate: keep a plausible prefix and
    // read past the rest so the stream stays aligned.
    let name_len = stream.read_u32().await? as usize;
    let keep = name_len.min(MAX_NAME);
    let mut name_buf = vec![0_u8; keep];
    stream.read_exact(&mut name_buf).await?;
    discard(stream, name_len - keep).await?;
    let name = String::from_utf8_lossy(&name_buf).into_owned();

    if send_our_pf {
        trace!("Send customized pixel format {:#?}", pf);
        ClientMsg::SetPixelFormat(*pf.as_ref().unwrap())
            .write(stream)
            .await?;
    }
    Ok((name, (screen_width, screen_height)))
}

/// Read and throw away `count` bytes, to stay aligned with the stream.
async fn discard<S>(stream: &mut S, mut count: usize) -> Result<(), VncError>
where
    S: AsyncRead + Unpin,
{
    let mut sink = [0u8; 4096];
    while count > 0 {
        let n = count.min(sink.len());
        stream.read_exact(&mut sink[..n]).await?;
        count -= n;
    }
    Ok(())
}

async fn send_client_encoding<S>(
    stream: &mut S,
    encodings: Vec<VncEncoding>,
    quality: Option<u8>,
    compression: Option<u8>,
) -> Result<(), VncError>
where
    S: AsyncWrite + Unpin,
{
    // The hints are encoding numbers with no rectangle behind them, so they belong in
    // the same list -- and it has to be one message, because a second `SetEncodings`
    // replaces the first rather than adding to it.
    let mut ids: Vec<i32> = encodings.into_iter().map(|e| e as i32).collect();
    if let Some(level) = quality {
        ids.push(crate::rfb::hint::quality(level));
    }
    if let Some(level) = compression {
        ids.push(crate::rfb::hint::compression(level));
    }
    ClientMsg::SetEncodingsRaw(ids).write(stream).await?;
    Ok(())
}

/// Read the payload of an `ExtendedDesktopSize` rectangle.
///
/// ```text
/// | 1                      | U8       | number-of-screens |
/// | 3                      |          | padding           |
/// | number-of-screens * 16 | SCREEN[] | screens           |
/// ```
///
/// The rectangle's x and y carry the reason and status codes rather than a
/// position, and its width and height are the new framebuffer size.
async fn read_extended_desktop_size<S>(
    reader: &mut S,
    rect: &Rect,
) -> Result<ScreenLayout, VncError>
where
    S: AsyncRead + Unpin,
{
    let count = reader.read_u8().await?;
    let mut padding = [0u8; 3];
    reader.read_exact(&mut padding).await?;

    let mut screens = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let mut buf = [0u8; ScreenInfo::WIRE_LEN];
        reader.read_exact(&mut buf).await?;
        screens.push(ScreenInfo::decode(&buf));
    }

    Ok(ScreenLayout {
        screen: (rect.width, rect.height).into(),
        screens,
        reason: rect.x,
        status: ResizeStatus::from(rect.y),
    })
}

async fn read_loop<S, F, Fut>(
    stream: &mut S,
    pf: &PixelFormat,
    output_func: &F,
) -> Result<(), VncError>
where
    S: AsyncRead + Unpin,
    F: Fn(VncEvent) -> Fut,
    Fut: Future<Output = Result<(), VncError>>,
{
    let mut raw_decoder = codec::RawDecoder::new();
    let mut zrle_decoder = codec::ZrleDecoder::new();
    let mut tight_decoder = codec::TightDecoder::new();
    let mut cursor = codec::CursorDecoder::new();

    loop {
        let server_msg = ServerMsg::read(stream).await?;
        trace!("Server message got: {:?}", server_msg);
        match server_msg {
            ServerMsg::FramebufferUpdate(rect_num) => {
                for _ in 0..rect_num {
                    let rect = ImageRect::read(stream).await?;

                    match rect.encoding {
                        VncEncoding::Raw => {
                            raw_decoder
                                .decode(pf, &rect.rect, stream, output_func)
                                .await?;
                        }
                        VncEncoding::CopyRect => {
                            let source_x = stream.read_u16().await?;
                            let source_y = stream.read_u16().await?;
                            let mut src_rect = rect.rect;
                            src_rect.x = source_x;
                            src_rect.y = source_y;
                            output_func(VncEvent::Copy(rect.rect, src_rect)).await?;
                        }
                        VncEncoding::Tight => {
                            tight_decoder
                                .decode(pf, &rect.rect, stream, output_func)
                                .await?;
                        }
                        VncEncoding::Zrle => {
                            zrle_decoder
                                .decode(pf, &rect.rect, stream, output_func)
                                .await?;
                        }
                        VncEncoding::CursorPseudo => {
                            cursor.decode(pf, &rect.rect, stream, output_func).await?;
                        }
                        VncEncoding::DesktopSizePseudo => {
                            output_func(VncEvent::SetResolution(
                                (rect.rect.width, rect.rect.height).into(),
                            ))
                            .await?;
                        }
                        VncEncoding::ExtendedDesktopSizePseudo => {
                            let layout = read_extended_desktop_size(stream, &rect.rect).await?;
                            debug!(
                                "desktop layout: {}x{} reason={} status={:?}",
                                layout.screen.width,
                                layout.screen.height,
                                layout.reason,
                                layout.status
                            );
                            output_func(VncEvent::DesktopLayout(layout)).await?;
                        }
                        VncEncoding::QemuLedStatePseudo => {
                            // One byte: scroll lock, num lock, caps lock.
                            let state = stream.read_u8().await?;
                            output_func(VncEvent::LedState {
                                scroll: state & 1 != 0,
                                num: state & 2 != 0,
                                caps: state & 4 != 0,
                            })
                            .await?;
                        }
                        // All three are negotiated through SetEncodings and answered
                        // with messages, never with rectangles. A server sending one
                        // here is out of contract, and guessing at a length would lose
                        // the stream.
                        VncEncoding::FencePseudo
                        | VncEncoding::ContinuousUpdatesPseudo
                        | VncEncoding::ExtendedClipboardPseudo => {
                            return Err(VncError::General(format!(
                                "server sent {:?} as a rectangle",
                                rect.encoding
                            )));
                        }
                        VncEncoding::LastRectPseudo => break,
                    }
                }
                // Tell the caller the update is complete, so it can ask for the
                // next one instead of guessing on a timer.
                output_func(VncEvent::FramebufferUpdateEnd).await?;
            }
            ServerMsg::Bell => {
                output_func(VncEvent::Bell).await?;
            }
            ServerMsg::ServerCutText(text) => {
                output_func(VncEvent::Text(text)).await?;
            }
            ServerMsg::ExtendedClipboard(msg) => match msg {
                clipboard::Message::Caps(caps) => {
                    debug!("server extended clipboard: {caps:?}");
                    output_func(VncEvent::ClipboardCaps(caps)).await?;
                }
                clipboard::Message::Notify { text } => {
                    output_func(VncEvent::ClipboardNotify { text }).await?;
                }
                clipboard::Message::Request => {
                    output_func(VncEvent::ClipboardRequest).await?;
                }
                clipboard::Message::Provide(text) => {
                    output_func(VncEvent::Text(text)).await?;
                }
                // A peek asks us to re-announce what we hold. We never advertised the
                // action, so a server sending one has gone past its own contract, and
                // the notify it wants would say nothing new.
                clipboard::Message::Peek | clipboard::Message::Ignored => {}
            },
            ServerMsg::EndOfContinuousUpdates => {
                output_func(VncEvent::EndOfContinuousUpdates).await?;
            }
            ServerMsg::ServerFence { flags, payload } => {
                output_func(VncEvent::Fence { flags, payload }).await?;
            }
        }
    }
}

async fn write_loop<W>(mut writer: W, mut input_ch: Receiver<ClientMsg>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(msg) = input_ch.recv().await {
        if let Err(err) = msg.write(&mut writer).await {
            error!("failed to send a message to the server: {err:?}");
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Run `read_loop` over a canned server stream and collect what came out.
    ///
    /// The stream always ends short, so the loop finishes with an EOF error; the
    /// events emitted before that are what is under test.
    async fn events_from(mut bytes: &[u8]) -> Vec<VncEvent> {
        let collected = Rc::new(RefCell::new(Vec::new()));
        let sink = |event: VncEvent| {
            let collected = Rc::clone(&collected);
            async move {
                collected.borrow_mut().push(event);
                Ok(())
            }
        };
        let pf = PixelFormat::bgra();
        let err = read_loop(&mut bytes, &pf, &sink).await.unwrap_err();
        assert!(
            matches!(&err, VncError::IoError(io) if io.kind() == std::io::ErrorKind::UnexpectedEof),
            "expected the stream to run out, got {err:?}"
        );
        collected.borrow().clone()
    }

    /// A `FramebufferUpdate` header for `n` rectangles.
    fn update_header(n: u16) -> Vec<u8> {
        let mut v = vec![0, 0];
        v.extend_from_slice(&n.to_be_bytes());
        v
    }

    fn rect_header(x: u16, y: u16, w: u16, h: u16, encoding: i32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&x.to_be_bytes());
        v.extend_from_slice(&y.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&encoding.to_be_bytes());
        v
    }

    #[tokio::test]
    async fn a_raw_rectangle_is_followed_by_an_update_end() {
        let mut stream = update_header(1);
        stream.extend(rect_header(0, 0, 2, 1, 0));
        stream.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // 2 pixels, BGRA

        let events = events_from(&stream).await;
        assert_eq!(events.len(), 2, "{events:?}");
        match &events[0] {
            VncEvent::RawImage(rect, data) => {
                assert_eq!(rect.width, 2);
                assert_eq!(rect.height, 1);
                assert_eq!(data, &[1, 2, 3, 4, 5, 6, 7, 8]);
            }
            other => panic!("expected a raw image, got {other:?}"),
        }
        assert!(matches!(events[1], VncEvent::FramebufferUpdateEnd));
    }

    #[tokio::test]
    async fn last_rect_ends_the_update_exactly_once() {
        // Servers that do not know how many rectangles they will send announce an
        // absurd count and terminate with LastRect instead.
        let mut stream = update_header(0xffff);
        stream.extend(rect_header(0, 0, 1, 1, 0));
        stream.extend_from_slice(&[9, 9, 9, 9]);
        stream.extend(rect_header(0, 0, 0, 0, -224)); // LastRectPseudo

        let events = events_from(&stream).await;
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, VncEvent::FramebufferUpdateEnd))
                .count(),
            1
        );
        assert!(matches!(
            events.last(),
            Some(VncEvent::FramebufferUpdateEnd)
        ));
    }

    #[tokio::test]
    async fn extended_desktop_size_decodes_reason_status_and_screens() {
        // A reply to our own request: reason 1, status 0, new size 1600x833.
        let mut stream = update_header(1);
        stream.extend(rect_header(1, 0, 1600, 833, -308));
        stream.push(1); // one screen
        stream.extend_from_slice(&[0, 0, 0]); // padding
        stream.extend_from_slice(&[0x00, 0x00, 0x00, 0x2a]); // id 42
        stream.extend_from_slice(&[0x00, 0x00]); // x
        stream.extend_from_slice(&[0x00, 0x00]); // y
        stream.extend_from_slice(&[0x06, 0x40]); // width 1600
        stream.extend_from_slice(&[0x03, 0x41]); // height 833
        stream.extend_from_slice(&[0x00, 0x00, 0x00, 0x07]); // flags

        let events = events_from(&stream).await;
        match &events[0] {
            VncEvent::DesktopLayout(layout) => {
                assert_eq!(layout.reason, 1);
                assert!(layout.is_reply_to_us());
                assert_eq!(layout.status, ResizeStatus::Success);
                assert_eq!(layout.screen.width, 1600);
                assert_eq!(layout.screen.height, 833);
                assert_eq!(layout.screens.len(), 1);
                assert_eq!(layout.screens[0].id, 42);
                assert_eq!(layout.screens[0].flags, 7);
            }
            other => panic!("expected a desktop layout, got {other:?}"),
        }
        assert!(matches!(events[1], VncEvent::FramebufferUpdateEnd));
    }

    #[tokio::test]
    async fn a_refused_resize_carries_the_reason() {
        // Status 1, administratively prohibited. x11vnc answers this.
        let mut stream = update_header(1);
        stream.extend(rect_header(1, 1, 0, 0, -308));
        stream.push(0);
        stream.extend_from_slice(&[0, 0, 0]);

        let events = events_from(&stream).await;
        match &events[0] {
            VncEvent::DesktopLayout(layout) => {
                assert!(layout.is_reply_to_us());
                assert_eq!(layout.status, ResizeStatus::Prohibited);
                assert!(layout.screens.is_empty());
            }
            other => panic!("expected a desktop layout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_forwarded_resize_is_not_a_failure() {
        // QEMU passes the request to the guest and cannot say yet.
        let mut stream = update_header(1);
        stream.extend(rect_header(1, 4, 1024, 768, -308));
        stream.push(0);
        stream.extend_from_slice(&[0, 0, 0]);

        let events = events_from(&stream).await;
        match &events[0] {
            VncEvent::DesktopLayout(layout) => {
                assert_eq!(layout.status, ResizeStatus::Forwarded);
            }
            other => panic!("expected a desktop layout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_server_side_resize_reports_reason_zero() {
        let mut stream = update_header(1);
        stream.extend(rect_header(0, 0, 1280, 720, -308));
        stream.push(0);
        stream.extend_from_slice(&[0, 0, 0]);

        let events = events_from(&stream).await;
        match &events[0] {
            VncEvent::DesktopLayout(layout) => {
                assert!(!layout.is_reply_to_us());
                assert_eq!(layout.screen.width, 1280);
            }
            other => panic!("expected a desktop layout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn led_state_decodes_each_lock_key() {
        for (byte, scroll, num, caps) in [
            (0b000u8, false, false, false),
            (0b001, true, false, false),
            (0b010, false, true, false),
            (0b100, false, false, true),
            (0b111, true, true, true),
            // The remaining bits are reserved and must be ignored rather than
            // mistaken for another lock key.
            (0b1111_1100, false, false, true),
        ] {
            let mut stream = update_header(1);
            stream.extend(rect_header(0, 0, 0, 0, -261));
            stream.push(byte);

            let events = events_from(&stream).await;
            match &events[0] {
                VncEvent::LedState {
                    scroll: s,
                    num: n,
                    caps: c,
                } => {
                    assert_eq!((*s, *n, *c), (scroll, num, caps), "byte {byte:#010b}");
                }
                other => panic!("expected LED state, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn end_of_continuous_updates_is_reported() {
        // A server sends this the first time it sees the encoding requested, which is
        // how support is discovered.
        let events = events_from(&[150]).await;
        assert!(matches!(events[0], VncEvent::EndOfContinuousUpdates));
    }

    #[tokio::test]
    async fn a_server_fence_arrives_with_its_flags_and_payload() {
        let mut stream = vec![248, 0, 0, 0];
        stream.extend_from_slice(&(0x8000_0003u32).to_be_bytes()); // request | before | after
        stream.push(4);
        stream.extend_from_slice(b"ping");

        let events = events_from(&stream).await;
        match &events[0] {
            VncEvent::Fence { flags, payload } => {
                assert_eq!(*flags, 0x8000_0003);
                assert_eq!(payload, b"ping");
            }
            other => panic!("expected a fence, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_overlong_fence_payload_is_read_in_full_but_kept_short() {
        // The stream has to stay aligned even when a server ignores the 64-byte cap,
        // so all of it is read and only what we keep is clipped.
        let mut stream = vec![248, 0, 0, 0];
        stream.extend_from_slice(&0u32.to_be_bytes());
        stream.push(200);
        stream.extend_from_slice(&[0xcd; 200]);
        // A message after it proves the reader did not lose its place.
        stream.push(2); // Bell

        let events = events_from(&stream).await;
        match &events[0] {
            VncEvent::Fence { payload, .. } => assert_eq!(payload.len(), 64),
            other => panic!("expected a fence, got {other:?}"),
        }
        assert!(
            matches!(events[1], VncEvent::Bell),
            "the stream lost alignment: {:?}",
            events
        );
    }

    #[tokio::test]
    async fn a_negotiation_only_encoding_sent_as_a_rectangle_is_refused() {
        // Fence and ContinuousUpdates are answered with messages. As a rectangle they
        // have no length, so guessing would lose the stream.
        let mut stream = update_header(1);
        stream.extend(rect_header(0, 0, 0, 0, -313));

        let collected = Rc::new(RefCell::new(Vec::new()));
        let sink = |event: VncEvent| {
            let collected = Rc::clone(&collected);
            async move {
                collected.borrow_mut().push(event);
                Ok(())
            }
        };
        let pf = PixelFormat::bgra();
        let mut slice = stream.as_slice();
        let err = read_loop(&mut slice, &pf, &sink).await.unwrap_err();
        assert!(err.to_string().contains("as a rectangle"), "{err}");
    }

    #[tokio::test]
    async fn the_older_desktop_size_encoding_still_works() {
        let mut stream = update_header(1);
        stream.extend(rect_header(0, 0, 640, 480, -223));

        let events = events_from(&stream).await;
        match &events[0] {
            VncEvent::SetResolution(screen) => {
                assert_eq!((screen.width, screen.height), (640, 480));
            }
            other => panic!("expected a resolution, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unrequested_encoding_is_an_error_not_a_guess() {
        // Guessing would mean reading a rectangle's worth of pixels out of a
        // stream that holds something else, and never resynchronising.
        let mut stream: &[u8] = &{
            let mut v = update_header(1);
            v.extend(rect_header(0, 0, 4, 4, 5)); // Hextile, never requested
            v.extend_from_slice(&[0; 64]);
            v
        };
        let collected = Rc::new(RefCell::new(Vec::new()));
        let sink = |event: VncEvent| {
            let collected = Rc::clone(&collected);
            async move {
                collected.borrow_mut().push(event);
                Ok(())
            }
        };
        let pf = PixelFormat::bgra();
        let err = read_loop(&mut stream, &pf, &sink).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("never requested"), "{message}");
        assert!(message.contains('5'), "{message}");
    }

    #[tokio::test]
    async fn a_copy_rect_reports_both_rectangles() {
        let mut stream = update_header(1);
        stream.extend(rect_header(10, 20, 30, 40, 1));
        stream.extend_from_slice(&[0, 1, 0, 2]); // source at 1,2

        let events = events_from(&stream).await;
        match &events[0] {
            VncEvent::Copy(dst, src) => {
                assert_eq!((dst.x, dst.y, dst.width, dst.height), (10, 20, 30, 40));
                assert_eq!((src.x, src.y), (1, 2));
                assert_eq!((src.width, src.height), (30, 40));
            }
            other => panic!("expected a copy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_colour_map_is_reported_rather_than_panicking() {
        // Upstream had `unimplemented!()` here, so a server could panic us.
        let mut stream: &[u8] = &[1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        let collected = Rc::new(RefCell::new(Vec::new()));
        let sink = |event: VncEvent| {
            let collected = Rc::clone(&collected);
            async move {
                collected.borrow_mut().push(event);
                Ok(())
            }
        };
        let pf = PixelFormat::bgra();
        let err = read_loop(&mut stream, &pf, &sink).await.unwrap_err();
        assert!(err.to_string().contains("colour map"), "{err}");
    }

    #[tokio::test]
    async fn extended_clipboard_messages_become_events_not_four_billion_bytes() {
        // The length is signed, and negative means the extended clipboard form. Read
        // as unsigned it becomes four billion, and the client spends the rest of the
        // session trying to skip that many bytes.
        let mut caps = (clipboard::flag::CAPS
            | clipboard::flag::TEXT
            | clipboard::flag::PROVIDE
            | clipboard::flag::REQUEST)
            .to_be_bytes()
            .to_vec();
        caps.extend_from_slice(&1024u32.to_be_bytes());

        let mut bytes = Vec::new();
        for body in [caps, clipboard::notify()] {
            bytes.extend_from_slice(&[3, 0, 0, 0]); // ServerCutText
            bytes.extend_from_slice(&(-(body.len() as i32)).to_be_bytes());
            bytes.extend_from_slice(&body);
        }

        let mut stream: &[u8] = &bytes;
        let collected = Rc::new(RefCell::new(Vec::new()));
        let sink = |event: VncEvent| {
            let collected = Rc::clone(&collected);
            async move {
                collected.borrow_mut().push(event);
                Ok(())
            }
        };
        let pf = PixelFormat::bgra();
        // Both messages are consumed by their lengths, so the loop runs on until the
        // stream simply ends.
        let _ = read_loop(&mut stream, &pf, &sink).await;

        let events = collected.borrow();
        assert!(
            matches!(events.first(), Some(VncEvent::ClipboardCaps(caps))
                if caps.takes_text() && caps.unsolicited_text() == 1024),
            "{events:?}"
        );
        assert!(
            matches!(
                events.get(1),
                Some(VncEvent::ClipboardNotify { text: true })
            ),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn an_implausible_clipboard_length_is_refused_rather_than_skipped() {
        // Just under two gigabytes: skipping it would read for minutes.
        let mut stream: &[u8] = &[3, 0, 0, 0, 0x7f, 0xff, 0xff, 0xff];
        let collected = Rc::new(RefCell::new(Vec::new()));
        let sink = |event: VncEvent| {
            let collected = Rc::clone(&collected);
            async move {
                collected.borrow_mut().push(event);
                Ok(())
            }
        };
        let pf = PixelFormat::bgra();
        let err = read_loop(&mut stream, &pf, &sink).await.unwrap_err();
        assert!(
            err.to_string().contains("clipboard payload"),
            "expected a refusal, got {err}"
        );
    }

    #[tokio::test]
    async fn a_clipboard_message_within_the_cap_arrives_intact() {
        let text = b"hello clipboard";
        let mut stream = vec![3, 0, 0, 0];
        stream.extend_from_slice(&(text.len() as u32).to_be_bytes());
        stream.extend_from_slice(text);

        let events = events_from(&stream).await;
        match &events[0] {
            VncEvent::Text(got) => assert_eq!(got, "hello clipboard"),
            other => panic!("expected clipboard text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_clipboard_payload_over_the_cap_is_truncated_not_allocated() {
        // Two megabytes is plausible enough to accept but past what is kept, so the
        // content is clipped and the rest read past. The stream ends early here, so
        // the read fails on the missing bytes rather than reserving them.
        let mut stream = vec![3, 0, 0, 0];
        stream.extend_from_slice(&(2_000_000u32).to_be_bytes());
        stream.extend_from_slice(b"the beginning of a very long selection");

        let collected = Rc::new(RefCell::new(Vec::new()));
        let sink = |event: VncEvent| {
            let collected = Rc::clone(&collected);
            async move {
                collected.borrow_mut().push(event);
                Ok(())
            }
        };
        let pf = PixelFormat::bgra();
        let mut slice = stream.as_slice();
        let err = read_loop(&mut slice, &pf, &sink).await.unwrap_err();
        assert!(
            matches!(&err, VncError::IoError(io) if io.kind() == std::io::ErrorKind::UnexpectedEof),
            "got {err:?}"
        );
    }

    fn layout(w: u16, h: u16, reason: u16, status: ResizeStatus) -> VncEvent {
        VncEvent::DesktopLayout(ScreenLayout {
            screen: (w, h).into(),
            screens: vec![],
            reason,
            status,
        })
    }

    #[test]
    fn a_failed_resize_reply_does_not_move_the_cached_size() {
        let start = (1024u16, 768u16);
        assert_eq!(
            adopt_size(start, &layout(0, 0, 1, ResizeStatus::Prohibited)),
            start,
            "a refusal leaves the dimensions undefined and must be ignored"
        );
        assert_eq!(
            adopt_size(start, &layout(0, 0, 1, ResizeStatus::Forwarded)),
            start,
            "a forwarded request has not happened yet"
        );
        assert_eq!(
            adopt_size(start, &layout(1600, 833, 1, ResizeStatus::Success)),
            (1600, 833),
            "a success must be adopted"
        );
        assert_eq!(
            adopt_size(start, &layout(800, 600, 0, ResizeStatus::Success)),
            (800, 600),
            "a server-side change must be adopted"
        );
        assert_eq!(
            adopt_size(start, &layout(640, 480, 2, ResizeStatus::Success)),
            (640, 480),
            "another client's change must be adopted"
        );
    }

    #[test]
    fn the_cached_size_follows_a_plain_resolution_event() {
        assert_eq!(
            adopt_size((100, 100), &VncEvent::SetResolution((1920, 1080).into())),
            (1920, 1080)
        );
        assert_eq!(
            adopt_size((100, 100), &VncEvent::FramebufferUpdateEnd),
            (100, 100)
        );
    }
}
