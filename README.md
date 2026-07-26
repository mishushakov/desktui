# desktui

A VNC client that draws the remote desktop in your terminal as real pixels, using
the Kitty graphics protocol — one remote pixel per terminal pixel, not a mosaic of
half-blocks.

https://github.com/user-attachments/assets/f897d7f1-bd6d-41dc-af56-a9d50dfba15e

No server handy? `make desktop` starts one in Docker and `make run` connects to it.

> [!NOTE]
> This was made over a weekend using Claude Code and Opus 5 with xhigh, to test
> frontier model capabilities.

## Features

**Any VNC server, addressed the way you already do it.** `desktui host`,
`desktui desk:1`, `desktui 10.0.0.5::5900`. Display numbers, raw ports and IPv6 parse
the way every other VNC client parses them.

**The desktop fits your window, four ways.** `--scale native | fit | integer | 1:1`,
switchable live with `Ctrl+A` then `n s i 1`. Native asks the server to resize itself
to the terminal so nothing is resampled; the other three cover servers that refuse.
Arrows pan when the view is cropped.

**A shared clipboard, both directions, in UTF-8.** Copy on the remote desktop and paste
locally; copy locally and paste there. Servers that support the `ExtendedClipboard`
extension — TigerVNC, RealVNC, noVNC — get the text as UTF-8, so Cyrillic, CJK and emoji
survive; older servers fall back to the Latin-1 messages RFB started with, which cannot
carry them. On by default, and `--no-clipboard` leaves your clipboard alone in both
directions.

**Look without touching.** `--view-only` for the session, `Ctrl+A v` to toggle. It
stops input reaching the remote and leaves the pointer for the server to draw.

**Tuned for the link you are on.** `--quality` and `--compression` trade sharpness and
server CPU against bandwidth, and `--fps` caps the frame rate. The defaults are
lossless at 60 fps; turn quality down only when the link cannot afford it — a screen
that changes wholesale, like a page being scrolled, is where lossless gets expensive,
and it is the server paying. `--no-push` goes further and takes the pace back from the
server; see below.

**Your keys reach the remote, not the terminal.** Everything is passed through,
including `Ctrl+C`. Local commands live behind `Ctrl+A`, which `--prefix` moves when it
collides with tmux.

## Installing

Needs Rust 1.88 or newer. Edition 2024 alone would only ask for 1.85, but the
terminal probe uses let-chains, which became stable in 1.88.

```
cargo install desktui --locked
```

That installs the [released version](https://crates.io/crates/desktui). To get
unreleased changes, build from the repository instead:

```
cargo install --git https://github.com/mishushakov/desktui --locked
```

Or from a checkout:

```
cargo install --path . --locked
```

All three build with the release profile and put `desktui` in `~/.cargo/bin`, which
needs to be on your `PATH`. `--locked` builds against the versions in `Cargo.lock`;
drop it to let Cargo pick newer ones.

To build without installing:

```
cargo build --release      # ./target/release/desktui
```

Run the desktui binary:

```
desktui desk:1
desktui 10.0.0.5::5900 --scale fit
desktui --print-caps          # what does this terminal support?
desktui --test-pattern        # check the pipeline without a server
```

## What makes it pixel-exact

Most terminal remote-desktop attempts scale the remote screen down to whatever the
terminal happens to be. This one asks the *server* to change instead: on connect it
sends `SetDesktopSize` for exactly the terminal's usable pixel area, so the remote
desktop is reflowed to fit and no resampling happens at all. Resize the terminal
window and the remote desktop follows.

When a server will not do that — `x11vnc` usually refuses, and some are not built
for it — a notification says why and falls back to scaling or a 1:1 view you can
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
| `t=s` shared memory | frames skip base64 and zlib, and pixels are packed straight into the object the terminal reads: ~15x the throughput on a full-screen update, and the default when offered | base64 + zlib, capped near 70 fps for full-screen motion |

Run `--print-caps` to see what your terminal answered. It reports the pixel geometry
from both `TIOCGWINSZ` and `CSI 14 t`, and warns when they disagree — that
disagreement is the HiDPI trap, and everything downstream depends on it.

## Keys

Everything goes to the remote desktop, so local commands live behind a prefix,
`Ctrl+A` by default (`--prefix`).

| `Ctrl+A` then | |
|---|---|
| `p` | the command menu |
| `q` | quit |
| `f` | full screen refresh |
| `r` | renegotiate the remote size |
| `m` | cycle the scaling mode: native / fit / integer / 1:1 |
| arrows | pan, when the view is cropped |
| `v` | toggle view-only |
| `c` | toggle statistics: ours (fps, tiles, bytes per frame) and the server's (updates per second, delivery time, megapixels per update), plus RTT and dropped frames |
| `Ctrl+A` | send a literal `Ctrl+A` |

Clipboard works in both directions: paste into the terminal and it reaches the
remote clipboard; the remote clipboard arrives locally through OSC 52. With the
`ExtendedClipboard` extension the text is UTF-8 rather than Latin-1, and each side
announces a change rather than pushing it — so a local paste crosses the wire when
something on the remote actually pastes.

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
status line and command menu stay readable on top of the remote screen.

The mouse pointer is drawn here rather than by the server: asking for the `Cursor`
pseudo-encoding gets the shape sent once and stops the server compositing it into the
framebuffer, so the pointer keeps up with your hand instead of lagging it by a round
trip. It is an image of its own with an id above every tile's, placed with sub-cell offsets and
moved with a placement rather than a transmission — so moving the mouse costs about forty
bytes, where blending it into each tile it touched used to retransmit two to four of them
per frame. Being above every tile also means it stays visible over the command menu, which
is what you click its items with. A view-only session does not ask, because there is no local
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
| pack BGRA → RGB into a buffer | 1.0 ms | 988 fps |
| `direct`: + zlib + base64 | 13.7 ms | **73 fps** |
| `shm`: an object per tile, packed then copied | 1.5 ms | **654 fps** |
| `shm`: an object per tile, packed in place | 0.9 ms | **1171 fps** |

zlib is the whole story on the direct path: compression and base64 cost twelve
milliseconds that shared memory does not pay at all. On the shared memory path it is the
pack — writing straight into the mapping the terminal will read, rather than into a buffer
for a copy to follow, which is why the last row comes in below the buffered pack above it.

One object for the whole frame instead of one per tile would be faster still, five system
calls rather than five per tile, and the protocol has the keys for it (`O=`/`S=`). Ghostty
draws nothing for a placement carrying them, so that is not what this does —
[`RENDERING.md`](RENDERING.md) has the details.

So on a local terminal, full-screen motion is not CPU-bound on this side; over SSH, where
shared memory cannot work, expect the compression ceiling. Server-side encoding and JPEG
decode come on top of both, and on a busy screen the server is usually the one that runs
out first — see `--quality`.

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

What it costs is the pace. A server pushing as fast as it can encode decides how hard
both ends work, and every update has to be decoded to keep the stream in sync whether or
not there is time to draw it — so `--fps` bounds the drawing and not the work. On a busy
screen that can saturate the server, this end, or both. `--no-push` never offers the
encoding, and ignores the announcement from a server that sends it unbidden: frames go
back to one request at a time, a round trip each.

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
make test-timing    # the wall-clock sensitive tests
make test-live      # against the real desktop container
make perf           # time the compose pipeline
```

The protocol layer is vendored rather than a dependency — [`src/rfb/README.md`](src/rfb/README.md)
says why, and what was fixed on the way in. [`TESTING.md`](TESTING.md) describes the test
suites and which of them needs what. [`RENDERING.md`](RENDERING.md) is the render
pipeline: what the graphics protocol dictates, how a resize is made cheap, and which parts
of that design are built.

The live suite needs the container:

```
make desktop
make test-live
make desktop-stop
```

TigerVNC on purpose: `-AcceptSetDesktopSize` defaults to on, so the negotiation is
genuinely exercised rather than quietly falling back. Point the client at an `x11vnc`
instance instead to exercise the other path — it usually answers "administratively
prohibited", which is what the notification has to explain.

The container publishes to `127.0.0.1` and nothing else, because VNC's password auth
is DES with an eight-character key and the session that follows is in the clear.
Reaching a real server over a network is a job for an SSH tunnel, not an open port:

```
ssh -f -N -L 5901:localhost:5901 user@host
VNC_PASSWORD=… desktui localhost::5901
```

`--log-file` is the only way to get diagnostics; anything written to stdout would
land in the middle of a graphics escape sequence.
