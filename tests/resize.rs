//! Negotiating the size of the remote desktop, against a fake server.
//!
//! The headline behaviour: asking the server to make its desktop exactly the size of
//! the terminal's pixel area, and coping with each way that can be answered --
//! accepted, refused, forwarded to a guest, unsupported, or accepted and then rounded
//! to a mode the server actually has.
//!
//! The other half of it is *when* the request goes out: at once for a lone resize,
//! coalesced for a drag, and without waiting for the server to say anything first.

mod common;

use std::time::Duration;

use common::server::{Extensions, FakeServer, Request, Resize};
use common::session::*;
use common::*;

/// The client's `RESIZE_DEBOUNCE`: how long a stream of resizes is held back for, and
/// how long one that has just been applied holds the next off. Written down rather than
/// imported, the client being a binary, so a change there has to be echoed here -- and
/// `a_single_resize_is_acted_on_at_once` is what would notice if it were not, having a
/// quarter of a second to beat.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(250);

/// Live counterpart: `live::tigervnc_grants_the_terminals_exact_pixel_size`.
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
    assert_pixel_exact(&term, Duration::from_secs(10));
    assert_reports_size(&term, EXPECTED_SIZE, Duration::from_secs(5));
    assert_drew(&term, Duration::from_secs(5));

    quit(&mut term);
    let status = term.wait(Duration::from_secs(10)).expect("did not exit");
    assert!(status.success(), "exited with {status:?}");
}

/// Live counterpart: `live::resizing_the_terminal_reshapes_the_real_desktop`.
#[test]
fn asks_for_the_new_size_when_the_terminal_is_resized() {
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    assert_pixel_exact(&term, Duration::from_secs(10));

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

    assert_reports_size(&term, "800x408", Duration::from_secs(10));

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
        "the refusal was not explained in a notification: {}",
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
    assert_drew(&term, Duration::from_secs(5));

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

/// Related: `live::one_client_adopts_a_resize_another_client_asked_for` covers the
/// other way a size change arrives without this client having caused it.
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
    assert_reports_size(&term, EXPECTED_SIZE, Duration::from_secs(10));
    assert_pixel_exact(&term, Duration::from_secs(5));

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
    assert_drew(&term, Duration::from_secs(5));

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn view_only_still_asks_for_the_terminals_size() {
    // View-only used to decline the resize, on the grounds that the desktop is
    // shared. The protocol never says how many clients are attached, so that traded
    // a pixel-exact picture for a courtesy to a second client that is usually not
    // there. Not sending input and not reshaping the desktop are separate things.
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
    let request = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::SetDesktopSize { .. })
        })
        .expect("view-only never asked the server to resize");
    match request {
        Request::SetDesktopSize { width, height, .. } => {
            assert_eq!(
                (width, height),
                EXPECTED_REQUEST,
                "asked for the wrong size"
            );
        }
        other => panic!("unexpected request {other:?}"),
    }

    // Still no input, which is the part view-only is actually about.
    term.send(b"\x1b[120u");
    term.send(b"\x1b[<0;137;229M");
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::Key { .. } | Request::Pointer { .. })),
        "view-only sent input: {:?}",
        server.requests()
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
    assert_reports_size(&term, "1600x800", Duration::from_secs(5));

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

/// Live counterpart: `live::an_idle_resize_completes_without_any_input`.
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
    assert_drew(&term, Duration::from_secs(10));

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

/// Live counterpart: `live::a_dragged_resize_settles_without_any_input`.
#[test]
fn a_drag_is_still_coalesced_into_a_few_requests() {
    // The flip side: acting at once must not turn a drag into one request per frame.
    //
    // What the client promises is one resize applied per debounce window and no more:
    // applying one shuts the leading edge for a debounce, and the trailing edge waits for
    // the newest resize to be a debounce old, so two applications can never fall inside
    // one window. That makes the ceiling a function of how long the drag lasted, which is
    // why the drag is timed and the number worked out from it. A fixed count instead --
    // twenty steps, at most six requests -- is a claim about the machine as much as the
    // client: stretch the steps past the debounce and each one settles on its own,
    // failing a client that did exactly what it promised.
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

    // Resize at about the rate a dragged window edge does, for long enough to cross
    // several windows however slowly the steps come out.
    let began = std::time::Instant::now();
    let mut steps = 0u16;
    while began.elapsed() < Duration::from_millis(600) {
        steps += 1;
        let cols = 100 + steps;
        term.resize(cols, 25, cols * 8, 425);
        std::thread::sleep(Duration::from_millis(16));
    }
    let drag = began.elapsed();

    // The last size is the one it has to settle on, and its arriving is also how we know
    // the whole drag has been answered. 25 rows of 17 pixels, less the status row, is 408.
    let settled = format!("{}x408", (100 + steps) * 8);
    assert_reports_size(&term, &settled, Duration::from_secs(10));

    // Every request the drag could produce went out inside here: the first resize opens
    // it, and the last one to be applied is the one whose answer has just been reported --
    // a resize applied after that asks for nothing, the desktop already being the size it
    // wants. Measured to the answer rather than to the last ioctl, because the client sees
    // a resize when it gets round to the signal, which on a busy machine is later than the
    // sending of it; a ceiling worked out from the drag alone would be short by that much.
    let window = began.elapsed();

    let asks = server
        .requests()
        .iter()
        .filter(|r| matches!(r, Request::SetDesktopSize { .. }))
        .count()
        - before;

    // Applications fall a debounce apart at the closest -- applying one shuts the leading
    // edge for that long, and the trailing edge waits for the newest resize to be that old
    // -- so no two of them fit inside one window, and a request takes an application. A
    // request can go out later than the application that earned it, the reply to the one
    // in flight being where a skipped ask is picked up, but that moves a request rather
    // than adding one.
    //
    // Plus one, and not because the arithmetic needs it: the client really does use its
    // whole allowance, and the difference this test is here to see is the one between a
    // handful of requests and one per step, not between three and four. A ceiling with no
    // room in it fails the day a detail of the rhythm turns out to be read wrong here.
    let allowed = window.div_duration_f64(RESIZE_DEBOUNCE) as usize + 2;
    assert!(
        asks <= allowed,
        "a drag of {drag:?} in {steps} steps should coalesce into at most {allowed} \
         requests, not one per step: saw {asks}"
    );
    // And there were enough steps for that to have caught anything. A machine too slow to
    // issue more resizes than the ceiling allows would pass a client asking once per step,
    // so say so rather than report a pass the run did not earn.
    assert!(
        usize::from(steps) > allowed,
        "{steps} resizes in {drag:?} is not a drag: too few for {allowed} requests to be \
         evidence of anything"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}
