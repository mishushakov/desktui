# Rendering

How a remote framebuffer becomes terminal graphics, and why the shape is what it is.

This is a design document as much as a description. Everything below describes the pipeline
as designed; roughly two thirds of it is built. [Where this stands](#where-this-stands) says
which parts, and [Not built yet](#not-built-yet) says what the rest costs to build and what
is not obvious about it. Anything here that reads as a statement of fact about the code
should be checked against those two first.

## What the medium dictates

Every design decision below follows from one of these.

| the protocol says | so |
|---|---|
| pixels arrive as escape sequences, or as a shared memory object the terminal maps | bytes per frame is the budget that matters, and `t=s` is worth the trouble |
| a placement lands at the cursor, on a cell | tiles are whole numbers of cells, and nothing needs a sub-cell offset to get wrong |
| but `X`/`Y` offset a placement *within* its first cell | the pointer can sit between cells without being drawn into the picture |
| re-transmitting an image id replaces its data and its placements | a tile's pixels are replaced by sending them, with no delete traffic |
| the same image id *and* placement id replaces a placement, "without flicker" | a picture that only moved costs a placement each and no pixels |
| `a=p` displays an image the terminal already holds, with no payload | ditto, in about forty bytes |
| `a=d,d=I` frees one image and its placements | a tile the grid no longer reaches is dropped by name, not by erasing the screen |
| at equal `z`, the higher image id composites above | the cursor and the overlay backdrops sit above the picture without a second z-layer |
| `z=-1` is below text and above the cell background | the status line and the menu stay legible over the picture, and a blank cell does not hide it |
| `t=s` takes `O=` and `S=`, an offset and a length | one object could hold a whole frame -- except that Ghostty draws nothing for a placement carrying them, so it is an object per tile |
| DEC 2026 brackets an atomic update | a frame is one write, and the screen never shows half of one |
| `Fence` synchronises, and `EnableContinuousUpdates` takes a flag | the server's pushing can be turned off and on rather than only declined, and encode time can be told from wire time |

## The three things a resize changes

Keeping these apart is the whole reason the pipeline is shaped as it is. A resize that
changes only the third costs nothing but placements; one that changes the first costs
pixels; only the last costs everything.

| what moved | what it invalidates | cost |
|---|---|---|
| where the picture sits (centring) | placements | one `a=p` per tile |
| which source pixels are visible (crop, pan, scale) | tile contents | the tiles whose source or size changed |
| the cell size (a font change) | tile geometry | every tile |

## The pipeline

```
server updates ──► Framebuffer ──► [update boundary] ──► damage generation
                                                              │
metrics, mode, pan ──► Map (src rect → dst px → origin cell)  │
                                                              ▼
                                              Plane: per tile, held vs want
                                                              │
                                        ┌─────────────┬────────┴────────┐
                                       drop          move             send
                                     a=d,d=I         a=p              a=T
                                        └─────────────┴────────┬────────┘
chrome text plane, diffed ──► erases, draws ───────────────────┤
cursor image, a=p with X/Y ───────────────────────────────────►│
                                                               ▼
                                            one buffer, BSU … ESU, one write
```

### Framebuffer

The remote picture, in the pixel format the wire delivers. BGRA on purpose: it is what an
x86 server produces natively, so the swizzle to packed RGB happens here rather than there.
That is a deliberate trade and not an accident -- the server is usually the busier end, so
the cost belongs on this side.

Rectangles are applied as they arrive, but damage is *published* only at the end of an
update. The rectangles of one update are one picture, and a frame composed from half of
them shows half the screen at one scroll position and half at another. Making the boundary
the only way damage reaches the renderer means a frame cannot be built from an incomplete
picture -- a property of the structure rather than a flag to remember to check.

Waiting has a limit. An update whose rectangles take longer to arrive than a frame can
wait cannot be drawn both whole and often, and a screen standing still is worse than a
screen with a seam.

### Map

Where the source goes on screen: the visible source rectangle, the destination size in
pixels, the cell the top-left corner lands on, the cell size, the scale factor and the
resampling filter. A pure function of terminal metrics, scaling mode, source size and pan.

Centring is on a cell boundary. Sub-cell centring would need placement offsets on every
tile, and half a cell of asymmetry is not worth it.

### Plane

The one piece of state that matters, and the one that makes resize cheap. For each tile,
what the terminal is holding and where:

```rust
struct Tile { held: Option<Content>, placed_at: Option<(u16, u16)>, touched: u32 }

/// Everything that decides a tile's pixels. Two equal keys are the same picture,
/// whatever layout produced them.
#[derive(PartialEq)]
struct Content {
    dst: Rect,             // where it is, in destination pixels
    from: (u32, u32),      // where the visible part of the source starts
    scale: f64,            // destination pixels per source pixel
    filter: Option<Filter>,
    generation: u32,       // the damage that last touched it
}
```

The mapping goes in the key rather than the tile's source rectangle, and that is the part
worth getting right. Under a scale the source region behind a destination rectangle is
fractional, so rounding it to a `Rect` makes two different mappings compare equal -- which
is a resize in `fit` keeping tiles whose pixels have all changed. `scale` decides the same
thing exactly: it is `1.0` in the 1:1 modes however large the window is, which is why a
window that grows keeps its interior tiles, and a different number every time a `fit` window
is resized, which is why that one does not.

A tile's id is its *position*: `IMAGE_ID_BASE + ty * TILE_ID_STRIDE + tx`. Numbering by
drawing order instead -- `ty * nx + tx` -- renumbers every tile a row below whenever the
grid changes width, so an id would name different pixels either side of a resize and
nothing the terminal holds could be kept. Positional ids are what make everything below
possible.

Each frame, per tile, compute the `Content` the map asks for and the generation of the
last damage that touched its source rectangle, then:

| state | action |
|---|---|
| held content and generation match, placed at the right cell | nothing |
| held content and generation match, cell differs | `a=p` -- move, no pixels |
| anything else | `a=T` -- send |
| id outside the new grid | `a=d,d=I` -- drop |

One comparison covers damage, resize, pan, mode changes and font changes. There is no
dirty bitmap, no record of how far the grid has been filled, and no comparison of one
layout against another to guess which tiles survived -- those are three approximations of
this question. The generation is a dirty bit in disguise, but a dirty bit that composes
with everything else that can make a tile wrong.

The count of what is owed is still kept, because `has_work` is asked every tick and must not
walk the grid to answer. It is maintained where the marks happen, which is the one piece of
bookkeeping the plane does not remove.

What a layout comparison cannot do, and this can: a frame that never reached the terminal
cannot leave a tile counted as held, because being held is a fact about a tile rather than
about how far the grid was filled.

### Frame builder

1. **Reconcile** the plane against the map: drops, moves, sends.
2. **Resample**, if the map scales, only the union of the sends' source rectangles plus a
   margin for the filter's support -- not the whole screen because one tile changed.
3. **Pack** each send straight into the shared memory object the terminal will read, so a
   frame is one pass over its pixels rather than a pack followed by a copy. An object per
   tile: the protocol would allow one per frame with an offset per tile, five system calls
   instead of five per tile, but see [the offset finding](#the-o-finding).
4. **Emit**, in this order, into one buffer: drops, chrome erases, moves, sends, chrome
   draws. Erases precede placements because a terminal may treat clearing a cell as
   dropping the placement under it.
5. **Submit** as one write inside `?2026h` … `?2026l`, and update the plane only if the
   writer took it. A frame the writer was too busy for was composed and thrown away; a
   tile recorded as held that was never sent is a hole that survives every later frame.

### Chrome

**A note for anyone writing a test.** Because the chrome is diffed, text that is on screen
is not necessarily contiguous in the escape stream: a bar reading `1600x832` where it read
`1024x768` reaches the terminal as the digits that differ and a cursor move between them, so
grepping the stream for the phrase finds nothing. `Screen::replay` in the test harness
reconstructs the cells, and every assertion about what the chrome says goes through it.

A private plane of the cells we own -- status line, menu, notifications -- double buffered
and diffed each frame. The diff produces the minimal cursor-positioned writes *and* the
cells the chrome has vacated, which the plane takes as damage so the picture beneath is
repaired.

A cell-diffing renderer that owned the whole screen would be wrong here: it would repaint
cells our placements live in, and those are not its to repaint. Owning one plane and
diffing that is the same idea with the boundary in the right place.

Overlay backdrops -- the menu's, a notification's -- are images of ours with ids above
every tile. A cell background cannot serve: it is painted *below* the picture, so a colour
set there is never seen.

### Cursor

One RGBA image with an id above every tile, placed with sub-cell `X`/`Y` offsets and moved
with `a=p`. Drawing the pointer locally is what lets it move at local speed rather than at
the speed of a round trip; compositing it into tile data would mean two tiles retransmitted
per motion event, where a placement is forty bytes.

## The resize path

1. A terminal resize arrives. Rate limited to one every hundred milliseconds, leading edge
   -- the first after a lull is acted on at once. This is what TigerVNC and noVNC both
   settled on, TigerVNC having moved deliberately away from a longer idle period because
   waiting for a drag to finish makes maximising feel like it did not take.
2. Query the metrics; compute the new map.
3. Erase the screen's text, and re-lay out the chrome in full. This is the one place a diff
   cannot be trusted: what a terminal leaves on the alternate screen after the window
   changed shape is not specified, so a client that assumes its cells kept their coordinates
   is guessing, and the guess showed -- fragments of an old command menu stranded around the
   screen. Cheap to be certain instead, because chrome is text.
4. Reconcile the plane against the new map, then put every tile the terminal still holds
   back on its cells with `a=p`. Forty bytes each and no pixels, and it settles the other
   half of the same question: whether erasing a cell drops the placement under it.
5. In native mode, ask the server for the new size -- one request in flight at a time, so
   a drag cannot pile them up.
6. Emit one frame. The erase is **inside** it, in the same synchronised block that draws the
   new layout, so the screen is never seen with less on it than before -- which is the whole
   difference between this and the wipe that used to go out on its own.
7. The server's new desktop size arrives as a complete update, producing a new map and a
   second reconciliation. Interior tiles survive it if their content key does.

## What it costs

| event | cost |
|---|---|
| pointer motion | one `a=p`, about forty bytes |
| caret blink | one tile |
| window grown two cells, 1:1 | the new column and row, plus the edge tiles that were clipped |
| window re-centred | one `a=p` per tile, no pixels |
| any resize, on top of the above | one erase and the chrome said again, plus one `a=p` per surviving tile |
| scaled resize | every tile, and one resample |
| font size change | every tile |
| full-screen frame, shared memory | five system calls per tile, one pass over the pixels |

## What to measure

Tiles sent, moved and dropped per frame; bytes and system calls per frame; resample, pack
and emit in milliseconds. And the other half, which is the half that decides whether any of
the rest is worth touching: what the *server* is doing.

`Ctrl+A c` reports both. Ours is the frame rate, tiles and bytes; theirs is three numbers
that fall out of the update boundary `mid_update` already tracks -- **updates per second**,
which with frames being pushed *is* the server's frame rate; **delivery time**, from an
update's first rectangle to its `FrameEnd`; and **picture per update** in megapixels, which
turns the two into a rate and separates a big screen from a slow server. Six updates
arriving while we could draw sixty is not a rendering problem, and no amount of work on
this side will move it -- the thing to reach for is `--quality`, which decides how hard the
server works and is otherwise invisible unless you read `--help`.

Megapixels rather than wire bytes because the byte count the server actually sent is behind
the protocol seam; getting at it wants a counting reader in the vendored client. And encode
time is not separable from wire time yet: `rtt` is measured from the request to `FrameEnd`,
so it contains the server's encoding. Splitting them wants a fence sent behind the update
request, whose reply says the server reached that point in its stream -- worth doing
alongside [pushing turned off and on again](#pushing-turned-off-and-on-again-rather-than-declined),
which needs the same fence bookkeeping.

## Where this stands

Built:

- positional tile ids, `TILE_ID_STRIDE` (`src/render/mod.rs`)
- atomic frames, one write per frame inside DEC 2026 (`src/session.rs`, `src/term/writer.rs`)
- no screen erase *outside a frame*: per-id tile drops, and a resize's erase carried into the
  synchronised block that redraws (`src/render/mod.rs`, `src/ui/chrome.rs`, `src/session.rs`)
- moves as `a=p` for a picture that only shifted (`src/term/kitty.rs`)
- damage published at the update boundary (`src/session.rs`)
- resize rate limit, leading edge (`src/session.rs`)
- tiles packed straight into the shared memory object the terminal reads, rather than into
  a buffer for a copy to follow (`src/term/shm.rs`, `src/render/framebuffer.rs`) --
  1.5 ms/frame to 0.9 on a full-screen update, measured by `make perf`
- shared memory swept by presence and a byte budget rather than a deadline
  (`src/term/shm.rs`)
- resampling only the region a frame is about to send, grown by the filter's reach
  (`src/render/scale.rs`)
- the server's own delivery in the statistics -- updates per second, delivery time,
  megapixels per update -- next to ours (`src/session.rs`)
- the plane: per-tile content keys in place of a dirty bitmap, a placed extent and a
  layout comparison (`src/render/mod.rs`)
- the pointer as its own placement, moved with `a=p` and sub-cell `X`/`Y`, above every tile
  (`src/term/kitty.rs`, `src/render/mod.rs`) -- verified against Ghostty with
  `make blend-probe` before a line of it was written
- the chrome as one diffed plane (`src/ui/chrome.rs`), which deleted `Menu::clear`,
  `status::clear`, `Session::clear_menu`, `clear_chrome_drawn_with` and the popup's
  `drawn`/`stale`/`moved` bookkeeping

## Not built yet

One thing left. What it *is* is above; what follows is what it costs to build and the parts
that are not obvious from the design -- including why it is worth being careful with.

### Pushing turned off and on again, rather than declined

This item said "Fence as flow control", on the reasoning that a client could hold a frame's
worth of credit and let the server run exactly as far ahead as it can draw. That is the
right *shape*, but Fence is not what does it, and it is worth correcting before anyone
builds against the wrong lever.

The throttle is already in the ContinuousUpdates extension. `EnableContinuousUpdates` takes
a flag, and the spec has the server stop pushing and answer with `EndOfContinuousUpdates`
when a client sets it to zero. So the credit protocol is: pushing on while we keep up, off
when we fall behind, on again when we catch up -- and the handshake for "it has actually
stopped" is a message that already exists. No fence required.

What Fence *is* for here is the measurement the statistics cannot make: `rtt` runs from an
update request to `FrameEnd` and so contains the server's encoding time, and a fence sent
behind the request separates the two. Worth having, but it is an instrument, not a control.

So the work is:

- `Input::Push(bool)` at the seam, and `VncBackend` distinguishing "we asked it to stop"
  from "it stopped of its own accord" -- today *every* `EndOfContinuousUpdates` re-arms
  pushing (`src/remote/vnc.rs`), which would fight a deliberate disable.
- A policy in the session. The numbers to drive it are now on screen: frames dropped, and
  the update rate against the frame rate.
- `--no-push` stays as the floor: the flag for a server whose pushing you want nothing to
  do with, and the first thing to try when the question is "is the server the problem?".

**The hazard, and why this is last of the four in practice.** A control loop that gets it
wrong oscillates, and one that disables without re-enabling leaves a frozen screen -- a
worse failure than the flat-out pushing it replaces. It wants sustained load to tune
against, which `make desktop` plus a scrolling browser gives and a test does not. Build the
policy behind a flag, default off, until it has been watched for a while.

### The `O=` finding

One object per frame, with every tile placed out of it at an offset, was built and then
taken out again. It is worth writing down so it is not built a second time by accident.

The protocol has the keys. The spec says a client "can also specify a size and offset to
tell the terminal emulator to only read a part of the specified file... using the `S` and
`O` keys", and its own example uses them with `t=s`:
`_Gs=10,v=2,t=s,S=80,O=10;<encoded name>`. What was emitted matched that shape, with
`S = w * h * 3` for `f=24`.

Ghostty drew nothing for it. The screen was black with the occasional tile appearing as
the pointer moved -- consistent with the one placement per frame that carries `O=0` being
accepted and every other one being dropped, though that was not confirmed. And it was
silent, because `q=2` suppresses the reply that would have said why.

To take it up again, in this order:

1. Reproduce it deliberately with `q=0` and read what the terminal answers. That turns a
   guess into a fact, and it is two lines of shell against a hand-made object.
2. Try kitty as well. If it works there, this is a Ghostty gap worth reporting rather than
   a misreading of the spec.
3. Whatever the answer, gate it on the capability probe rather than on the terminal's name.
   `src/term/caps.rs` already probes `t=s` with `a=q`, which validates a transmission
   without storing anything -- the same probe with `S=` and `O=` set answers this question
   at startup, and the answer decides whether a frame is one object or one per tile.

The prize, measured by `make perf` on a full-screen update: 0.9 ms/frame against 0.6, most
of it system calls. Worth having, not worth guessing at.
`pixels_travel_through_shared_memory_when_the_terminal_offers_it` asserts that no `O=`
appears in a placement, so this cannot come back without someone reading this first.

### Considered and rejected

**Damage patches above the tiles.** A placement can display a source rectangle of a
transmitted image, so a frame could send its coalesced damage as one patch image above the
tile layer and fold patches into tiles later. A caret blink would cost 256 bytes instead of
a 128x136 tile. Rejected: it buys nothing for the case that actually hurts, since a
scrolling page dirties everything either way, and it costs patch lifetime, ordering and
fold-in policy. It profiles well on the case we are already fast at.

**Tile size as a constant.** `TILE_TARGET_PX = 128` is a tuning parameter dressed as a law:
whole-screen scrolling wants larger tiles to amortise the escape and syscall overhead, a
caret wants smaller. Choosing it from the layout, or from the damage pattern, is a real
improvement -- but measure first, and note that the per-tile system calls are still there,
so the argument for larger tiles is stronger than it looked when one object per frame
seemed to be on the table.
