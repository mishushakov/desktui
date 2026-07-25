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
use crate::term::kitty::{IMAGE_ID_BASE, KittyEncoder, Placement};
use crate::term::shm::ShmPool;
use framebuffer::Framebuffer;
use scale::{Filter, Scaler};

/// Tiles aim for this many pixels on a side. Small enough that a cursor blink
/// costs almost nothing, large enough that zlib has something to work with and
/// the per-tile escape overhead stays in the noise.
const TILE_TARGET_PX: u32 = 128;

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
        Rect::new(x, y, self.w + (self.x - x) + pad, self.h + (self.y - y) + pad)
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
            let fit = (f64::from(area_w) / f64::from(src_w)).min(f64::from(area_h) / f64::from(src_h));
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
        let nx = layout.dst_w.div_ceil(tile_px_w) as u16;
        let ny = layout.dst_h.div_ceil(tile_px_h) as u16;

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
        let x1 = ((r.right().saturating_sub(1)) / tw).min(u32::from(self.nx.saturating_sub(1))) as u16;
        let y1 = ((r.bottom().saturating_sub(1)) / th).min(u32::from(self.ny.saturating_sub(1))) as u16;
        (x0, y0, x1, y1)
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

pub struct Renderer {
    layout: Layout,
    grid: TileGrid,
    dirty: Vec<bool>,
    dirty_count: usize,
    enc: KittyEncoder,
    scaler: Scaler,
    scaled: Framebuffer,
    scratch: Vec<u8>,
    /// Tiles present in the terminal's image store, so a shrinking grid can
    /// clean up after itself.
    placed: usize,
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
            dirty: vec![true; grid.len()],
            dirty_count: grid.len(),
            grid,
            layout,
            enc: KittyEncoder::new(compress),
            scaler: Scaler::new(),
            scaled: Framebuffer::new(0, 0),
            scratch: Vec::with_capacity(TILE_TARGET_PX as usize * TILE_TARGET_PX as usize * 3),
            placed: 0,
            transfer,
            shm: ShmPool::new(),
            shm_failed: false,
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Adopt a new layout, redrawing everything.
    ///
    /// Returns escape bytes that must be written before the next frame when the
    /// grid shrank, to drop images that no longer have a tile.
    pub fn relayout(&mut self, layout: Layout) -> Vec<u8> {
        let grid = TileGrid::new(&layout);
        let mut cleanup = Vec::new();
        if grid.len() < self.placed {
            for idx in grid.len()..self.placed {
                KittyEncoder::delete(&mut cleanup, IMAGE_ID_BASE + idx as u32);
            }
            self.placed = grid.len();
        }

        self.layout = layout;
        self.grid = grid;
        self.dirty.clear();
        self.dirty.resize(self.grid.len(), true);
        self.dirty_count = self.grid.len();
        if self.layout.needs_scaling() {
            self.scaled.resize(self.layout.dst_w, self.layout.dst_h);
        } else {
            self.scaled.resize(0, 0);
        }
        cleanup
    }

    /// Mark damage, in source-framebuffer pixels.
    pub fn mark(&mut self, r: Rect) {
        if r.is_empty() || self.grid.len() == 0 {
            return;
        }
        let Some(dst) = self.layout.src_to_dst(r) else {
            return;
        };
        let (x0, y0, x1, y1) = self.grid.tiles_covering(dst);
        for ty in y0..=y1 {
            for tx in x0..=x1 {
                let idx = usize::from(ty) * usize::from(self.grid.nx) + usize::from(tx);
                if let Some(slot) = self.dirty.get_mut(idx)
                    && !*slot {
                        *slot = true;
                        self.dirty_count += 1;
                    }
            }
        }
    }

    pub fn mark_all(&mut self) {
        for slot in &mut self.dirty {
            *slot = true;
        }
        self.dirty_count = self.dirty.len();
    }

    pub fn has_work(&self) -> bool {
        self.dirty_count > 0
    }

    #[cfg(test)]
    pub fn dirty_tiles(&self) -> usize {
        self.dirty_count
    }

    pub fn tile_count(&self) -> usize {
        self.grid.len()
    }

    /// Compose the dirty tiles into `out`.
    ///
    /// The caller wraps this in synchronised-output markers and adds its own
    /// chrome; nothing here writes to the terminal directly.
    pub fn compose(&mut self, fb: &Framebuffer, out: &mut Vec<u8>) -> FrameStats {
        let mut stats = FrameStats::default();
        if self.dirty_count == 0 || self.grid.len() == 0 {
            return stats;
        }
        let before = out.len();

        // Resample once per frame, not once per tile.
        if self.layout.needs_scaling() {
            if self.scaled.width() != self.layout.dst_w || self.scaled.height() != self.layout.dst_h
            {
                self.scaled.resize(self.layout.dst_w, self.layout.dst_h);
            }
            let filter = self.layout.filter();
            if let Err(err) = self
                .scaler
                .resize(fb, self.layout.src, &mut self.scaled, filter)
            {
                tracing::warn!("resample failed: {err:#}");
                return stats;
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
                if !self.dirty[idx] {
                    continue;
                }
                let tile = self.grid.tile_rect(tx, ty);
                if tile.is_empty() {
                    continue;
                }

                self.scratch.clear();
                source.pack_rgb(
                    Rect::new(tile.x + off_x, tile.y + off_y, tile.w, tile.h),
                    &mut self.scratch,
                );

                let col = self.layout.origin_col + tx * self.grid.tile_cols;
                let row = self.layout.origin_row + ty * self.grid.tile_rows;
                let at = Placement {
                    id: IMAGE_ID_BASE + idx as u32,
                    col,
                    row,
                    w: tile.w,
                    h: tile.h,
                };

                let mut sent = false;
                if self.transfer == Transfer::Shm && !self.shm_failed {
                    match self.shm.publish(&self.scratch) {
                        Ok(name) => {
                            self.enc.place_shm(out, at, &name);
                            sent = true;
                        }
                        Err(err) => {
                            // One complaint, then carry on the slow way for the
                            // rest of the session.
                            tracing::warn!("shared memory unavailable, using base64: {err}");
                            self.shm_failed = true;
                        }
                    }
                }
                if !sent {
                    self.enc.place_rgb(out, at, &self.scratch);
                }

                stats.tiles += 1;
                stats.pixels += tile.area();
                self.placed = self.placed.max(idx + 1);
            }
        }

        stats.bytes = out.len() - before;
        stats
    }

    /// Called once a composed frame has actually reached the terminal.
    pub fn commit(&mut self) {
        for slot in &mut self.dirty {
            *slot = false;
        }
        self.dirty_count = 0;
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
        let (sx, sy) = l.terminal_px_to_src(ox + l.dst_w - 1, oy + l.dst_h - 1).unwrap();
        assert!(sx >= 1918 && sy >= 1078, "{sx},{sy}");
        // The letterbox above the image is not part of the desktop.
        if oy > 0 {
            assert_eq!(l.terminal_px_to_src(ox, oy - 1), None);
        }
    }

    #[test]
    fn shrinking_the_grid_deletes_the_orphaned_images() {
        let m = ghostty();
        let (w, h) = m.image_area();
        let big = Layout::compute(&m, ScaleMode::Native, w, h, (0, 0));
        let mut r = Renderer::new(big, true, Transfer::Direct);

        let fb = Framebuffer::new(w, h);
        let mut out = Vec::new();
        let stats = r.compose(&fb, &mut out);
        assert_eq!(stats.tiles, r.tile_count());
        r.commit();

        let small = Layout::compute(&m, ScaleMode::Fit, 64, 64, (0, 0));
        let cleanup = r.relayout(small);
        let text = String::from_utf8(cleanup).unwrap();
        assert!(text.contains("a=d"), "expected deletes, got {text:?}");
        assert!(r.has_work(), "a relayout must redraw everything");
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
        assert_eq!(a.intersect(&Rect::new(5, 5, 10, 10)), Some(Rect::new(5, 5, 5, 5)));
        assert_eq!(a.intersect(&Rect::new(10, 0, 5, 5)), None);
        assert_eq!(Rect::new(0, 0, 2, 2).expand(3), Rect::new(0, 0, 5, 5));
        assert_eq!(Rect::new(5, 5, 2, 2).expand(3), Rect::new(2, 2, 8, 8));
        assert!(Rect::new(0, 0, 0, 5).is_empty());
    }
}
