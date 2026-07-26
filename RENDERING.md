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
| `Fence` synchronises | in-flight work can be bounded without giving up pushed frames |

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
struct Tile { held: Option<Held>, placed_at: Option<(u16, u16)> }
struct Held { content: Content, generation: u32 }

/// Everything that decides a tile's pixels. Two equal keys are the same picture,
/// whatever layout produced them.
#[derive(PartialEq)]
struct Content { src: Rect, size: (u32, u32), filter: Filter }
```

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
3. Re-lay out the chrome. Its diff hands back the cells it vacated; the plane takes them
   as damage.
4. Reconcile the plane against the new map.
5. In native mode, ask the server for the new size -- one request in flight at a time, so
   a drag cannot pile them up.
6. Emit one frame. **Nothing erases the screen.** Tiles are dropped by id and text is
   diffed, so there is never a moment with less on screen than before.
7. The server's new desktop size arrives as a complete update, producing a new map and a
   second reconciliation. Interior tiles survive it if their content key does.

## What it costs

| event | cost |
|---|---|
| pointer motion | one `a=p`, about forty bytes |
| caret blink | one tile |
| window grown two cells, 1:1 | the new column and row, plus the edge tiles that were clipped |
| window re-centred | one `a=p` per tile, no pixels |
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
alongside [Fence as flow control](#2-fence-as-flow-control), which has to solve the same
probe-versus-flow-control problem anyway.

## Where this stands

Built:

- positional tile ids, `TILE_ID_STRIDE` (`src/render/mod.rs`)
- atomic frames, one write per frame inside DEC 2026 (`src/session.rs`, `src/term/writer.rs`)
- no screen erase on relayout: per-id drops, targeted chrome erases (`src/render/mod.rs`, `src/ui/status.rs`)
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

## Not built yet

Four things, ordered by what each is worth rather than by how tidy it would be. What each
one *is* is above; what follows is what it costs to build and the parts that are not
obvious from the design.

The first two are felt immediately; the last two are structural, worth building for the bugs
they make impossible rather than for anything measurable. The one ordering constraint is
that the cursor is easier before the plane, not after: it deletes two of the marking paths
the plane would otherwise have to convert.

### 1. The cursor as a placement

Replaces `blend_cursor`, `cursor_rect` and the cursor's dirty-marking in
`src/render/mod.rs` with one image whose id is above every tile's, placed with sub-cell
`X`/`Y` offsets and moved with `a=p`.

**Answer this first: `make blend-probe`.** The protocol says semi-transparent placements
that overlap are blended, and that at equal `z` the higher id is on top. Whether Ghostty
actually blends an RGBA (`f=32`) placement over another placement -- rather than compositing
it against the cell background, or drawing it opaque, or dropping it -- is not something the
docs settle, and getting it wrong means a black or solid rectangle where the pointer should
be.

`docker/blend-probe.sh` draws a green square with a half-transparent red one over its
middle, at the same `z` and a higher id, positioned with the `X`/`Y` sub-cell offsets this
plan also depends on. An olive middle means blended and the plan is sound. Solid red means
opaque, black means composited against the cell background, and nothing means the placement
was dropped -- all three mean the cursor stays blended into tiles and this needs rethinking
rather than writing. Run it on Ghostty and on kitty; a difference between them is a bug
report rather than a misreading of the spec.

This is not caution for its own sake. One shared memory object per frame was built from the
spec without a terminal to try it against, and the answer was a black screen -- see
[the offset finding](#the-o-finding).

If it holds, the rest is small. `X`/`Y` must be smaller than the cell, so the placement
cell is `hotspot / cell_size` and the offset is the remainder.

What it is worth: moving the pointer marks the tile it left and the tile it arrived at, and
a cursor straddling a boundary touches four. So a frame while the mouse is moving
retransmits two to four tiles, at fifty-odd kilobytes packed each, up to sixty times a
second -- **six to twelve megabytes a second of shared memory traffic, and hundreds of
system calls, for moving a pointer over a picture that has not changed**. As a placement it
is one release and one `a=p`: about forty bytes. It also takes the cursor out of the tile
hot path, where `blend_cursor` currently runs over every tile it touches on every frame.

It also fixes a wart nobody has reported: `cursor_at` is in destination pixels, and a
resize changes what a given screen position maps to, but nothing re-derives it -- so after
a resize the pointer is drawn at a stale spot until it next moves. A placement is
re-placed from the new map as a matter of course.

### 2. Fence as flow control

`--no-push` declines continuous updates wholesale, at a round trip per frame. Fence exists
to bound in-flight work without giving that up: the server answers one when it reaches that
point in its stream, so a client can hold a frame's worth of credit and let the server run
exactly as far ahead as it can draw.

This is the principled version of a problem already met and worked around. `--quality`
reduces how expensive a frame is to *encode*; this bounds how many the server *attempts*.
Today the choice is binary -- pushed frames with no back-pressure at all, or `--no-push` at
a round trip each, which the README puts at the difference between twenty frames a second
and forty on a 25 ms link. With credit, a server that can only encode ten full screens a
second is asked for ten, instead of encoding flat out while this end decodes frames it will
never have time to draw.

Needs a real server to tune against, and one trap: fences are already used for the latency
probe, marked by `RTT_PROBE_MARKER` in `src/remote/vnc.rs`. A flow-control fence has to be
distinguishable from a probe, or the round-trip figure becomes a measurement of the frame
queue.

`--no-push` stays regardless -- it is the fallback for a server without Fence, and the
thing to reach for when the question is "is the server the problem?".

### 3. The plane

Replaces three mechanisms with the one [Plane](#plane) describes: `dirty: Vec<bool>`,
`placed: (u16, u16)` and `Layout::maps_alike`, all in `src/render/mod.rs`.

Mechanical, once two things are seen:

**`commit` can recompute rather than remember.** The obvious reading is that composing has
to record what it sent so the commit can promote it, which wants a second structure
alongside the plane. It does not: `want` is a pure function of the layout, the grid and the
tile's damage generation, so the commit can compute it again. Every tile is then
`held = want; placed_at = cell` -- which is what today's `placed = (nx, ny)` already is,
generalised from an extent to a key.

**The derived count stays.** `has_work` is called every tick and must not walk the grid
computing keys. So the plane keeps its out-of-date count the way the dirty bitmap does
today, maintained where the marks happen rather than recomputed: `mark`, `mark_dst`,
`mark_cells`, `mark_all`, `move_cursor`, `set_cursor`, `relayout`. Those are the paths to
change, and they are the same paths that carry today's bitmap -- the change is what a mark
*writes*, not where marks come from. Two of them go away entirely if the cursor becomes a
placement first, which is one reason to do that one before this one.

The padding in `Layout::src_to_dst` stays as it is: it answers which tiles a source pixel
can reach, which is still the question a damage rectangle asks.

One behaviour changes, and it is the point of the exercise: the density invariant becomes
structural. `placed` as a rectangle is only correct because frames arrive whole -- a per-tile
key cannot be wrong that way, so "a frame dropped mid-resize leaves a hole" stops being
something to reason about and becomes something that cannot happen.

What it does *not* buy, despite an earlier claim here: tile reuse across a scaled resize.
Comparing keys rather than layouts would allow it in principle, but a resize in `fit` changes
the scale, which changes every tile's destination size, which changes every key. The case
where a scaled tile's source *and* size both survive is real but rare -- a mode switch that
happens to land on the same geometry -- and not a reason to do this.

Verification is the existing suite, which is why this is worth doing carefully rather than
quickly: `growing_the_window_sends_the_new_tiles_and_not_the_rest`,
`moving_the_picture_costs_placements_and_not_pixels`,
`a_frame_that_never_reached_the_terminal_leaves_its_tiles_owing` and
`a_grid_that_only_grows_keeps_the_tiles_that_did_not_move` are the four that would catch a
mistake. Add one for the invariant itself: a frame composed and dropped, then a resize,
must not leave a tile counted as held.

### 4. The chrome as one diffed plane

Replaces `Toast::drawn`/`stale`/`moved`, `Session::clear_menu`, `status::clear` and
`Menu::clear` with a private cell buffer, double buffered and diffed per frame.

The rule that makes it correct is the one `src/ui/mod.rs` already states: never touch a
cell we did not write. A renderer that owns the whole screen is wrong here because it would
repaint the cells our placements live in; owning one plane and diffing that is the same
idea with the boundary in the right place. `ui::paint::write_cells` is most of the emit
side already, including the wide-glyph handling that a naive diff gets wrong.

Two things fall out rather than being added: the diff yields the cells the chrome has
vacated, which is exactly the damage `mark_cells` wants, so the "erased chrome is damage"
rule stops being a thing to remember; and erases precede placements because that is the
order the emit does it in, rather than because three call sites each remember to.

The overlay backdrops stay as images. A cell background cannot serve -- it is painted below
the picture, so a colour set there is never seen.

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
