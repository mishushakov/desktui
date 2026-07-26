//! End-to-end tests driving the real binary inside a fake terminal.
//!
//! These cover what unit tests cannot reach: the probe round trip, geometry from
//! `TIOCGWINSZ`, the setup and teardown sequences, resize handling, and whether
//! frames actually come out.

mod common;

use common::*;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn probes_before_touching_the_screen() {
    let mut term = FakeTerm::spawn(200, 50, 1600, 850, &["--test-pattern"]);
    assert!(term.wait_for(b"\x1b[c", Duration::from_secs(10)));
    let probe = term.output();

    assert!(
        contains(&probe, b"a=q"),
        "no graphics query: {}",
        show(&probe)
    );
    assert!(contains(&probe, b"\x1b[14t"), "no pixel-size query");
    assert!(contains(&probe, b"\x1b[?1016$p"), "no pixel-mouse query");
    assert!(contains(&probe, b"\x1b[?2026$p"), "no sync-output query");
    assert!(
        !contains(&probe, b"\x1b[?1049h"),
        "entered the alternate screen before knowing the terminal can draw"
    );
    term.send(GHOSTTY_REPLIES);
    term.send(b"q");
    term.wait(Duration::from_secs(10));
}

#[test]
fn renders_frames_and_restores_the_terminal() {
    let mut term = FakeTerm::spawn(
        200,
        50,
        1600,
        850,
        &["--test-pattern", "--fps", "30", "--transfer", "direct"],
    );
    term.answer_probe(GHOSTTY_REPLIES);

    assert!(
        term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)),
        "no image was transmitted: {}",
        show(&term.output())
    );
    // A full first frame is ~91 tiles followed by the status line, so wait for the
    // status line before looking: sampling at the first tile catches the frame
    // half written.
    assert!(
        term.wait_for(b"\x1b[50;1H", Duration::from_secs(10)),
        "status line never reached the last row: {}",
        show(&term.output())
    );
    let frames = term.output();

    assert!(contains(&frames, b"\x1b[?1049h"), "no alternate screen");
    assert!(contains(&frames, b"\x1b[?7l"), "autowrap left on");
    assert!(contains(&frames, b"\x1b[?1003h"), "motion reporting off");
    assert!(contains(&frames, b"\x1b[?1016h"), "pixel mouse not enabled");
    assert!(
        contains(&frames, b"\x1b[>11u"),
        "kitty keyboard not enabled"
    );
    assert!(
        contains(&frames, b"\x1b[?2026h"),
        "frames are not synchronised"
    );
    assert!(contains(&frames, b"z=-1"), "images must sit below text");
    assert!(contains(&frames, b"o=z"), "payload should be compressed");
    assert!(
        contains(&frames, b"C=1"),
        "placements must not move the cursor"
    );
    assert!(
        contains(&frames, b"q=2"),
        "the terminal must not reply per tile"
    );

    // Status line on the bottom row of a 50-row terminal.
    assert!(
        contains(&frames, b"\x1b[50;1H"),
        "status line is not on the last row: {}",
        show(&frames)
    );
    // 1600x850 of 8x17 cells leaves 49 usable rows: 1600x833. The pattern is
    // generated at exactly that size, so the mapping must be pixel-exact.
    assert!(
        contains(&frames, b"1600x833 native 1:1"),
        "expected a pixel-exact native layout: {}",
        show(&frames)
    );

    // Keep going: the bouncing box has to produce fresh frames over time.
    let before = count(&term.output(), b"\x1b_Ga=T");
    std::thread::sleep(Duration::from_millis(500));
    let after = count(&term.output(), b"\x1b_Ga=T");
    assert!(
        after > before + 2,
        "expected continued frames, went from {before} to {after} tiles"
    );

    term.send(b"q");
    let status = term
        .wait(Duration::from_secs(10))
        .expect("did not exit after q");
    assert!(status.success(), "exited with {status:?}");

    let teardown = term.output();
    assert!(
        contains(&teardown, b"\x1b_Ga=d,d=A"),
        "images were not deleted on exit"
    );
    assert!(
        contains(&teardown, b"\x1b[?1049l"),
        "alternate screen not left"
    );
    assert!(
        contains(&teardown, b"\x1b[?1003l"),
        "mouse reporting left on"
    );
    assert!(contains(&teardown, b"\x1b[?1016l"), "pixel mouse left on");
    assert!(contains(&teardown, b"\x1b[<u"), "keyboard flags not popped");
    assert!(contains(&teardown, b"\x1b[?25h"), "cursor left hidden");
    assert!(contains(&teardown, b"\x1b[?7h"), "autowrap left off");
}

#[test]
fn only_dirty_tiles_are_retransmitted() {
    // A 1600x833 image is 91 tiles. The first frame draws all of them; later
    // frames should only carry the handful the bouncing box touches, which is
    // the whole reason for the tile grid.
    let mut term = FakeTerm::spawn(200, 50, 1600, 850, &["--test-pattern", "--fps", "20"]);
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)));

    // Let the first full redraw finish.
    std::thread::sleep(Duration::from_millis(300));
    let after_first = count(&term.output(), b"\x1b_Ga=T");
    std::thread::sleep(Duration::from_millis(500));
    let later = count(&term.output(), b"\x1b_Ga=T");

    let per_frame = (later - after_first) as f64 / 10.0;
    assert!(
        per_frame < 20.0,
        "steady-state frames should touch a few tiles, not ~91; saw about \
         {per_frame:.1} tiles per frame"
    );

    term.send(b"q");
    term.wait(Duration::from_secs(10));
}

#[test]
fn refuses_a_terminal_without_graphics_without_touching_the_screen() {
    let mut term = FakeTerm::spawn(80, 24, 640, 384, &["--test-pattern"]);
    term.answer_probe(PLAIN_REPLIES);

    assert!(
        term.wait_for(b"Kitty graphics", Duration::from_secs(10)),
        "expected an explanation: {}",
        show(&term.output())
    );
    let status = term.wait(Duration::from_secs(10)).expect("did not exit");
    assert!(!status.success(), "should have failed, got {status:?}");

    let all = term.output();
    assert!(
        contains(&all, b"Ghostty"),
        "the error should name terminals that work: {}",
        show(&all)
    );
    assert!(
        !contains(&all, b"\x1b[?1049h"),
        "must not flash the alternate screen before refusing"
    );
    assert!(
        !contains(&all, b"\x1b_Ga=T"),
        "must not transmit images to a terminal that cannot show them"
    );
}

#[test]
fn print_caps_reports_both_geometry_sources_and_flags_a_mismatch() {
    let mut term = FakeTerm::spawn(100, 30, 800, 480, &["--print-caps"]);
    // Deliberately disagree with the pty's own size: this is the HiDPI trap the
    // report exists to surface.
    term.answer_probe(
        b"\x1b_Gi=1893;OK\x1b\\\x1b[4;960;1600t\x1b[6;16;16t\x1b[?1016;2$y\x1b[?2026;2$y\x1b[?62;22c",
    );

    assert!(term.wait_for(b"warning", Duration::from_secs(10)));
    let status = term.wait(Duration::from_secs(10)).expect("did not exit");
    assert!(status.success(), "exited with {status:?}");

    let text = String::from_utf8_lossy(&term.output()).into_owned();
    assert!(text.contains("kitty graphics       yes"), "{text}");
    assert!(text.contains("pixel mouse (1016)   yes"), "{text}");
    assert!(text.contains("800 x 480"), "ioctl geometry missing: {text}");
    assert!(
        text.contains("1600 x 960"),
        "CSI 14 t geometry missing: {text}"
    );
    assert!(
        text.contains("warning"),
        "a disagreement between the two must be called out: {text}"
    );
}

#[test]
fn a_resize_relayouts_and_releases_the_old_images() {
    let mut term = FakeTerm::spawn(120, 40, 960, 640, &["--test-pattern", "--fps", "30"]);
    term.answer_probe(
        b"\x1b_Gi=1893;OK\x1b\\\x1b[4;640;960t\x1b[6;16;8t\x1b[?1016;2$y\x1b[?2026;2$y\x1b[?62;22c",
    );

    assert!(
        term.wait_for(b"\x1b[40;1H", Duration::from_secs(10)),
        "no status line on row 40: {}",
        show(&term.output())
    );

    term.resize(60, 20, 480, 320);

    assert!(
        term.wait_for(b"\x1b[20;1H", Duration::from_secs(10)),
        "status line did not move to the new last row: {}",
        show(&term.output())
    );
    // A layout change drops every placement and redraws, rather than picking out
    // the orphans: the whole grid is retransmitted anyway, and leaving the rest in
    // place is what left stale rows on screen when the window grew.
    assert!(
        term.wait_for(b"a=d,d=A", Duration::from_secs(5)),
        "a resize should release the old placements: {}",
        show(&term.output())
    );
    // And the new geometry has to be what it draws at.
    assert!(
        term.wait_for(b"480x304", Duration::from_secs(5)),
        "expected the new 480x304 image area: {}",
        show(&term.output())
    );

    term.send(b"q");
    let status = term.wait(Duration::from_secs(10)).expect("did not exit");
    assert!(status.success());
}

#[test]
fn a_terminal_that_stops_reading_does_not_wedge_the_client() {
    // Ghostty will not do this, but a suspended terminal or a stalled ssh link
    // will, and quitting must not depend on the terminal's cooperation.
    let mut term = FakeTerm::spawn(200, 50, 1600, 850, &["--test-pattern", "--fps", "60"]);
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)));

    // Stop draining and let the pty buffer fill.
    drop(std::mem::take(&mut *term.seen.lock().unwrap()));
    let stalled = Arc::clone(&term.seen);
    std::thread::sleep(Duration::from_millis(300));
    drop(stalled);

    term.send(b"q");
    let status = term.wait(Duration::from_secs(10));
    assert!(
        status.is_some(),
        "client did not exit while the terminal was not reading"
    );
}

#[test]
fn pixels_travel_through_shared_memory_when_the_terminal_offers_it() {
    // The fast path, asked for explicitly: the payload is an object name rather
    // than the pixels, so a frame costs one memcpy instead of a base64 pass over
    // megabytes.
    let mut term = FakeTerm::spawn(
        200,
        50,
        1600,
        850,
        &["--test-pattern", "--fps", "20", "--transfer", "shm"],
    );
    term.answer_probe(GHOSTTY_REPLIES);

    assert!(
        term.wait_for(b"f=24,t=s,i=", Duration::from_secs(10)),
        "expected tiles to be placed from shared memory: {}",
        show(&term.output())
    );
    let output = term.output();
    assert!(
        !contains(&output, b"o=z"),
        "shared memory needs no compression: {}",
        show(&output)
    );

    term.send(b"q");
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_terminal_without_shared_memory_gets_compressed_base64() {
    // Silence about the `t=s` probe has to mean base64 rather than a blank screen:
    // frames go out with responses suppressed, so a transmission the terminal
    // cannot handle would fail invisibly.
    let mut term = FakeTerm::spawn(200, 50, 1600, 850, &["--test-pattern", "--fps", "20"]);
    term.answer_probe(DIRECT_ONLY_REPLIES);

    assert!(
        term.wait_for(b"o=z", Duration::from_secs(10)),
        "expected compressed base64: {}",
        show(&term.output())
    );
    assert!(
        !contains(&term.output(), b"f=24,t=s,i="),
        "must not place tiles through a medium the terminal never agreed to"
    );

    term.send(b"q");
    term.wait(Duration::from_secs(10));
}

#[test]
fn shared_memory_is_the_default_when_the_terminal_answered_for_it() {
    // It is worth roughly ten times the throughput on a full-screen update, and the
    // probe is what makes defaulting to it safe.
    let mut term = FakeTerm::spawn(200, 50, 1600, 850, &["--test-pattern", "--fps", "20"]);
    term.answer_probe(GHOSTTY_REPLIES);

    assert!(
        term.wait_for(b"f=24,t=s,i=", Duration::from_secs(10)),
        "expected tiles to be placed from shared memory by default: {}",
        show(&term.output())
    );

    term.send(b"q");
    term.wait(Duration::from_secs(10));
}

#[test]
fn growing_the_window_does_not_leave_stale_status_lines() {
    // Text does not move when the grid grows, so the status line written on the old
    // last row stays there. Without a wipe, a window dragged larger accumulates one
    // stale line per size it passed through, and the old placements linger with them.
    let mut term = FakeTerm::spawn(80, 24, 640, 408, &["--test-pattern", "--fps", "20"]);
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(
        term.wait_for(b"\x1b[24;1H", Duration::from_secs(10)),
        "no status line on row 24: {}",
        show(&term.output())
    );

    // Grow twice, the way a drag does.
    for (cols, rows, px, py) in [(120u16, 36u16, 960u16, 612u16), (200, 50, 1600, 850)] {
        term.resize(cols, rows, px, py);
        assert!(
            term.wait_for(
                format!("\x1b[{rows};1H").as_bytes(),
                Duration::from_secs(10)
            ),
            "status line never reached row {rows}: {}",
            show(&term.output())
        );
    }

    // Every relayout has to erase the screen and drop the old placements, or the
    // rows the status line used to occupy keep their text for ever.
    let out = term.output();
    assert!(
        count(&out, b"\x1b[2J") >= 3,
        "expected an erase per layout change, saw {}",
        count(&out, b"\x1b[2J")
    );
    assert!(
        count(&out, b"a=d,d=A") >= 2,
        "expected the old placements to be dropped on each resize, saw {}",
        count(&out, b"a=d,d=A")
    );

    // And after the last resize, the only status row still being written is the new
    // one: nothing should be repainting rows 24 or 36 any more. Whole frames, so that the
    // rest of one composed at the old size cannot be read as a repaint at it.
    let since = term
        .drawn_after(out.len(), 3, Duration::from_secs(10))
        .expect("the client stopped drawing after the last resize");
    assert!(
        contains(&since, b"\x1b[50;1H"),
        "the status line stopped being drawn on the new last row"
    );
    for stale in [&b"\x1b[24;1H"[..], b"\x1b[36;1H"] {
        assert!(
            !contains(&since, stale),
            "still writing to an old status row: {}",
            String::from_utf8_lossy(stale)
        );
    }

    term.send(b"q");
    term.wait(Duration::from_secs(10));
}

#[test]
fn focus_reporting_is_enabled_so_held_keys_can_be_released() {
    // Without mode 1004 the terminal never reports focus loss, so the code that
    // releases everything held on the remote is unreachable and a modifier held while
    // switching away stays held over there.
    let mut term = FakeTerm::spawn(200, 50, 1600, 850, &["--test-pattern"]);
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(term.wait_for(b"\x1b_Ga=T", Duration::from_secs(10)));

    assert!(
        contains(&term.output(), b"\x1b[?1004h"),
        "focus reporting was never enabled: {}",
        show(&term.output())
    );

    term.send(b"q");
    let status = term.wait(Duration::from_secs(10)).expect("did not exit");
    assert!(status.success());
    assert!(
        contains(&term.output(), b"\x1b[?1004l"),
        "focus reporting was left on at exit"
    );
}
