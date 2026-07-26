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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::{Condvar, LazyLock};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_desktui");

/// The escape that opens a frame. The client wraps each draw in synchronised output,
/// so one of these begins everything a single frame has to say.
///
/// Only for the terminals that answer for mode 2026, which is every one here bar
/// [`PLAIN_REPLIES`]: without the answer there is no wrapper to count.
pub const FRAME: &[u8] = b"\x1b[?2026h";

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
    /// Set by the drain thread when the pty has run dry, which cannot happen while a
    /// slave fd is still open. See [`FakeTerm::settle`].
    drained: Arc<AtomicBool>,
    /// Our own end of the slave, closed to let the drain thread reach end-of-file.
    slave: Option<OwnedFd>,
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
        let drained = Arc::new(AtomicBool::new(false));
        let mut reader = std::fs::File::from(master.try_clone().unwrap());
        let sink = Arc::clone(&seen);
        let done = Arc::clone(&drained);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 16384];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break, // no slave left; EIO is the usual report
                    Ok(n) => sink.lock().unwrap().extend_from_slice(&chunk[..n]),
                }
            }
            done.store(true, Ordering::Release);
        });

        Self {
            _slot: slot,
            child,
            master: std::fs::File::from(master),
            seen,
            drained,
            slave: Some(slave),
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

    /// Wait for `needle` to appear at or after `from`, and answer with where it ended.
    ///
    /// The offset back is what makes these compose: a claim about what the client did
    /// *next* begins where the evidence for the last one finished.
    pub fn wait_for_after(&self, from: usize, needle: &[u8], timeout: Duration) -> Option<usize> {
        let start = Instant::now();
        loop {
            let out = self.output();
            if let Some(at) = find(&out[from.min(out.len())..], needle) {
                return Some(from + at + needle.len());
            }
            if start.elapsed() >= timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Everything drawn from the first frame to begin at or after `from`, once `frames`
    /// of them have begun.
    ///
    /// This is what a claim about what is *no longer* on the screen needs. Marking the
    /// output, sleeping and reading the tail looks like the same thing and is not: that
    /// window opens wherever the last read happened to end, which is usually partway
    /// through a frame the client had already composed -- before it had seen the
    /// keystroke, and so evidence about nothing. Opening on a frame boundary instead,
    /// and waiting for the frames rather than hoping they fit inside a fixed sleep,
    /// asks what the client drew once it knew.
    ///
    /// `None` if the frames never came, which is a failure worth reporting rather than
    /// passing over: an empty window satisfies an absence assertion for want of
    /// evidence rather than because of it.
    pub fn drawn_after(&self, from: usize, frames: usize, timeout: Duration) -> Option<Vec<u8>> {
        let start = Instant::now();
        loop {
            let out = self.output();
            let mut at = from.min(out.len());
            let mut first = None;
            let mut seen = 0;
            while let Some(next) = find(&out[at..], FRAME) {
                at += next + FRAME.len();
                first.get_or_insert(at - FRAME.len());
                seen += 1;
            }
            if let Some(first) = first
                && seen >= frames
            {
                return Some(out[first..].to_vec());
            }
            if start.elapsed() >= timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
    ///
    /// A status back means the output is complete as well as the process: see
    /// [`Self::settle`].
    pub fn wait(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.settle();
                    return Some(status);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => return None,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        None
    }

    /// Wait for the pty to run dry, the child that was filling it having gone.
    ///
    /// `try_wait` answers about the process, not about the pty. The last things the
    /// client says are the teardown and the line explaining why it stopped, and those
    /// can still be in the buffer when it exits, so a test that reads the output the
    /// moment it does sometimes misses the end of them. Closing our own end of the
    /// slave leaves nothing that could add more, so the drain thread reads to
    /// end-of-file and finishes -- and its finishing is the same statement as
    /// "everything that was said has arrived".
    fn settle(&mut self) {
        drop(self.slave.take());
        let began = Instant::now();
        // Capped, because a test hung here would say nothing at all about what it was
        // checking, where one that goes on asserts against what did arrive.
        while !self.drained.load(Ordering::Acquire) && began.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
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

/// Every offset at which `needle` appears.
pub fn offsets(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = find(&haystack[from..], needle) {
        found.push(from + at);
        from += at + 1;
    }
    found
}

/// Erasing the whole screen, which a layout change is never allowed to need.
pub const ERASE_SCREEN: &[u8] = b"\x1b[2J";
/// The two narrower ways of taking something off the screen: a row of text, and one
/// image and the data behind it.
pub const ERASE_ROW: &[u8] = b"\x1b[2K";
pub const DELETE_IMAGE: &[u8] = b"a=d,d=I";
/// Synchronised output. Everything a layout change takes off the screen has to be
/// inside a block that puts the new one there.
pub const BEGIN_SYNC: &[u8] = b"\x1b[?2026h";
pub const END_SYNC: &[u8] = b"\x1b[?2026l";

/// The synchronised block holding the first occurrence of `needle`, which is one frame.
///
/// What a frame contains is often the claim -- that an erase and the tiles that undo it
/// travel together, that every tile of one frame came out of one shared memory object --
/// and a claim about a frame has to be measured inside its own markers.
pub fn frame_containing<'a>(out: &'a [u8], needle: &[u8]) -> Option<&'a [u8]> {
    let at = find(out, needle)?;
    let open = offsets(out, BEGIN_SYNC).into_iter().rfind(|o| *o < at)?;
    let from = open + BEGIN_SYNC.len();
    let len = find(&out[from..], END_SYNC)?;
    Some(&out[from..from + len])
}

/// A layout change since `before` never left the screen blank.
///
/// Text and placements alike stay on the cells they were written to when the grid
/// changes shape, so a relayout has to take the stale ones off. It used to do that by
/// erasing the whole screen and deleting every image -- and to write that on its own,
/// ahead of the frame that fills the screen back in, so the terminal was blank until the
/// next one composed. Twice or three times over, a resize settling through several paths.
///
/// Two claims, then. The screen is never erased wholesale: a relayout names the rows and
/// the images it is actually taking, and everything else is replaced where it stands. And
/// what it does take, it takes inside the synchronised block that redraws -- so the
/// terminal shows one layout or the other and never neither.
///
/// `before` has to be an offset past the setup sequence, whose own erase is the alternate
/// screen being entered and has nothing to redraw yet.
///
/// Both loops make this claim -- the session and the test pattern -- so it is written
/// once here rather than twice.
#[track_caller]
pub fn assert_a_relayout_never_blanks_the_screen(term: &FakeTerm, before: usize) {
    // A frame is written in one go, but a snapshot can still catch one mid-write, so
    // wait for a block carrying tiles to be there in full.
    let mut out = term.output();
    for _ in 0..500 {
        let whole = {
            let seen = &out[before..];
            find(seen, session::DREW).is_some_and(|drew| find(&seen[drew..], END_SYNC).is_some())
        };
        if whole {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
        out = term.output();
    }

    let seen = &out[before..];
    assert!(
        contains(seen, session::DREW),
        "the new layout was never drawn: {}",
        show(seen)
    );
    let opens = offsets(seen, BEGIN_SYNC);
    let closes = offsets(seen, END_SYNC);

    // A resize does erase the screen -- what a terminal leaves on the alternate screen after
    // the window changed shape is not something we can claim to know -- so the property is
    // not that it never happens but that it is never *seen*: the block that erases has to be
    // the block that puts the picture back. Erases before the first block are the setup
    // entering the alternate screen, with nothing drawn yet to lose.
    for at in offsets(seen, ERASE_SCREEN) {
        let Some(open) = opens.iter().copied().rfind(|o| *o < at) else {
            continue;
        };
        // `None < Some(_)`, so an open marker before it with no close since means the erase
        // is inside that block rather than after it ended.
        let since = closes.iter().copied().rfind(|c| *c < at);
        assert!(
            since < Some(open),
            "the screen was erased at {at} outside a synchronised block \
             (last open {open}, last close {since:?}): {}",
            show(seen)
        );
        let close = closes
            .iter()
            .copied()
            .find(|c| *c > at)
            .unwrap_or(seen.len());
        assert!(
            contains(&seen[open..close], session::DREW),
            "the screen was erased at {at} in a block that drew nothing back: {}",
            show(&seen[open..close])
        );
    }

    for taken in [ERASE_ROW, DELETE_IMAGE] {
        for at in offsets(seen, taken) {
            let open = opens.iter().copied().rfind(|o| *o < at);
            let close = closes.iter().copied().rfind(|c| *c < at);
            // `None < Some(_)`, so something with an open marker before it and no close
            // since is inside the block.
            assert!(
                open.is_some() && close < open,
                "{} at {at} is not inside a synchronised block \
                 (last open {open:?}, last close {close:?}): {}",
                show(taken),
                show(seen)
            );
        }
    }
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

/// The cells a stream of escapes would leave on screen.
///
/// The chrome is diffed frame to frame, so a message that replaces another is written only
/// where the two differ: "scaling: scaled" over "scaling: scaled up" reaches the terminal as
/// a few cells and a cursor move, and searching the stream for the whole phrase finds
/// nothing. What is on screen is the claim worth making, so this reconstructs it.
///
/// Only what the chrome uses: absolute cursor positioning, printable text, and erase-to-end
/// of line. Everything else -- graphics commands, SGR, mode changes -- is skipped, which is
/// exactly right: none of it puts a glyph in a cell.
pub struct Screen {
    rows: Vec<Vec<char>>,
}

impl Screen {
    /// Replay `out` onto a grid large enough for any terminal a test opens, so a resize
    /// mid-stream does not need to be tracked to read what is on screen.
    pub fn of(out: &[u8]) -> Self {
        Self::replay(out, 512, 256)
    }

    /// Replay `out` and return what it left on a `cols` by `rows` screen.
    pub fn replay(out: &[u8], cols: usize, rows: usize) -> Self {
        let mut screen = Self {
            rows: vec![vec![' '; cols]; rows],
        };
        let (mut row, mut col) = (0usize, 0usize);
        let text = String::from_utf8_lossy(out).into_owned();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                if c.is_control() {
                    continue;
                }
                if let Some(line) = screen.rows.get_mut(row)
                    && let Some(cell) = line.get_mut(col)
                {
                    *cell = c;
                }
                col += 1;
                continue;
            }
            // An escape: collect it, then act on the ones that move or blank cells.
            let Some(kind) = chars.next() else { break };
            if kind == '_' || kind == 'P' || kind == ']' {
                // A device-control or graphics string, terminated by ST.
                let mut last = '\0';
                for c in chars.by_ref() {
                    if last == '\x1b' && c == '\\' {
                        break;
                    }
                    last = c;
                }
                continue;
            }
            if kind != '[' {
                continue;
            }
            let mut params = String::new();
            let mut final_byte = '\0';
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    final_byte = c;
                    break;
                }
                params.push(c);
            }
            let numbers: Vec<usize> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
            match final_byte {
                'H' => {
                    row = numbers.first().copied().unwrap_or(1).saturating_sub(1);
                    col = numbers.get(1).copied().unwrap_or(1).saturating_sub(1);
                }
                'K' => {
                    // 2 blanks the whole line, 0 or absent from the cursor on.
                    let from = if numbers.first() == Some(&2) { 0 } else { col };
                    if let Some(line) = screen.rows.get_mut(row) {
                        for cell in &mut line[from.min(cols)..] {
                            *cell = ' ';
                        }
                    }
                }
                'J' => {
                    for line in &mut screen.rows {
                        line.fill(' ');
                    }
                }
                _ => {}
            }
        }
        screen
    }

    /// One row, trailing blanks trimmed.
    pub fn row(&self, row: usize) -> String {
        self.rows
            .get(row)
            .map(|line| line.iter().collect::<String>().trim_end().to_string())
            .unwrap_or_default()
    }

    /// Is `text` on any row?
    pub fn contains(&self, text: &str) -> bool {
        (0..self.rows.len()).any(|row| self.row(row).contains(text))
    }
}
