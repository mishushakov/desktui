//! Negotiating the size of the remote desktop, against a fake server.
//!
//! The headline behaviour: asking the server to make its desktop exactly the size of
//! the terminal's pixel area, and coping with each way that can be answered --
//! accepted, refused, forwarded to a guest, unsupported, or accepted and then rounded
//! to a mode the server actually has.
//!
//! The other half of it is *when* the request goes out: at once for a lone resize,
//! coalesced for a drag, and without waiting for the server to say anything first.
//!
//! And what the screen looks like while all that happens: a resize that redraws
//! everything is expected, a resize that shows the screen mid-redraw is not.

mod common;

use std::time::Duration;

use common::server::{Extensions, FakeServer, Request, Resize};
use common::session::*;
use common::*;

/// The client's `RESIZE_INTERVAL`: how long one resize that has been applied holds the
/// next off, a drag being answered while it happens rather than once it ends. Written down
/// rather than imported, the client being a binary, so a change there has to be echoed
/// here -- and `a_single_resize_is_acted_on_at_once` is what would notice if it were not.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(100);

/// What the tail of a drag adds to the span its requests fall in: a debounce for the
/// trailing edge, and a frame and a round trip for the granted size to be reported. Only
/// used to see the ceiling coming while a drag is still going on -- the assertion is made
/// against the span that actually elapsed.
const SETTLING: Duration = Duration::from_millis(400);

/// How many `SetDesktopSize` requests a drag whose answers all fall inside `window` is
/// allowed to have produced.
///
/// Applications fall a debounce apart at the closest -- applying one shuts the leading edge
/// for that long, and the trailing edge waits for the newest resize to be that old -- so no
/// two of them fit inside one window, and a request takes an application. A request can go
/// out later than the application that earned it, the reply to the one in flight being
/// where a skipped ask is picked up, but that moves a request rather than adding one.
///
/// Plus one beyond that, and not because the arithmetic needs it: the client really does
/// use its whole allowance, and the difference worth seeing here is the one between a
/// handful of requests and one per step, not between three and four. A ceiling with no room
/// in it fails the day a detail of the rhythm turns out to have been read wrong.
fn ceiling(window: Duration) -> usize {
    window.div_duration_f64(RESIZE_DEBOUNCE) as usize + 2
}

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
fn a_resize_never_blanks_the_screen() {
    // A relayout takes the stale rows and images off the screen, because text and
    // placements both stay on the cells they were written to. It used to take the whole
    // screen, in a write of its own ahead of the frame that fills it back in -- so the
    // terminal was blank until that frame composed, several times over per resize.
    let (_server, mut term) = start(Resize::Accept, (1024, 768));
    assert_drew(&term, Duration::from_secs(10));

    // Everything from here on is the resize: the erase in the setup sequence, which is
    // the alternate screen being entered and has nothing to redraw, is behind us.
    let before = term.output().len();
    // Grown, which is the direction that leaves the bar behind: the row it was on is an
    // interior one now, and the glyphs on it are drawn above the image.
    term.resize(220, 60, 1760, 1020);
    // The new size in the status line, not a cursor move to the new last row: the rows
    // tiles are placed on are cursor moves too, and one of them is the row that used to
    // be a quarter of the way down.
    assert_reports_size(&term, "1760x1002", Duration::from_secs(10));
    assert_a_relayout_never_blanks_the_screen(&term, before);

    // The row the bar left has to come out blank. On the screen rather than in the stream:
    // the chrome is diffed, so blanking a row is whatever cells differ from what was on it,
    // not an erase sequence to grep for.
    let screen = Screen::of(&term.output());
    assert!(
        screen.row(usize::from(ROWS) - 1).is_empty(),
        "the row the bar was on still reads {:?}",
        screen.row(usize::from(ROWS) - 1)
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn growing_the_window_sends_the_new_tiles_and_not_the_rest() {
    // A desktop bigger than the window is cropped, and the crop starts at the same corner
    // however big the window is: growing it exposes tiles and moves none. Every other tile
    // is already in the terminal's image store, under the id it still answers to, so the
    // resize is worth a strip of tiles rather than a screenful.
    //
    // 200x50 cells of 8x17 crops 1600x833 out of the desktop, in a grid of 13x7 tiles of
    // 128x136 -- 91 of them, all but the clipped last row and column full size. Grown to
    // 210x55 the crop is 1680x918, a grid of 14x7, of which the 12x6 that were full size
    // and did not move are kept: 26 to send, where it used to be all 98.
    let (_server, mut term) = start_with(Extensions::default(), (2000, 2000), &["--scale", "1:1"]);
    assert_drew(&term, Duration::from_secs(10));
    // The whole first screen has to be out before there is anything to keep. A cropped 1:1
    // window is pixel-exact, so the bar does not name a scaled size to wait for -- but it
    // is on the last row, and no tile is placed there.
    assert!(
        term.wait_for(b"\x1b[50;1H", Duration::from_secs(10)),
        "no bar on the last row: {}",
        tail(&term.output())
    );
    std::thread::sleep(Duration::from_millis(300));

    let before = tiles_drawn(&term);
    term.resize(210, 55, 1680, 935);
    assert!(
        term.wait_for(b"\x1b[55;1H", Duration::from_secs(10)),
        "the bar never reached the new last row: {}",
        tail(&term.output())
    );
    std::thread::sleep(Duration::from_millis(300));
    let sent = tiles_drawn(&term) - before;

    assert!(sent > 0, "the new tiles were never drawn");
    assert!(
        sent <= 40,
        "a resize sent {sent} tiles; the grid is 98, and the tiles that neither moved \
         nor changed size were supposed to be kept: {}",
        tail(&term.output())
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn moving_the_picture_costs_placements_and_not_pixels() {
    // A desktop smaller than the window is centred in it, so a window of another width
    // puts every tile on another cell without changing a pixel of any of them. The
    // terminal is holding all of it already and the protocol can say so: `a=p` puts an
    // image it has on other cells, with no payload.
    let (_server, mut term) = start_with(Extensions::default(), (640, 480), &["--scale", "1:1"]);
    assert_drew(&term, Duration::from_secs(10));
    // Wait for the screen to stop changing. The note the client puts up at startup marks
    // everything when it goes, and a redraw arriving in the middle of this would be read
    // as the resize having sent pixels.
    settle(&term);

    let before = tiles_drawn(&term);
    term.resize(202, 50, 1616, 850);
    assert!(
        term.wait_for(b"a=p,", Duration::from_secs(10)),
        "the picture was never put on its new cells: {}",
        tail(&term.output())
    );

    // In the frame that moved it, and that frame alone: the placements go out ahead of
    // whatever the compose has to say, so a move with nothing to send is a frame with no
    // transmission in it at all.
    let out = term.output();
    let moved = frame_containing(&out, b"a=p,").expect("no frame carried the move");
    assert!(
        !contains(moved, DREW),
        "moving the picture sent pixels too: {}",
        show(moved)
    );
    assert!(
        count(moved, b"a=p,") > 1,
        "only one tile was moved, so this is not the whole picture: {}",
        show(moved)
    );
    assert_eq!(
        tiles_drawn(&term),
        before,
        "the move should not have drawn a tile anywhere"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

/// Wait for the screen to stop changing, so a later count means what happened next.
///
/// Several quiet samples rather than one: a frame the writer was too busy to take keeps
/// its damage and goes out on a later tick, so a single quiet moment is not the same as
/// nothing being owed. That frame arriving in the middle of the resize would be read as
/// the move having sent pixels.
fn settle(term: &FakeTerm) {
    let mut quiet = 0;
    let mut last = tiles_drawn(term);
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(150));
        let now = tiles_drawn(term);
        quiet = if now == last { quiet + 1 } else { 0 };
        if quiet >= 3 {
            return;
        }
        last = now;
    }
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
    // A lone resize should not wait out the interval: the limit is there to thin a drag,
    // and making a single one wait made the window look like it had not taken.
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
        "a single resize waited {took:?} for a rate limit that had nothing to thin"
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

    // Resize at about the rate a dragged window edge does, and keep going until the steps
    // outnumber the ceiling they will be judged against.
    //
    // How many fit inside a window is the machine's business, and it varies more than seems
    // reasonable: a 16ms sleep has been seen to take 125ms on a loaded runner, which is two
    // steps to a window where a quiet machine manages fifteen. Two per window is still a
    // drag, but a short drag of them settles nothing -- a client asking once per step would
    // not have exceeded the ceiling either -- and dragging on until it would is the same
    // claim without the machine in it. `SETTLING` stands in for the tail the real window
    // adds while the projection is being made; the assertion below uses what elapsed.
    let began = std::time::Instant::now();
    let mut steps = 0u16;
    let mut conclusive = false;
    while began.elapsed() < Duration::from_secs(4) {
        steps += 1;
        let cols = 100 + steps;
        term.resize(cols, 25, cols * 8, 425);
        std::thread::sleep(Duration::from_millis(16));
        let far_enough = began.elapsed() >= 3 * RESIZE_DEBOUNCE;
        if far_enough && usize::from(steps) > ceiling(began.elapsed() + SETTLING) + 3 {
            conclusive = true;
            break;
        }
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

    let allowed = ceiling(window);
    assert!(
        asks <= allowed,
        "a drag of {drag:?} in {steps} steps should coalesce into at most {allowed} \
         requests, not one per step: saw {asks}"
    );

    // Whether this run could have caught a client asking once per step at all: with fewer
    // steps than the ceiling allows, one per step would have stayed under it. The loop
    // drags on until that is not so, and a machine slow enough to reach its limit anyway
    // has said everything it can -- the ceiling above still held of it. Reported rather
    // than asserted, because failing the build over how a shared runner was scheduled is
    // what had this test out of CI in the first place; a red build there says only that a
    // runner was busy. Cargo keeps the line for whoever runs the test themselves, `--
    // --nocapture` or a failure being what brings it out.
    if !conclusive || usize::from(steps) <= allowed {
        eprintln!(
            "note: {steps} resizes in {drag:?}, against a ceiling of {allowed} requests -- \
             this run held the client to the ceiling without being able to catch one that \
             asked per step"
        );
    }

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}
