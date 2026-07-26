// Each test binary compiles this module and uses a different subset of it, so
// unused items here are expected rather than a sign of rot.
#![allow(dead_code)]

//! A fake terminal, shared by the integration tests.
//!
//! These run the real binary inside a pty that this process drives, answering
//! its capability queries the way Ghostty does and then inspecting the escape
//! stream it produces. That covers what unit tests cannot reach: the probe round
//! trip, geometry from `TIOCGWINSZ`, the setup and teardown sequences, resize
//! handling, and whether frames actually come out.
//!
//! A background thread drains the pty without pause, because that is what a real
//! terminal does. Reading only when the test wants to look would let the pty
//! buffer fill and stall the client, turning every timing assertion into a
//! measurement of this harness instead of the client.

pub mod server;
pub mod session;

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::{Condvar, LazyLock};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_desktui");

/// Image id 1893 is 0x765, the id the client probes with.
pub const GHOSTTY_REPLIES: &[u8] = b"\x1b_Gi=1893;OK\x1b\\\
                                 \x1b[4;850;1600t\
                                 \x1b[6;17;8t\
                                 \x1b[?1016;2$y\
                                 \x1b[?2026;2$y\
                                 \x1b[?0u\
                                 \x1b_Gi=1894;OK\x1b\\\
                                 \x1b[?62;22c";

/// A terminal that answers nothing about its keyboard, so key releases never
/// arrive and the client has to synthesise them.
pub const NO_KEYBOARD_REPLIES: &[u8] = b"\x1b_Gi=1893;OK\x1b\\\
                                        \x1b[4;850;1600t\
                                        \x1b[6;17;8t\
                                        \x1b[?1016;2$y\
                                        \x1b[?2026;2$y\
                                        \x1b[?62;22c";

/// A terminal that draws but cannot map shared memory, so pixels have to travel
/// as base64. Note the absent answer to the `t=s` question.
pub const DIRECT_ONLY_REPLIES: &[u8] = b"\x1b_Gi=1893;OK\x1b\\\
                                        \x1b[4;850;1600t\
                                        \x1b[6;17;8t\
                                        \x1b[?1016;2$y\
                                        \x1b[?2026;2$y\
                                        \x1b[?0u\
                                        \x1b[?62;22c";

/// A terminal that knows nothing but primary device attributes.
pub const PLAIN_REPLIES: &[u8] = b"\x1b[?6c";

/// A fake terminal: a pty, a child attached to it, and everything it has said.
/// The number of pty pairs allowed open at once.
///
/// The harness runs every test in parallel, and a machine has a finite supply of
/// ptys: past a couple of dozen, `openpty` starts failing and the tests measure
/// that rather than the client. Four keeps the suite quick and the failures real.
const MAX_CONCURRENT_PTYS: usize = 4;

static PTY_SLOTS: LazyLock<(Mutex<usize>, Condvar)> =
    LazyLock::new(|| (Mutex::new(MAX_CONCURRENT_PTYS), Condvar::new()));

/// Held for the lifetime of a fake terminal, released on drop.
struct PtySlot;

impl PtySlot {
    fn acquire() -> Self {
        let (lock, cvar) = &*PTY_SLOTS;
        let mut free = lock.lock().unwrap();
        while *free == 0 {
            free = cvar.wait(free).unwrap();
        }
        *free -= 1;
        Self
    }
}

impl Drop for PtySlot {
    fn drop(&mut self) {
        let (lock, cvar) = &*PTY_SLOTS;
        *lock.lock().unwrap() += 1;
        cvar.notify_one();
    }
}

pub struct FakeTerm {
    _slot: PtySlot,
    child: Child,
    master: std::fs::File,
    pub seen: Arc<Mutex<Vec<u8>>>,
    _slave: OwnedFd,
}

impl FakeTerm {
    pub fn spawn(cols: u16, rows: u16, xpixel: u16, ypixel: u16, args: &[&str]) -> Self {
        Self::spawn_with_env(cols, rows, xpixel, ypixel, args, &[])
    }

    /// As [`Self::spawn`], with extra environment for the child.
    pub fn spawn_with_env(
        cols: u16,
        rows: u16,
        xpixel: u16,
        ypixel: u16,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Self {
        let slot = PtySlot::acquire();
        let mut master_fd = -1;
        let mut slave_fd = -1;
        let mut ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: xpixel,
            ws_ypixel: ypixel,
        };
        // Even under the concurrency cap, a pty freed by a previous test can take a
        // moment to come back, so give it a few tries before giving up.
        let mut rc = -1;
        for attempt in 0..20 {
            // SAFETY: out-params are valid locals; the termios argument is optional.
            //
            // `winp` is a raw pointer rather than `&mut ws` because the two platforms
            // disagree about it: Apple declares it `*mut winsize` and Linux
            // `*const winsize`. A `*mut` coerces to either, where `&mut` satisfies
            // only Apple and `&` only Linux.
            rc = unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut ws,
                )
            };
            if rc == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50 * (attempt + 1)));
        }
        assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());
        // SAFETY: both fds come from a successful openpty and are unowned.
        let (master, slave) = unsafe {
            (
                OwnedFd::from_raw_fd(master_fd),
                OwnedFd::from_raw_fd(slave_fd),
            )
        };

        let raw_slave = slave.as_raw_fd();
        let mut cmd = Command::new(BIN);
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.args(args)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));

        // Make the pty the child's controlling terminal: crossterm reaches for
        // /dev/tty, which would otherwise be the terminal running the tests.
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(raw_slave, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().expect("failed to start desktui");

        // Drain continuously, the way a terminal does.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut reader = std::fs::File::from(master.try_clone().unwrap());
        let sink = Arc::clone(&seen);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 16384];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break, // child gone; EIO is the usual report
                    Ok(n) => sink.lock().unwrap().extend_from_slice(&chunk[..n]),
                }
            }
        });

        Self {
            _slot: slot,
            child,
            master: std::fs::File::from(master),
            seen,
            _slave: slave,
        }
    }

    /// Write to the pty, tolerating a child that has already exited.
    ///
    /// A real terminal does not fall over because the program it was running went
    /// away, and a panic here would mask whatever actually failed.
    pub fn send(&mut self, bytes: &[u8]) {
        let _ = self.master.write_all(bytes);
        let _ = self.master.flush();
    }

    pub fn output(&self) -> Vec<u8> {
        self.seen.lock().unwrap().clone()
    }

    /// Wait for `needle` to appear in everything said so far.
    pub fn wait_for(&self, needle: &[u8], timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if contains(&self.output(), needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        contains(&self.output(), needle)
    }

    /// Answer the capability probe once it arrives.
    pub fn answer_probe(&mut self, replies: &[u8]) {
        assert!(
            self.wait_for(b"\x1b[c", Duration::from_secs(10)),
            "no capability probe arrived: {}",
            show(&self.output())
        );
        self.send(replies);
    }

    pub fn resize(&mut self, cols: u16, rows: u16, xpixel: u16, ypixel: u16) {
        let mut ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: xpixel,
            ws_ypixel: ypixel,
        };
        // SAFETY: valid fd and winsize; SIGWINCH to a live child.
        unsafe {
            assert_eq!(
                libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &mut ws),
                0,
                "TIOCSWINSZ failed: {}",
                std::io::Error::last_os_error()
            );
            libc::kill(self.child.id() as i32, libc::SIGWINCH);
        }
    }

    /// Wait for the child to exit, killing it if it overstays.
    pub fn wait(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => return None,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        None
    }
}

impl Drop for FakeTerm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

pub fn count(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

/// The readable part of the output, for assertion messages: escape-heavy tails are
/// unreadable, and the status line is what actually says what happened.
///
/// Where `show` dumps the head of the stream with its escapes spelled out, this keeps
/// only the last few status lines. That is the right end to look at when a claim about
/// what the client ended up reporting fails.
pub fn tail(buf: &[u8]) -> String {
    let text = String::from_utf8_lossy(buf);
    let mut lines: Vec<&str> = text
        .split('\x1b')
        .filter(|s| s.contains("desktui") || s.contains("1:1") || s.contains("error"))
        .collect();
    lines.dedup();
    let n = lines.len();
    lines.drain(..n.saturating_sub(4));
    lines.join(" | ")
}

/// Escapes made readable, for assertion messages.
pub fn show(buf: &[u8]) -> String {
    let mut s = String::new();
    for &b in buf.iter().take(3000) {
        match b {
            0x1b => s.push_str("<ESC>"),
            b'\n' => s.push_str("\\n"),
            b'\r' => s.push_str("\\r"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    s
}
