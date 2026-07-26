//! What the client asks the server to send, and how the frame stream is paced.
//!
//! Encodings first, because advertising one there is no decoder for would leave the
//! stream unrecoverable. Then the pacing: one update request in flight at a time, or
//! none at all once the server agrees to push frames instead. Fences are what remains
//! to measure a round trip with when nothing is being requested, and the cursor shape is
//! the one pseudo-encoding worth asking for on its own -- it is what lets the pointer
//! move at local speed rather than at a round trip's.

mod common;

use std::time::Duration;

use common::server::{
    EXTENDED_CLIPBOARD_ENCODING, Extensions, FakeServer, Request, Resize, SPLIT_FIRST,
    SPLIT_SECOND, SPLIT_STALL,
};
use common::session::*;
use common::*;

#[test]
fn a_frame_is_never_composed_from_half_an_update() {
    // The rectangles of one update are one picture, and they arrive one at a time. A
    // client that draws whatever it holds when its own clock strikes puts the first
    // rectangle on screen without the second -- and on a scrolling page, where the whole
    // screen is one update, that is half of it at the new scroll position and half at the
    // old. Which is what it looks like: blocks all over the screen, misplaced, until the
    // scrolling stops and one whole update lands.
    let (_server, mut term) = start_with(
        Extensions {
            split_updates: true,
            ..Default::default()
        },
        (1024, 768),
        &[],
    );
    assert_drew(&term, Duration::from_secs(10));
    // A good many stalls' worth, so every tick that wanted to draw inside one has had its
    // chance. The pacing is off the end of each update, so these run back to back.
    std::thread::sleep(SPLIT_STALL * 10);

    // 1:1 over a desktop resized to the terminal, so a remote pixel is a terminal pixel
    // and the tiles are 128x136 of them: the first rectangle is tile 0,0 and the second
    // spans tiles 2,1 and 2,2.
    let first = tile_id(SPLIT_FIRST);
    let second = [tile_id(SPLIT_SECOND), tile_id((SPLIT_SECOND.0, 320))];
    let out = term.output();
    let (mut whole, mut half) = (0, 0);
    for block in blocks(&out) {
        let drew_first = contains(block, transmit(first).as_bytes());
        let drew_second = second
            .iter()
            .any(|id| contains(block, transmit(*id).as_bytes()));
        match (drew_first, drew_second) {
            (true, true) => whole += 1,
            (true, false) | (false, true) => half += 1,
            (false, false) => {}
        }
    }
    assert!(
        whole > 0,
        "no frame ever carried an update whole: {}",
        tail(&out)
    );
    assert_eq!(
        half,
        0,
        "{half} frames carried one half of an update without the other, out of \
         {} that carried any of it. A frame has to wait for the update it is drawing \
         to be over.",
        whole + half
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

/// The image id of the tile covering a pixel, for the 1:1 layout the session harness
/// negotiates: 8x17 cells make tiles of 128x136, numbered by where they sit.
fn tile_id((x, y): (u16, u16)) -> u32 {
    const BASE: u32 = 0x7600;
    const STRIDE: u32 = 512;
    BASE + u32::from(y / 136) * STRIDE + u32::from(x / 128)
}

/// The transmit command for an image id, which is what says a tile reached the screen --
/// as opposed to the delete that precedes it, which names the same id.
fn transmit(id: u32) -> String {
    format!("i={id},p=1,s=")
}

/// Everything between a begin-sync marker and its end, which is one frame.
fn blocks(out: &[u8]) -> Vec<&[u8]> {
    let mut found = Vec::new();
    for open in offsets(out, BEGIN_SYNC) {
        let from = open + BEGIN_SYNC.len();
        if let Some(len) = find(&out[from..], END_SYNC) {
            found.push(&out[from..from + len]);
        }
    }
    found
}

#[test]
fn keeps_at_most_one_update_request_in_flight() {
    // A client that fires requests on a timer piles them up on the server. Pacing
    // off the end of each update means there is never more than one outstanding,
    // and the watchdog only re-asks about once a second.
    let (server, mut term) = start(Resize::Accept, (640, 480));
    assert_drew(&term, Duration::from_secs(10));

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

            assert!(
                encodings.contains(&EXTENDED_CLIPBOARD_ENCODING),
                "the extended clipboard is what carries anything outside Latin-1: \
                 {encodings:?}"
            );

            // Anything advertised has to be something we can consume. For a data
            // encoding that means a decoder, because rectangles carry no length and a
            // surprise would leave the stream unrecoverable. Fence,
            // ContinuousUpdates and ExtendedClipboard are different in kind: they are
            // answered with messages, never rectangles.
            const DECODABLE_RECTS: [i32; 9] = [0, 1, 7, 16, -223, -224, -239, -261, -308];
            const NEGOTIATION_ONLY: [i32; 3] = [-312, -313, EXTENDED_CLIPBOARD_ENCODING];
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
fn the_statistics_say_what_the_other_end_is_doing() {
    // Everything else in the bar is this side. A session that crawls because the server is
    // re-encoding a whole screen looks exactly like one that crawls because we are, and
    // that question has cost a working day before now.
    //
    // The split-update server delivers one picture per stall, so its rate is known ahead
    // of time: a shade under five a second, where the client is free to draw sixty.
    let (_server, mut term) = start_with(
        Extensions {
            split_updates: true,
            ..Default::default()
        },
        (1024, 768),
        &[],
    );
    assert_drew(&term, Duration::from_secs(10));

    // Ctrl+A then c turns the figures on.
    term.send(&[0x01]);
    std::thread::sleep(Duration::from_millis(50));
    term.send(b"c");
    assert!(
        term.wait_for(b"up/s", Duration::from_secs(10)),
        "the bar never reported an update rate: {}",
        tail(&term.output())
    );
    // Long enough for the rolling window to hold several stalls.
    std::thread::sleep(SPLIT_STALL * 6);

    let out = term.output();
    let (rate, delivery) = last_delivery(&out).expect("no delivery figures in the bar");
    assert!(
        (2.0..8.0).contains(&rate),
        "expected about five updates a second from a {SPLIT_STALL:?} stall, bar said \
         {rate}: {}",
        tail(&out)
    );
    // Measured from the first rectangle *arriving*, where the server's stall starts before
    // it writes one, so this reads a few milliseconds under the stall rather than over it.
    assert!(
        delivery + 20 >= SPLIT_STALL.as_millis() as u32,
        "an update split across a {SPLIT_STALL:?} stall cannot have been delivered in \
         {delivery}ms: {}",
        tail(&out)
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

/// The update rate and per-update delivery time from the last time the bar was drawn.
fn last_delivery(out: &[u8]) -> Option<(f64, u32)> {
    let text = String::from_utf8_lossy(out);
    let at = text.rfind("up/s")?;
    let before = &text[..at];
    // "  4.9 up/s   215ms /up" -- the rate runs back to the space before it, and the
    // delivery is the next figure along.
    let rate: f64 = before.rsplit(' ').find(|s| !s.is_empty())?.parse().ok()?;
    let after = &text[at + "up/s".len()..];
    let ms: u32 = after
        .split("ms")
        .next()?
        .trim()
        .rsplit(' ')
        .find(|s| !s.is_empty())?
        .parse()
        .ok()?;
    Some((rate, ms))
}

#[test]
fn no_push_never_offers_the_extension_and_keeps_asking() {
    // Pushed frames are cheaper by a round trip and unbounded: the server decides how
    // hard both ends work, and every update has to be decoded whether or not there is
    // time to draw it. `--no-push` puts the pace back here.
    //
    // Declined by never offering it. A server announces the extension by answering
    // `SetEncodings`, and only a client that asked may enable it, so leaving the encoding
    // out is what stops the whole arrangement rather than turning it down afterwards.
    let ext = Extensions {
        continuous_updates: true,
        ..Extensions::default()
    };
    let (server, mut term) = start_with(ext, (800, 600), &["--scale", "fit", "--no-push"]);

    let request = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::SetEncodings(_))
        })
        .expect("no encodings were negotiated");
    match request {
        Request::SetEncodings(encodings) => {
            assert!(
                !encodings.contains(&-313),
                "ContinuousUpdates must not be offered: {encodings:?}"
            );
            assert!(
                encodings.contains(&-312),
                "Fence is a separate extension and the latency probe needs it: \
                 {encodings:?}"
            );
        }
        other => panic!("unexpected request {other:?}"),
    }

    // So it is never turned on, and the frames stay something this end asks for. The
    // watchdog is what keeps asking here, the fake server having nothing to answer an
    // incremental request with, so give it longer than its second.
    assert_drew(&term, Duration::from_secs(10));
    let before = server
        .requests()
        .iter()
        .filter(|r| matches!(r, Request::FramebufferUpdate { .. }))
        .count();
    std::thread::sleep(Duration::from_secs(2));
    let asked = server
        .requests()
        .iter()
        .filter(|r| matches!(r, Request::FramebufferUpdate { .. }))
        .count();
    assert!(
        asked > before,
        "stopped asking for frames without anything pushing them"
    );
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::EnableContinuousUpdates { .. })),
        "asked the server to push frames anyway: {:?}",
        server.requests()
    );
    assert!(
        !contains(&term.output(), b"pushing frames"),
        "said the remote was pushing frames: {}",
        tail(&term.output())
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

/// Live counterpart: `live::a_real_server_negotiates_continuous_updates`.
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
        term.wait_for(b"pushing frames", Duration::from_secs(10)),
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

/// Live counterpart: `live::growing_the_window_fills_the_new_area_on_a_real_server`,
/// which observes the same behaviour from the other end -- that the new area is filled
/// rather than that the request went out.
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

/// Live counterpart: `live::a_real_server_answers_the_latency_probe`.
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
    assert_drew(&term, Duration::from_secs(10));

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
    assert_drew(&term, Duration::from_secs(10));
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

/// Live counterpart: `live::a_real_server_sends_a_cursor_shape`.
#[test]
// Needs the redraw to land inside a fixed 400ms window, which a loaded shared
// runner misses. Passes on a real machine; run it with --ignored.
#[ignore = "wall-clock sensitive: unreliable on shared CI runners"]
fn the_cursor_shape_is_requested_and_drawn_locally() {
    // Asking for the shape is what stops the server compositing the pointer, so the
    // pointer can then move at local speed rather than at a round trip's.
    let ext = Extensions {
        cursor: true,
        ..Extensions::default()
    };
    let (server, mut term) = start_with(ext, (800, 600), &["--scale", "fit"]);
    assert_drew(&term, Duration::from_secs(10));

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
    let before = tiles_drawn(&term);
    for x in (200..500).step_by(30) {
        term.send(format!("\x1b[<35;{x};200M").as_bytes());
        std::thread::sleep(Duration::from_millis(30));
    }
    std::thread::sleep(Duration::from_millis(400));
    assert_kept_drawing(
        &term,
        before,
        "moving the pointer, so the cursor is not being composited locally",
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
