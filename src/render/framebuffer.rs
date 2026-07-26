//! The local copy of the remote screen.
//!
//! Stored as BGRA because that is the pixel format we ask the server for, which
//! makes raw rectangles a straight `memcpy`. The fourth byte is padding as far
//! as RFB is concerned; we keep it at `0xff` so nothing downstream can mistake
//! it for a transparent pixel.
//!
//! Every mutator clamps to the framebuffer bounds. A server is free to send us a
//! rectangle that does not fit -- through a bug or on purpose -- and none of
//! these methods may panic when it does.

use super::Rect;

#[derive(Debug)]
pub struct Framebuffer {
    w: u32,
    h: u32,
    data: Vec<u8>,
}

impl Framebuffer {
    pub fn new(w: u32, h: u32) -> Self {
        let mut fb = Self {
            w: 0,
            h: 0,
            data: Vec::new(),
        };
        fb.resize(w, h);
        fb
    }

    pub fn width(&self) -> u32 {
        self.w
    }

    pub fn height(&self) -> u32 {
        self.h
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    #[cfg(test)]
    pub fn rect(&self) -> Rect {
        Rect::new(0, 0, self.w, self.h)
    }

    /// Reallocate to a new size, keeping whatever content still fits.
    ///
    /// The overlap has to be preserved. After the desktop changes size the server
    /// sends only what it considers changed, and it does not consider content that
    /// merely moved to be changed -- so anything cleared here that the server does
    /// not resend stays black until something else happens to damage it. The spec
    /// rules out the obvious alternative: a client must not answer an
    /// `ExtendedDesktopSize` rectangle with a non-incremental request, because the
    /// server answers *that* with another rectangle and the two never stop. noVNC
    /// preserves its canvas across a resize for the same reason.
    pub fn resize(&mut self, w: u32, h: u32) {
        if w == self.w && h == self.h {
            return;
        }

        let mut next = vec![0u8; (w as usize) * (h as usize) * 4];
        for px in next.chunks_exact_mut(4) {
            px[3] = 0xff;
        }

        // Copy the region present in both, row by row: the stride changed.
        let copy_w = (w.min(self.w) as usize) * 4;
        let copy_h = h.min(self.h) as usize;
        for row in 0..copy_h {
            let from = row * (self.w as usize) * 4;
            let to = row * (w as usize) * 4;
            next[to..to + copy_w].copy_from_slice(&self.data[from..from + copy_w]);
        }

        self.data = next;
        self.w = w;
        self.h = h;
    }

    /// Clip a rectangle to the framebuffer, returning `None` if nothing is left.
    fn clip(&self, r: Rect) -> Option<Rect> {
        let x = r.x.min(self.w);
        let y = r.y.min(self.h);
        let w = r.w.min(self.w - x);
        let h = r.h.min(self.h - y);
        (w > 0 && h > 0).then_some(Rect::new(x, y, w, h))
    }

    fn row_range(&self, x: u32, y: u32, w: u32) -> std::ops::Range<usize> {
        let start = ((y as usize) * (self.w as usize) + (x as usize)) * 4;
        start..start + (w as usize) * 4
    }

    /// Write a BGRA rectangle straight in. `src` is tightly packed, `w * h * 4`.
    ///
    /// Returns the region actually written, which is the caller's damage.
    pub fn apply_bgra(&mut self, r: Rect, src: &[u8]) -> Option<Rect> {
        let clipped = self.clip(r)?;
        let src_stride = (r.w as usize) * 4;
        if src.len() < src_stride * (r.h as usize) {
            return None;
        }
        for row in 0..clipped.h {
            let src_off = (row as usize) * src_stride;
            let dst = self.row_range(clipped.x, clipped.y + row, clipped.w);
            let n = (clipped.w as usize) * 4;
            self.data[dst].copy_from_slice(&src[src_off..src_off + n]);
        }
        self.force_opaque(clipped);
        Some(clipped)
    }

    /// Write a packed RGB rectangle, as produced by decoding a Tight JPEG.
    pub fn apply_rgb(&mut self, r: Rect, src: &[u8]) -> Option<Rect> {
        let clipped = self.clip(r)?;
        let src_stride = (r.w as usize) * 3;
        if src.len() < src_stride * (r.h as usize) {
            return None;
        }
        for row in 0..clipped.h {
            let src_off = (row as usize) * src_stride;
            let dst_range = self.row_range(clipped.x, clipped.y + row, clipped.w);
            let dst = &mut self.data[dst_range];
            for (px, rgb) in dst
                .chunks_exact_mut(4)
                .zip(src[src_off..].chunks_exact(3).take(clipped.w as usize))
            {
                px[0] = rgb[2];
                px[1] = rgb[1];
                px[2] = rgb[0];
                px[3] = 0xff;
            }
        }
        Some(clipped)
    }

    /// Move a rectangle within the framebuffer, as `CopyRect` asks.
    ///
    /// Source and destination routinely overlap -- that is the whole point of
    /// the encoding when a window is dragged -- so rows are copied in the order
    /// that keeps the untouched half intact.
    pub fn copy_rect(&mut self, dst: Rect, src_x: u32, src_y: u32) -> Option<Rect> {
        // Both ends have to fit, so shrink the request until they do.
        let w = dst
            .w
            .min(self.w.saturating_sub(dst.x))
            .min(self.w.saturating_sub(src_x));
        let h = dst
            .h
            .min(self.h.saturating_sub(dst.y))
            .min(self.h.saturating_sub(src_y));
        if w == 0 || h == 0 {
            return None;
        }

        let rows: Vec<u32> = if dst.y > src_y {
            (0..h).rev().collect()
        } else {
            (0..h).collect()
        };
        for row in rows {
            let from = self.row_range(src_x, src_y + row, w);
            let to = self.row_range(dst.x, dst.y + row, w).start;
            self.data.copy_within(from, to);
        }
        Some(Rect::new(dst.x, dst.y, w, h))
    }

    pub fn fill(&mut self, r: Rect, bgr: [u8; 3]) -> Option<Rect> {
        let clipped = self.clip(r)?;
        for row in 0..clipped.h {
            let range = self.row_range(clipped.x, clipped.y + row, clipped.w);
            for px in self.data[range].chunks_exact_mut(4) {
                px[0] = bgr[0];
                px[1] = bgr[1];
                px[2] = bgr[2];
                px[3] = 0xff;
            }
        }
        Some(clipped)
    }

    fn force_opaque(&mut self, r: Rect) {
        for row in 0..r.h {
            let range = self.row_range(r.x, r.y + row, r.w);
            for px in self.data[range].chunks_exact_mut(4) {
                px[3] = 0xff;
            }
        }
    }

    /// Append a rectangle to `out` as packed RGB, which is what the graphics
    /// protocol takes. Any part of `r` outside the framebuffer is emitted black
    /// so the output is always exactly `r.w * r.h * 3` bytes.
    pub fn pack_rgb(&self, r: Rect, out: &mut Vec<u8>) {
        out.reserve((r.w as usize) * (r.h as usize) * 3);
        for row in 0..r.h {
            let y = r.y + row;
            let inside_w = if y < self.h {
                r.w.min(self.w.saturating_sub(r.x))
            } else {
                0
            };
            if inside_w > 0 {
                let range = self.row_range(r.x, y, inside_w);
                for px in self.data[range].chunks_exact(4) {
                    out.extend_from_slice(&[px[2], px[1], px[0]]);
                }
            }
            for _ in inside_w..r.w {
                out.extend_from_slice(&[0, 0, 0]);
            }
        }
    }

    /// The same, into a slice of exactly `r.w * r.h * 3` bytes.
    ///
    /// For packing straight into the shared memory the terminal will map, which is what
    /// makes a frame one pass over its pixels rather than a pack followed by a copy.
    /// Anything outside the framebuffer is black, as above.
    pub fn pack_rgb_into(&self, r: Rect, out: &mut [u8]) {
        let stride = (r.w as usize) * 3;
        debug_assert_eq!(out.len(), stride * (r.h as usize));
        for (row, line) in out.chunks_exact_mut(stride).enumerate() {
            let y = r.y + row as u32;
            let inside_w = if y < self.h {
                r.w.min(self.w.saturating_sub(r.x))
            } else {
                0
            };
            let split = (inside_w as usize) * 3;
            if inside_w > 0 {
                let range = self.row_range(r.x, y, inside_w);
                for (px, rgb) in self.data[range]
                    .chunks_exact(4)
                    .zip(line[..split].chunks_exact_mut(3))
                {
                    rgb.copy_from_slice(&[px[2], px[1], px[0]]);
                }
            }
            line[split..].fill(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra(fb: &Framebuffer, x: u32, y: u32) -> [u8; 4] {
        let i = ((y as usize) * (fb.width() as usize) + x as usize) * 4;
        fb.data()[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn new_framebuffer_is_opaque_black() {
        let fb = Framebuffer::new(2, 2);
        assert_eq!(bgra(&fb, 1, 1), [0, 0, 0, 0xff]);
    }

    #[test]
    fn raw_rect_lands_where_asked() {
        let mut fb = Framebuffer::new(4, 4);
        let src = vec![1u8, 2, 3, 0, 4, 5, 6, 0];
        let damage = fb.apply_bgra(Rect::new(1, 2, 2, 1), &src).unwrap();
        assert_eq!(damage, Rect::new(1, 2, 2, 1));
        assert_eq!(bgra(&fb, 1, 2), [1, 2, 3, 0xff]);
        assert_eq!(bgra(&fb, 2, 2), [4, 5, 6, 0xff]);
        assert_eq!(bgra(&fb, 0, 2), [0, 0, 0, 0xff]);
    }

    #[test]
    fn out_of_bounds_rects_are_clipped_not_fatal() {
        let mut fb = Framebuffer::new(4, 4);
        let src = vec![9u8; 4 * 4 * 4];
        let damage = fb.apply_bgra(Rect::new(3, 3, 4, 4), &src).unwrap();
        assert_eq!(damage, Rect::new(3, 3, 1, 1));

        // Entirely outside, and a rect whose declared size exceeds its data.
        assert!(fb.apply_bgra(Rect::new(9, 9, 2, 2), &src).is_none());
        assert!(fb.apply_bgra(Rect::new(0, 0, 4, 4), &[0u8; 4]).is_none());
    }

    #[test]
    fn jpeg_rect_is_byte_swapped_into_place() {
        let mut fb = Framebuffer::new(2, 1);
        // Pure red in RGB order.
        fb.apply_rgb(Rect::new(0, 0, 1, 1), &[0xff, 0, 0]).unwrap();
        assert_eq!(bgra(&fb, 0, 0), [0, 0, 0xff, 0xff]);
    }

    #[test]
    fn copy_rect_handles_downward_overlap() {
        let mut fb = Framebuffer::new(1, 4);
        for y in 0..4u32 {
            fb.fill(Rect::new(0, y, 1, 1), [y as u8, 0, 0]);
        }
        // Shift rows 0..3 down by one, the classic drag-a-window case.
        fb.copy_rect(Rect::new(0, 1, 1, 3), 0, 0).unwrap();
        assert_eq!(bgra(&fb, 0, 1)[0], 0);
        assert_eq!(bgra(&fb, 0, 2)[0], 1);
        assert_eq!(bgra(&fb, 0, 3)[0], 2);
    }

    #[test]
    fn copy_rect_handles_upward_and_sideways_overlap() {
        let mut fb = Framebuffer::new(4, 2);
        for x in 0..4u32 {
            fb.fill(Rect::new(x, 0, 1, 1), [x as u8 + 1, 0, 0]);
        }
        // Same row, shifted left by one.
        fb.copy_rect(Rect::new(0, 0, 3, 1), 1, 0).unwrap();
        assert_eq!(bgra(&fb, 0, 0)[0], 2);
        assert_eq!(bgra(&fb, 1, 0)[0], 3);
        assert_eq!(bgra(&fb, 2, 0)[0], 4);
    }

    #[test]
    fn copy_rect_shrinks_rather_than_reading_past_the_end() {
        let mut fb = Framebuffer::new(4, 4);
        let damage = fb.copy_rect(Rect::new(2, 2, 4, 4), 3, 3).unwrap();
        assert_eq!(damage, Rect::new(2, 2, 1, 1));
        assert!(fb.copy_rect(Rect::new(0, 0, 2, 2), 9, 9).is_none());
    }

    #[test]
    fn pack_rgb_swizzles_and_pads_past_the_edge() {
        let mut fb = Framebuffer::new(2, 1);
        fb.apply_bgra(Rect::new(0, 0, 2, 1), &[10, 20, 30, 0, 40, 50, 60, 0])
            .unwrap();

        let mut out = Vec::new();
        fb.pack_rgb(Rect::new(0, 0, 2, 1), &mut out);
        assert_eq!(out, vec![30, 20, 10, 60, 50, 40]);

        // A tile hanging off the right and bottom edges still yields full rows.
        out.clear();
        fb.pack_rgb(Rect::new(1, 0, 2, 2), &mut out);
        assert_eq!(out.len(), 2 * 2 * 3);
        assert_eq!(&out[0..3], &[60, 50, 40]);
        assert_eq!(&out[3..12], &[0; 9]);
    }

    #[test]
    fn resize_keeps_the_overlap_and_blacks_out_the_rest() {
        // The server sends only what it thinks changed after a desktop resize, and
        // content that merely moved does not count -- so anything discarded here that
        // the server does not resend would stay black. Answering the resize with a
        // non-incremental request is not allowed, so the overlap has to survive.
        let mut fb = Framebuffer::new(2, 2);
        fb.fill(Rect::new(0, 0, 1, 1), [11, 12, 13]);
        fb.fill(Rect::new(1, 0, 1, 1), [21, 22, 23]);
        fb.fill(Rect::new(0, 1, 1, 1), [31, 32, 33]);

        // Grow: everything that was there is still there, in the same place.
        fb.resize(4, 3);
        assert_eq!((fb.width(), fb.height()), (4, 3));
        assert_eq!(bgra(&fb, 0, 0), [11, 12, 13, 0xff]);
        assert_eq!(bgra(&fb, 1, 0), [21, 22, 23, 0xff]);
        assert_eq!(
            bgra(&fb, 0, 1),
            [31, 32, 33, 0xff],
            "the stride changed too"
        );
        // And the new area is opaque black rather than whatever the allocator had.
        assert_eq!(bgra(&fb, 3, 2), [0, 0, 0, 0xff]);
        assert_eq!(bgra(&fb, 2, 0), [0, 0, 0, 0xff]);
    }

    #[test]
    fn shrinking_keeps_the_top_left_and_drops_the_rest() {
        let mut fb = Framebuffer::new(4, 4);
        fb.fill(fb.rect(), [9, 9, 9]);
        fb.fill(Rect::new(0, 0, 1, 1), [1, 2, 3]);
        fb.resize(2, 2);
        assert_eq!((fb.width(), fb.height()), (2, 2));
        assert_eq!(bgra(&fb, 0, 0), [1, 2, 3, 0xff]);
        assert_eq!(bgra(&fb, 1, 1), [9, 9, 9, 0xff]);
    }

    #[test]
    fn resizing_to_the_same_size_is_a_no_op() {
        let mut fb = Framebuffer::new(2, 2);
        fb.fill(fb.rect(), [7, 7, 7]);
        fb.resize(2, 2);
        assert_eq!(
            bgra(&fb, 1, 1),
            [7, 7, 7, 0xff],
            "content must not be dropped"
        );
    }

    #[test]
    fn resizing_through_zero_does_not_panic() {
        let mut fb = Framebuffer::new(4, 4);
        fb.resize(0, 0);
        assert_eq!((fb.width(), fb.height()), (0, 0));
        fb.resize(2, 2);
        assert_eq!(bgra(&fb, 0, 0), [0, 0, 0, 0xff]);
    }
}
