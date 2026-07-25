//! End-to-end against a real VNC server.
//!
//! The other suites use a fake server, which proves the client speaks the protocol
//! as written but says nothing about how a real one behaves. These run the actual
//! binary, in a pty that answers like Ghostty, against TigerVNC serving a real XFCE
//! desktop -- so real Tight encoding, real JPEG rectangles, and a real answer to a
//! resize request.
//!
//! Ignored by default because they need the container:
//!
//! ```text
//! make desktop
//! cargo test --test live -- --ignored --nocapture
//! ```
//!
//! Override the target with `VNCTUI_TEST_SERVER` and `VNCTUI_TEST_PASSWORD`.

mod common;

use std::time::Duration;

use common::*;

/// A 200x50 terminal of 8x17 cells: 1600x850 pixels, 49 rows usable, so the client
/// should ask TigerVNC for 1600x832 and be given it.
const COLS: u16 = 200;
const ROWS: u16 = 50;
const PIXELS: (u16, u16) = (1600, 850);
const EXPECTED_SIZE: &str = "1600x832";

fn server() -> String {
    std::env::var("VNCTUI_TEST_SERVER").unwrap_or_else(|_| "localhost::5901".to_string())
}

fn password() -> String {
    std::env::var("VNCTUI_TEST_PASSWORD").unwrap_or_else(|_| "vnctui".to_string())
}

fn start(extra: &[&str]) -> FakeTerm {
    let addr = server();
    let mut args = vec![addr.as_str(), "--fps", "20"];
    args.extend_from_slice(extra);
    let mut term = FakeTerm::spawn_with_env(
        COLS,
        ROWS,
        PIXELS.0,
        PIXELS.1,
        &args,
        &[("VNC_PASSWORD", &password())],
    );
    term.answer_probe(GHOSTTY_REPLIES);
    term
}

fn quit(term: &mut FakeTerm) {
    term.send(&[0x01]);
    std::thread::sleep(Duration::from_millis(50));
    term.send(b"q");
}

#[test]
#[ignore = "needs the desktop container: make desktop"]
fn tigervnc_grants_the_terminals_exact_pixel_size() {
    // The headline claim, against a server that really implements it.
    let mut term = start(&["--scale", "native"]);

    assert!(
        term.wait_for(EXPECTED_SIZE.as_bytes(), Duration::from_secs(30)),
        "the server never reported {EXPECTED_SIZE}: {}",
        tail(&term.output())
    );
    assert!(
        term.wait_for(b"native 1:1", Duration::from_secs(10)),
        "the mapping never became pixel-exact: {}",
        tail(&term.output())
    );
    assert!(
        contains(&term.output(), b"\x1b_Ga=T"),
        "nothing was drawn: {}",
        tail(&term.output())
    );

    quit(&mut term);
    let status = term.wait(Duration::from_secs(15)).expect("did not exit");
    assert!(status.success(), "exited with {status:?}");
}

#[test]
#[ignore = "needs the desktop container: make desktop"]
fn a_real_desktop_decodes_and_keeps_drawing() {
    // Tight is first in the encoding list, so this is the path that exercises the
    // JPEG decoder and the palette filters against a real encoder.
    let mut term = start(&[]);
    assert!(
        term.wait_for(b"\x1b_Ga=T", Duration::from_secs(30)),
        "nothing was drawn: {}",
        tail(&term.output())
    );

    // Move the pointer across the desktop: the server has to send us the cursor
    // moving, which is the simplest reliable source of continuing damage.
    let before = count(&term.output(), b"\x1b_Ga=T");
    for x in (100..900).step_by(40) {
        term.send(format!("\x1b[<35;{x};300M").as_bytes());
        std::thread::sleep(Duration::from_millis(40));
    }
    std::thread::sleep(Duration::from_millis(500));
    let after = count(&term.output(), b"\x1b_Ga=T");

    assert!(
        after > before,
        "the screen never changed after moving the pointer ({before} -> {after} tiles)"
    );
    // And no decoder gave up along the way.
    let out = term.output();
    for bad in [
        &b"cannot be decoded"[..],
        b"undecodable",
        b"never requested",
        b"colour map",
    ] {
        assert!(
            !contains(&out, bad),
            "decoder trouble: {}",
            String::from_utf8_lossy(bad)
        );
    }

    quit(&mut term);
    term.wait(Duration::from_secs(15));
}

#[test]
#[ignore = "needs the desktop container: make desktop"]
fn resizing_the_terminal_reshapes_the_real_desktop() {
    let mut term = start(&["--scale", "native"]);
    assert!(
        term.wait_for(EXPECTED_SIZE.as_bytes(), Duration::from_secs(30)),
        "never reached the first size: {}",
        tail(&term.output())
    );

    // Half the window. 100x25 cells of 8x17 leaves 24 usable rows: 800x408.
    term.resize(100, 25, 800, 425);
    assert!(
        term.wait_for(b"800x408", Duration::from_secs(30)),
        "the desktop did not follow the terminal: {}",
        tail(&term.output())
    );
    assert!(
        term.wait_for(b"native 1:1", Duration::from_secs(10)),
        "still not pixel-exact at the new size: {}",
        tail(&term.output())
    );

    quit(&mut term);
    term.wait(Duration::from_secs(15));
}

#[test]
#[ignore = "needs the desktop container: make desktop"]
fn a_wrong_password_fails_clearly() {
    let addr = server();
    let mut term = FakeTerm::spawn_with_env(
        COLS,
        ROWS,
        PIXELS.0,
        PIXELS.1,
        &[addr.as_str()],
        &[("VNC_PASSWORD", "definitelywrong")],
    );
    term.answer_probe(GHOSTTY_REPLIES);

    let status = term.wait(Duration::from_secs(30)).expect("did not exit");
    assert!(!status.success(), "a bad password should fail");
    let out = term.output();
    assert!(
        contains(&out, b"assword") || contains(&out, b"uthentication"),
        "the failure should mention the password: {}",
        tail(&out)
    );
    // The server complaining that *it* has no password configured looks identical
    // from a distance and is what a broken container produced, so rule it out --
    // otherwise this test passes whenever anything at all is wrong.
    assert!(
        !contains(&out, b"No password configured"),
        "the server has no password set, so this proves nothing about ours: {}",
        tail(&out)
    );
    assert!(
        !contains(&out, b"\x1b[?1049h"),
        "must not enter the alternate screen when the connection failed"
    );
}

/// The readable part of the output, for assertion messages: escape-heavy tails are
/// unreadable, and the status line is what actually says what happened.
fn tail(buf: &[u8]) -> String {
    let text = String::from_utf8_lossy(buf);
    let mut lines: Vec<&str> = text
        .split('\x1b')
        .filter(|s| s.contains("vnctui") || s.contains("1:1") || s.contains("error"))
        .collect();
    lines.dedup();
    let n = lines.len();
    lines.drain(..n.saturating_sub(4));
    lines.join(" | ")
}
