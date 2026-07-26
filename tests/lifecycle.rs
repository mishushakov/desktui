//! Connecting, losing the connection, and leaving the terminal as it was found.
//!
//! A client that draws over the whole screen has to be able to undo that on every exit
//! path, including the ones it did not choose. A connection that never came up must not
//! touch the screen at all, and one that goes away mid-session has to say so and then
//! either exit or come back, depending on what was asked for.

mod common;

use std::time::Duration;

use common::server::{FakeServer, Resize};
use common::session::*;
use common::*;

#[test]
fn the_server_going_away_is_reported_and_the_terminal_restored() {
    let server = FakeServer::start(640, 480, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(COLS, ROWS, PIXELS.0, PIXELS.1, &[&addr, "--fps", "10"]);
    term.answer_probe(GHOSTTY_REPLIES);
    assert_drew(&term, Duration::from_secs(10));

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

/// Live counterpart: `live::a_wrong_password_fails_clearly`, which is the same claim
/// about a failure that happens one step later -- after the connection, during auth.
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
    assert_drew_nothing(&term);
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
    assert_drew(&term, Duration::from_secs(15));

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
    assert_drew(&term, Duration::from_secs(15));

    drop(server);
    let status = term
        .wait(Duration::from_secs(15))
        .expect("client should have exited");
    assert!(!status.success());
}
