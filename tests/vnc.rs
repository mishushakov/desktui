//! End-to-end tests of a real session: the binary in a fake terminal, talking to
//! a fake VNC server.
//!
//! These are the tests that cover the headline behaviour -- asking the server to
//! make its desktop exactly the size of the terminal's pixel area, and coping
//! with each way that can be answered.

mod common;

use std::time::Duration;

use common::server::{Extensions, FakeServer, Request, Resize};
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
    assert!(contains(&term.output(), b"\x1b_Ga=T"), "nothing was drawn");

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
            // The extensions asked for by number.
            assert!(
                encodings.contains(&-313),
                "ContinuousUpdates missing: {encodings:?}"
            );
            assert!(encodings.contains(&-312), "Fence missing: {encodings:?}");
            assert!(
                encodings.contains(&-261),
                "LED state missing: {encodings:?}"
            );
            assert!(
                encodings.contains(&-239),
                "the cursor shape is what makes a local pointer possible: {encodings:?}"
            );

            // Anything advertised has to be something we can consume. For a data
            // encoding that means a decoder, because rectangles carry no length and a
            // surprise would leave the stream unrecoverable. Fence and
            // ContinuousUpdates are different in kind: they are answered with
            // messages, never rectangles.
            const DECODABLE_RECTS: [i32; 9] = [0, 1, 7, 16, -223, -224, -239, -261, -308];
            const NEGOTIATION_ONLY: [i32; 2] = [-312, -313];
            for encoding in &encodings {
                assert!(
                    DECODABLE_RECTS.contains(encoding) || NEGOTIATION_ONLY.contains(encoding),
                    "advertised {encoding} with nothing to handle it"
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
fn the_help_overlay_goes_away_on_the_next_key() {
    // The bug this replaces: only the prefix command toggled the overlay, so the
    // "any other key dismisses this" it advertises was untrue and the box stayed
    // on screen for the rest of the session.
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    assert!(term.wait_for(b"native 1:1", Duration::from_secs(10)));

    // Ctrl+A then ? raises it.
    term.send(&[0x01]);
    std::thread::sleep(Duration::from_millis(50));
    term.send(b"?");
    assert!(
        term.wait_for(b"Renegotiate the remote size", Duration::from_secs(10)),
        "the overlay never appeared"
    );

    // While it is up it is redrawn every frame, so a tail of the output that no
    // longer mentions it is the overlay being gone rather than merely not resent.
    let mark = term.output().len();
    term.send(b"\x1b[120u");
    std::thread::sleep(Duration::from_millis(500));
    let tail = term.output()[mark..].to_vec();
    assert!(
        !contains(&tail, b"Renegotiate the remote size"),
        "the overlay was still being drawn after a dismissing key"
    );

    // Stopping the redraw is not the same as taking it off the screen. The glyphs
    // outlive any repaint of the image below them, and the backdrop outranks every
    // tile, so both have to be taken off explicitly -- and only the teardown deletes
    // an image by id, which makes it the thing to look for.
    assert!(
        contains(&tail, b"a=d,d=I,i="),
        "the overlay was never cleared, so it is still on screen"
    );

    // The key that dismisses is swallowed rather than passed on: the overlay
    // said it dismisses, not that it types.
    assert!(
        !server.requests().iter().any(|r| matches!(
            r,
            Request::Key {
                keysym: 0x78,
                down: true
            }
        )),
        "the dismissing key was forwarded to the server as well"
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
    assert!(
        !sent_input,
        "view-only forwarded input: {:?}",
        server.requests()
    );

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
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::CutText(_))
        })
        .expect("the paste never reached the server");
    match cut {
        Request::CutText(text) => assert_eq!(text, "hello there"),
        other => panic!("unexpected request {other:?}"),
    }

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_paste_outside_latin1_is_substituted_not_silently_shortened() {
    // RFB clipboard traffic is Latin-1 only. Dropping the rest moves every character
    // after it; a question mark keeps the text the same shape and is visibly a
    // substitution, which is what noVNC does too.
    let (server, mut term) = start(Resize::Accept, (800, 600));
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)));

    term.send("\x1b[200~caf\u{e9} \u{2615} tea\x1b[201~".as_bytes());
    let cut = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::CutText(_))
        })
        .expect("the paste never reached the server");
    match cut {
        Request::CutText(text) => {
            // The e-acute is Latin-1 and survives; the coffee cup is not and becomes
            // '?', leaving the length and the spaces where they were.
            assert_eq!(text, "caf\u{e9} ? tea", "got {text:?}");
        }
        other => panic!("unexpected request {other:?}"),
    }
    // And the user is told, rather than left wondering what happened to it.
    assert!(
        term.wait_for(b"not Latin-1", Duration::from_secs(5)),
        "the substitution was not reported: {}",
        show(&term.output())
    );

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
    assert!(
        contains(&term.output(), b"1024x768"),
        "{}",
        show(&term.output())
    );

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

/// Start a server with extensions enabled, and a client pointed at it.
fn start_with(ext: Extensions, remote: (u16, u16), extra: &[&str]) -> (FakeServer, FakeTerm) {
    let server = FakeServer::start_with(remote.0, remote.1, Resize::Accept, ext);
    let addr = server.addr.to_string();
    let mut args = vec![addr.as_str(), "--fps", "15"];
    args.extend_from_slice(extra);
    let mut term = FakeTerm::spawn(COLS, ROWS, PIXELS.0, PIXELS.1, &args);
    term.answer_probe(GHOSTTY_REPLIES);
    (server, term)
}

#[test]
fn continuous_updates_are_enabled_and_stop_the_request_traffic() {
    // The server pushing frames saves a round trip each time. Once it is on, asking as
    // well would defeat the point, so the requests have to stop.
    let ext = Extensions {
        continuous_updates: true,
        ..Extensions::default()
    };
    let (server, mut term) = start_with(ext, (800, 600), &["--scale", "fit"]);

    let enabled = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::EnableContinuousUpdates { enable: true, .. })
        })
        .expect("continuous updates were never enabled");
    match enabled {
        Request::EnableContinuousUpdates { width, height, .. } => {
            assert_eq!(
                (width, height),
                (800, 600),
                "should cover the whole framebuffer"
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(
        term.wait_for(b"continuous updates", Duration::from_secs(10)),
        "the user was not told: {}",
        show(&term.output())
    );

    // From here on, no more update requests: the count must stop moving.
    let requests_before = server
        .requests()
        .iter()
        .filter(|r| matches!(r, Request::FramebufferUpdate { .. }))
        .count();
    std::thread::sleep(Duration::from_secs(2));
    let requests_after = server
        .requests()
        .iter()
        .filter(|r| matches!(r, Request::FramebufferUpdate { .. }))
        .count();
    assert_eq!(
        requests_before, requests_after,
        "kept asking for updates after the server started pushing them"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_server_fence_is_echoed_with_the_request_bit_cleared() {
    // A fence is a synchronisation point: the server asks, and the answer has to come
    // back with the request bit cleared and any flag we do not implement removed.
    let ext = Extensions {
        fence: true,
        ..Extensions::default()
    };
    let (server, mut term) = start_with(ext, (800, 600), &["--scale", "fit"]);

    let echoed = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::Fence { .. })
        })
        .expect("the fence was never answered");
    match echoed {
        Request::Fence { flags, payload } => {
            assert_eq!(
                flags & (1 << 31),
                0,
                "the request bit must be cleared in a response"
            );
            assert_eq!(flags, 0b11, "BlockBefore and BlockAfter should survive");
            assert_eq!(payload, b"hail", "the payload comes back unchanged");
        }
        other => panic!("unexpected {other:?}"),
    }

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_disagreeing_caps_lock_is_corrected_before_the_keystroke() {
    // The server says its caps lock is on; the terminal reports a key pressed with it
    // off. Sent as-is the letter would arrive in the wrong case, so a caps lock tap has
    // to go first.
    let ext = Extensions {
        led_caps_on: true,
        ..Extensions::default()
    };
    let (server, mut term) = start_with(ext, (800, 600), &["--scale", "fit"]);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)));

    // A Kitty-protocol press of 'x' with no lock modifiers: caps lock is off locally.
    term.send(b"\x1b[120u");

    let keys = || -> Vec<(u32, bool)> {
        server
            .requests()
            .iter()
            .filter_map(|r| match r {
                Request::Key { keysym, down } => Some((*keysym, *down)),
                _ => None,
            })
            .collect()
    };
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if keys().iter().any(|(k, _)| *k == 0x78) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let sent = keys();
    let caps = 0xffe5u32;
    let caps_at = sent.iter().position(|(k, down)| *k == caps && *down);
    let x_at = sent.iter().position(|(k, down)| *k == 0x78 && *down);
    assert!(
        caps_at.is_some(),
        "no caps lock correction was sent: {sent:x?}"
    );
    assert!(
        caps_at < x_at,
        "the correction has to precede the keystroke: {sent:x?}"
    );
    // And it is a tap, not a key left down.
    assert!(
        sent.iter().any(|(k, down)| *k == caps && !*down),
        "caps lock was pressed and never released: {sent:x?}"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_matching_caps_lock_is_left_alone() {
    // Nothing to fix, so nothing should be sent: a spurious tap would turn the remote
    // caps lock on and make every later keystroke wrong.
    let (server, mut term) = start_with(Extensions::default(), (800, 600), &["--scale", "fit"]);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)));

    term.send(b"\x1b[120u");
    std::thread::sleep(Duration::from_millis(600));

    let touched_caps = server
        .requests()
        .iter()
        .any(|r| matches!(r, Request::Key { keysym: 0xffe5, .. }));
    assert!(
        !touched_caps,
        "sent a lock-key correction with no LED state to justify it: {:?}",
        server.requests()
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn the_cursor_shape_is_requested_and_drawn_locally() {
    // Asking for the shape is what stops the server compositing the pointer, so the
    // pointer can then move at local speed rather than at a round trip's.
    let ext = Extensions {
        cursor: true,
        ..Extensions::default()
    };
    let (server, mut term) = start_with(ext, (800, 600), &["--scale", "fit"]);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)));

    match server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::SetEncodings(_))
        })
        .expect("no encodings negotiated")
    {
        Request::SetEncodings(encodings) => assert!(
            encodings.contains(&-239),
            "the cursor encoding was not requested: {encodings:?}"
        ),
        other => panic!("unexpected {other:?}"),
    }

    // Moving the pointer has to produce frames without the server sending anything:
    // the overlay is drawn on this side.
    let before = count(&term.output(), b"\x1b_Ga=T");
    for x in (200..500).step_by(30) {
        term.send(format!("\x1b[<35;{x};200M").as_bytes());
        std::thread::sleep(Duration::from_millis(30));
    }
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        count(&term.output(), b"\x1b_Ga=T") > before,
        "moving the pointer redrew nothing, so the cursor is not being composited"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn view_only_leaves_the_cursor_to_the_server() {
    // With no local pointer worth drawing, the server should keep compositing its own
    // -- otherwise a view-only session shows no pointer at all.
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

    match server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::SetEncodings(_))
        })
        .expect("no encodings negotiated")
    {
        Request::SetEncodings(encodings) => assert!(
            !encodings.contains(&-239),
            "a view-only session should not take the cursor off the server: {encodings:?}"
        ),
        other => panic!("unexpected {other:?}"),
    }

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn growing_the_desktop_re_enables_continuous_updates_for_the_new_area() {
    // The pushed region is remembered by the server, so a desktop that grows needs the
    // request repeating. Without that, everything outside the old rectangle is never
    // sent and stays black -- and only *growing* shows it, because a smaller rectangle
    // still fits inside the one the server already had.
    let ext = Extensions {
        continuous_updates: true,
        ..Extensions::default()
    };
    let server = FakeServer::start_with(1024, 768, Resize::Accept, ext);
    let addr = server.addr.to_string();
    // Start small: 100x25 cells of 8x17 leaves 24 usable rows, so 800x408.
    let mut term = FakeTerm::spawn(
        100,
        25,
        800,
        425,
        &[&addr, "--fps", "15", "--scale", "native"],
    );
    term.answer_probe(GHOSTTY_REPLIES);

    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::EnableContinuousUpdates {
                    enable: true,
                    width: 800,
                    height: 408,
                }
            ))
            .is_some(),
        "never enabled for the first size: {:?}",
        server.requests()
    );

    // Now grow it.
    term.resize(200, 50, 1600, 850);

    assert!(
        server
            .wait_for(Duration::from_secs(15), |r| matches!(
                r,
                Request::EnableContinuousUpdates {
                    enable: true,
                    width: 1600,
                    height: 832,
                }
            ))
            .is_some(),
        "the pushed region was never widened, so the new area would stay black: {:?}",
        server.requests()
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn the_round_trip_is_measured_with_a_fence_once_frames_are_pushed() {
    // A pushed frame answers nothing, so there is no request to time against. The
    // status line used to show the age of the last request ever sent, which climbed for
    // ever. A fence gives a real figure, because the server bounces it straight back.
    let ext = Extensions {
        continuous_updates: true,
        fence: true,
        ..Extensions::default()
    };
    let (server, mut term) = start_with(ext, (800, 600), &["--scale", "fit"]);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)));

    // Our own probe is a fence carrying a marker, distinct from the echo of the
    // server's own fence.
    let probe = server
        .wait_for(Duration::from_secs(10), |r| match r {
            Request::Fence { payload, .. } => payload == b"desktui-rtt",
            _ => false,
        })
        .expect("no latency probe was sent");
    match probe {
        Request::Fence { flags, .. } => {
            assert_eq!(
                flags & (1 << 31),
                1 << 31,
                "a probe has to set the request bit or the server will not answer it"
            );
        }
        other => panic!("unexpected {other:?}"),
    }

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn no_fence_support_means_no_round_trip_figure_rather_than_a_wrong_one() {
    // Continuous updates without fences: nothing can be measured, so the status line
    // has to admit it instead of showing a number that grows.
    let ext = Extensions {
        continuous_updates: true,
        fence: false,
        ..Extensions::default()
    };
    let (server, mut term) = start_with(ext, (800, 600), &["--scale", "fit"]);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)));
    std::thread::sleep(Duration::from_secs(2));

    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::Fence { .. })),
        "sent a fence to a server that never offered them"
    );
    let out = term.output();
    assert!(
        contains(&out, b"--"),
        "the status line should say the round trip is unknown: {}",
        show(&out)
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_resize_goes_out_while_the_server_is_idle() {
    // Nothing but the render tick drives a debounced resize, so it has to complete
    // without help. With frames pushed rather than requested there is no traffic at all
    // from an idle desktop, and a loop that only makes progress when something arrives
    // would sit there until the user happened to move the mouse.
    let ext = Extensions {
        continuous_updates: true,
        ..Extensions::default()
    };
    let server = FakeServer::start_with(1024, 768, Resize::Accept, ext);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(
        100,
        25,
        800,
        425,
        &[&addr, "--fps", "20", "--scale", "native"],
    );
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(
        term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)),
        "never drew anything: {}",
        show(&term.output())
    );

    // Let the session settle so the server has stopped saying anything.
    std::thread::sleep(Duration::from_secs(1));
    let before = server.requests().len();

    // Resize, then touch nothing whatsoever.
    term.resize(200, 50, 1600, 850);

    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::SetDesktopSize {
                    width: 1600,
                    height: 832,
                    ..
                }
            ))
            .is_some(),
        "the resize never went out without input to prod it; requests since the resize: \
         {:?}",
        &server.requests()[before.min(server.requests().len())..]
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_single_resize_is_acted_on_at_once() {
    // A lone resize should not sit out the debounce: the delay is there to coalesce a
    // drag, and making a single one wait made the window look like it had not taken.
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::SetDesktopSize { .. }
            ))
            .is_some(),
        "no initial negotiation"
    );
    std::thread::sleep(Duration::from_millis(600));

    let at = std::time::Instant::now();
    term.resize(100, 25, 800, 425);
    assert!(
        server
            .wait_for(Duration::from_secs(5), |r| matches!(
                r,
                Request::SetDesktopSize {
                    width: 800,
                    height: 408,
                    ..
                }
            ))
            .is_some(),
        "the resize never went out: {:?}",
        server.requests()
    );
    let took = at.elapsed();
    assert!(
        took < Duration::from_millis(250),
        "a single resize waited {took:?} for the debounce it does not need"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_drag_is_still_coalesced_into_a_few_requests() {
    // The flip side: acting at once must not turn a drag into one request per frame.
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::SetDesktopSize { .. }
            ))
            .is_some(),
        "no initial negotiation"
    );
    let before = server
        .requests()
        .iter()
        .filter(|r| matches!(r, Request::SetDesktopSize { .. }))
        .count();

    // Twenty size changes in under a second, as dragging an edge produces.
    for step in 1..=20u16 {
        let cols = 100 + step;
        term.resize(cols, 25, cols * 8, 425);
        std::thread::sleep(Duration::from_millis(30));
    }
    std::thread::sleep(Duration::from_secs(1));

    let asks = server
        .requests()
        .iter()
        .filter(|r| matches!(r, Request::SetDesktopSize { .. }))
        .count()
        - before;
    assert!(
        asks <= 6,
        "a drag should coalesce into a handful of requests, not one per step: saw {asks}"
    );
    // And the last size still has to be the one it settles on.
    assert!(
        term.wait_for(b"960x408", Duration::from_secs(10)),
        "did not settle on the final size: {}",
        show(&term.output())
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}
