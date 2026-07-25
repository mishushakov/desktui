# desktui

A VNC client that draws the remote desktop in your terminal as real pixels, using
the Kitty graphics protocol — one remote pixel per terminal pixel, not a mosaic of
half-blocks.

```
desktui desk:1
desktui 10.0.0.5::5900 --scale fit
desktui --print-caps          # what does this terminal support?
desktui --test-pattern        # check the pipeline without a server
```

No server handy? `make desktop` starts one in Docker and `make run` connects to it.

## Features

**Any VNC server, addressed the way you already do it.** `desktui host`,
`desktui desk:1`, `desktui 10.0.0.5::5900`. Display numbers, raw ports and IPv6 parse
the way every other VNC client parses them.

**The desktop fits your window, four ways.** `--scale native | fit | integer | 1:1`,
switchable live with `Ctrl+A` then `n s i 1`. Native asks the server to resize itself
to the terminal so nothing is resampled; the other three cover servers that refuse.
Arrows pan when the view is cropped.

**A shared clipboard, both directions.** Copy on the remote desktop and paste locally;
copy locally and paste there. On by default, and `--no-clipboard` leaves your clipboard
alone in both directions.

**Look without touching.** `--view-only` for the session, `Ctrl+A v` to toggle. It
stops input reaching the remote and leaves the pointer for the server to draw.

**Tuned for the link you are on.** `--quality` and `--compression` trade sharpness and
server CPU against bandwidth, and `--fps` caps the frame rate. The defaults are
lossless at 60 fps; turn quality down only when the link cannot afford it.

**Your keys reach the remote, not the terminal.** Everything is passed through,
including `Ctrl+C`. Local commands live behind `Ctrl+A`, which `--prefix` moves when it
collides with tmux.

## Installing

Needs a Rust toolchain that speaks edition 2024 (1.85 or newer).

```
cargo install --git https://github.com/mishushakov/desktui --locked
```

Or from a checkout:

```
cargo install --path . --locked
```

Either one builds with the release profile and puts `desktui` in `~/.cargo/bin`, which
needs to be on your `PATH`. `--locked` builds against the versions in `Cargo.lock`;
drop it to let Cargo pick newer ones.

To build without installing:

```
cargo build --release      # ./target/release/desktui
```

## What makes it pixel-exact

Most terminal remote-desktop attempts scale the remote screen down to whatever the
terminal happens to be. This one asks the *server* to change instead: on connect it
sends `SetDesktopSize` for exactly the terminal's usable pixel area, so the remote
desktop is reflowed to fit and no resampling happens at all. Resize the terminal
window and the remote desktop follows.

When a server will not do that — `x11vnc` usually refuses, and some are not built
for it — the status line says why and falls back to scaling or a 1:1 view you can
pan.

## Requirements

A terminal that implements the Kitty graphics protocol. **Ghostty**, **kitty** and
**WezTerm** all do. A terminal with no image protocol cannot work at all. The startup
probe checks, and refuses with an explanation rather than drawing nothing.

Also used when the terminal offers it:

| Feature | What it buys | Without it |
|---|---|---|
| Mouse mode 1016 (SGR-pixel) | pointer position in pixels | pointer snaps to cell centres |
| Mode 2026 (synchronised output) | a frame commits without tearing | multi-tile frames may tear |
| Kitty keyboard protocol | real key releases, bare modifier keys | a release is synthesised after each press |
| `t=s` shared memory | frames skip base64 and zlib: ~10x the throughput on a full-screen update, and the default when offered | base64 + zlib, capped near 48 fps for full-screen motion |

Run `--print-caps` to see what your terminal answered. It reports the pixel geometry
from both `TIOCGWINSZ` and `CSI 14 t`, and warns when they disagree — that
disagreement is the HiDPI trap, and everything downstream depends on it.

## Keys

Everything goes to the remote desktop, so local commands live behind a prefix,
`Ctrl+A` by default (`--prefix`).

| `Ctrl+A` then | |
|---|---|
| `q` | quit |
| `f` | full screen refresh |
| `r` | renegotiate the remote size |
| `n` `s` `i` `1` | native / fit / integer / 1:1 mapping |
| arrows | pan, when the view is cropped |
| `v` | toggle view-only |
| `c` | toggle statistics (fps, tiles, bytes per frame, server RTT) |
| `h` `?` | help |
| `Ctrl+A` | send a literal `Ctrl+A` |

Clipboard works in both directions: paste into the terminal and it reaches the
remote clipboard; the remote clipboard arrives locally through OSC 52.

## Authentication

None and VNC password, which covers TigerVNC, x11vnc, TightVNC, QEMU and Kasm. The
password comes from `--password-file`, `$VNC_PASSWORD`, or a prompt — and the prompt
only appears if the server actually asks.

**Not supported:** macOS Screen Sharing (Apple Remote Desktop auth), RealVNC's
proprietary scheme, and VeNCrypt/TLS-only servers. These fail with a clear message
rather than hanging.

## How it draws

The screen is a grid of tiles, each about 128×128 pixels and a whole number of
cells, each with its own image id. Only tiles that changed are retransmitted, and
re-sending an id replaces the image and its placement atomically, so partial updates
need no delete traffic and never flicker. A one-pixel change costs about a kilobyte.

Tiles are placed with `z=-1`, below text and above the cell background, so the
status line and help overlay stay readable on top of the remote screen.

The mouse pointer is drawn here rather than by the server: asking for the `Cursor`
pseudo-encoding gets the shape sent once and stops the server compositing it into the
framebuffer, so the pointer keeps up with your hand instead of lagging it by a round
trip. It is blended into each tile as that tile is packed, which leaves the server's
picture untouched — nothing to save and restore, and a pointer that moves twice between
frames costs nothing extra. A view-only session does not ask, because there is no local
pointer worth drawing and the server's own is the only way to see where the real one is.

Update requests are paced against the end of each framebuffer update, so there is
never more than one in flight and the request goes out the moment the previous update
finishes — the server encodes the next frame while we are still drawing this one. A
watchdog re-asks if a server goes quiet. Frames are composed into a single buffer and
handed to a dedicated writer thread, which drops frames rather than queueing stale
ones when the terminal cannot keep up.

## Moving pictures

Full-screen video works, and the limit is CPU on this side rather than the protocol.
Measured on a 1600x832 update in 91 tiles, single threaded
(`cargo test --release --test perf -- --ignored --nocapture`):

| stage | per frame | ceiling |
|---|---|---|
| pack BGRA → RGB (every path pays this) | 1.4 ms | 700 fps |
| `direct`: + zlib + base64 | 21.0 ms | **48 fps** |
| `shm`: + one object per tile | 2.1 ms | **466 fps** |

zlib is the whole story: the 546 syscalls behind 91 shared memory objects cost 0.7 ms
between them, and compression costs twenty. So on a local terminal, full-screen
motion is not CPU-bound; over SSH, where shared memory cannot work, expect the
compression ceiling. Server-side encoding and JPEG decode come on top of both.

What would help beyond this is the **continuous updates** extension, where the server
pushes frames instead of answering a request each time. That saves one network round
trip per frame, which matters on a high-latency link and is nearly free locally. It
is not a prerequisite for video.

## Extensions it negotiates

Beyond the encodings, three extensions earn their keep.

**Continuous updates** (-313), with **Fence** (-312). The server pushes frames instead
of answering a request each time, saving a round trip per frame — the difference
between 20 fps and 40 on a 25 ms link. The server admits the extension exists by
answering `SetEncodings` with `EndOfContinuousUpdates`; from then on no requests go out
at all, and a fence is echoed back with the request bit cleared and any flag we do not
implement stripped. TigerVNC negotiates this, so `make test-live` covers it against a
real server rather than only a cooperative fake.

**Lock-key state** (`QEMULedEvent`, -261). A remote caps lock that disagrees with the
local keyboard turns every keystroke into the wrong case, and nothing in the keystroke
itself reveals it. The server reports its lock state, and when that disagrees with what
the terminal says, a lock-key tap goes out *before* the keystroke. The remembered state
is cleared at that moment: the correction takes a round trip to be reflected, and
acting on the stale value would toggle it straight back. Local state comes from the
Kitty keyboard protocol; without it we report "unknown" and leave the remote alone,
because an unset bit would otherwise read as "off".

**Quality and compression hints** (-32..-23, -256..-247), via `--quality` and
`--compression`. Both default to *unset*, and for quality that is the important case:
the spec says Tight does not use JPEG at all unless a quality level is given, so saying
nothing is the only way to ask for a lossless picture. Setting `--quality` turns JPEG
on — what you want on a link too slow for lossless, and not otherwise.

## Development

```
make test           # no server or terminal needed
make check          # fmt, clippy, tests
make test-live      # against the real desktop container
make perf           # time the compose pipeline
```

The protocol layer is vendored rather than a dependency — [`src/rfb/README.md`](src/rfb/README.md)
says why, and what was fixed on the way in.

There are three integration suites and they cover different ground.

`tests/pty.rs` runs the real binary inside a pty that the test drives, answering
capability queries the way Ghostty does and then inspecting the escape stream it
produces. `tests/vnc.rs` adds a fake VNC server, so the whole session — including
every way a server can answer a resize request — is verified without Docker.

`tests/live.rs` is the one that catches what the others cannot: it runs the binary
against **TigerVNC serving a real XFCE desktop**, so real Tight encoding, real JPEG
rectangles, and a real answer to `SetDesktopSize`. It needs the container:

```
make desktop
make test-live
make desktop-stop
```

TigerVNC on purpose: `-AcceptSetDesktopSize` defaults to on, so the negotiation is
genuinely exercised rather than quietly falling back. Point the client at an `x11vnc`
instance instead to exercise the other path — it usually answers "administratively
prohibited", which is what the status line has to explain.

The container publishes to `127.0.0.1` and nothing else, because VNC's password auth
is DES with an eight-character key and the session that follows is in the clear.
Reaching a real server over a network is a job for an SSH tunnel, not an open port:

```
ssh -f -N -L 5901:localhost:5901 user@host
VNC_PASSWORD=… desktui localhost::5901
```

`--log-file` is the only way to get diagnostics; anything written to stdout would
land in the middle of a graphics escape sequence.
