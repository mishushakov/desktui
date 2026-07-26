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
//! Override the target with `DESKTUI_TEST_SERVER` and `DESKTUI_TEST_PASSWORD`.

mod common;

use std::time::Duration;

// The geometry and the shared claims live in `common::session`, because each of the
// tests below has a counterpart in one of the fake-server suites and the two are only
// worth having if they check the same thing.
use common::session::*;
use common::*;

fn server() -> String {
    std::env::var("DESKTUI_TEST_SERVER").unwrap_or_else(|_| "localhost::5901".to_string())
}

fn password() -> String {
    std::env::var("DESKTUI_TEST_PASSWORD").unwrap_or_else(|_| "desktui".to_string())
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

/// Fake-server counterpart: `resize::negotiates_the_terminals_exact_pixel_size`.
#[test]
#[ignore = "needs the desktop container: make desktop"]
fn tigervnc_grants_the_terminals_exact_pixel_size() {
    // The headline claim, against a server that really implements it.
    let mut term = start(&["--scale", "native"]);

    assert_reports_size(&term, EXPECTED_SIZE, Duration::from_secs(30));
    assert_pixel_exact(&term, Duration::from_secs(10));
    assert_drew(&term, Duration::from_secs(5));

    quit(&mut term);
    let status = term.wait(Duration::from_secs(15)).expect("did not exit");
    assert!(status.success(), "exited with {status:?}");
}

/// No fake-server counterpart, and that is the point: nothing in the fake suites
/// exercises a real Tight encoder, so this is the only test that runs the JPEG decoder
/// and the palette filters against bytes we did not produce ourselves.
#[test]
#[ignore = "needs the desktop container: make desktop"]
fn a_real_desktop_decodes_and_keeps_drawing() {
    // Tight is first in the encoding list, so this is the path that exercises the
    // JPEG decoder and the palette filters against a real encoder.
    let mut term = start(&[]);
    assert_drew(&term, Duration::from_secs(30));

    // Move the pointer across the desktop: the server has to send us the cursor
    // moving, which is the simplest reliable source of continuing damage.
    let before = tiles_drawn(&term);
    for x in (100..900).step_by(40) {
        term.send(format!("\x1b[<35;{x};300M").as_bytes());
        std::thread::sleep(Duration::from_millis(40));
    }
    std::thread::sleep(Duration::from_millis(500));
    assert_kept_drawing(&term, before, "moving the pointer");

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

/// Fake-server counterpart: `resize::asks_for_the_new_size_when_the_terminal_is_resized`.
#[test]
#[ignore = "needs the desktop container: make desktop"]
fn resizing_the_terminal_reshapes_the_real_desktop() {
    let mut term = start(&["--scale", "native"]);
    assert_reports_size(&term, EXPECTED_SIZE, Duration::from_secs(30));

    // Half the window. 100x25 cells of 8x17 leaves 24 usable rows: 800x408.
    term.resize(100, 25, 800, 425);
    assert_reports_size(&term, "800x408", Duration::from_secs(30));
    assert_pixel_exact(&term, Duration::from_secs(10));

    quit(&mut term);
    term.wait(Duration::from_secs(15));
}

/// Fake-server counterpart: `lifecycle::a_wrong_address_fails_before_touching_the_screen`
/// -- the same claim about a failure one step later, during authentication rather than
/// during connect.
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

/// Related: `resize::a_forwarded_resize_is_picked_up_when_it_lands` covers the other way
/// a size change arrives without this client having caused it. That one is the deferred
/// answer to our own request; this is another client's, which no fake server produces.
#[test]
#[ignore = "needs the desktop container: make desktop"]
fn one_client_adopts_a_resize_another_client_asked_for() {
    // The reason=2 path -- "another client requested this" -- which the unit tests
    // cover but nothing had exercised against a real server. Two clients attach at
    // once, which also proves shared sessions work.
    //
    // The sizes are chosen so only one of them ever asks: the watcher is in `fit`
    // mode and never requests anything, so any change it sees came from the other.
    let mut watcher = FakeTerm::spawn_with_env(
        120,
        30,
        960,
        510,
        &[server().as_str(), "--scale", "fit", "--fps", "20"],
        &[("VNC_PASSWORD", &password())],
    );
    watcher.answer_probe(GHOSTTY_REPLIES);
    assert!(
        watcher.wait_for(b"\x1b_Ga=T", Duration::from_secs(30)),
        "the watcher never drew anything: {}",
        tail(&watcher.output())
    );

    let mut resizer = start(&["--scale", "native"]);
    assert!(
        resizer.wait_for(EXPECTED_SIZE.as_bytes(), Duration::from_secs(30)),
        "the second client never got its size: {}",
        tail(&resizer.output())
    );

    // The watcher asked for nothing, so seeing the new size means it took the
    // change from the server rather than causing it.
    assert!(
        watcher.wait_for(EXPECTED_SIZE.as_bytes(), Duration::from_secs(20)),
        "the watcher never noticed the other client's resize: {}",
        tail(&watcher.output())
    );
    // And it kept drawing afterwards rather than wedging on the size change.
    let before = count(&watcher.output(), b"\x1b_Ga=T");
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        count(&watcher.output(), b"\x1b_Ga=T") > before,
        "the watcher stopped drawing after the resize"
    );

    quit(&mut resizer);
    quit(&mut watcher);
    resizer.wait(Duration::from_secs(15));
    watcher.wait(Duration::from_secs(15));
}

/// Fake-server counterpart: `input::a_pasted_selection_is_announced_first_and_sent_when_asked`,
/// which follows the same exchange through to the data against a server that answers
/// exactly as written. What only a real server can settle is the part below: that
/// TigerVNC recognises the pseudo-encoding, answers with capabilities, and accepts what
/// we send it under a negative length.
#[test]
#[ignore = "needs the desktop container: make desktop"]
fn a_real_server_negotiates_the_extended_clipboard() {
    // Cyrillic, which does not exist in Latin-1. Without the extension the client would
    // have had to substitute it and would say so; the plain note means the negotiation
    // succeeded, since the UTF-8 path is only taken once the server's `caps` message has
    // arrived.
    let mut term = start(&["--scale", "fit"]);
    assert_drew(&term, Duration::from_secs(30));

    let mark = term.output().len();
    term.send("\x1b[200~Привет, мир\x1b[201~".as_bytes());
    assert!(
        term.wait_for(b"pasted to the remote clipboard", Duration::from_secs(10)),
        "the paste was never acted on: {}",
        tail(&term.output())
    );
    assert!(
        !contains(&term.output()[mark..], b"not Latin-1"),
        "fell back to Latin-1, so the extension was not negotiated: {}",
        tail(&term.output())
    );

    // And the server was happy with it. A malformed extended message is a protocol
    // error to TigerVNC, which drops the connection -- so still drawing a second later
    // is the assertion that our message was well formed.
    let before = tiles_drawn(&term);
    std::thread::sleep(Duration::from_secs(1));
    assert_kept_drawing(&term, before, "announcing the clipboard");

    quit(&mut term);
    let status = term.wait(Duration::from_secs(15)).expect("did not exit");
    assert!(status.success(), "exited with {status:?}");
}

/// No fake-server counterpart: the fake server does not model "only send what changed",
/// so the black-after-resize bug this covers cannot be reproduced against it.
#[test]
#[ignore = "needs the desktop container: make desktop"]
fn the_desktop_is_redrawn_in_full_after_a_resize() {
    // The second half of the resize glitch: the framebuffer was cleared to black on
    // a size change, and the server only sends what it thinks changed -- so whatever
    // it considered unchanged stayed black. Answering the resize with a
    // non-incremental request is forbidden, so the overlap has to be kept instead.
    let mut term = start(&["--scale", "native"]);
    assert!(
        term.wait_for(EXPECTED_SIZE.as_bytes(), Duration::from_secs(30)),
        "never reached the first size: {}",
        tail(&term.output())
    );
    // Let the first screenful settle.
    std::thread::sleep(Duration::from_millis(1500));

    let before = count(&term.output(), b"\x1b_Ga=T");
    term.resize(100, 25, 800, 425);
    assert!(
        term.wait_for(b"800x408", Duration::from_secs(30)),
        "the desktop did not follow: {}",
        tail(&term.output())
    );
    std::thread::sleep(Duration::from_millis(1500));
    let after = count(&term.output(), b"\x1b_Ga=T");

    // A full redraw at 800x408 is 7x4 = 28 tiles of 128x136. Comfortably more than a
    // handful, which is what a partial repaint would produce.
    let drawn = after - before;
    assert!(
        drawn >= 20,
        "expected the whole screen to be redrawn after the resize, saw {drawn} tiles"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(15));
}

/// Fake-server counterpart:
/// `updates::continuous_updates_are_enabled_and_stop_the_request_traffic`.
#[test]
#[ignore = "needs the desktop container: make desktop"]
fn a_real_server_negotiates_continuous_updates() {
    // TigerVNC implements the extension, so this is the one test that proves the
    // negotiation works against something that was not written to agree with us.
    let mut term = start(&["--scale", "native", "--log-file", "/tmp/desktui-live.log"]);
    assert!(
        term.wait_for(b"pushing frames", Duration::from_secs(30)),
        "the server never enabled continuous updates: {}",
        tail(&term.output())
    );

    // And it keeps drawing on pushed frames alone.
    let before = count(&term.output(), b"\x1b_Ga=T");
    for x in (100..600).step_by(50) {
        term.send(format!("\x1b[<35;{x};300M").as_bytes());
        std::thread::sleep(Duration::from_millis(40));
    }
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        count(&term.output(), b"\x1b_Ga=T") > before,
        "no frames arrived once requests stopped"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(15));
}

/// Fake-server counterpart: `input::a_disagreeing_caps_lock_is_corrected_before_the_keystroke`
/// asserts the correction. This asserts its precondition -- that a real server sends the
/// LED state at all -- which the fake one is simply configured to do.
#[test]
#[ignore = "needs the desktop container: make desktop"]
fn a_real_server_reports_its_lock_key_state() {
    // Without this the caps-lock correction has nothing to compare against, so it is
    // worth knowing that a real server actually sends it rather than only our fake one.
    // The state arrives as a debug event, so the log is where it can be observed.
    let log = std::env::temp_dir().join("desktui-live-led.log");
    let _ = std::fs::remove_file(&log);

    let addr = server();
    let mut term = FakeTerm::spawn_with_env(
        COLS,
        ROWS,
        PIXELS.0,
        PIXELS.1,
        &[
            addr.as_str(),
            "--fps",
            "15",
            "--log-file",
            log.to_str().unwrap(),
        ],
        &[("VNC_PASSWORD", &password()), ("DESKTUI_LOG", "debug")],
    );
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(30)));

    let start = std::time::Instant::now();
    let mut logged = String::new();
    while start.elapsed() < Duration::from_secs(15) {
        logged = std::fs::read_to_string(&log).unwrap_or_default();
        if logged.contains("remote lock keys") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        logged.contains("remote lock keys"),
        "the server never reported its lock keys; the correction would have nothing to \
         compare against. Log:\n{logged}"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(15));
    let _ = std::fs::remove_file(&log);
}

/// Fake-server counterpart: `updates::the_cursor_shape_is_requested_and_drawn_locally`.
#[test]
#[ignore = "needs the desktop container: make desktop"]
fn a_real_server_sends_a_cursor_shape() {
    // TigerVNC only sends the shape once the encoding is requested, so this proves the
    // pointer is genuinely ours to draw rather than baked into the framebuffer.
    let log = std::env::temp_dir().join("desktui-live-cursor.log");
    let _ = std::fs::remove_file(&log);

    let addr = server();
    let mut term = FakeTerm::spawn_with_env(
        COLS,
        ROWS,
        PIXELS.0,
        PIXELS.1,
        &[
            addr.as_str(),
            "--fps",
            "15",
            "--log-file",
            log.to_str().unwrap(),
        ],
        &[("VNC_PASSWORD", &password()), ("DESKTUI_LOG", "debug")],
    );
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(30)));

    // Nudge the pointer: some servers only send the shape once it is over the desktop.
    for x in (200..500).step_by(50) {
        term.send(format!("\x1b[<35;{x};200M").as_bytes());
        std::thread::sleep(Duration::from_millis(50));
    }

    let start = std::time::Instant::now();
    let mut logged = String::new();
    while start.elapsed() < Duration::from_secs(15) {
        logged = std::fs::read_to_string(&log).unwrap_or_default();
        if logged.contains("cursor shape") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        logged.contains("cursor shape"),
        "the server never sent a cursor shape, so there is nothing to draw locally. \
         Log:\n{logged}"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(15));
    let _ = std::fs::remove_file(&log);
}

/// Fake-server counterpart:
/// `updates::growing_the_desktop_re_enables_continuous_updates_for_the_new_area`.
#[test]
#[ignore = "needs the desktop container: make desktop"]
fn growing_the_window_fills_the_new_area_on_a_real_server() {
    // Growing is the direction that used to fail: the server keeps pushing whatever
    // rectangle it was last told about, so the area beyond it never arrived and stayed
    // black. Shrinking hid the bug, because the old rectangle still covered everything.
    let log = std::env::temp_dir().join("desktui-live-grow.log");
    let _ = std::fs::remove_file(&log);

    let addr = server();
    // Start small: 100x25 cells of 8x17 leaves 24 usable rows, so 800x408.
    let mut term = FakeTerm::spawn_with_env(
        100,
        25,
        800,
        425,
        &[
            addr.as_str(),
            "--fps",
            "15",
            "--scale",
            "native",
            "--log-file",
            log.to_str().unwrap(),
        ],
        &[("VNC_PASSWORD", &password()), ("DESKTUI_LOG", "debug")],
    );
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(
        term.wait_for(b"800x408", Duration::from_secs(30)),
        "never reached the first size: {}",
        tail(&term.output())
    );

    term.resize(200, 50, 1600, 850);
    assert!(
        term.wait_for(b"1600x832", Duration::from_secs(30)),
        "the desktop did not grow: {}",
        tail(&term.output())
    );

    // The proof is in the traffic: the pushed region has to be widened to the new size,
    // or the bottom and right of the screen would never be sent.
    let start = std::time::Instant::now();
    let mut logged = String::new();
    while start.elapsed() < Duration::from_secs(15) {
        logged = std::fs::read_to_string(&log).unwrap_or_default();
        if logged.contains("continuous updates enabled for 1600x832") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        logged.contains("continuous updates enabled for 1600x832"),
        "the pushed region was never widened after the desktop grew. Log:\n{logged}"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(15));
    let _ = std::fs::remove_file(&log);
}

/// Fake-server counterpart:
/// `updates::the_round_trip_is_measured_with_a_fence_once_frames_are_pushed`.
#[test]
#[ignore = "needs the desktop container: make desktop"]
fn a_real_server_answers_the_latency_probe() {
    // The number in the status line is only worth showing if a real server bounces the
    // fence back. TigerVNC sends fences of its own constantly, so it clearly supports
    // them -- this checks it answers ours.
    let log = std::env::temp_dir().join("desktui-live-rtt.log");
    let _ = std::fs::remove_file(&log);

    let addr = server();
    let mut term = FakeTerm::spawn_with_env(
        COLS,
        ROWS,
        PIXELS.0,
        PIXELS.1,
        &[
            addr.as_str(),
            "--fps",
            "15",
            "--log-file",
            log.to_str().unwrap(),
        ],
        &[("VNC_PASSWORD", &password()), ("DESKTUI_LOG", "debug")],
    );
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(30)));

    // A measured round trip appears in the status line as a number of milliseconds.
    // Before the fix it was there too, but climbing; now it can only appear at all if a
    // fence came back.
    //
    // Matched on the figure alone rather than on the binding that used to follow it: the
    // status line gained per-span colours, so an escape sequence sits between the two and
    // no contiguous needle can span them. `ms` with the trailing pad is enough on its
    // own -- an unmeasured round trip is drawn as dashes, so the unit only appears when
    // there is a number in front of it.
    let start = std::time::Instant::now();
    let mut seen = false;
    while start.elapsed() < Duration::from_secs(20) {
        let out = term.output();
        if contains(&out, b"ms  ") {
            seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        seen,
        "no round trip was ever measured: {}",
        tail(&term.output())
    );

    // And it must not be a figure that only grows. Sample it twice, far apart.
    let sample = |out: &[u8]| -> Option<u128> {
        let text = String::from_utf8_lossy(out).into_owned();
        text.rmatch_indices("ms  ").next().and_then(|(i, _)| {
            let head = &text[..i];
            let digits: String = head
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            digits.chars().rev().collect::<String>().parse().ok()
        })
    };
    let first = sample(&term.output());
    std::thread::sleep(Duration::from_secs(3));
    let second = sample(&term.output());
    if let (Some(a), Some(b)) = (first, second) {
        assert!(
            b < a + 2000,
            "the round trip is climbing rather than being measured: {a}ms then {b}ms"
        );
    }

    quit(&mut term);
    term.wait(Duration::from_secs(15));
    let _ = std::fs::remove_file(&log);
}

/// Fake-server counterpart: `resize::a_resize_goes_out_while_the_server_is_idle`.
#[test]
#[ignore = "needs the desktop container: make desktop"]
fn an_idle_resize_completes_without_any_input() {
    // Reported symptom: a resize sometimes only takes effect once the mouse is moved.
    // Nothing but the render tick should be needed, so this resizes and then touches
    // nothing at all, timing how long the new size takes to appear.
    let log = std::env::temp_dir().join("desktui-idle-resize.log");
    let _ = std::fs::remove_file(&log);

    let addr = server();
    let mut term = FakeTerm::spawn_with_env(
        100,
        25,
        800,
        425,
        &[
            addr.as_str(),
            "--fps",
            "20",
            "--scale",
            "native",
            "--log-file",
            log.to_str().unwrap(),
        ],
        &[("VNC_PASSWORD", &password()), ("DESKTUI_LOG", "debug")],
    );
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(
        term.wait_for(b"800x408", Duration::from_secs(30)),
        "never reached the first size: {}",
        tail(&term.output())
    );
    // Let everything settle, so the desktop is genuinely idle.
    std::thread::sleep(Duration::from_secs(2));

    let at = std::time::Instant::now();
    term.resize(200, 50, 1600, 850);
    let arrived = term.wait_for(b"1600x832", Duration::from_secs(20));
    let took = at.elapsed();

    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        arrived,
        "the resize never completed with no input to prod it. Log:\n{logged}"
    );
    // The debounce is 250ms, so anything past a couple of seconds means it was waiting
    // for something rather than driving itself.
    assert!(
        took < Duration::from_secs(3),
        "the resize took {took:?}, which means something had to wake it. Log:\n{logged}"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(15));
    let _ = std::fs::remove_file(&log);
}

/// Fake-server counterpart: `resize::a_drag_is_still_coalesced_into_a_few_requests`.
#[test]
#[ignore = "needs the desktop container: make desktop"]
fn a_dragged_resize_settles_without_any_input() {
    // Closer to how a window is actually resized: dozens of size changes in a second as
    // the edge is dragged, then nothing. The last one has to settle on its own.
    let log = std::env::temp_dir().join("desktui-drag-resize.log");
    let _ = std::fs::remove_file(&log);

    let addr = server();
    let mut term = FakeTerm::spawn_with_env(
        100,
        25,
        800,
        425,
        &[
            addr.as_str(),
            "--fps",
            "20",
            "--scale",
            "native",
            "--log-file",
            log.to_str().unwrap(),
        ],
        &[("VNC_PASSWORD", &password()), ("DESKTUI_LOG", "debug")],
    );
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(term.wait_for(b"800x408", Duration::from_secs(30)));
    std::thread::sleep(Duration::from_secs(2));

    // Drag: grow a few cells at a time, as a window manager would report it.
    for step in 1..=20u16 {
        let cols = 100 + step * 5;
        let rows = 25 + step;
        term.resize(cols, rows, cols * 8, rows * 17);
        std::thread::sleep(Duration::from_millis(40));
    }

    // Released. From here on nothing is touched at all.
    let at = std::time::Instant::now();
    let (cols, rows) = (200u16, 45u16);
    let expected = format!("{}x{}", cols as u32 * 8, (rows as u32 - 1) * 17);
    let arrived = term.wait_for(expected.as_bytes(), Duration::from_secs(20));
    let took = at.elapsed();

    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    let applied = logged.matches("applying resize").count();
    assert!(
        arrived,
        "the dragged resize never settled on {expected} with no input. Applied {applied} \
         times. Log tail:\n{}",
        logged.lines().rev().take(25).collect::<Vec<_>>().join("\n")
    );
    assert!(
        took < Duration::from_secs(4),
        "settling took {took:?} after the drag stopped, so something had to wake it"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(15));
    let _ = std::fs::remove_file(&log);
}
