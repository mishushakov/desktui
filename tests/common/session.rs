//! The session every suite starts from, and the claims more than one of them makes.
//!
//! Three suites reason about the same window -- a 200x50 terminal of 8x17 cells -- so
//! the geometry is written down once here rather than copied into each.
//!
//! The assertions at the bottom are the ones a fake-server test shares with its live
//! counterpart. A pair exists to check one behaviour against two servers, so the claim
//! itself must not be written twice: two copies drift, and the live half is `#[ignore]`d,
//! so nothing would say when it had.

use std::time::Duration;

use super::server::{Extensions, FakeServer, Resize};
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
#[track_caller]
pub fn assert_kept_drawing(term: &FakeTerm, before: usize, after: &str) {
    let now = tiles_drawn(term);
    assert!(
        now > before,
        "the screen never changed after {after} ({before} -> {now} tiles): {}",
        tail(&term.output())
    );
}
