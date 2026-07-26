//! Turning a remote framebuffer into terminal graphics.
//!
//! The screen is divided into tiles that are whole numbers of cells, each with a
//! stable image id. Only tiles touched since the last frame are retransmitted,
//! and retransmitting an id atomically replaces the image and its placement, so
//! partial updates need no delete traffic and never flicker.
//!
//! Tiles are cell-aligned on purpose: a placement lands at the cursor, so an
//! aligned tile needs nothing but a cursor move, with no sub-cell offsets to get
//! wrong.

pub mod framebuffer;
pub mod scale;
pub mod testpattern;

use crate::cli::ScaleMode;
use crate::term::Metrics;
use crate::term::kitty::{
    At, CURSOR_IMAGE_ID, IMAGE_ID_BASE, KittyEncoder, Placement, delete_image, hide_image,
    place_existing, place_existing_at, place_rgba,
};
use crate::term::shm::ShmPool;
use framebuffer::Framebuffer;
use scale::{Filter, Scaler};

/// Tiles aim for this many pixels on a side. Small enough that a cursor blink
/// costs almost nothing, large enough that zlib has something to work with and
/// the per-tile escape overhead stays in the noise.
const TILE_TARGET_PX: u32 = 128;

/// Tiles per row of the image id space.
///
/// A tile's id is `IMAGE_ID_BASE + ty * TILE_ID_STRIDE + tx`, which is what makes an
/// id mean the same rectangle before and after a resize. Numbering them in the order
/// they are drawn -- `ty * nx + tx` -- renumbers every tile on the row below whenever
/// the grid changes width, so an id would name different pixels either side of a
/// resize and none of what the terminal already holds could be kept.
///
/// A tile is never narrower than [`TILE_TARGET_PX`], and no terminal reports a pixel
/// size that does not fit a `u16`, so 512 of them is past any grid a real window can
/// ask for. [`TileGrid::new`] refuses to exceed it rather than let two tiles collide
/// on one id.
pub const TILE_ID_STRIDE: u16 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn right(&self) -> u32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> u32 {
        self.y + self.h
    }

    pub fn area(&self) -> u64 {
        u64::from(self.w) * u64::from(self.h)
    }

    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        (right > x && bottom > y).then(|| Rect::new(x, y, right - x, bottom - y))
    }

    /// Grow by `pad` on every side, without going below zero.
    pub fn expand(&self, pad: u32) -> Rect {
        let x = self.x.saturating_sub(pad);
        let y = self.y.saturating_sub(pad);
        Rect::new(
            x,
            y,
            self.w + (self.x - x) + pad,
            self.h + (self.y - y) + pad,
        )
    }
}

/// Where the remote framebuffer goes on screen, and at what scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub mode: ScaleMode,
    /// Full size of the remote framebuffer.
    pub src_w: u32,
    pub src_h: u32,
    /// The part of the remote framebuffer that is visible. Smaller than the
    /// whole thing only when panning a 1:1 view.
    pub src: Rect,
    /// Size of the drawn image, in terminal pixels.
    pub dst_w: u32,
    pub dst_h: u32,
    /// Cell holding the image's top-left corner.
    pub origin_col: u16,
    pub origin_row: u16,
    pub cell_w: u32,
    pub cell_h: u32,
    /// Pixel area available to the image.
    pub area_w: u32,
    pub area_h: u32,
    /// Destination pixels per source pixel.
    pub scale: f64,
}

impl Layout {
    /// Work out the layout for a source of `src_w` x `src_h`.
    ///
    /// `pan` is the top-left of the visible window in source pixels, and is
    /// clamped to whatever actually fits; it only matters in 1:1 modes.
    pub fn compute(
        metrics: &Metrics,
        mode: ScaleMode,
        src_w: u32,
        src_h: u32,
        pan: (u32, u32),
    ) -> Self {
        let (area_w, area_h) = metrics.image_area();
        let (cell_w, cell_h) = (metrics.cell_w.max(1), metrics.cell_h.max(1));

        // `Native` means "the server agreed to be exactly our size". Until it
        // does, or when it refuses, it behaves as a 1:1 window.
        let one_to_one = matches!(mode, ScaleMode::Native | ScaleMode::OneToOne);

        let (src, dst_w, dst_h, scale) = if one_to_one {
            let vis_w = src_w.min(area_w);
            let vis_h = src_h.min(area_h);
            let x = pan.0.min(src_w - vis_w);
            let y = pan.1.min(src_h - vis_h);
            (Rect::new(x, y, vis_w, vis_h), vis_w, vis_h, 1.0)
        } else if src_w == 0 || src_h == 0 {
            (Rect::new(0, 0, 0, 0), 0, 0, 1.0)
        } else {
            let fit =
                (f64::from(area_w) / f64::from(src_w)).min(f64::from(area_h) / f64::from(src_h));
            let scale = match mode {
                // Whole-number scaling only, and never below 1: if the remote
                // desktop is larger than the terminal there is no integer
                // factor, so fall back to fitting it.
                ScaleMode::Integer if fit >= 1.0 => fit.floor(),
                _ => fit,
            };
            let dst_w = ((f64::from(src_w) * scale).round() as u32).clamp(1, area_w);
            let dst_h = ((f64::from(src_h) * scale).round() as u32).clamp(1, area_h);
            (Rect::new(0, 0, src_w, src_h), dst_w, dst_h, scale)
        };

        // Centre on a cell boundary. Sub-cell centring would need placement
        // offsets, and half a cell of asymmetry is not worth the complexity.
        let origin_col = ((area_w.saturating_sub(dst_w)) / 2 / cell_w) as u16;
        let origin_row = ((area_h.saturating_sub(dst_h)) / 2 / cell_h) as u16;

        Self {
            mode,
            src_w,
            src_h,
            src,
            dst_w,
            dst_h,
            origin_col,
            origin_row,
            cell_w,
            cell_h,
            area_w,
            area_h,
            scale,
        }
    }

    /// True when the source has to be resampled to reach the destination.
    pub fn needs_scaling(&self) -> bool {
        self.dst_w != self.src.w || self.dst_h != self.src.h
    }

    pub fn filter(&self) -> Filter {
        // A whole-number factor is pixel doubling, which should stay crisp.
        let integral = (self.scale - self.scale.round()).abs() < 1e-9;
        if integral && self.scale >= 1.0 {
            Filter::Nearest
        } else {
            Filter::Lanczos3
        }
    }

    /// Is every remote pixel landing on exactly one terminal pixel?
    pub fn is_pixel_exact(&self) -> bool {
        !self.needs_scaling()
    }

    /// Is any of the remote framebuffer hidden off-screen?
    pub fn is_cropped(&self) -> bool {
        self.src.w < self.src_w || self.src.h < self.src_h
    }

    /// How far the visible window can be panned, in source pixels.
    pub fn pan_limits(&self) -> (u32, u32) {
        (
            self.src_w.saturating_sub(self.src.w),
            self.src_h.saturating_sub(self.src.h),
        )
    }

    /// Map a rectangle of source pixels to destination pixels, padded for the
    /// resampling filter's reach and clipped to the image.
    pub fn src_to_dst(&self, r: Rect) -> Option<Rect> {
        let visible = r.intersect(&self.src)?;
        let rel_x = visible.x - self.src.x;
        let rel_y = visible.y - self.src.y;

        let x0 = (f64::from(rel_x) * self.scale).floor() as u32;
        let y0 = (f64::from(rel_y) * self.scale).floor() as u32;
        let x1 = (f64::from(rel_x + visible.w) * self.scale).ceil() as u32;
        let y1 = (f64::from(rel_y + visible.h) * self.scale).ceil() as u32;

        let dst = Rect::new(x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0));
        let padded = if self.needs_scaling() {
            dst.expand(self.filter().dst_padding())
        } else {
            dst
        };
        padded.intersect(&Rect::new(0, 0, self.dst_w, self.dst_h))
    }

    /// Map a terminal pixel position into destination pixels, or `None` when it is
    /// outside the drawn image.
    pub fn terminal_px_to_dst(&self, tx: u32, ty: u32) -> Option<(u32, u32)> {
        let ox = u32::from(self.origin_col) * self.cell_w;
        let oy = u32::from(self.origin_row) * self.cell_h;
        if tx < ox || ty < oy {
            return None;
        }
        let (dx, dy) = (tx - ox, ty - oy);
        (dx < self.dst_w && dy < self.dst_h).then_some((dx, dy))
    }

    /// Map a terminal pixel position to a source pixel, clamped into the image.
    ///
    /// Returns `None` when the position is outside the drawn image, so a click
    /// on the letterbox is not reported as a click on its nearest edge.
    pub fn terminal_px_to_src(&self, tx: u32, ty: u32) -> Option<(u16, u16)> {
        let ox = u32::from(self.origin_col) * self.cell_w;
        let oy = u32::from(self.origin_row) * self.cell_h;
        if tx < ox || ty < oy {
            return None;
        }
        let (dx, dy) = (tx - ox, ty - oy);
        if dx >= self.dst_w || dy >= self.dst_h {
            return None;
        }
        let sx = self.src.x + (f64::from(dx) / self.scale) as u32;
        let sy = self.src.y + (f64::from(dy) / self.scale) as u32;
        Some((
            sx.min(self.src.right().saturating_sub(1)) as u16,
            sy.min(self.src.bottom().saturating_sub(1)) as u16,
        ))
    }
}

/// The tile grid covering the drawn image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TileGrid {
    /// Cells per tile.
    tile_cols: u16,
    tile_rows: u16,
    /// Tiles across and down.
    nx: u16,
    ny: u16,
    cell_w: u32,
    cell_h: u32,
    dst_w: u32,
    dst_h: u32,
}

impl TileGrid {
    fn new(layout: &Layout) -> Self {
        let (cell_w, cell_h) = (layout.cell_w.max(1), layout.cell_h.max(1));
        let tile_cols = TILE_TARGET_PX.div_ceil(cell_w).max(1) as u16;
        let tile_rows = TILE_TARGET_PX.div_ceil(cell_h).max(1) as u16;

        let tile_px_w = u32::from(tile_cols) * cell_w;
        let tile_px_h = u32::from(tile_rows) * cell_h;
        // Clamped rather than allowed to wrap around the id space, where two tiles far
        // apart would take the same id and fight over one image. It costs the far edge of
        // a window that no terminal can be asked to open, which is the better failure.
        let nx = grid_side(layout.dst_w.div_ceil(tile_px_w), "columns");
        let ny = grid_side(layout.dst_h.div_ceil(tile_px_h), "rows");

        Self {
            tile_cols,
            tile_rows,
            nx,
            ny,
            cell_w,
            cell_h,
            dst_w: layout.dst_w,
            dst_h: layout.dst_h,
        }
    }

    fn len(&self) -> usize {
        usize::from(self.nx) * usize::from(self.ny)
    }

    /// Image id for the tile at `(tx, ty)`.
    ///
    /// A place in the grid rather than a place in the drawing order, so the id survives
    /// a resize: whatever the grid's width becomes, this tile is still the tile that was
    /// there before, and the image the terminal holds under that id is still its pixels.
    fn id(&self, tx: u16, ty: u16) -> u32 {
        IMAGE_ID_BASE + u32::from(ty) * u32::from(TILE_ID_STRIDE) + u32::from(tx)
    }

    /// Destination-pixel rectangle covered by a tile, clipped at the edges.
    fn tile_rect(&self, tx: u16, ty: u16) -> Rect {
        let x = u32::from(tx) * u32::from(self.tile_cols) * self.cell_w;
        let y = u32::from(ty) * u32::from(self.tile_rows) * self.cell_h;
        let w = (u32::from(self.tile_cols) * self.cell_w).min(self.dst_w.saturating_sub(x));
        let h = (u32::from(self.tile_rows) * self.cell_h).min(self.dst_h.saturating_sub(y));
        Rect::new(x, y, w, h)
    }

    /// Inclusive tile index range covering a destination rectangle.
    fn tiles_covering(&self, r: Rect) -> (u16, u16, u16, u16) {
        let tw = u32::from(self.tile_cols) * self.cell_w;
        let th = u32::from(self.tile_rows) * self.cell_h;
        let x0 = (r.x / tw) as u16;
        let y0 = (r.y / th) as u16;
        let x1 =
            ((r.right().saturating_sub(1)) / tw).min(u32::from(self.nx.saturating_sub(1))) as u16;
        let y1 =
            ((r.bottom().saturating_sub(1)) / th).min(u32::from(self.ny.saturating_sub(1))) as u16;
        (x0, y0, x1, y1)
    }
}

/// Packed RGB bytes one tile comes to.
fn tile_bytes(tile: Rect) -> usize {
    (tile.w as usize) * (tile.h as usize) * 3
}

/// One side of the tile grid, refused past what the id space has room for.
fn grid_side(tiles: u32, what: &str) -> u16 {
    if tiles > u32::from(TILE_ID_STRIDE) {
        tracing::warn!(
            "a grid of {tiles} tile {what} is past the {TILE_ID_STRIDE} the image ids \
             have room for; the far edge of the screen will not be drawn"
        );
        return TILE_ID_STRIDE;
    }
    tiles as u16
}

/// A mouse cursor, drawn by us rather than by the server.
///
/// Asking for the `Cursor` pseudo-encoding stops the server compositing the pointer
/// into the framebuffer and has it send the shape instead, which is what lets the
/// pointer move at local speed rather than at the speed of a round trip.
#[derive(Debug, Clone)]
pub struct Cursor {
    pub w: u32,
    pub h: u32,
    /// The point within the image that sits under the pointer.
    pub hot_x: u32,
    pub hot_y: u32,
    /// BGRA, with alpha taken from the cursor's mask, so 0 or 255.
    pub pixels: Vec<u8>,
}

impl Cursor {
    /// Is there anything to draw? A zero-sized cursor is how a server hides it.
    fn is_visible(&self) -> bool {
        self.w > 0 && self.h > 0 && self.pixels.len() >= (self.w as usize) * (self.h as usize) * 4
    }
}

/// What one composed frame cost.
#[derive(Debug, Default, Clone, Copy)]
pub struct FrameStats {
    pub tiles: usize,
    pub bytes: usize,
    pub pixels: u64,
}

/// How tile pixels reach the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    /// Base64 in the escape sequence. Works everywhere, including over SSH.
    Direct,
    /// A shared memory object the terminal maps. Local terminals only, but skips
    /// base64 and compression entirely.
    Shm,
}

/// Everything that decides a tile's pixels.
///
/// Two equal keys are the same picture, whatever layout produced them -- so a tile the
/// terminal already holds can be compared against what it would be sent, which is the
/// question, rather than one layout being compared against another, which is a guess at it.
///
/// `generation` is the damage side of the same question: bumped whenever the source under
/// this tile changed, so a stale tile and a moved one are told apart by the same test.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Content {
    /// The tile's rectangle in destination pixels: where it is, and how big.
    dst: Rect,
    /// Where the visible part of the source starts. Panning moves this and nothing else.
    from: (u32, u32),
    /// Destination pixels per source pixel, which is what says *which* source pixels reach
    /// a given destination rectangle. Exactly `1.0` in the 1:1 modes however large the
    /// window is -- which is why a window that grows keeps its interior tiles -- and a
    /// different number every time a `fit` window is resized, which is why that one does
    /// not.
    scale: f64,
    /// Which resampling produced it, if any. Two modes can land on the same scale and
    /// filter it differently.
    filter: Option<Filter>,
    generation: u32,
}

/// One tile of the plane: what the terminal holds under its id, and where.
#[derive(Debug, Clone, Copy, Default)]
struct Tile {
    /// The content the terminal is holding. `None` means it has nothing under this id.
    held: Option<Content>,
    /// The cell its placement sits on, if it has one.
    placed_at: Option<(u16, u16)>,
    /// The generation of the last damage that touched this tile's source.
    touched: u32,
}

pub struct Renderer {
    layout: Layout,
    grid: TileGrid,
    /// What the terminal holds, tile by tile, in the current grid's order.
    tiles: Vec<Tile>,
    /// How many tiles are not as they should be. Derived, and maintained where the marks
    /// happen: `has_work` is asked every tick and must not walk the grid to answer.
    owing: usize,
    /// Bumped by every mark, so a tile that was damaged since it was last sent can be told
    /// from one that was not by comparing a number rather than clearing a flag.
    generation: u32,
    enc: KittyEncoder,
    scaler: Scaler,
    scaled: Framebuffer,
    scratch: Vec<u8>,
    cursor: Option<Cursor>,
    /// Where the cursor's hotspot is, in destination pixels.
    cursor_at: Option<(u32, u32)>,
    /// The terminal holds this shape's pixels, so moving it is a placement.
    cursor_sent: bool,
    /// Where its placement currently sits, if it has one.
    cursor_placed: Option<At>,
    /// It has moved, changed shape or gone, and the next frame owes it something.
    cursor_owing: bool,
    transfer: Transfer,
    shm: ShmPool,
    /// Set once, if shared memory turns out not to work, so the warning is not
    /// repeated every frame.
    shm_failed: bool,
}

impl Renderer {
    pub fn new(layout: Layout, compress: bool, transfer: Transfer) -> Self {
        let grid = TileGrid::new(&layout);
        Self {
            // Nothing is held, so every tile is owed.
            tiles: vec![Tile::default(); grid.len()],
            owing: grid.len(),
            generation: 0,
            grid,
            layout,
            enc: KittyEncoder::new(compress),
            scaler: Scaler::new(),
            scaled: Framebuffer::new(0, 0),
            scratch: Vec::with_capacity(TILE_TARGET_PX as usize * TILE_TARGET_PX as usize * 3),
            cursor: None,
            cursor_at: None,
            cursor_sent: false,
            cursor_placed: None,
            cursor_owing: false,
            transfer,
            shm: ShmPool::new(),
            shm_failed: false,
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Adopt a new layout, keeping whatever it has not changed.
    ///
    /// Returns escape bytes to write before the next frame: the tiles the new grid has
    /// no place for. Placements belong to the cells they were made at rather than to
    /// the tile that made one, so a tile the grid has dropped stays on screen until it
    /// is deleted by id.
    ///
    /// What the terminal already holds is worth keeping. A window that grew by two cells
    /// has the same pixels in almost every tile, and they are already there: a tile is
    /// addressed by where it sits in the grid, so the id still names its rectangle, and
    /// only the tiles whose rectangle actually moved or changed size have anything new to
    /// say. Retransmitting the lot is what made a resize cost a whole screen, and erasing
    /// the screen first is what made it blank.
    ///
    /// Nothing is compared layout against layout. Each tile is asked one question -- are
    /// the pixels the terminal holds for you the pixels you would be sent? -- and the
    /// answer covers a resize, a scale change, a pan, a font change and plain damage
    /// without distinguishing between them. [`Content`] is that question written down.
    ///
    /// The stale *text* is not this layer's to erase -- it has drawn none -- and belongs
    /// to whoever owns the chrome. `ui::status::clear` and `Menu::clear` are that. What
    /// they erase, they have to say, because a terminal may treat clearing a cell as
    /// dropping the placement under it: [`Renderer::mark_cells`].
    ///
    /// A layout equal to the one in force is left alone. One resize settles through
    /// several paths -- the debounce, the request that follows it, the server's reply,
    /// the re-check after that -- and all but the first arrive with the geometry they
    /// are asking for already adopted. Redrawing the same pixels is the flicker, so the
    /// ones with nothing to say are not allowed to.
    pub fn relayout(&mut self, layout: Layout) -> Vec<u8> {
        if layout == self.layout {
            return Vec::new();
        }
        let grid = TileGrid::new(&layout);
        let was_grid = self.grid;
        let was = std::mem::take(&mut self.tiles);

        // Everything the new grid has no place for, of what the terminal is holding. A
        // tile it still reaches keeps its id and its pixels; only these have nobody
        // coming for them.
        let mut cleanup = Vec::new();
        for ty in 0..was_grid.ny {
            for tx in 0..was_grid.nx {
                let idx = usize::from(ty) * usize::from(was_grid.nx) + usize::from(tx);
                if (tx >= grid.nx || ty >= grid.ny) && was[idx].held.is_some() {
                    delete_image(&mut cleanup, was_grid.id(tx, ty));
                }
            }
        }

        // Carry each tile across to where it now sits. The grid's shape changed, so the
        // index did; what the terminal holds did not.
        self.tiles = vec![Tile::default(); grid.len()];
        for ty in 0..grid.ny.min(was_grid.ny) {
            for tx in 0..grid.nx.min(was_grid.nx) {
                self.tiles[usize::from(ty) * usize::from(grid.nx) + usize::from(tx)] =
                    was[usize::from(ty) * usize::from(was_grid.nx) + usize::from(tx)];
            }
        }

        self.layout = layout;
        self.grid = grid;
        if self.layout.needs_scaling() {
            self.scaled.resize(self.layout.dst_w, self.layout.dst_h);
        } else {
            self.scaled.resize(0, 0);
        }

        // The pointer is placed against the terminal's grid, which has just moved under it.
        self.cursor_owing = true;
        self.cursor_placed = None;

        // And now what is owed, which is the same reckoning a damaged frame does: the
        // pixels a tile holds against the pixels it would be sent. A tile whose content
        // survived but whose cell moved -- centring an image in a window of another width
        // shifts every one of them -- costs a placement and no pixels.
        self.owing = 0;
        for ty in 0..self.grid.ny {
            for tx in 0..self.grid.nx {
                let want = self.want(tx, ty);
                let idx = usize::from(ty) * usize::from(self.grid.nx) + usize::from(tx);
                let cell = self.cell_of(tx, ty);
                let tile = &mut self.tiles[idx];
                if tile.held != Some(want) {
                    self.owing += 1;
                } else if tile.placed_at != Some(cell) {
                    place_existing(&mut cleanup, self.grid.id(tx, ty), cell.0, cell.1);
                    tile.placed_at = Some(cell);
                }
            }
        }
        cleanup
    }

    /// The content a tile should be holding, as the layout in force describes it.
    ///
    /// Not the source rectangle: under a scale the source region behind a destination
    /// rectangle is fractional, and rounding it would make two different mappings compare
    /// equal. The mapping itself goes in the key instead -- where the crop starts and how
    /// many destination pixels a source pixel becomes -- which decides the same thing
    /// exactly.
    fn want(&self, tx: u16, ty: u16) -> Content {
        Content {
            dst: self.grid.tile_rect(tx, ty),
            from: (self.layout.src.x, self.layout.src.y),
            scale: self.layout.scale,
            filter: self.layout.needs_scaling().then(|| self.layout.filter()),
            generation: self.tiles[usize::from(ty) * usize::from(self.grid.nx) + usize::from(tx)]
                .touched,
        }
    }

    /// The cell a tile's placement belongs on.
    fn cell_of(&self, tx: u16, ty: u16) -> (u16, u16) {
        (
            self.layout.origin_col + tx * self.grid.tile_cols,
            self.layout.origin_row + ty * self.grid.tile_rows,
        )
    }

    /// Note that a tile's source has changed, and whether that leaves it owing.
    fn touch(&mut self, tx: u16, ty: u16) {
        let generation = self.generation;
        let want = self.want(tx, ty);
        let idx = usize::from(ty) * usize::from(self.grid.nx) + usize::from(tx);
        let tile = &mut self.tiles[idx];
        let was_owed = tile.held != Some(want);
        tile.touched = generation;
        if !was_owed {
            self.owing += 1;
        }
    }

    /// Mark damage, in source-framebuffer pixels.
    pub fn mark(&mut self, r: Rect) {
        if r.is_empty() || self.grid.len() == 0 {
            return;
        }
        let Some(dst) = self.layout.src_to_dst(r) else {
            return;
        };
        self.mark_dst(dst);
    }

    /// Mark damage already in destination pixels, as the cursor overlay is.
    pub fn mark_dst(&mut self, r: Rect) {
        if r.is_empty() || self.grid.len() == 0 {
            return;
        }
        let Some(dst) = r.intersect(&Rect::new(0, 0, self.layout.dst_w, self.layout.dst_h)) else {
            return;
        };
        // One generation per mark, so a tile touched by this damage is distinguishable
        // from one holding pixels taken before it.
        self.generation = self.generation.wrapping_add(1);
        let (x0, y0, x1, y1) = self.grid.tiles_covering(dst);
        for ty in y0..=y1 {
            for tx in x0..=x1 {
                if tx < self.grid.nx && ty < self.grid.ny {
                    self.touch(tx, ty);
                }
            }
        }
    }

    /// Mark the tiles under a rectangle of terminal cells, zero-based and absolute.
    ///
    /// For chrome that has been erased. A terminal may treat clearing a cell as dropping
    /// the placement under it, and a relayout that keeps most of its tiles has nothing
    /// else coming to put those back.
    pub fn mark_cells(&mut self, col: u16, row: u16, cols: u16, rows: u16) {
        let (cell_w, cell_h) = (self.layout.cell_w, self.layout.cell_h);
        // Cells left of or above the image clamp to its corner, which marks more than was
        // erased. The other direction would leave a tile out.
        let x = u32::from(col.saturating_sub(self.layout.origin_col)) * cell_w;
        let y = u32::from(row.saturating_sub(self.layout.origin_row)) * cell_h;
        self.mark_dst(Rect::new(
            x,
            y,
            u32::from(cols) * cell_w,
            u32::from(rows) * cell_h,
        ));
    }

    /// Adopt a new cursor shape.
    ///
    /// The pointer is an image of its own rather than pixels blended into the tiles, so a
    /// new shape is a transmission and nothing else -- no tile is disturbed by it.
    pub fn set_cursor(&mut self, cursor: Option<Cursor>) {
        self.cursor = cursor;
        self.cursor_sent = false;
        self.cursor_owing = true;
    }

    /// Move the cursor, in destination pixels. `None` hides it.
    ///
    /// A placement, so this costs one escape however far it moves. Blended into the tiles,
    /// as it used to be, the same movement retransmitted the tile it left and the tile it
    /// arrived at -- two to four tiles of fifty kilobytes, sixty times a second, for a
    /// picture that had not changed.
    pub fn move_cursor(&mut self, at: Option<(u32, u32)>) {
        if self.cursor_at == at {
            return;
        }
        self.cursor_at = at;
        self.cursor_owing = true;
    }

    /// Where the cursor sits in destination pixels, if it is being drawn at all.
    fn cursor_rect(&self) -> Option<Rect> {
        let cursor = self.cursor.as_ref()?;
        if !cursor.is_visible() {
            return None;
        }
        let (x, y) = self.cursor_at?;
        // The hotspot is the point under the pointer, so the image starts above and
        // left of it -- and may start off-screen, which is normal near an edge.
        Some(Rect::new(
            x.saturating_sub(cursor.hot_x),
            y.saturating_sub(cursor.hot_y),
            cursor.w,
            cursor.h,
        ))
    }

    /// Where the pointer's image belongs on the terminal's own grid.
    ///
    /// Destination pixels are relative to the image's corner, so the origin goes back on
    /// before the cell is worked out; the remainder is the sub-cell offset, which is what
    /// lets the pointer sit between cells rather than snapping to a corner.
    fn cursor_at_cell(&self) -> Option<At> {
        let rect = self.cursor_rect()?;
        let (cell_w, cell_h) = (self.layout.cell_w.max(1), self.layout.cell_h.max(1));
        let x = u32::from(self.layout.origin_col) * cell_w + rect.x;
        let y = u32::from(self.layout.origin_row) * cell_h + rect.y;
        Some(At {
            col: (x / cell_w) as u16,
            row: (y / cell_h) as u16,
            x: x % cell_w,
            y: y % cell_h,
        })
    }

    /// Draw, move or hide the pointer, whichever it is owed.
    ///
    /// Called after the tiles so it lands on top of them. Its id is above every tile's, so
    /// the terminal composites it there regardless of order, but keeping the order honest
    /// costs nothing.
    fn compose_cursor(&mut self, out: &mut Vec<u8>) {
        if !self.cursor_owing {
            return;
        }
        self.cursor_owing = false;
        let Some(at) = self.cursor_at_cell() else {
            // Off the screen or hidden by the server. The data stays in the terminal, so
            // coming back is a placement rather than another transmission.
            if self.cursor_placed.take().is_some() {
                hide_image(out, CURSOR_IMAGE_ID);
            }
            return;
        };
        let cursor = self.cursor.as_ref().expect("a rect means a cursor");
        if !self.cursor_sent {
            // BGRA on the wire, RGBA in the protocol.
            let mut rgba = Vec::with_capacity(cursor.pixels.len());
            for px in cursor.pixels.chunks_exact(4) {
                rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
            place_rgba(out, CURSOR_IMAGE_ID, at, cursor.w, cursor.h, &rgba);
            self.cursor_sent = true;
        } else if self.cursor_placed != Some(at) {
            place_existing_at(out, CURSOR_IMAGE_ID, at);
        }
        self.cursor_placed = Some(at);
    }

    pub fn mark_all(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        for ty in 0..self.grid.ny {
            for tx in 0..self.grid.nx {
                self.touch(tx, ty);
            }
        }
    }

    pub fn has_work(&self) -> bool {
        self.owing > 0 || self.cursor_owing
    }

    #[cfg(test)]
    pub fn dirty_tiles(&self) -> usize {
        self.owing
    }

    /// Is this tile owed pixels? Not whether it is owed a *placement*, which is the other
    /// half of the reckoning and costs no transmission.
    #[cfg(test)]
    fn is_dirty(&self, tx: u16, ty: u16) -> bool {
        let idx = usize::from(ty) * usize::from(self.grid.nx) + usize::from(tx);
        self.tiles[idx].held != Some(self.want(tx, ty))
    }

    pub fn tile_count(&self) -> usize {
        self.grid.len()
    }

    /// The destination pixels a frame is going to send, as one rectangle, grown by the
    /// filter's reach and clipped to the image.
    ///
    /// One rectangle rather than a resample per tile: the union costs a little more
    /// resampling than the tiles strictly need and saves an edge between every pair of
    /// them, each of which would have to be grown and thrown away separately.
    ///
    /// The growing is what makes a region resampled alone match the same pixels resampled
    /// with the whole screen. In destination pixels, so the reach -- which is a count of
    /// destination pixels one source pixel touches -- has to be scaled up when the picture
    /// is being magnified.
    fn dirty_region(&self, filter: Filter) -> Option<Rect> {
        let mut region: Option<Rect> = None;
        for ty in 0..self.grid.ny {
            for tx in 0..self.grid.nx {
                if self.tiles[usize::from(ty) * usize::from(self.grid.nx) + usize::from(tx)].held
                    == Some(self.want(tx, ty))
                {
                    continue;
                }
                let tile = self.grid.tile_rect(tx, ty);
                region = Some(match region {
                    None => tile,
                    Some(so_far) => {
                        let x = so_far.x.min(tile.x);
                        let y = so_far.y.min(tile.y);
                        Rect::new(
                            x,
                            y,
                            so_far.right().max(tile.right()) - x,
                            so_far.bottom().max(tile.bottom()) - y,
                        )
                    }
                });
            }
        }
        region?
            .expand(filter.dst_reach(self.layout.scale))
            .intersect(&Rect::new(0, 0, self.layout.dst_w, self.layout.dst_h))
    }

    /// Compose the dirty tiles into `out`.
    ///
    /// The caller wraps this in synchronised-output markers and adds its own
    /// chrome; nothing here writes to the terminal directly.
    pub fn compose(&mut self, fb: &Framebuffer, out: &mut Vec<u8>) -> FrameStats {
        let mut stats = FrameStats::default();
        let before = out.len();
        if self.owing == 0 || self.grid.len() == 0 {
            // No tile is owed anything, but the pointer may be: it is a placement of its
            // own now, and a frame that carries nothing else still has to carry that.
            self.compose_cursor(out);
            stats.bytes = out.len() - before;
            return stats;
        }

        // Resample once per frame, and only the part of the picture being sent. A caret
        // blinking in a scaled session used to cost a whole screen of resampling for one
        // tile's worth of pixels.
        if self.layout.needs_scaling() {
            if self.scaled.width() != self.layout.dst_w || self.scaled.height() != self.layout.dst_h
            {
                self.scaled.resize(self.layout.dst_w, self.layout.dst_h);
            }
            let filter = self.layout.filter();
            if let Some(region) = self.dirty_region(filter) {
                // The source rectangle this region came from, in fractional pixels: the
                // ratio is the one the layout actually achieved rather than its rounded
                // scale, or the region would land a fraction of a pixel off the rest.
                let per_x = f64::from(self.layout.src.w) / f64::from(self.layout.dst_w);
                let per_y = f64::from(self.layout.src.h) / f64::from(self.layout.dst_h);
                let crop = (
                    f64::from(self.layout.src.x) + f64::from(region.x) * per_x,
                    f64::from(self.layout.src.y) + f64::from(region.y) * per_y,
                    f64::from(region.w) * per_x,
                    f64::from(region.h) * per_y,
                );
                if let Err(err) =
                    self.scaler
                        .resize_region(fb, crop, &mut self.scaled, region, filter)
                {
                    tracing::warn!("resample failed: {err:#}");
                    return stats;
                }
            }
        }

        // In 1:1 modes tiles are cut straight out of the source, offset by the
        // pan position; when scaling they come from the resampled copy.
        let (source, off_x, off_y) = if self.layout.needs_scaling() {
            (&self.scaled, 0, 0)
        } else {
            (fb, self.layout.src.x, self.layout.src.y)
        };

        for ty in 0..self.grid.ny {
            for tx in 0..self.grid.nx {
                let idx = usize::from(ty) * usize::from(self.grid.nx) + usize::from(tx);
                let want = self.want(tx, ty);
                if self.tiles[idx].held == Some(want) {
                    continue;
                }
                let tile = self.grid.tile_rect(tx, ty);
                if tile.is_empty() {
                    continue;
                }

                let from = Rect::new(tile.x + off_x, tile.y + off_y, tile.w, tile.h);
                let col = self.layout.origin_col + tx * self.grid.tile_cols;
                let row = self.layout.origin_row + ty * self.grid.tile_rows;
                let at = Placement {
                    id: self.grid.id(tx, ty),
                    col,
                    row,
                    w: tile.w,
                    h: tile.h,
                };
                let bytes = tile_bytes(tile);

                // Packed straight into an object of its own where shared memory is in
                // play, and into a buffer to be encoded where it is not.
                let mut sent = false;
                if self.transfer == Transfer::Shm && !self.shm_failed {
                    match self.shm.frame(bytes) {
                        Ok(mut object) => {
                            if let Some((_, into)) = object.next(bytes) {
                                source.pack_rgb_into(from, into);
                                self.enc.place_shm(out, at, object.name());
                                sent = true;
                            }
                        }
                        Err(err) => {
                            // One complaint, then carry on the slow way for the rest of
                            // the session.
                            tracing::warn!("shared memory unavailable, using base64: {err}");
                            self.shm_failed = true;
                        }
                    }
                }
                if !sent {
                    self.scratch.clear();
                    source.pack_rgb(from, &mut self.scratch);
                    self.enc.place_rgb(out, at, &self.scratch);
                }

                stats.tiles += 1;
                stats.pixels += tile.area();
            }
        }

        // After the tiles, so the pointer is written over the picture it sits on. Its id is
        // above every tile's, so the terminal would composite it there whatever the order,
        // but a frame that reads in the order it draws is easier to follow.
        self.compose_cursor(out);

        stats.bytes = out.len() - before;
        stats
    }

    /// Called once a composed frame has actually reached the terminal.
    ///
    /// Which is where the terminal's image store is caught up with, rather than in
    /// `compose`: a frame the writer was too busy for never arrived, and a tile counted as
    /// held that was never sent is a hole in the next resize that keeps it.
    ///
    /// Every tile now holds what the layout asks of it: the ones that were owed have just
    /// been transmitted, and the rest already held it. Recomputed rather than remembered --
    /// `want` is a pure function of the layout and the tile's damage generation, so there is
    /// nothing to carry from the compose except the fact that it reached the terminal.
    pub fn commit(&mut self) {
        for ty in 0..self.grid.ny {
            for tx in 0..self.grid.nx {
                let want = self.want(tx, ty);
                let cell = self.cell_of(tx, ty);
                let idx = usize::from(ty) * usize::from(self.grid.nx) + usize::from(tx);
                self.tiles[idx].held = Some(want);
                self.tiles[idx].placed_at = Some(cell);
            }
        }
        self.owing = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(cols: u16, rows: u16, cell_w: u32, cell_h: u32) -> Metrics {
        Metrics {
            cols,
            rows,
            px_w: u32::from(cols) * cell_w,
            px_h: u32::from(rows) * cell_h,
            cell_w,
            cell_h,
        }
    }

    /// 200x50 cells of 8x17: a 1600x833 image area once the status row is taken.
    fn ghostty() -> Metrics {
        metrics(200, 50, 8, 17)
    }

    #[test]
    fn image_area_excludes_the_status_row() {
        let m = ghostty();
        assert_eq!(m.image_area(), (1600, 49 * 17));
    }

    #[test]
    fn native_size_match_is_pixel_exact_and_unscaled() {
        let m = ghostty();
        let (w, h) = m.image_area();
        let l = Layout::compute(&m, ScaleMode::Native, w, h, (0, 0));
        assert!(l.is_pixel_exact());
        assert!(!l.needs_scaling());
        assert!(!l.is_cropped());
        assert_eq!((l.dst_w, l.dst_h), (w, h));
        assert_eq!((l.origin_col, l.origin_row), (0, 0));
    }

    #[test]
    fn one_to_one_crops_and_clamps_the_pan() {
        let m = ghostty();
        let l = Layout::compute(&m, ScaleMode::OneToOne, 1920, 1080, (9999, 9999));
        assert!(l.is_cropped());
        assert_eq!(l.src.w, 1600);
        assert_eq!(l.src.h, 49 * 17);
        // Pan is clamped so the window stays inside the source.
        assert_eq!(l.src.x, 1920 - 1600);
        assert_eq!(l.src.y, 1080 - 49 * 17);
        assert!(l.is_pixel_exact());
    }

    #[test]
    fn fit_preserves_aspect_and_letterboxes() {
        let m = ghostty();
        let l = Layout::compute(&m, ScaleMode::Fit, 1920, 1080, (0, 0));
        let (area_w, area_h) = m.image_area();
        assert!(l.dst_w <= area_w && l.dst_h <= area_h);
        let src_aspect = 1920.0 / 1080.0;
        let dst_aspect = f64::from(l.dst_w) / f64::from(l.dst_h);
        assert!((src_aspect - dst_aspect).abs() < 0.01, "{dst_aspect}");
        assert!(!l.is_pixel_exact());
        // Centred on a cell boundary.
        assert!(l.origin_row > 0 || l.dst_h == area_h);
    }

    #[test]
    fn integer_mode_picks_a_whole_factor_and_stays_crisp() {
        let m = ghostty();
        let l = Layout::compute(&m, ScaleMode::Integer, 640, 400, (0, 0));
        assert_eq!(l.scale, 2.0);
        assert_eq!((l.dst_w, l.dst_h), (1280, 800));
        assert_eq!(l.filter(), Filter::Nearest);
    }

    #[test]
    fn integer_mode_falls_back_to_fitting_when_nothing_fits() {
        let m = ghostty();
        let l = Layout::compute(&m, ScaleMode::Integer, 3840, 2160, (0, 0));
        assert!(l.scale < 1.0);
        assert_eq!(l.filter(), Filter::Lanczos3);
        assert!(l.dst_w <= 1600);
    }

    #[test]
    fn odd_sizes_do_not_overflow_the_area() {
        let m = metrics(37, 9, 7, 13);
        for (w, h) in [(1, 1), (3, 5000), (5000, 3), (65535, 65535)] {
            for mode in [
                ScaleMode::Native,
                ScaleMode::Fit,
                ScaleMode::Integer,
                ScaleMode::OneToOne,
            ] {
                let l = Layout::compute(&m, mode, w, h, (0, 0));
                let (area_w, area_h) = m.image_area();
                assert!(l.dst_w <= area_w, "{mode:?} {w}x{h} -> {}", l.dst_w);
                assert!(l.dst_h <= area_h, "{mode:?} {w}x{h} -> {}", l.dst_h);
            }
        }
    }

    #[test]
    fn a_zero_sized_source_does_not_panic() {
        let m = ghostty();
        let l = Layout::compute(&m, ScaleMode::Fit, 0, 0, (0, 0));
        assert_eq!((l.dst_w, l.dst_h), (0, 0));
        assert!(l.src_to_dst(Rect::new(0, 0, 10, 10)).is_none());
    }

    #[test]
    fn damage_maps_to_the_tiles_that_actually_contain_it() {
        let m = ghostty();
        let (w, h) = m.image_area();
        let l = Layout::compute(&m, ScaleMode::Native, w, h, (0, 0));
        let grid = TileGrid::new(&l);
        // Cells of 8x17 -> 16x8 cells per tile -> 128x136 pixels per tile.
        assert_eq!((grid.tile_cols, grid.tile_rows), (16, 8));

        let mut r = Renderer::new(l, true, Transfer::Direct);
        r.commit();
        assert!(!r.has_work());

        // A single pixel dirties exactly one tile.
        r.mark(Rect::new(0, 0, 1, 1));
        assert_eq!(r.dirty_tiles(), 1);

        // A pixel straddling a tile boundary dirties two.
        r.commit();
        r.mark(Rect::new(127, 0, 2, 1));
        assert_eq!(r.dirty_tiles(), 2);

        // Marking the same tile twice does not double-count.
        r.commit();
        r.mark(Rect::new(0, 0, 1, 1));
        r.mark(Rect::new(1, 1, 1, 1));
        assert_eq!(r.dirty_tiles(), 1);
    }

    #[test]
    fn damage_outside_the_visible_window_is_ignored() {
        let m = ghostty();
        let l = Layout::compute(&m, ScaleMode::OneToOne, 3000, 3000, (0, 0));
        let mut r = Renderer::new(l, true, Transfer::Direct);
        r.commit();
        r.mark(Rect::new(2900, 2900, 10, 10));
        assert_eq!(r.dirty_tiles(), 0);
    }

    #[test]
    fn scaled_damage_is_padded_for_the_filter() {
        let m = ghostty();
        let l = Layout::compute(&m, ScaleMode::Fit, 1920, 1080, (0, 0));
        assert!(l.needs_scaling());
        // A source pixel right on a tile seam must dirty both neighbours,
        // because the filter reads across it.
        let dst = l.src_to_dst(Rect::new(100, 100, 1, 1)).unwrap();
        assert!(dst.w >= 1 + 2 * l.filter().dst_padding() - 1);
    }

    #[test]
    fn every_tile_is_covered_exactly_once_and_stays_in_bounds() {
        let m = ghostty();
        for (sw, sh, mode) in [
            (1600u32, 833u32, ScaleMode::Native),
            (1920, 1080, ScaleMode::Fit),
            (640, 400, ScaleMode::Integer),
            (3000, 3000, ScaleMode::OneToOne),
        ] {
            let l = Layout::compute(&m, mode, sw, sh, (0, 0));
            let grid = TileGrid::new(&l);
            let mut covered = 0u64;
            for ty in 0..grid.ny {
                for tx in 0..grid.nx {
                    let t = grid.tile_rect(tx, ty);
                    assert!(t.right() <= l.dst_w, "{mode:?} tile past right edge");
                    assert!(t.bottom() <= l.dst_h, "{mode:?} tile past bottom edge");
                    covered += t.area();
                }
            }
            assert_eq!(
                covered,
                u64::from(l.dst_w) * u64::from(l.dst_h),
                "{mode:?} tiles must tile the image exactly"
            );
        }
    }

    #[test]
    fn the_last_cell_row_is_never_used() {
        // Placing an image on the final row would scroll the screen and shift
        // every other placement, so the status row must be excluded.
        let m = ghostty();
        let (w, h) = m.image_area();
        let l = Layout::compute(&m, ScaleMode::Native, w, h, (0, 0));
        let grid = TileGrid::new(&l);
        let last_row_cell = u32::from(l.origin_row)
            + u32::from(grid.ny - 1) * u32::from(grid.tile_rows)
            + grid.tile_rect(0, grid.ny - 1).h.div_ceil(l.cell_h);
        assert!(last_row_cell <= u32::from(m.image_rows()));
    }

    #[test]
    fn mouse_mapping_round_trips_at_1_to_1() {
        let m = ghostty();
        let (w, h) = m.image_area();
        let l = Layout::compute(&m, ScaleMode::Native, w, h, (0, 0));
        assert_eq!(l.terminal_px_to_src(0, 0), Some((0, 0)));
        assert_eq!(l.terminal_px_to_src(37, 91), Some((37, 91)));
        // Outside the image is not clamped onto it.
        assert_eq!(l.terminal_px_to_src(w, 0), None);
        assert_eq!(l.terminal_px_to_src(0, h), None);
    }

    #[test]
    fn mouse_mapping_accounts_for_pan_and_letterbox() {
        let m = ghostty();
        let l = Layout::compute(&m, ScaleMode::OneToOne, 3000, 3000, (100, 200));
        assert_eq!(l.terminal_px_to_src(0, 0), Some((100, 200)));

        let l = Layout::compute(&m, ScaleMode::Fit, 1920, 1080, (0, 0));
        let ox = u32::from(l.origin_col) * l.cell_w;
        let oy = u32::from(l.origin_row) * l.cell_h;
        assert_eq!(l.terminal_px_to_src(ox, oy), Some((0, 0)));
        let (sx, sy) = l
            .terminal_px_to_src(ox + l.dst_w - 1, oy + l.dst_h - 1)
            .unwrap();
        assert!(sx >= 1918 && sy >= 1078, "{sx},{sy}");
        // The letterbox above the image is not part of the desktop.
        if oy > 0 {
            assert_eq!(l.terminal_px_to_src(ox, oy - 1), None);
        }
    }

    /// A 2x2 cursor: opaque red, transparent, transparent, opaque blue. BGRA, with
    /// alpha in the fourth byte, which is what the cursor decoder produces.
    fn test_cursor(hot_x: u32, hot_y: u32) -> Cursor {
        Cursor {
            w: 2,
            h: 2,
            hot_x,
            hot_y,
            pixels: vec![
                0, 0, 255, 255, // red, opaque
                0, 0, 0, 0, // transparent
                0, 0, 0, 0, // transparent
                255, 0, 0, 255, // blue, opaque
            ],
        }
    }

    fn native_renderer(m: &Metrics) -> (Renderer, Framebuffer) {
        let (w, h) = m.image_area();
        let layout = Layout::compute(m, ScaleMode::Native, w, h, (0, 0));
        (
            Renderer::new(layout, false, Transfer::Direct),
            Framebuffer::new(w, h),
        )
    }

    #[test]
    fn the_pointer_is_sent_once_and_then_only_moved() {
        // The whole point of it being a placement. Blended into the tiles, every motion
        // event retransmitted the tile it left and the tile it arrived at.
        let m = ghostty();
        let (mut r, fb) = native_renderer(&m);
        let mut out = Vec::new();
        r.compose(&fb, &mut out);
        r.commit();

        r.set_cursor(Some(test_cursor(0, 0)));
        r.move_cursor(Some((40, 40)));
        assert!(r.has_work(), "a pointer that has moved is work to do");
        out.clear();
        r.compose(&fb, &mut out);
        r.commit();
        let first = String::from_utf8_lossy(&out).into_owned();
        assert!(
            first.contains(&format!("f=32,i={CURSOR_IMAGE_ID},")),
            "the shape should have been transmitted as RGBA: {first:?}"
        );
        assert_eq!(r.dirty_tiles(), 0, "and no tile disturbed by it");

        // Moving it again is a placement and nothing else.
        r.move_cursor(Some((48, 44)));
        out.clear();
        r.compose(&fb, &mut out);
        r.commit();
        let moved = String::from_utf8_lossy(&out).into_owned();
        assert!(
            moved.contains(&format!("a=p,q=2,C=1,z=-1,i={CURSOR_IMAGE_ID},")),
            "moving should be a placement: {moved:?}"
        );
        assert!(
            !moved.contains("f=32"),
            "moving must not send the shape again: {moved:?}"
        );
        assert!(
            !moved.contains("a=T"),
            "and must not send a tile either: {moved:?}"
        );
        assert!(
            moved.len() < 120,
            "a move is a few bytes: {} of them",
            moved.len()
        );
    }

    #[test]
    fn the_pointer_sits_between_cells() {
        // What `X`/`Y` are for: a pointer that could only land on a cell corner would jump
        // eight pixels at a time across a 8x17 grid.
        let m = ghostty();
        let (mut r, _fb) = native_renderer(&m);
        r.set_cursor(Some(test_cursor(0, 0)));

        // Origin is the top-left corner in a native layout, so destination pixels are
        // terminal pixels: 19 across is cell 2 with 3 left over, 40 down is row 2 with 6.
        r.move_cursor(Some((19, 40)));
        let at = r.cursor_at_cell().expect("the pointer should be placed");
        assert_eq!((at.col, at.x), (2, 3));
        assert_eq!((at.row, at.y), (2, 6));
        assert!(
            at.x < m.cell_w && at.y < m.cell_h,
            "the offsets have to be inside one cell"
        );
    }

    #[test]
    fn a_pointer_that_leaves_is_taken_off_without_forgetting_its_shape() {
        // Coming back should be a placement, not another transmission.
        let m = ghostty();
        let (mut r, fb) = native_renderer(&m);
        let mut out = Vec::new();
        r.set_cursor(Some(test_cursor(0, 0)));
        r.move_cursor(Some((40, 40)));
        r.compose(&fb, &mut out);
        r.commit();

        r.move_cursor(None);
        out.clear();
        r.compose(&fb, &mut out);
        r.commit();
        let gone = String::from_utf8_lossy(&out).into_owned();
        assert!(
            gone.contains(&format!("a=d,d=i,i={CURSOR_IMAGE_ID},")),
            "the placement should be released: {gone:?}"
        );
        assert!(
            !gone.contains("d=I"),
            "but the data kept, so the shape need not travel again: {gone:?}"
        );

        r.move_cursor(Some((40, 40)));
        out.clear();
        r.compose(&fb, &mut out);
        let back = String::from_utf8_lossy(&out).into_owned();
        assert!(
            back.contains("a=p") && !back.contains("f=32"),
            "coming back is a placement: {back:?}"
        );
    }

    #[test]
    fn the_cursor_reaches_the_terminal_when_composed() {
        let m = ghostty();
        let (mut r, mut fb) = native_renderer(&m);
        fb.fill(fb.rect(), [10, 20, 30]);
        r.set_cursor(Some(test_cursor(0, 0)));
        r.move_cursor(Some((0, 0)));

        let mut out = Vec::new();
        let stats = r.compose(&fb, &mut out);
        assert!(
            stats.tiles > 0,
            "a cursor with a shape has to produce a frame"
        );
    }

    #[test]
    fn moving_the_cursor_costs_no_tile_at_all() {
        // It used to damage the tile it left and the tile it arrived at, which is what made
        // moving the mouse the most expensive thing a still screen could do.
        let m = ghostty();
        let (mut r, fb) = native_renderer(&m);
        let mut out = Vec::new();
        r.compose(&fb, &mut out);
        r.set_cursor(Some(test_cursor(0, 0)));
        r.move_cursor(Some((10, 10)));
        r.compose(&fb, &mut out);
        r.commit();
        assert!(!r.has_work());

        // Far enough to land in another tile, which is no longer any concern of the tiles.
        r.move_cursor(Some((600, 10)));
        assert_eq!(r.dirty_tiles(), 0, "no tile is owed anything");
        assert!(r.has_work(), "but the frame is: the pointer has moved");

        // A move to where it already is asks for nothing.
        r.compose(&fb, &mut out);
        r.commit();
        r.move_cursor(Some((600, 10)));
        assert!(!r.has_work());
    }

    #[test]
    fn the_hotspot_places_the_cursor_relative_to_the_pointer() {
        let m = ghostty();
        let (mut r, _fb) = native_renderer(&m);
        // A hotspot in the middle of the image means the image starts above and left.
        r.set_cursor(Some(test_cursor(1, 1)));
        r.move_cursor(Some((100, 100)));
        assert_eq!(r.cursor_rect(), Some(Rect::new(99, 99, 2, 2)));

        // Near the top-left corner it is clamped rather than wrapping around.
        r.move_cursor(Some((0, 0)));
        assert_eq!(r.cursor_rect(), Some(Rect::new(0, 0, 2, 2)));
    }

    #[test]
    fn a_cursor_at_the_edge_is_clipped_not_fatal() {
        let m = ghostty();
        let (mut r, fb) = native_renderer(&m);
        let (w, h) = m.image_area();
        r.set_cursor(Some(test_cursor(0, 0)));
        // Straddling the bottom-right corner: half of it has nowhere to go.
        r.move_cursor(Some((w - 1, h - 1)));
        let mut out = Vec::new();
        r.compose(&fb, &mut out);
        assert!(!out.is_empty());
    }

    #[test]
    fn hiding_the_cursor_takes_it_off_without_touching_a_tile() {
        let m = ghostty();
        let (mut r, fb) = native_renderer(&m);
        let mut out = Vec::new();
        r.set_cursor(Some(test_cursor(0, 0)));
        r.move_cursor(Some((50, 50)));
        r.compose(&fb, &mut out);
        r.commit();

        r.move_cursor(None);
        assert_eq!(r.dirty_tiles(), 0, "the picture underneath never changed");
        assert!(r.has_work(), "but the placement has to come off");
        assert!(r.cursor_rect().is_none());
    }

    #[test]
    fn a_zero_sized_cursor_is_how_a_server_hides_it() {
        let m = ghostty();
        let (mut r, _fb) = native_renderer(&m);
        r.set_cursor(Some(Cursor {
            w: 0,
            h: 0,
            hot_x: 0,
            hot_y: 0,
            pixels: Vec::new(),
        }));
        r.move_cursor(Some((10, 10)));
        assert!(
            r.cursor_rect().is_none(),
            "nothing to draw, so nothing drawn"
        );
    }

    #[test]
    fn a_cursor_shorter_than_its_dimensions_is_refused() {
        // A malformed shape must not be indexed into.
        let m = ghostty();
        let (mut r, _fb) = native_renderer(&m);
        r.set_cursor(Some(Cursor {
            w: 16,
            h: 16,
            hot_x: 0,
            hot_y: 0,
            pixels: vec![0; 4],
        }));
        r.move_cursor(Some((10, 10)));
        assert!(r.cursor_rect().is_none());
    }

    #[test]
    fn a_grid_that_only_grows_keeps_the_tiles_that_did_not_move() {
        // The whole point of addressing a tile by where it sits rather than by when it is
        // drawn. A window that grew has the same pixels in every tile the edge did not
        // reach, and the terminal is already holding them under the same ids -- so a resize
        // is the new tiles and the tiles that changed size, not a whole screen.
        let m = ghostty();
        let (w, h) = m.image_area();
        let mut r = Renderer::new(
            Layout::compute(&m, ScaleMode::Native, w, h, (0, 0)),
            true,
            Transfer::Direct,
        );
        let fb = Framebuffer::new(w, h);
        let mut out = Vec::new();
        r.compose(&fb, &mut out);
        r.commit();
        let (was_nx, was_ny) = (r.grid.nx, r.grid.ny);

        let bigger = metrics(240, 70, 8, 17);
        let (bw, bh) = bigger.image_area();
        let cleanup = r.relayout(Layout::compute(&bigger, ScaleMode::Native, bw, bh, (0, 0)));
        assert_eq!(
            String::from_utf8(cleanup).unwrap(),
            "",
            "a bigger grid drops no tiles"
        );

        assert!(
            !r.is_dirty(0, 0),
            "an interior tile has the pixels it had before"
        );
        assert!(
            r.is_dirty(was_nx, 0) && r.is_dirty(0, was_ny),
            "the tiles the grid has only just reached have never been sent"
        );
        assert!(
            r.is_dirty(was_nx - 1, 0) && r.is_dirty(0, was_ny - 1),
            "the tiles that were clipped at the edge are a different size now"
        );
        // Everything but the last row and column either way, which is what is left when
        // the two edges that changed size and the two the grid has just reached are taken
        // out of it.
        let kept = usize::from(was_nx - 1) * usize::from(was_ny - 1);
        assert_eq!(r.dirty_tiles(), r.tile_count() - kept);
        assert!(
            r.dirty_tiles() < r.tile_count() / 2,
            "most of the grid should have been kept: {} of {}",
            r.dirty_tiles(),
            r.tile_count()
        );
    }

    #[test]
    fn a_picture_that_only_moved_is_re_placed_rather_than_re_sent() {
        // A desktop smaller than the window is centred in it, so a window of another width
        // puts every tile on another cell without changing a pixel of any of them. That is
        // a placement each and no payload at all -- where it used to be the whole picture,
        // because the tiles had moved and nothing could tell that from their having
        // changed.
        let m = metrics(200, 50, 8, 17);
        let mut r = Renderer::new(
            Layout::compute(&m, ScaleMode::OneToOne, 640, 480, (0, 0)),
            true,
            Transfer::Direct,
        );
        let fb = Framebuffer::new(640, 480);
        let mut out = Vec::new();
        r.compose(&fb, &mut out);
        r.commit();
        let tiles = r.tile_count();

        // Two columns wider: the centred image starts one cell further along.
        let wider = metrics(202, 50, 8, 17);
        let moved = Layout::compute(&wider, ScaleMode::OneToOne, 640, 480, (0, 0));
        assert_ne!(
            moved.origin_col, r.layout.origin_col,
            "the image was supposed to move"
        );
        let cleanup = r.relayout(moved);
        let text = String::from_utf8(cleanup).unwrap();

        assert_eq!(r.dirty_tiles(), 0, "not a pixel of it has changed");
        assert_eq!(
            text.matches("a=p,").count(),
            tiles,
            "every tile should have been put on its new cells: {text:?}"
        );
        assert!(
            !text.contains("a=T"),
            "a move must not transmit anything: {text:?}"
        );
        // At the cells the new layout puts the first tile on, one-based.
        let at = format!("\x1b[{};{}H", moved.origin_row + 1, moved.origin_col + 1);
        assert!(
            text.contains(&at),
            "the first tile was not put at {at:?}: {text:?}"
        );
    }

    #[test]
    fn a_grid_laid_out_differently_keeps_nothing() {
        // Another scale means every destination pixel comes from somewhere new, tiles that
        // did not move included: the resampled copy is made whole, from the visible source
        // to the whole destination.
        let m = ghostty();
        let mut r = Renderer::new(
            Layout::compute(&m, ScaleMode::Fit, 800, 600, (0, 0)),
            true,
            Transfer::Direct,
        );
        let fb = Framebuffer::new(800, 600);
        let mut out = Vec::new();
        r.compose(&fb, &mut out);
        r.commit();

        let bigger = metrics(240, 70, 8, 17);
        r.relayout(Layout::compute(&bigger, ScaleMode::Fit, 800, 600, (0, 0)));
        assert_eq!(
            r.dirty_tiles(),
            r.tile_count(),
            "a rescaled picture has to be sent again in full"
        );
    }

    #[test]
    fn a_frame_that_never_reached_the_terminal_leaves_its_tiles_owing() {
        // What a tile holds is settled on the commit, not the compose: a frame the writer
        // was too busy for was composed and thrown away, and a tile counted as held that
        // was never sent is a hole that survives every resize that keeps it.
        let m = ghostty();
        let (w, h) = m.image_area();
        let mut r = Renderer::new(
            Layout::compute(&m, ScaleMode::Native, w, h, (0, 0)),
            true,
            Transfer::Direct,
        );
        let fb = Framebuffer::new(w, h);
        let mut out = Vec::new();
        r.compose(&fb, &mut out); // ... and no commit: the frame was dropped.

        let bigger = metrics(240, 70, 8, 17);
        let (bw, bh) = bigger.image_area();
        r.relayout(Layout::compute(&bigger, ScaleMode::Native, bw, bh, (0, 0)));
        assert_eq!(
            r.dirty_tiles(),
            r.tile_count(),
            "nothing reached the terminal, so nothing can be kept"
        );
    }

    #[test]
    fn a_tile_that_is_owed_pixels_stays_owed_across_a_resize() {
        // Damage that arrived before a resize is not answered by the resize. The tile it
        // touched has to survive as owing, or a change the server reported once is lost --
        // which a bitmap cleared by the relayout could do and a per-tile key cannot.
        let m = ghostty();
        let (w, h) = m.image_area();
        let mut r = Renderer::new(
            Layout::compute(&m, ScaleMode::Native, w, h, (0, 0)),
            true,
            Transfer::Direct,
        );
        let fb = Framebuffer::new(w, h);
        let mut out = Vec::new();
        r.compose(&fb, &mut out);
        r.commit();
        assert!(!r.has_work(), "the screen is up to date");

        // One tile's worth of damage, in a corner a growing window does not disturb.
        r.mark(Rect::new(0, 0, 8, 8));
        assert!(r.is_dirty(0, 0));

        let bigger = metrics(202, 50, 8, 17);
        let (bw, bh) = bigger.image_area();
        r.relayout(Layout::compute(&bigger, ScaleMode::Native, bw, bh, (0, 0)));
        assert!(
            r.is_dirty(0, 0),
            "the tile was owed pixels before the resize and is still owed them after"
        );
    }

    #[test]
    fn a_relayout_to_the_layout_already_in_force_does_nothing() {
        // One resize settles through several paths, and all but the first are handed the
        // geometry they are asking for. Each of them used to wipe the screen and mark
        // every tile again, so a single resize flickered two or three times over.
        let m = ghostty();
        let (w, h) = m.image_area();
        let same = Layout::compute(&m, ScaleMode::Native, w, h, (0, 0));
        let mut r = Renderer::new(same, true, Transfer::Direct);
        let fb = Framebuffer::new(w, h);
        let mut out = Vec::new();
        r.compose(&fb, &mut out);
        r.commit();

        assert!(
            r.relayout(same).is_empty(),
            "an unchanged layout must not wipe the screen"
        );
        assert!(!r.has_work(), "nor redraw a screen that already shows it");
    }

    #[test]
    fn shrinking_the_grid_drops_the_images_it_has_no_place_for() {
        // The other half: a tile the new grid does not reach is never retransmitted, so
        // nothing would move its placement off the cells it is on. Those are named one by
        // one, which is the whole difference between this and erasing the screen.
        let m = ghostty();
        let (w, h) = m.image_area();
        let big = Layout::compute(&m, ScaleMode::Native, w, h, (0, 0));
        let mut r = Renderer::new(big, true, Transfer::Direct);

        let fb = Framebuffer::new(w, h);
        let mut out = Vec::new();
        let stats = r.compose(&fb, &mut out);
        let before = r.tile_count();
        assert_eq!(stats.tiles, before);
        r.commit();

        let was = r.grid;
        let small = Layout::compute(&m, ScaleMode::Fit, 64, 64, (0, 0));
        let cleanup = r.relayout(small);
        let text = String::from_utf8(cleanup).unwrap();
        assert!(r.tile_count() < before, "the grid was supposed to shrink");
        assert!(
            !text.contains("d=A") && !text.contains("\x1b[2J"),
            "the screen must not be erased wholesale: {text:?}"
        );

        let mut dropped = 0;
        for ty in 0..was.ny {
            for tx in 0..was.nx {
                let id = was.id(tx, ty);
                let named = text.contains(&format!("a=d,d=I,i={id},"));
                if tx >= r.grid.nx || ty >= r.grid.ny {
                    assert!(named, "tile {tx},{ty} was left on the screen: {text:?}");
                    dropped += 1;
                } else {
                    assert!(
                        !named,
                        "tile {tx},{ty} is still in the grid and was deleted: {text:?}"
                    );
                }
            }
        }
        assert_eq!(
            text.matches("a=d").count(),
            dropped,
            "only the dropped tiles are deleted: {text:?}"
        );
        assert!(r.has_work(), "and what is left has to be drawn again");
    }

    #[test]
    fn composing_emits_one_command_group_per_dirty_tile() {
        let m = metrics(32, 9, 8, 16);
        let (w, h) = m.image_area();
        let l = Layout::compute(&m, ScaleMode::Native, w, h, (0, 0));
        let mut r = Renderer::new(l, true, Transfer::Direct);
        let fb = Framebuffer::new(w, h);

        let mut out = Vec::new();
        r.compose(&fb, &mut out);
        r.commit();

        out.clear();
        r.mark(Rect::new(0, 0, 1, 1));
        let stats = r.compose(&fb, &mut out);
        assert_eq!(stats.tiles, 1);
        assert!(stats.bytes > 0);
        assert_eq!(String::from_utf8_lossy(&out).matches("a=T").count(), 1);
    }

    #[test]
    fn nothing_dirty_means_nothing_written() {
        let m = ghostty();
        let (w, h) = m.image_area();
        let l = Layout::compute(&m, ScaleMode::Native, w, h, (0, 0));
        let mut r = Renderer::new(l, true, Transfer::Direct);
        r.commit();
        let fb = Framebuffer::new(w, h);
        let mut out = Vec::new();
        let stats = r.compose(&fb, &mut out);
        assert_eq!(stats.tiles, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn rect_helpers_behave_at_the_edges() {
        let a = Rect::new(0, 0, 10, 10);
        assert_eq!(
            a.intersect(&Rect::new(5, 5, 10, 10)),
            Some(Rect::new(5, 5, 5, 5))
        );
        assert_eq!(a.intersect(&Rect::new(10, 0, 5, 5)), None);
        assert_eq!(Rect::new(0, 0, 2, 2).expand(3), Rect::new(0, 0, 5, 5));
        assert_eq!(Rect::new(5, 5, 2, 2).expand(3), Rect::new(2, 2, 8, 8));
        assert!(Rect::new(0, 0, 0, 5).is_empty());
    }
}
