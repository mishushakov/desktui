//! End-to-end tests of a real session: the binary in a fake terminal, talking to
//! a fake VNC server.
//!
//! These are the tests that cover the headline behaviour -- asking the server to
//! make its desktop exactly the size of the terminal's pixel area, and coping
//! with each way that can be answered.

mod common;

use std::time::Duration;

use common::server::{FakeServer, Request, Resize};
use common::*;

/// A 200x50 terminal of 8x17 cells: 1600x850 pixels, of which 49 rows are usable.
///
/// That leaves an image area of 1600x833, and the client rounds its request down
/// to even numbers, so it should ask for 1600x832.
const COLS: u16 = 200;
const ROWS: u16 = 50;
const PIXELS: (u16, u16) = (1600, 850);
const EXPECTED_REQUEST: (u16, u16) = (1600, 832);

fn start(resize: Resize, remote: (u16, u16)) -> (FakeServer, FakeTerm) {
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

/// Quit through the prefix: Ctrl+A then q.
fn quit(term: &mut FakeTerm) {
    term.send(&[0x01]);
    std::thread::sleep(Duration::from_millis(50));
    term.send(b"q");
}

#[test]
fn negotiates_the_terminals_exact_pixel_size() {
    let (server, mut term) = start(Resize::Accept, (1024, 768));

    let request = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::SetDesktopSize { .. })
        })
        .expect("client never asked the server to resize");

    match request {
        Request::SetDesktopSize {
            width,
            height,
            screen_ids,
        } => {
            assert_eq!(
                (width, height),
                EXPECTED_REQUEST,
                "should ask for the image area rounded down to even numbers"
            );
            assert_eq!(
                screen_ids,
                vec![0x2a],
                "the server's own screen id has to be carried over"
            );
        }
        other => panic!("unexpected request {other:?}"),
    }

    // Once accepted, every remote pixel lands on one terminal pixel.
    assert!(
        term.wait_for(b"native 1:1", Duration::from_secs(10)),
        "status line never reported a pixel-exact mapping: {}",
        show(&term.output())
    );
    assert!(
        term.wait_for(b"1600x832", Duration::from_secs(5)),
        "status line does not show the negotiated size: {}",
        show(&term.output())
    );
    assert!(
        contains(&term.output(), b"\x1b_Ga=T"),
        "nothing was drawn"
    );

    quit(&mut term);
    let status = term.wait(Duration::from_secs(10)).expect("did not exit");
    assert!(status.success(), "exited with {status:?}");
}

#[test]
fn asks_for_the_new_size_when_the_terminal_is_resized() {
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    assert!(term.wait_for(b"native 1:1", Duration::from_secs(10)));

    // Halve the window. 100x25 cells of 8x17 leaves 24 usable rows: 800x408.
    term.resize(100, 25, 800, 425);

    let second = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(
                r,
                Request::SetDesktopSize {
                    width: 800,
                    height: 408,
                    ..
                }
            )
        })
        .expect("the client did not renegotiate after the terminal resized");
    assert!(matches!(second, Request::SetDesktopSize { .. }));

    assert!(
        term.wait_for(b"800x408", Duration::from_secs(10)),
        "status line does not show the new size: {}",
        show(&term.output())
    );

    // And it must not have spammed the server: one request per settled size.
    let count = server
        .requests()
        .iter()
        .filter(|r| matches!(r, Request::SetDesktopSize { .. }))
        .count();
    assert!(
        count <= 3,
        "expected the resize to be debounced, saw {count} requests"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_refused_resize_falls_back_and_says_why() {
    let (server, mut term) = start(Resize::Refuse, (1024, 768));

    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::SetDesktopSize { .. }
            ))
            .is_some(),
        "client never tried"
    );

    // 1024x768 fits inside 1600x833, so a refusal leaves it 1:1 but cropped
    // nowhere -- and the reason has to reach the user.
    assert!(
        term.wait_for(b"prohibited", Duration::from_secs(10)),
        "the refusal was not explained in the status line: {}",
        show(&term.output())
    );
    let output = term.output();
    assert!(
        contains(&output, b"1024x768"),
        "should keep showing the server's actual size: {}",
        show(&output)
    );
    assert!(
        !contains(&output, b"0x0"),
        "must not adopt the undefined dimensions of a refusal: {}",
        show(&output)
    );
    assert!(contains(&output, b"\x1b_Ga=T"), "nothing was drawn");

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_forwarded_resize_is_picked_up_when_it_lands() {
    // QEMU hands the request to the guest and answers "forwarded"; success turns
    // up later, and the client has to notice.
    let (server, mut term) = start(Resize::Forward, (1024, 768));

    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::SetDesktopSize { .. }
            ))
            .is_some(),
        "client never tried"
    );
    assert!(
        term.wait_for(b"1600x832", Duration::from_secs(10)),
        "the deferred success was never adopted: {}",
        show(&term.output())
    );
    assert!(
        term.wait_for(b"native 1:1", Duration::from_secs(5)),
        "should be pixel-exact once the resize landed: {}",
        show(&term.output())
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_server_that_cannot_resize_is_scaled_instead() {
    // No layout rectangle ever arrives, so the client has to conclude that
    // resizing is unavailable and map what it was given.
    let (_server, mut term) = start(Resize::Unsupported, (1920, 1080));

    assert!(
        term.wait_for(b"cannot resize", Duration::from_secs(10)),
        "the fallback was not explained: {}",
        show(&term.output())
    );
    let output = term.output();
    // 1920x1080 does not fit in 1600x833, so it has to be scaled down.
    assert!(
        contains(&output, b"scaled") || contains(&output, b"1:1 cropped"),
        "expected a fallback mapping: {}",
        show(&output)
    );
    assert!(contains(&output, b"1920x1080"), "{}", show(&output));
    assert!(contains(&output, b"\x1b_Ga=T"), "nothing was drawn");

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn keeps_at_most_one_update_request_in_flight() {
    // A client that fires requests on a timer piles them up on the server. Pacing
    // off the end of each update means there is never more than one outstanding,
    // and the watchdog only re-asks about once a second.
    let (server, mut term) = start(Resize::Accept, (640, 480));
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)));

    let before = server
        .requests()
        .iter()
        .filter(|r| matches!(r, Request::FramebufferUpdate { .. }))
        .count();
    std::thread::sleep(Duration::from_secs(2));
    let after = server
        .requests()
        .iter()
        .filter(|r| matches!(r, Request::FramebufferUpdate { .. }))
        .count();

    // The fake server never answers incremental requests, so only the watchdog
    // should fire: a handful over two seconds, not sixty.
    let per_second = (after - before) as f64 / 2.0;
    assert!(
        per_second <= 4.0,
        "expected at most one request in flight plus a watchdog, saw \
         {per_second:.1} requests per second"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn requests_the_encodings_it_can_actually_decode() {
    let (server, mut term) = start(Resize::Accept, (640, 480));
    let request = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::SetEncodings(_))
        })
        .expect("no encodings were negotiated");

    match request {
        Request::SetEncodings(encodings) => {
            assert!(encodings.contains(&7), "Tight missing: {encodings:?}");
            assert!(encodings.contains(&16), "ZRLE missing: {encodings:?}");
            assert!(encodings.contains(&1), "CopyRect missing: {encodings:?}");
            assert!(encodings.contains(&0), "Raw is mandatory: {encodings:?}");
            assert!(
                encodings.contains(&-308),
                "ExtendedDesktopSize missing: {encodings:?}"
            );
            assert!(
                !encodings.contains(&15),
                "TRLE must not be advertised: its decoder was dropped, so a server \
                 taking us up on it would desynchronise the stream"
            );
            // Advertising an encoding we cannot decode would leave the stream
            // unrecoverable, since rectangles carry no length.
            for encoding in &encodings {
                assert!(
                    [0, 1, 7, 16, -223, -224, -308].contains(encoding),
                    "advertised {encoding} with no decoder for it"
                );
            }
        }
        other => panic!("unexpected request {other:?}"),
    }

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn input_reaches_the_server_with_pixel_exact_coordinates() {
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    assert!(term.wait_for(b"native 1:1", Duration::from_secs(10)));

    // SGR-pixel mouse report: button 0 pressed at pixel 137,229. In native
    // mapping that is exactly where it should land on the remote desktop.
    term.send(b"\x1b[<0;137;229M");
    let pointer = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::Pointer { buttons: 1, .. })
        })
        .expect("the click never arrived");
    match pointer {
        Request::Pointer { x, y, buttons } => {
            // Reports are one-based, so the pixel is 136,228.
            assert_eq!((x, y), (136, 228), "pointer landed on the wrong pixel");
            assert_eq!(buttons, 1);
        }
        other => panic!("unexpected request {other:?}"),
    }
    term.send(b"\x1b[<0;137;229m");

    // The terminal answered the keyboard-protocol probe, so it owes us a release
    // for every press and the client must not invent one.
    term.send(b"\x1b[120u");
    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::Key {
                    keysym: 0x78,
                    down: true
                }
            ))
            .is_some(),
        "the keystroke never arrived"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !server.requests().iter().any(|r| matches!(
            r,
            Request::Key {
                keysym: 0x78,
                down: false
            }
        )),
        "a terminal that reports releases must not have them synthesised too"
    );

    // Now the release the terminal owes us.
    term.send(b"\x1b[120;1:3u");
    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::Key {
                    keysym: 0x78,
                    down: false
                }
            ))
            .is_some(),
        "the release was not forwarded"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_terminal_without_key_releases_gets_them_synthesised() {
    // Otherwise the remote would hold every key down for ever.
    let server = FakeServer::start(1024, 768, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(COLS, ROWS, PIXELS.0, PIXELS.1, &[&addr, "--fps", "10"]);
    term.answer_probe(NO_KEYBOARD_REPLIES);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(15)));

    term.send(b"x");
    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::Key {
                    keysym: 0x78,
                    down: true
                }
            ))
            .is_some(),
        "the keystroke never arrived"
    );
    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::Key {
                    keysym: 0x78,
                    down: false
                }
            ))
            .is_some(),
        "no release followed the press"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn view_only_sends_nothing() {
    let server = FakeServer::start(800, 600, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(
        COLS,
        ROWS,
        PIXELS.0,
        PIXELS.1,
        &[&addr, "--view-only", "--scale", "fit", "--fps", "10"],
    );
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(term.wait_for(b"view-only", Duration::from_secs(10)));

    term.send(b"\x1b[<0;100;100M");
    term.send(b"hello");
    std::thread::sleep(Duration::from_millis(500));

    let sent_input = server.requests().iter().any(|r| {
        matches!(
            r,
            Request::Key { .. } | Request::Pointer { .. } | Request::CutText(_)
        )
    });
    assert!(!sent_input, "view-only forwarded input: {:?}", server.requests());

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_pasted_selection_goes_to_the_remote_clipboard() {
    let (server, mut term) = start(Resize::Accept, (800, 600));
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)));

    // Bracketed paste, as a terminal delivers it.
    term.send(b"\x1b[200~hello there\x1b[201~");
    let cut = server
        .wait_for(Duration::from_secs(10), |r| matches!(r, Request::CutText(_)))
        .expect("the paste never reached the server");
    match cut {
        Request::CutText(text) => assert_eq!(text, "hello there"),
        other => panic!("unexpected request {other:?}"),
    }

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn the_server_going_away_is_reported_and_the_terminal_restored() {
    let server = FakeServer::start(640, 480, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(COLS, ROWS, PIXELS.0, PIXELS.1, &[&addr, "--fps", "10"]);
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)));

    // Drop the server; the client should notice and leave cleanly.
    drop(server);

    let status = term
        .wait(Duration::from_secs(15))
        .expect("client did not exit after the server vanished");
    assert!(!status.success(), "a dropped session is a failure");

    let output = term.output();
    assert!(
        contains(&output, b"\x1b[?1049l"),
        "the alternate screen was not left: {}",
        show(&output)
    );
    assert!(
        contains(&output, b"\x1b_Ga=d,d=A"),
        "images were not released"
    );
    assert!(
        contains(&output, b"session ended") || contains(&output, b"connection failed"),
        "the reason was not reported: {}",
        show(&output)
    );
}

#[test]
fn a_wrong_address_fails_before_touching_the_screen() {
    // Port 1 on loopback refuses immediately.
    let mut term = FakeTerm::spawn(COLS, ROWS, PIXELS.0, PIXELS.1, &["127.0.0.1::1"]);
    term.answer_probe(GHOSTTY_REPLIES);

    let status = term.wait(Duration::from_secs(15)).expect("did not exit");
    assert!(!status.success());
    let output = term.output();
    assert!(
        contains(&output, b"connecting to 127.0.0.1:1"),
        "the failure should name the address: {}",
        show(&output)
    );
    assert!(
        !contains(&output, b"\x1b_Ga=T"),
        "nothing should have been drawn"
    );
}

#[test]
fn reconnects_after_the_server_drops_the_session() {
    // The server accepts one connection and then goes away; with --reconnect the
    // client has to come back rather than exiting.
    let server = FakeServer::start(800, 600, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(
        COLS,
        ROWS,
        PIXELS.0,
        PIXELS.1,
        &[&addr, "--reconnect", "--fps", "10"],
    );
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(15)));

    drop(server);
    assert!(
        term.wait_for(b"reconnect", Duration::from_secs(15)),
        "the client neither reported nor attempted a reconnect: {}",
        show(&term.output())
    );

    // It must still be running, retrying, rather than having exited.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        term.wait(Duration::from_millis(200)).is_none(),
        "should still be retrying"
    );
}

#[test]
fn without_reconnect_a_dropped_session_exits() {
    let server = FakeServer::start(800, 600, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(COLS, ROWS, PIXELS.0, PIXELS.1, &[&addr, "--fps", "10"]);
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(15)));

    drop(server);
    let status = term
        .wait(Duration::from_secs(15))
        .expect("client should have exited");
    assert!(!status.success());
}

#[test]
fn view_only_does_not_reshape_the_remote_desktop() {
    // The desktop is shared with whoever else is connected, so a session that
    // promised not to interact must not resize it. noVNC takes the same position.
    let server = FakeServer::start(1024, 768, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(
        COLS,
        ROWS,
        PIXELS.0,
        PIXELS.1,
        &[&addr, "--view-only", "--scale", "native", "--fps", "10"],
    );
    term.answer_probe(GHOSTTY_REPLIES);

    assert!(
        term.wait_for(b"view-only", Duration::from_secs(15)),
        "{}",
        show(&term.output())
    );
    // It has to say why it is not doing the thing it was asked to do.
    assert!(
        term.wait_for(b"not resizing", Duration::from_secs(10)),
        "the suppressed resize was not explained: {}",
        show(&term.output())
    );

    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::SetDesktopSize { .. })),
        "view-only asked the server to resize: {:?}",
        server.requests()
    );
    // And it still draws, at the size the server chose.
    assert!(contains(&term.output(), b"1024x768"), "{}", show(&term.output()));

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_server_that_snaps_to_its_own_size_does_not_cause_a_request_loop() {
    // Real servers accept a resize and then round to a mode they actually have.
    // Re-asking whenever the granted size differs from the wanted one would loop
    // for ever, so a repeat may only follow a change in what *we* want.
    let server = FakeServer::start(1024, 768, Resize::Snap);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(
        COLS,
        ROWS,
        PIXELS.0,
        PIXELS.1,
        &[&addr, "--scale", "native", "--fps", "10"],
    );
    term.answer_probe(GHOSTTY_REPLIES);

    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::SetDesktopSize { .. }
            ))
            .is_some(),
        "client never asked"
    );
    std::thread::sleep(Duration::from_secs(2));

    let asks = server
        .requests()
        .iter()
        .filter(|r| matches!(r, Request::SetDesktopSize { .. }))
        .count();
    assert!(
        asks <= 2,
        "expected the client to stop asking once the server had answered, saw {asks} requests"
    );
    // The size it settled on is the server's, not the one it asked for.
    assert!(
        term.wait_for(b"1600x800", Duration::from_secs(5)),
        "should show the size the server actually granted: {}",
        show(&term.output())
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}
