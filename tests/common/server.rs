//! A fake VNC server, enough of RFB 3.8 to drive the client end to end.
//!
//! Speaks the handshake, answers framebuffer requests, and -- the point of the
//! exercise -- implements the `ExtendedDesktopSize` half of the resize
//! negotiation, with a switch for each way a real server can answer.
//!
//! Blocking I/O on its own thread: the point is to be obviously correct, not
//! fast.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How a server answers `SetDesktopSize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resize {
    /// Agree, and report the new size back.
    Accept,
    /// Refuse: administratively prohibited, which is what x11vnc says.
    Refuse,
    /// Say the request was forwarded and cannot be judged yet, the way a
    /// hypervisor does, then accept it a moment later.
    Forward,
    /// Never send an `ExtendedDesktopSize` rectangle at all, so the client has to
    /// conclude resizing is unavailable.
    Unsupported,
    /// Accept, but round the height down to a mode of its own choosing, the way a
    /// server backed by real display modes does.
    Snap,
}

/// What the client asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    SetPixelFormat,
    SetEncodings(Vec<i32>),
    FramebufferUpdate {
        incremental: bool,
    },
    Key {
        keysym: u32,
        down: bool,
    },
    Pointer {
        x: u16,
        y: u16,
        buttons: u8,
    },
    CutText(String),
    SetDesktopSize {
        width: u16,
        height: u16,
        screen_ids: Vec<u32>,
    },
    EnableContinuousUpdates {
        enable: bool,
        width: u16,
        height: u16,
    },
    Fence {
        flags: u32,
        payload: Vec<u8>,
    },
}

/// Extensions the fake server offers, beyond the resize behaviour.
#[derive(Debug, Clone, Copy, Default)]
pub struct Extensions {
    /// Answer `SetEncodings` with `EndOfContinuousUpdates`, which is how a server
    /// says the extension exists, then push frames without being asked.
    pub continuous_updates: bool,
    /// Send a `ServerFence` with the request bit set, and expect it echoed back.
    pub fence: bool,
    /// Report lock-key state, with caps lock on.
    pub led_caps_on: bool,
    /// Send a cursor shape, which a server only does once the client has asked for the
    /// Cursor pseudo-encoding.
    pub cursor: bool,
}

pub struct FakeServer {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
}

impl FakeServer {
    /// Start listening on a loopback port chosen by the kernel.
    pub fn start(width: u16, height: u16, resize: Resize) -> Self {
        Self::start_with(width, height, resize, Extensions::default())
    }

    /// As [`Self::start`], with extensions enabled.
    pub fn start_with(width: u16, height: u16, resize: Resize, extensions: Extensions) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind");
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let _ = serve(
                    stream,
                    width,
                    height,
                    resize,
                    extensions,
                    thread_requests,
                    thread_stop,
                );
            }
        });

        Self {
            addr,
            requests,
            stop,
        }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// Wait for a request matching `predicate`, returning it.
    pub fn wait_for<F>(&self, timeout: Duration, predicate: F) -> Option<Request>
    where
        F: Fn(&Request) -> bool,
    {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if let Some(found) = self.requests().into_iter().find(|r| predicate(r)) {
                return Some(found);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn serve(
    mut stream: TcpStream,
    mut width: u16,
    mut height: u16,
    resize: Resize,
    extensions: Extensions,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;

    // Version handshake.
    stream.write_all(b"RFB 003.008\n")?;
    let mut version = [0u8; 12];
    stream.read_exact(&mut version)?;

    // Security: offer only None, then report success.
    stream.write_all(&[1, 1])?;
    let mut chosen = [0u8; 1];
    stream.read_exact(&mut chosen)?;
    assert_eq!(
        chosen[0], 1,
        "client picked a security type we did not offer"
    );
    stream.write_all(&0u32.to_be_bytes())?;

    // ClientInit, then ServerInit.
    let mut shared = [0u8; 1];
    stream.read_exact(&mut shared)?;
    send_server_init(&mut stream, width, height)?;

    let mut pending_forward: Option<(u16, u16)> = None;

    while !stop.load(Ordering::SeqCst) {
        let mut kind = [0u8; 1];
        match stream.read_exact(&mut kind) {
            Ok(()) => {}
            Err(_) => break,
        }

        match kind[0] {
            0 => {
                // SetPixelFormat: three bytes of padding and sixteen of format.
                let mut rest = [0u8; 19];
                stream.read_exact(&mut rest)?;
                record(&requests, Request::SetPixelFormat);
            }
            2 => {
                let mut head = [0u8; 3];
                stream.read_exact(&mut head)?;
                let count = u16::from_be_bytes([head[1], head[2]]);
                let mut encodings = Vec::new();
                for _ in 0..count {
                    let mut e = [0u8; 4];
                    stream.read_exact(&mut e)?;
                    encodings.push(i32::from_be_bytes(e));
                }
                let encodings_seen = encodings.clone();
                record(&requests, Request::SetEncodings(encodings));

                // The first sight of the ContinuousUpdates encoding is answered with
                // EndOfContinuousUpdates, which is how the spec has a server admit the
                // extension exists.
                if extensions.continuous_updates {
                    stream.write_all(&[150])?;
                    stream.flush()?;
                }
                // Likewise a fence: the server sends one to show it understands them.
                if extensions.fence {
                    let mut msg = vec![248u8, 0, 0, 0];
                    msg.extend_from_slice(&(0x8000_0003u32).to_be_bytes());
                    msg.push(4);
                    msg.extend_from_slice(b"hail");
                    stream.write_all(&msg)?;
                    stream.flush()?;
                }
                if extensions.led_caps_on {
                    send_led_state(&mut stream, true, false)?;
                }
                if extensions.cursor {
                    // Only legal because the client asked: without the encoding the
                    // server has to composite the pointer itself.
                    assert!(
                        encodings_seen.contains(&-239),
                        "sent a cursor shape to a client that never asked for one"
                    );
                    send_cursor(&mut stream)?;
                }
            }
            3 => {
                let mut rest = [0u8; 9];
                stream.read_exact(&mut rest)?;
                let incremental = rest[0] == 1;
                record(&requests, Request::FramebufferUpdate { incremental });

                if !incremental {
                    // The spec requires a layout rectangle in reply to a
                    // non-incremental request, and its absence is how a client
                    // learns that resizing is unavailable.
                    if resize != Resize::Unsupported {
                        send_extended_desktop_size(&mut stream, width, height, 0, 0)?;
                    }
                    send_full_frame(&mut stream, width, height)?;
                }
            }
            4 => {
                let mut rest = [0u8; 7];
                stream.read_exact(&mut rest)?;
                record(
                    &requests,
                    Request::Key {
                        down: rest[0] == 1,
                        keysym: u32::from_be_bytes([rest[3], rest[4], rest[5], rest[6]]),
                    },
                );
            }
            5 => {
                let mut rest = [0u8; 5];
                stream.read_exact(&mut rest)?;
                record(
                    &requests,
                    Request::Pointer {
                        buttons: rest[0],
                        x: u16::from_be_bytes([rest[1], rest[2]]),
                        y: u16::from_be_bytes([rest[3], rest[4]]),
                    },
                );
            }
            6 => {
                let mut head = [0u8; 7];
                stream.read_exact(&mut head)?;
                let len = u32::from_be_bytes([head[3], head[4], head[5], head[6]]) as usize;
                let mut text = vec![0u8; len];
                stream.read_exact(&mut text)?;
                // Latin-1, as the protocol says -- one byte per character. Decoding
                // this as UTF-8 would accept the client sending UTF-8 too, which is
                // exactly the bug the paste tests are here to catch.
                record(
                    &requests,
                    Request::CutText(text.iter().map(|&b| b as char).collect()),
                );
            }
            150 => {
                let mut rest = [0u8; 9];
                stream.read_exact(&mut rest)?;
                record(
                    &requests,
                    Request::EnableContinuousUpdates {
                        enable: rest[0] == 1,
                        width: u16::from_be_bytes([rest[5], rest[6]]),
                        height: u16::from_be_bytes([rest[7], rest[8]]),
                    },
                );
                if rest[0] == 1 {
                    // Push a frame unasked, which is the whole point of the extension.
                    send_full_frame(&mut stream, width, height)?;
                } else {
                    // Turning it off is acknowledged with EndOfContinuousUpdates.
                    stream.write_all(&[150])?;
                    stream.flush()?;
                }
            }
            248 => {
                let mut head = [0u8; 8];
                stream.read_exact(&mut head)?;
                let flags = u32::from_be_bytes([head[3], head[4], head[5], head[6]]);
                let len = head[7] as usize;
                let mut payload = vec![0u8; len];
                stream.read_exact(&mut payload)?;
                record(&requests, Request::Fence { flags, payload });
            }
            251 => {
                // SetDesktopSize: padding, width, height, screen count, padding,
                // then sixteen bytes per screen.
                let mut head = [0u8; 7];
                stream.read_exact(&mut head)?;
                let want_w = u16::from_be_bytes([head[1], head[2]]);
                let want_h = u16::from_be_bytes([head[3], head[4]]);
                let count = head[5];
                let mut screen_ids = Vec::new();
                for _ in 0..count {
                    let mut screen = [0u8; 16];
                    stream.read_exact(&mut screen)?;
                    screen_ids.push(u32::from_be_bytes(screen[0..4].try_into().unwrap()));
                }
                record(
                    &requests,
                    Request::SetDesktopSize {
                        width: want_w,
                        height: want_h,
                        screen_ids,
                    },
                );

                match resize {
                    Resize::Snap => {
                        width = want_w;
                        // Round the height down to a multiple of 100: accepted, but
                        // not what was asked for.
                        height = (want_h / 100) * 100;
                        send_extended_desktop_size(&mut stream, width, height, 1, 0)?;
                        send_full_frame(&mut stream, width, height)?;
                    }
                    Resize::Accept => {
                        width = want_w;
                        height = want_h;
                        // reason 1 (this client asked), status 0 (success)
                        send_extended_desktop_size(&mut stream, width, height, 1, 0)?;
                        send_full_frame(&mut stream, width, height)?;
                    }
                    Resize::Refuse => {
                        // reason 1, status 1: administratively prohibited. The
                        // dimensions are undefined in this case, so send zeros --
                        // a client that adopts them is broken.
                        send_extended_desktop_size(&mut stream, 0, 0, 1, 1)?;
                    }
                    Resize::Forward => {
                        // reason 1, status 4: cannot say yet.
                        send_extended_desktop_size(&mut stream, 0, 0, 1, 4)?;
                        pending_forward = Some((want_w, want_h));
                    }
                    Resize::Unsupported => {}
                }
            }
            other => panic!("client sent an unknown message type {other}"),
        }

        // A forwarded request completes a moment later, out of band, exactly as a
        // hypervisor would report it once the guest caught up.
        if let Some((w, h)) = pending_forward.take() {
            std::thread::sleep(Duration::from_millis(50));
            width = w;
            height = h;
            send_extended_desktop_size(&mut stream, width, height, 1, 0)?;
            send_full_frame(&mut stream, width, height)?;
        }
    }
    Ok(())
}

/// A 4x4 cursor with a hotspot in the middle, as a Cursor pseudo-rectangle.
///
/// The payload is the pixels followed by a 1-bit-per-pixel mask, which is where the
/// transparency comes from.
fn send_cursor(stream: &mut TcpStream) -> std::io::Result<()> {
    let (w, h) = (4u16, 4u16);
    let mut msg = vec![0, 0];
    msg.extend_from_slice(&1u16.to_be_bytes());
    msg.extend_from_slice(&2u16.to_be_bytes()); // x carries the hotspot
    msg.extend_from_slice(&2u16.to_be_bytes()); // y carries the hotspot
    msg.extend_from_slice(&w.to_be_bytes());
    msg.extend_from_slice(&h.to_be_bytes());
    msg.extend_from_slice(&(-239i32).to_be_bytes());
    // Pixels: opaque white, BGRA.
    for _ in 0..(w as u32 * h as u32) {
        msg.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
    }
    // Mask: one byte per row for a 4-wide cursor, top two rows visible.
    msg.extend_from_slice(&[0xf0, 0xf0, 0x00, 0x00]);
    stream.write_all(&msg)?;
    stream.flush()
}

/// One `FramebufferUpdate` holding a QEMU LED state rectangle.
fn send_led_state(stream: &mut TcpStream, caps: bool, num: bool) -> std::io::Result<()> {
    let mut msg = vec![0, 0];
    msg.extend_from_slice(&1u16.to_be_bytes());
    msg.extend_from_slice(&0u16.to_be_bytes()); // x
    msg.extend_from_slice(&0u16.to_be_bytes()); // y
    msg.extend_from_slice(&0u16.to_be_bytes()); // width
    msg.extend_from_slice(&0u16.to_be_bytes()); // height
    msg.extend_from_slice(&(-261i32).to_be_bytes());
    let mut state = 0u8;
    if num {
        state |= 2;
    }
    if caps {
        state |= 4;
    }
    msg.push(state);
    stream.write_all(&msg)?;
    stream.flush()
}

fn record(requests: &Arc<Mutex<Vec<Request>>>, request: Request) {
    requests.lock().unwrap().push(request);
}

fn send_server_init(stream: &mut TcpStream, width: u16, height: u16) -> std::io::Result<()> {
    let mut msg = Vec::new();
    msg.extend_from_slice(&width.to_be_bytes());
    msg.extend_from_slice(&height.to_be_bytes());
    // BGRA, which is what the client asks for anyway.
    msg.extend_from_slice(&[
        32, 24, 0, 1, // bpp, depth, big endian, true colour
        0, 255, 0, 255, 0, 255, // max values
        16, 8, 0, // shifts
        0, 0, 0, // padding
    ]);
    let name = b"fake-desktop";
    msg.extend_from_slice(&(name.len() as u32).to_be_bytes());
    msg.extend_from_slice(name);
    stream.write_all(&msg)?;
    stream.flush()
}

/// One `FramebufferUpdate` holding a single `ExtendedDesktopSize` rectangle.
///
/// Sent on its own because the spec forbids mixing framebuffer data into an
/// update that carries one.
fn send_extended_desktop_size(
    stream: &mut TcpStream,
    width: u16,
    height: u16,
    reason: u16,
    status: u16,
) -> std::io::Result<()> {
    let mut msg = vec![0, 0];
    msg.extend_from_slice(&1u16.to_be_bytes()); // one rectangle
    msg.extend_from_slice(&reason.to_be_bytes()); // x carries the reason
    msg.extend_from_slice(&status.to_be_bytes()); // y carries the status
    msg.extend_from_slice(&width.to_be_bytes());
    msg.extend_from_slice(&height.to_be_bytes());
    msg.extend_from_slice(&(-308i32).to_be_bytes());
    msg.push(1); // one screen
    msg.extend_from_slice(&[0, 0, 0]); // padding
    msg.extend_from_slice(&0x2au32.to_be_bytes()); // screen id 42
    msg.extend_from_slice(&0u16.to_be_bytes()); // x
    msg.extend_from_slice(&0u16.to_be_bytes()); // y
    msg.extend_from_slice(&width.to_be_bytes());
    msg.extend_from_slice(&height.to_be_bytes());
    msg.extend_from_slice(&0x8000_0001u32.to_be_bytes()); // flags, incl. an unknown bit
    stream.write_all(&msg)?;
    stream.flush()
}

/// A raw rectangle in the top-left corner, enough to give the client something
/// to draw without pushing megabytes through the test.
fn send_full_frame(stream: &mut TcpStream, width: u16, height: u16) -> std::io::Result<()> {
    let w = width.min(64);
    let h = height.min(64);
    if w == 0 || h == 0 {
        return Ok(());
    }
    let mut msg = vec![0, 0];
    msg.extend_from_slice(&1u16.to_be_bytes());
    msg.extend_from_slice(&0u16.to_be_bytes()); // x
    msg.extend_from_slice(&0u16.to_be_bytes()); // y
    msg.extend_from_slice(&w.to_be_bytes());
    msg.extend_from_slice(&h.to_be_bytes());
    msg.extend_from_slice(&0i32.to_be_bytes()); // Raw
    // A recognisable colour: pure red, as BGRA.
    for _ in 0..(u32::from(w) * u32::from(h)) {
        msg.extend_from_slice(&[0, 0, 0xff, 0xff]);
    }
    stream.write_all(&msg)?;
    stream.flush()
}
