//! Keyboard, pointer and clipboard reaching the server, against a fake server.
//!
//! The terminal is the only input device, and what it reports varies: some terminals
//! answer for the keyboard protocol and owe a release for every press, some report
//! nothing and have their releases synthesised. On top of that sit the things the
//! client swallows rather than forwards -- the prefix chord, the key that dismisses the
//! command menu -- and the corrections it inserts, like a caps lock the server disagrees
//! with.

mod common;

use std::time::Duration;

use common::server::{Extensions, FakeServer, Request, Resize};
use common::session::*;
use common::*;

#[test]
fn input_reaches_the_server_with_pixel_exact_coordinates() {
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    assert_pixel_exact(&term, Duration::from_secs(10));

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
fn the_command_menu_holds_the_focus_until_escape() {
    // The menu has the focus while it is up: a key neither dismisses it nor reaches
    // the remote, and escape -- which is what the title offers -- is the way out.
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    assert_pixel_exact(&term, Duration::from_secs(10));

    // Ctrl+A then p raises it.
    term.send(&[0x01]);
    std::thread::sleep(Duration::from_millis(50));
    term.send(b"p");
    assert!(
        term.wait_for(b"Renegotiate the remote size", Duration::from_secs(10)),
        "the menu never appeared"
    );

    // An ordinary key changes nothing. It is redrawn every frame while it is up, so
    // a tail that still mentions it is the box still being there.
    let mark = term.output().len();
    term.send(b"\x1b[120u");
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        contains(&term.output()[mark..], b"Renegotiate the remote size"),
        "an ordinary key put the menu away"
    );

    // And it went nowhere else either: with the focus in the menu there is nothing
    // behind it to type into.
    assert!(
        !server.requests().iter().any(|r| matches!(
            r,
            Request::Key {
                keysym: 0x78,
                down: true
            }
        )),
        "a key typed at the menu was forwarded to the server"
    );

    // Escape is the way out. Stopping the redraw is not the same as taking it off the
    // screen: the glyphs outlive any repaint of the image below them, and the backdrop
    // outranks every tile, so both have to be taken off explicitly -- and only the
    // teardown deletes an image by id, which makes it the thing to look for.
    let mark = term.output().len();
    term.send(b"\x1b");
    std::thread::sleep(Duration::from_millis(500));
    let since = term.output()[mark..].to_vec();
    assert!(
        !contains(&since, b"Renegotiate the remote size"),
        "escape did not put the menu away"
    );
    assert!(
        contains(&since, b"a=d,d=I,i="),
        "the menu was never cleared, so it is still on screen"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn clicking_a_scaling_option_selects_that_one() {
    // The menu is the only way to reach a scaling mode by name. The key cannot: one
    // binding for four modes can only step to the next.
    //
    // The server refuses to resize, so the modes are told apart by what they do to a
    // 1024x768 desktop in a 1600x833 area. Native falls back, integer scales by one
    // and 1:1 is itself -- all three pixel-exact. Only fit resamples, so "scaled" is
    // proof that the click landed on fit itself: one step on from the mode native fell
    // back to would have been native again.
    let (server, mut term) = start(Resize::Refuse, (1024, 768));
    assert!(
        term.wait_for(b"1:1", Duration::from_secs(10)),
        "never fell back from native: {}",
        tail(&term.output())
    );

    // Ctrl+A then p raises the menu.
    term.send(&[0x01]);
    std::thread::sleep(Duration::from_millis(50));
    term.send(b"p");
    assert!(
        term.wait_for(b"Scaling", Duration::from_secs(10)),
        "the menu never appeared: {}",
        tail(&term.output())
    );

    // Where fit is. Twenty-three entries and their padding make a box 53 by 25,
    // centred across 200 columns and the 49 rows above the status line, so its top
    // left is cell 73,12. The scaling options are the sixteenth entry, three columns
    // in past the padding, and fit is the second of them -- ten cells along, native
    // taking eight and the gap two. Cell 89,28, which in 8x17 cells is pixel 716,484,
    // and mouse reports are one-based.
    //
    // Checked rather than assumed, because a click aimed at a row that has moved
    // would fail somewhere far less obvious than here.
    let out = term.output();
    let at = find(&out, b"\x1b[29;74H").unwrap_or_else(|| {
        panic!(
            "the menu does not reach cell 74,29 any more: {}",
            tail(&out)
        )
    });
    assert!(
        contains(&out[at..(at + 160).min(out.len())], b"Native"),
        "cell 74,29 is no longer the row of scaling options, so the click would miss"
    );
    let mark = out.len();
    term.send(b"\x1b[<0;717;485M");
    term.send(b"\x1b[<0;717;485m");

    assert!(
        term.wait_for(b"scaling: scaled", Duration::from_secs(10)),
        "the click did not select fit: {}",
        tail(&term.output())
    );

    // The menu stays up, and shows the choice it just made: the brackets mark the mode
    // in force, so they move to fit. Only the dismissal takes the box down, which is
    // what makes the row a control you can watch rather than one that ends the session
    // with the menu.
    std::thread::sleep(Duration::from_millis(500));
    let since = term.output()[mark..].to_vec();
    assert!(
        contains(&since, b"Renegotiate the remote size"),
        "the menu went away on a click that was not the dismissal"
    );
    assert!(
        contains(&since, b"[Fit]"),
        "the option in force is not the one that was clicked: {}",
        tail(&since)
    );

    // And the click went to the menu alone. A button that reached the remote as well
    // would have pressed whatever the box was covering.
    assert!(
        server
            .wait_for(Duration::from_secs(1), |r| matches!(
                r,
                Request::Pointer { buttons: 1, .. }
            ))
            .is_none(),
        "the click was forwarded to the server as well"
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
    assert_drew(&term, Duration::from_secs(15));

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
    assert_drew(&term, Duration::from_secs(10));

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
    assert_drew(&term, Duration::from_secs(10));

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

/// Related: `live::a_real_server_reports_its_lock_key_state` checks the precondition,
/// that a real server sends the LED state this correction compares against at all.
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
    assert_drew(&term, Duration::from_secs(10));

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
    let began = std::time::Instant::now();
    while began.elapsed() < Duration::from_secs(10) {
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
    assert_drew(&term, Duration::from_secs(10));

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
