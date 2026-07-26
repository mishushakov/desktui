//! The session every suite starts from, and the claims more than one of them makes.
//!
//! Three suites reason about the same window -- a 200x50 terminal of 8x17 cells -- so
//! the geometry is written down once here rather than copied into each.
//!
//! The assertions at the bottom are the ones a fake-server test shares with its live
//! counterpart. A pair exists to check one behaviour against two servers, so the claim
//! itself must not be written twice: two copies drift, and the live half is `#[ignore]`d,
//! so nothing would say when it had.

use std::time::{Duration, Instant};

use super::server::{Extensions, FakeServer, Request, Resize};
use super::{FakeTerm, GHOSTTY_REPLIES, Screen, contains, count, tail};

/// A 200x50 terminal of 8x17 cells: 1600x850 pixels, of which 49 rows are usable.
///
/// That leaves an image area of 1600x833, and the client rounds its request down
/// to even numbers, so it should ask for 1600x832.
pub const COLS: u16 = 200;
pub const ROWS: u16 = 50;
pub const PIXELS: (u16, u16) = (1600, 850);
pub const EXPECTED_REQUEST: (u16, u16) = (1600, 832);

/// `EXPECTED_REQUEST` as the status line writes it. A `const` cannot be formatted from
/// another, so these two are kept in step by hand.
pub const EXPECTED_SIZE: &str = "1600x832";

/// The escape that carries one tile to the terminal. Counting these counts drawing.
pub const DREW: &[u8] = b"\x1b_Ga=T";

/// The image ids the client's overlays take: `kitty::IMAGE_ID_BASE + 0x40000` for the
/// menu's backdrop, and one each above it for the bar under the pointer and the
/// notification popup. Worked out here the way the client works them out, because a
/// test reads the numbers off the wire rather than the constants behind them -- which
/// means this has to move when the client's base does, as it did when tiles started
/// taking ids by position and needed the room below the overlays.
pub const MENU_ID: u32 = 0x7600 + 0x40000;
pub const MENU_HIGHLIGHT_ID: u32 = MENU_ID + 1;
pub const TOAST_ID: u32 = MENU_HIGHLIGHT_ID + 1;

/// The escape that takes one of those off the screen.
///
/// The id is the point, and `a=d,d=I` on its own will not do: a delete by id also goes
/// out on every frame where the pointer is on no row of the menu, to drop the highlight
/// bar, so the bare form is satisfied by the menu being *up* rather than by its having
/// gone. Deleting is also the only way an overlay leaves: its glyphs outlive any repaint
/// of the image below them, and its backdrop outranks every tile.
pub fn deleted(id: u32) -> Vec<u8> {
    format!("\x1b_Ga=d,d=I,i={id}").into_bytes()
}

/// A fake server of the given size, and a client pointed at it in native mapping.
pub fn start(resize: Resize, remote: (u16, u16)) -> (FakeServer, FakeTerm) {
    let server = FakeServer::start(remote.0, remote.1, resize);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(
        COLS,
        ROWS,
        PIXELS.0,
        PIXELS.1,
        &[&addr, "--fps", "15", "--scale", "native"],
    );
    term.answer_probe(GHOSTTY_REPLIES);
    (server, term)
}

/// The same, with extensions enabled on the server and extra arguments to the client.
pub fn start_with(ext: Extensions, remote: (u16, u16), extra: &[&str]) -> (FakeServer, FakeTerm) {
    let server = FakeServer::start_with(remote.0, remote.1, Resize::Accept, ext);
    let addr = server.addr.to_string();
    let mut args = vec![addr.as_str(), "--fps", "15"];
    args.extend_from_slice(extra);
    let mut term = FakeTerm::spawn(COLS, ROWS, PIXELS.0, PIXELS.1, &args);
    term.answer_probe(GHOSTTY_REPLIES);
    (server, term)
}

/// Quit through the prefix: Ctrl+A then q.
pub fn quit(term: &mut FakeTerm) {
    term.send(&[0x01]);
    std::thread::sleep(Duration::from_millis(50));
    term.send(b"q");
}

/// Paste `text` as a terminal does, in brackets, and keep saying it until the client acts
/// on it -- `acted` naming the request that would say so.
///
/// A paste can be lost on the way in, and the loss is nothing to do with the clipboard.
/// The bytes are `\x1b[200~text\x1b[201~`, and if a read happens to end after that first
/// `\x1b` alone, the terminal library delivers it as the *Escape key*: the byte is both
/// the key and the start of every sequence, so it has to guess, and with no more input in
/// hand it guesses key. The rest then arrives as ordinary characters and the paste is
/// gone -- which is how `a_pasted_selection_is_announced_first_and_sent_when_asked` failed
/// on a macOS runner, where a frame crosses the pty in several reads. Saying it again is
/// all that is needed, and it costs a run nothing when the first one landed.
///
/// The ambiguity itself belongs to the client, which cannot tell the two apart either.
/// Worth remembering when a paste is reported lost in real use.
/// Answers with the request that satisfied `acted`, that being the thing a caller wants to
/// look inside and there being no sense in waiting for it twice.
#[track_caller]
pub fn paste(
    term: &mut FakeTerm,
    text: &str,
    server: &FakeServer,
    acted: impl Fn(&Request) -> bool,
) -> Request {
    for attempt in 1..=3 {
        term.send(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        if let Some(request) = server.wait_for(Duration::from_secs(2), &acted) {
            return request;
        }
        eprintln!("the paste was not read as one; saying it again (attempt {attempt})");
    }
    panic!(
        "three pastes reached the client and none of them was acted on: {}",
        tail(&term.output())
    );
}

// ----------------------------------------------------------------- shared claims
//
// Each of these is asserted by a fake-server test and by its live counterpart. The
// timeout is a parameter because a real server takes longer to answer than a fake one,
// and pretending otherwise would either slow every fake failure down or make the live
// suite flaky.

/// The status line reports `size` as the desktop it is showing.
///
/// Read off the screen rather than the stream. The chrome is diffed frame to frame, so a
/// size that replaces another reaches the terminal as the digits that differ and a cursor
/// move -- "1600x832" over "1024x768" shares its fifth character, and the phrase is nowhere
/// in the bytes even though it is on screen.
#[track_caller]
pub fn assert_reports_size(term: &FakeTerm, size: &str, timeout: Duration) {
    assert!(
        wait_for_text(term, size, timeout),
        "the status line never reported {size}: {}",
        Screen::of(&term.output()).row(49)
    );
}

/// Wait for `text` to be on screen, wherever it is.
pub fn wait_for_text(term: &FakeTerm, text: &str, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if Screen::of(&term.output()).contains(text) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Screen::of(&term.output()).contains(text)
}

/// Every remote pixel lands on exactly one terminal pixel.
#[track_caller]
pub fn assert_pixel_exact(term: &FakeTerm, timeout: Duration) {
    assert!(
        wait_for_text(term, "native 1:1", timeout),
        "the mapping never became pixel-exact: {}",
        Screen::of(&term.output()).row(49)
    );
}

/// Something reached the screen.
#[track_caller]
pub fn assert_drew(term: &FakeTerm, timeout: Duration) {
    assert!(
        term.wait_for(DREW, timeout),
        "nothing was drawn: {}",
        tail(&term.output())
    );
}

/// Nothing reached the screen -- for the failures that must not touch it at all.
#[track_caller]
pub fn assert_drew_nothing(term: &FakeTerm) {
    assert!(
        !contains(&term.output(), DREW),
        "should not have drawn anything: {}",
        tail(&term.output())
    );
}

/// Tiles transmitted so far, to be compared against a later count.
pub fn tiles_drawn(term: &FakeTerm) -> usize {
    count(&term.output(), DREW)
}

/// More tiles arrived than `before`, so the picture is still moving. `after` names what
/// was supposed to have caused it, because "nothing was drawn" on its own never says
/// which prod failed.
///
/// Waits, like the claims above it. It used to compare on the spot, which left every
/// caller to sleep first and made each of those sleeps a deadline for the client to
/// answer within -- a fixed window that a loaded machine misses without anything being
/// wrong with what it drew.
#[track_caller]
pub fn assert_kept_drawing(term: &FakeTerm, before: usize, after: &str, timeout: Duration) {
    let began = Instant::now();
    loop {
        let now = tiles_drawn(term);
        if now > before {
            return;
        }
        assert!(
            began.elapsed() < timeout,
            "the screen never changed after {after} ({before} -> {now} tiles): {}",
            tail(&term.output())
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
