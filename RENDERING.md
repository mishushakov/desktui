# Rendering

How a remote framebuffer becomes terminal graphics, and why the shape is what it is.

This is a design document as much as a description: the parts already built are marked as
such at the end, and the rest is where the pipeline is going. Anything here that reads as
a statement of fact about the code should be checked against
[the last section](#where-this-stands) first.

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
| `t=s` takes `O=` and `S=`, an offset and a length | one shared memory object per *frame*, not per tile |
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

Two cases fall out of it that a layout comparison gets wrong: a scaled resize where a
given tile's source and size happen to survive is kept for free, and a frame that never
reached the terminal cannot leave a tile counted as held.

### Frame builder

1. **Reconcile** the plane against the map: drops, moves, sends.
2. **Resample**, if the map scales, only the union of the sends' source rectangles plus a
   margin for the filter's support -- not the whole screen because one tile changed.
3. **Pack** each send straight into the frame's shared memory object, at its own offset.
   One pass over the pixels, and one object per frame rather than one per tile: a
   full-screen frame is five system calls instead of five hundred tiles' worth.
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
| full-screen frame, shared memory | five system calls, one pass over the pixels |

## What to measure

Tiles sent, moved and dropped per frame; bytes and system calls per frame; resample, pack
and emit in milliseconds; and the one number still missing -- how long the *server* spent
encoding. Without it, a slow session looks like a rendering problem, and the first thing
to reach for is `--quality`, which decides how hard the server works and is invisible
unless you read `--help`.

## Where this stands

Built:

- positional tile ids, `TILE_ID_STRIDE` (`src/render/mod.rs`)
- atomic frames, one write per frame inside DEC 2026 (`src/session.rs`, `src/term/writer.rs`)
- no screen erase on relayout: per-id drops, targeted chrome erases (`src/render/mod.rs`, `src/ui/status.rs`)
- moves as `a=p` for a picture that only shifted (`src/term/kitty.rs`)
- damage published at the update boundary (`src/session.rs`)
- resize rate limit, leading edge (`src/session.rs`)
- one shared memory object per frame, `O=`/`S=` offsets, packed straight into the mapping
  (`src/term/shm.rs`, `src/render/framebuffer.rs`) -- 1.5 ms/frame to 0.7 on a full-screen
  update, measured by `make perf`

Not built, in the order worth doing:

- **the plane as described**: `Content` keys instead of `dirty: Vec<bool>`, `placed` as a
  rectangle, and `Layout::maps_alike`.
- **resample only the dirty region** in scaled modes.
- **the cursor as a placement** with `X`/`Y`, replacing `blend_cursor`.
- **the chrome as one diffed plane**, replacing `Toast`'s own stale-cell bookkeeping,
  `clear_menu`, and `status::clear`.
- **shared memory swept by a byte budget**, not a five hundred millisecond timer: an
  object that still exists is one the terminal has not read yet, so a deadline is the
  wrong rule.
- **`Fence` as flow control**, so pushed frames can be bounded instead of declined
  wholesale with `--no-push`.
