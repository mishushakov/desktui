# Testing

```
make test           # everything that needs neither a server nor a real terminal
make check          # fmt, clippy, test
make test-live      # against the real desktop container
make perf           # time the compose pipeline
```

`make test` is what CI runs, on Linux and macOS. The others are opt-in, and
`make test` prints them at the end so they are not forgotten.

## The layers

**Unit tests, in `src/`** sit next to what they test. The decoders and the handshake are
worth calling out.

The decoders in `src/rfb/codec/` are tested against rectangles built byte by byte:
every Tight compression type and filter (fill, JPEG, copy, mono and indexed palette,
gradient), every ZRLE subencoding, the cursor mask, and the refusals that keep a
malformed rectangle from becoming a panic. This is the one part of the client that turns
attacker-controlled bytes into pixels, so the tests assert the pixels rather than that
something was drawn. `codec/mod.rs` holds the shared harness — a sink for emitted
events, a `bgra()` so an expectation names a colour instead of a byte order, and a
`deflate()` that sync-flushes rather than ending the stream, because a server keeps one
zlib stream open for the whole session and never ends it.

The handshake in `src/rfb/client/` is tested through a scripted stream where running out
of script is end-of-stream rather than pending. That is what lets a test stop at the end
of the handshake: whatever the client does next errors, and by then the bytes worth
asserting on have been written. It covers version negotiation, the unrecognised-version
fallback to 3.3 that RFC 6143 §7.1.1 asks for, and the fact that 3.3, 3.7 and 3.8 each
answer the security handshake differently — 3.3 says nothing, 3.7 echoes the type, 3.8
echoes it and consumes a `SecurityResult`. `ServerMsg::read` is covered separately, and
there the recurring assertion is what is *left* on the stream: the contract is that a
message is consumed whole even when it is refused, because a parser that stops early
leaves the next read mid-message, where everything afterwards is garbage in a way that
looks nothing like its cause.

**Integration suites, in `tests/`** — every one runs the real binary inside a pty that
the test drives, answering capability queries the way Ghostty does and then inspecting
the escape stream it produces. They differ in what else is real.

| suite | what it covers |
|---|---|
| [`pty.rs`](tests/pty.rs) | the terminal boundary alone: the probe round trip, geometry from `TIOCGWINSZ`, setup and teardown, shared memory, whether frames come out |
| [`resize.rs`](tests/resize.rs) | size negotiation, and every way a server can answer it |
| [`input.rs`](tests/input.rs) | keyboard, pointer, clipboard, lock-key correction |
| [`updates.rs`](tests/updates.rs) | encodings, request pacing, continuous updates, fences, cursor shape |
| [`lifecycle.rs`](tests/lifecycle.rs) | connecting, losing the connection, leaving the terminal as it was found |
| [`live.rs`](tests/live.rs) | the same client against a real TigerVNC desktop |
| [`perf.rs`](tests/perf.rs) | throughput of the compose path |

`resize`, `input`, `updates` and `lifecycle` add a fake VNC server, so a whole session
is verified without Docker. They are split by subject rather than by machinery — the
file names say what is being checked, not which fakes are involved.

## The live suite

`live.rs` catches what the others cannot: it runs the binary against **TigerVNC serving
a real XFCE desktop**, so real Tight encoding, real JPEG rectangles, and a real answer
to `SetDesktopSize`. It needs the container:

```
make desktop
make test-live
make desktop-stop
```

TigerVNC on purpose: `-AcceptSetDesktopSize` defaults to on, so the negotiation is
genuinely exercised rather than quietly falling back. Point the client at an `x11vnc`
instance instead to exercise the other path — it usually answers "administratively
prohibited", which is what the status line has to explain.

Override the target with `DESKTUI_TEST_SERVER` and `DESKTUI_TEST_PASSWORD`.

## Fake and live counterparts

Most live tests exist because a fake-server test already makes the same claim, and the
live one asks whether a server nobody wrote to agree with us behaves the same way. Each
names its counterpart in a doc comment, in both directions, including those that
have none and why.

The claims a pair shares are written once, in
[`tests/common/session.rs`](tests/common/session.rs) — `assert_reports_size`,
`assert_pixel_exact`, `assert_drew`, `assert_kept_drawing`. A pair is only worth having
if both halves check the same thing, and the live half is `#[ignore]`d, so nothing else
would say when they had drifted apart.

They are deliberately *not* renamed to match each other. Only some are strictly the same
claim; the rest are complementary — `a_real_server_reports_its_lock_key_state` checks
the precondition for `input::a_disagreeing_caps_lock_is_corrected_before_the_keystroke`,
not the same thing — and a matching name would assert an equivalence that does not hold.

## Claims about a change, not about a deadline

Every integration test reads a stream that is still arriving while it is being read, which
makes *when* a claim is made as easy to get wrong as the claim itself. Three rules, each
learnt from a test that failed on a loaded macOS runner and nowhere else.

**Wait for the answer rather than sleeping and looking.** `FakeTerm::wait_for_after` takes
an offset and gives back where the needle ended, so the next claim begins where the last
one's evidence finished. A sleep in that position is a deadline for the client to answer
within, and a busy machine misses it without anything being wrong with the answer.

**An absence claim opens on a frame boundary.** `FakeTerm::drawn_after` waits for whole
frames past a point and hands back what they said. Marking the output and reading the tail
after a sleep looks like the same thing and is not: the mark usually lands partway through
a frame the client had already composed — before it had seen the keystroke, so what it
holds answers nothing. `input::the_command_menu_holds_the_focus_until_escape` failed that
way, and the platform was no coincidence: macOS splits a frame across several pty reads
where Linux hands it over in one.

**A duration that is part of the client's behaviour belongs in the arithmetic, not in the
test's schedule.** The resize debounce really is a quarter of a second, so
`resize::a_drag_is_still_coalesced_into_a_few_requests` times its drag and works the
ceiling out from what the drag turned out to be, rather than asserting a fixed count that
only holds while the steps keep to schedule. It also asserts that the drag had more steps
than that ceiling, so a machine too slow to produce a drag at all says so instead of
passing for want of evidence.

Output *after* the client exits is the harness's business rather than each test's.
`FakeTerm::wait` closes its own end of the pty slave and waits for the drain thread to
reach end-of-file, so a status back means everything the client said has arrived as well.
Reading `output()` the moment `try_wait` succeeded used to miss the teardown and the line
saying why the session stopped.

## What is skipped, and why

**`live.rs`** needs the container, so it is `#[ignore]`d and out of CI.

**`perf.rs`** is a measurement, not an assertion. A shared runner cannot make it
honestly.

**Fragmented delivery is not tested**, on purpose. noVNC feeds its decoders a byte at a
time to break code that assumes a whole message arrived, because it hand-rolls a receive
queue where every decoder must check for buffered input before reading. The rfb layer
here has no bare `read()` calls at all — `read_exact` and the typed helpers, over a
`BufReader` — and `read_exact` handles a short read by contract, so a fragmenting fake
server would pass on the first run and could never fail.
