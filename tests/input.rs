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

use base64::Engine as _;
use common::server::{
    EXTENDED_CLIPBOARD_ENCODING, Extensions, FakeServer, REMOTE_CLIPBOARD, Request, Resize,
};
use common::session::*;
use common::*;

#[test]
fn input_reaches_the_server_with_pixel_exact_coordinates() {
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    // The negotiated size first, and the mapping only after it. Pixel-exact is true of
    // the 1024x768 desktop the session opens on as well -- drawn 1:1, letterboxed in the
    // middle of the area -- so on its own it says nothing about where a terminal pixel
    // lands. A click aimed at the desktop that fills the area falls in that letterbox
    // instead, outside the image, where the client drops it rather than reporting it as a
    // click on the nearest edge.
    assert_reports_size(&term, EXPECTED_SIZE, Duration::from_secs(10));
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
    // the remote, and escape is the way out -- as is the [x] on the title, and the
    // toggle at the top of the box.
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

    // An ordinary key changes nothing. The box is redrawn every frame while it is up, so
    // a later frame that still holds it is the box still being there.
    //
    // The wait is not a deadline for an answer -- there is no answer to wait for -- but
    // long enough for a wrong one to have gone out in, after which what the client is
    // drawing *now* is what settles it.
    term.send(b"\x1b[120u");
    std::thread::sleep(Duration::from_millis(500));
    let since = term
        .drawn_after(term.output().len(), 2, Duration::from_secs(10))
        .expect("the client stopped drawing with the menu up");
    assert!(
        contains(&since, b"Renegotiate the remote size"),
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
    // outranks every tile, so both have to be taken off explicitly -- and the delete of
    // the backdrop is what says the teardown ran, which makes it the thing to wait for.
    let mark = term.output().len();
    term.send(b"\x1b");
    let cleared = term
        .wait_for_after(mark, &deleted(MENU_ID), Duration::from_secs(10))
        .unwrap_or_else(|| {
            panic!(
                "the menu was never cleared, so it is still on screen: {}",
                tail(&term.output())
            )
        });

    // And it stays away. Watched from the clearing rather than from the keystroke,
    // because the frame in flight when the escape went out had been composed before the
    // client had seen it, and holds the box for that reason alone.
    let since = term
        .drawn_after(cleared, 2, Duration::from_secs(10))
        .expect("the client stopped drawing once the menu was dismissed");
    assert!(
        !contains(&since, b"Renegotiate the remote size"),
        "escape did not put the menu away"
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

    let selected = term
        .wait_for_after(mark, b"scaling: scaled", Duration::from_secs(10))
        .unwrap_or_else(|| panic!("the click did not select fit: {}", tail(&term.output())));

    // The menu stays up, and shows the choice it just made: the brackets mark the mode
    // in force, so they move to fit. Only the dismissal takes the box down, which is
    // what makes the row a control you can watch rather than one that ends the session
    // with the menu.
    //
    // Watched from the selection on, so that a frame composed before the click cannot
    // stand in for the box having stayed.
    let since = term
        .drawn_after(selected, 2, Duration::from_secs(10))
        .expect("the client stopped drawing after the click");
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
fn clicking_the_close_button_puts_the_notification_away() {
    // The popup's `[x]` is the one target outside the menu. A click on it has to take
    // the box off the screen before its linger is up -- and take it off properly, the
    // message being text that no repaint of the tiles under it would erase.
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    assert_drew(&term, Duration::from_secs(10));

    // Ctrl+A then f asks for a full refresh, which is the shortest way to a note.
    term.send(&[0x01]);
    std::thread::sleep(Duration::from_millis(50));
    term.send(b"f");
    assert!(
        term.wait_for(b"full refresh requested", Duration::from_secs(10)),
        "the notification never appeared: {}",
        tail(&term.output())
    );

    // Where the button is. The message is 22 columns and the box adds eight -- three of
    // gap, one for the `x`, two of padding either side -- so it is 30 wide, held two
    // columns off the right of 200 and one row down. That puts the `x` at cell 195,2,
    // which in 8x17 cells is pixel 1560,34; mouse reports are one-based.
    //
    // Checked rather than assumed, because a click aimed at a box that has moved would
    // fail somewhere far less obvious than here.
    let out = term.output();
    let at = find(&out, b"\x1b[3;169H").unwrap_or_else(|| {
        panic!(
            "the notification does not start at cell 169,3 any more: {}",
            tail(&out)
        )
    });
    assert!(
        contains(
            &out[at..(at + 120).min(out.len())],
            b"full refresh requested"
        ),
        "cell 169,3 is no longer the message's row, so the click would miss"
    );

    let mark = out.len();
    term.send(b"\x1b[<0;1561;35M");
    term.send(b"\x1b[<0;1561;35m");

    // Two seconds is far more than the frame or two the client needs and still well
    // inside the note's four-second linger, so what took the box off was the click
    // rather than time running out on it.
    let cleared = term
        .wait_for_after(mark, &deleted(TOAST_ID), Duration::from_secs(2))
        .unwrap_or_else(|| {
            panic!(
                "the popup's backdrop was never deleted, so the box is still there: {}",
                tail(&term.output())
            )
        });
    let since = term
        .drawn_after(cleared, 2, Duration::from_secs(10))
        .expect("the client stopped drawing once the note was closed");
    assert!(
        !contains(&since, b"full refresh requested"),
        "the message is still being redrawn: {}",
        tail(&since)
    );

    // And the click went to the popup alone: a press that reached the remote as well
    // would have landed on whatever the box was covering.
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
fn the_close_button_answers_while_the_menu_is_up() {
    // Which is most of the time a note is on screen: half the commands that raise one
    // are reached through the menu, and a click on a menu item leaves the box up. The
    // popup is drawn over the menu, so it has to be clickable over it too -- otherwise
    // the menu, which takes the whole pointer while it is up, eats the click.
    let (_server, mut term) = start(Resize::Accept, (1024, 768));
    assert_drew(&term, Duration::from_secs(10));

    term.send(&[0x01]);
    std::thread::sleep(Duration::from_millis(50));
    term.send(b"p");
    assert!(
        term.wait_for(b"Renegotiate the remote size", Duration::from_secs(10)),
        "the menu never appeared: {}",
        tail(&term.output())
    );

    // A local command still works with the menu up, and this one puts a note on screen.
    term.send(&[0x01]);
    std::thread::sleep(Duration::from_millis(50));
    term.send(b"f");
    assert!(
        term.wait_for(b"full refresh requested", Duration::from_secs(10)),
        "the notification never appeared: {}",
        tail(&term.output())
    );

    // The button, as in `clicking_the_close_button_puts_the_notification_away`: cell
    // 195,2 of an 8x17 grid, and mouse reports are one-based.
    let mark = term.output().len();
    term.send(b"\x1b[<0;1561;35M");
    term.send(b"\x1b[<0;1561;35m");

    // As in `clicking_the_close_button_puts_the_notification_away`: inside the linger, so
    // the delete is the click's work and not the clock's.
    let cleared = term
        .wait_for_after(mark, &deleted(TOAST_ID), Duration::from_secs(2))
        .unwrap_or_else(|| {
            panic!(
                "the menu swallowed the click on the popup drawn over it: {}",
                tail(&term.output())
            )
        });
    let since = term
        .drawn_after(cleared, 2, Duration::from_secs(10))
        .expect("the client stopped drawing once the note was closed");
    assert!(
        !contains(&since, b"full refresh requested"),
        "the message is still being redrawn: {}",
        tail(&since)
    );
    // And the menu is still up: the cross closes the note, not the box behind it.
    assert!(
        contains(&since, b"Renegotiate the remote size"),
        "closing the notification put the menu away too: {}",
        tail(&since)
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
    paste(&mut term, "hello there", &server, |r| {
        matches!(r, Request::CutText(_))
    });
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
fn the_extended_clipboard_brings_cyrillic_back_from_the_remote() {
    // The point of the extension. Over legacy cut text this arrives as a row of
    // question marks, because Latin-1 has nowhere to put a Cyrillic letter.
    let (server, mut term) = start_with(
        Extensions {
            extended_clipboard: true,
            announce_clipboard: true,
            ..Default::default()
        },
        (800, 600),
        &["--scale", "fit"],
    );
    assert_drew(&term, Duration::from_secs(10));

    // The server announced a clipboard without sending it, so the client has to ask.
    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::ClipboardRequest
            ))
            .is_some(),
        "the client never asked for the clipboard it was told about"
    );

    // And what came back went to the local clipboard through OSC 52, in UTF-8.
    let encoded = base64::engine::general_purpose::STANDARD.encode(REMOTE_CLIPBOARD);
    assert!(
        term.wait_for(
            format!("\x1b]52;c;{encoded}").as_bytes(),
            Duration::from_secs(10)
        ),
        "the remote clipboard never reached the local one: {}",
        tail(&term.output())
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_pasted_selection_is_announced_first_and_sent_when_asked() {
    // With the server advertising a zero unsolicited size -- which is what the spec
    // recommends and what TigerVNC does -- a paste says "I have text" and the bytes
    // follow only when something over there asks for them.
    let (server, mut term) = start_with(
        Extensions {
            extended_clipboard: true,
            ..Default::default()
        },
        (800, 600),
        &["--scale", "fit"],
    );
    assert_drew(&term, Duration::from_secs(10));

    // Our own capabilities go out during the handshake, before any of this.
    let caps = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::ClipboardCaps { .. })
        })
        .expect("the client never sent its clipboard capabilities");
    match caps {
        Request::ClipboardCaps { text_size, .. } => assert_eq!(
            text_size, 0,
            "a nonzero size invites the server to push every remote selection"
        ),
        other => panic!("unexpected request {other:?}"),
    }

    // The server's own capabilities have arrived by now, which is what decides how a paste
    // goes out: without them the client falls back to Latin-1 cut text, correctly, and
    // nothing would ever announce anything. `assert_drew` above is the wait for them --
    // they answer the `SetEncodings`, so no frame can have overtaken them.
    paste(&mut term, "Привет, мир", &server, |r| {
        // Either way it went, so that a fallback fails the assertion below rather than
        // being taken for a paste that never arrived and said again.
        matches!(r, Request::ClipboardNotify | Request::CutText(_))
    });
    assert!(
        server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::ClipboardNotify)),
        "the paste was never announced: {:?}",
        server.requests()
    );
    // The fake server answers a notify with a request, so the text should follow --
    // whole, which is the other half of what the extension is for.
    let provided = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::ClipboardProvide(_))
        })
        .expect("the announced text never followed");
    match provided {
        Request::ClipboardProvide(text) => assert_eq!(text, "Привет, мир"),
        other => panic!("unexpected request {other:?}"),
    }
    // And none of it went out as Latin-1, which is where the question marks came from.
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::CutText(_))),
        "the paste also went out as legacy cut text"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn no_clipboard_never_offers_the_extension() {
    // The pseudo-encoding is a standing offer to exchange clipboards. A session that
    // wants none should not make it, whatever the server would have done with it.
    let (server, mut term) = start_with(Extensions::default(), (800, 600), &["--no-clipboard"]);
    assert_drew(&term, Duration::from_secs(10));

    let encodings = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::SetEncodings(_))
        })
        .expect("no encodings were sent");
    match encodings {
        Request::SetEncodings(ids) => assert!(
            !ids.contains(&EXTENDED_CLIPBOARD_ENCODING),
            "offered the extended clipboard with --no-clipboard: {ids:?}"
        ),
        other => panic!("unexpected request {other:?}"),
    }
    // And nothing extended was sent either, capabilities included.
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::ClipboardCaps { .. })),
        "sent clipboard capabilities for an encoding it never asked for"
    );

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

    paste(&mut term, "caf\u{e9} \u{2615} tea", &server, |r| {
        matches!(r, Request::CutText(_))
    });
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
