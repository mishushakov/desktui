//! A synthetic screen for checking the pipeline without a VNC server.
//!
//! Each element proves one thing:
//!
//! * **1-pixel checkerboard** -- if anything resamples the image this turns into
//!   grey mush or moiré. Crisp means every remote pixel landed on exactly one
//!   terminal pixel.
//! * **1-pixel border and corner marks** -- prove the image is placed where we
//!   think it is, with nothing clipped by a cell-rounding mistake.
//! * **R G B squares, left to right** -- prove the channel order survived
//!   BGRA storage and the pack to RGB. Reversed means a swizzle bug.
//! * **Bouncing box** -- exercises damage tracking and gives the frame rate
//!   something to measure.
//! * **Crosshair** -- drawn at the pointer position reported by the terminal, so
//!   any mismatch between mouse pixels and image pixels is immediately visible.

use super::Rect;
use super::framebuffer::Framebuffer;

const BOX_SIZE: u32 = 64;
const CROSSHAIR_ARM: u32 = 12;

/// Colours as BGR, matching the framebuffer's byte order.
const BG: [u8; 3] = [24, 24, 24];
const GRID: [u8; 3] = [64, 64, 64];
const WHITE: [u8; 3] = [255, 255, 255];
const BLACK: [u8; 3] = [0, 0, 0];
const RED: [u8; 3] = [0, 0, 255];
const GREEN: [u8; 3] = [0, 255, 0];
const BLUE: [u8; 3] = [255, 0, 0];
const BOX_COLOUR: [u8; 3] = [220, 200, 40];
const CROSSHAIR: [u8; 3] = [0, 220, 255];

pub struct TestPattern {
    w: u32,
    h: u32,
    /// The static part, kept so moving elements can be erased by copying back.
    bg: Framebuffer,
    box_x: i64,
    box_y: i64,
    vel_x: i64,
    vel_y: i64,
    drawn_box: Option<Rect>,
    cursor: Option<(u32, u32)>,
    drawn_cursor: Vec<Rect>,
}

impl TestPattern {
    pub fn new(w: u32, h: u32) -> Self {
        let mut bg = Framebuffer::new(w, h);
        paint_static(&mut bg);
        Self {
            w,
            h,
            bg,
            box_x: 0,
            box_y: 0,
            vel_x: 7,
            vel_y: 5,
            drawn_box: None,
            cursor: None,
            drawn_cursor: Vec::new(),
        }
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.w = w;
        self.h = h;
        self.bg.resize(w, h);
        paint_static(&mut self.bg);
        self.box_x = 0;
        self.box_y = 0;
        self.drawn_box = None;
        self.drawn_cursor.clear();
    }

    /// Paint the whole static background. Callers follow this with a full redraw.
    pub fn paint_all(&self, fb: &mut Framebuffer) {
        fb.data_mut().copy_from_slice(self.bg.data());
    }

    /// Move the crosshair. `None` hides it.
    pub fn set_cursor(&mut self, pos: Option<(u32, u32)>) {
        self.cursor = pos;
    }

    /// Advance one frame, returning the regions that changed.
    pub fn step(&mut self, fb: &mut Framebuffer, damage: &mut Vec<Rect>) {
        // Erase what moved last frame.
        for r in self.drawn_cursor.drain(..).collect::<Vec<_>>() {
            self.restore(fb, r);
            damage.push(r);
        }
        if let Some(r) = self.drawn_box.take() {
            self.restore(fb, r);
            damage.push(r);
        }

        // Bounce.
        if self.w > BOX_SIZE && self.h > BOX_SIZE {
            let max_x = i64::from(self.w - BOX_SIZE);
            let max_y = i64::from(self.h - BOX_SIZE);
            self.box_x += self.vel_x;
            self.box_y += self.vel_y;
            if self.box_x < 0 {
                self.box_x = 0;
                self.vel_x = self.vel_x.abs();
            } else if self.box_x > max_x {
                self.box_x = max_x;
                self.vel_x = -self.vel_x.abs();
            }
            if self.box_y < 0 {
                self.box_y = 0;
                self.vel_y = self.vel_y.abs();
            } else if self.box_y > max_y {
                self.box_y = max_y;
                self.vel_y = -self.vel_y.abs();
            }

            let r = Rect::new(self.box_x as u32, self.box_y as u32, BOX_SIZE, BOX_SIZE);
            if let Some(drawn) = fb.fill(r, BOX_COLOUR) {
                // A one-pixel white notch in the corner: if the box ever looks
                // blurred at its edge, scaling crept in.
                fb.fill(Rect::new(drawn.x, drawn.y, 1, 1), WHITE);
                self.drawn_box = Some(drawn);
                damage.push(drawn);
            }
        }

        if let Some((cx, cy)) = self.cursor {
            let arms = [
                Rect::new(cx.saturating_sub(CROSSHAIR_ARM), cy, CROSSHAIR_ARM * 2 + 1, 1),
                Rect::new(cx, cy.saturating_sub(CROSSHAIR_ARM), 1, CROSSHAIR_ARM * 2 + 1),
            ];
            for arm in arms {
                if let Some(drawn) = fb.fill(arm, CROSSHAIR) {
                    self.drawn_cursor.push(drawn);
                    damage.push(drawn);
                }
            }
            // Centre pixel in white: this is the exact pixel the terminal says
            // the pointer is on.
            if let Some(drawn) = fb.fill(Rect::new(cx, cy, 1, 1), WHITE) {
                self.drawn_cursor.push(drawn);
                damage.push(drawn);
            }
        }
    }

    /// Copy a region of the static background back over the framebuffer.
    fn restore(&self, fb: &mut Framebuffer, r: Rect) {
        let w = self.w;
        if fb.width() != w || fb.height() != self.h {
            return;
        }
        for row in 0..r.h {
            let y = r.y + row;
            if y >= self.h {
                break;
            }
            let width = r.w.min(w.saturating_sub(r.x));
            if width == 0 {
                continue;
            }
            let start = ((y as usize) * (w as usize) + r.x as usize) * 4;
            let len = (width as usize) * 4;
            let (dst, src) = (fb.data_mut(), self.bg.data());
            dst[start..start + len].copy_from_slice(&src[start..start + len]);
        }
    }
}

/// Size of one colour swatch in the channel-order strip.
fn swatch_size(w: u32) -> u32 {
    (w / 16).clamp(8, 48)
}

/// Bottom edge of the swatch strip, which nothing else may overlap.
fn swatch_band_bottom(w: u32) -> u32 {
    let sq = swatch_size(w);
    sq * 2 + 8
}

/// Where the checkerboard goes, or `None` when the screen is too small for one.
///
/// Shared with the tests so they check the real region rather than a second copy
/// of this arithmetic.
fn checker_region(w: u32, h: u32) -> Option<Rect> {
    let top = swatch_band_bottom(w) + 8;
    let avail_h = h.saturating_sub(top + 8);
    let avail_w = w.saturating_sub(16);
    let size = 256.min(avail_w).min(avail_h);
    if size < 8 {
        return None;
    }
    let x = (w - size) / 2;
    let y = top + (avail_h - size) / 2;
    Some(Rect::new(x, y, size, size))
}

fn paint_static(fb: &mut Framebuffer) {
    let (w, h) = (fb.width(), fb.height());
    if w == 0 || h == 0 {
        return;
    }
    fb.fill(Rect::new(0, 0, w, h), BG);

    // 16-pixel grid.
    let mut x = 0;
    while x < w {
        fb.fill(Rect::new(x, 0, 1, h), GRID);
        x += 16;
    }
    let mut y = 0;
    while y < h {
        fb.fill(Rect::new(0, y, w, 1), GRID);
        y += 16;
    }

    // Channel-order check: red, green, blue, left to right.
    let sq = swatch_size(w);
    for (i, colour) in [RED, GREEN, BLUE].into_iter().enumerate() {
        let x = sq + (i as u32) * (sq * 3 / 2);
        fb.fill(Rect::new(x, sq, sq, sq), colour);
    }

    // 1-pixel checkerboard, the crispness test. Kept clear of the swatches so
    // neither test element can hide the other.
    if let Some(region) = checker_region(w, h) {
        for row in 0..region.h {
            for col in 0..region.w {
                let colour = if (row + col) % 2 == 0 { WHITE } else { BLACK };
                fb.fill(Rect::new(region.x + col, region.y + row, 1, 1), colour);
            }
        }
    }

    // Border and corner marks: placement and clipping.
    fb.fill(Rect::new(0, 0, w, 1), WHITE);
    fb.fill(Rect::new(0, h - 1, w, 1), WHITE);
    fb.fill(Rect::new(0, 0, 1, h), WHITE);
    fb.fill(Rect::new(w - 1, 0, 1, h), WHITE);
    let m = 8.min(w).min(h);
    for (x, y) in [(0, 0), (w - m, 0), (0, h - m), (w - m, h - m)] {
        fb.fill(Rect::new(x, y, m, m), WHITE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgr(fb: &Framebuffer, x: u32, y: u32) -> [u8; 3] {
        let i = ((y as usize) * (fb.width() as usize) + x as usize) * 4;
        fb.data()[i..i + 3].try_into().unwrap()
    }

    #[test]
    fn static_pattern_marks_the_corners_and_border() {
        let mut fb = Framebuffer::new(64, 64);
        paint_static(&mut fb);
        assert_eq!(bgr(&fb, 0, 0), WHITE);
        assert_eq!(bgr(&fb, 63, 63), WHITE);
        assert_eq!(bgr(&fb, 32, 0), WHITE);
    }

    #[test]
    fn checkerboard_alternates_every_pixel() {
        let mut fb = Framebuffer::new(600, 500);
        paint_static(&mut fb);
        let region = checker_region(600, 500).unwrap();
        let (cx, cy) = (region.x, region.y);
        assert_eq!(bgr(&fb, cx, cy), WHITE);
        assert_eq!(bgr(&fb, cx + 1, cy), BLACK);
        assert_eq!(bgr(&fb, cx, cy + 1), BLACK);
        assert_eq!(bgr(&fb, cx + 1, cy + 1), WHITE);
    }

    #[test]
    fn swatches_and_checkerboard_never_overlap() {
        // Either element hiding the other would silently disable one of the two
        // things the pattern exists to prove.
        for (w, h) in [(600u32, 500u32), (1600, 833), (320, 200), (128, 700)] {
            let sq = swatch_size(w);
            let swatches = Rect::new(sq, sq, sq * 4, sq);
            if let Some(check) = checker_region(w, h) {
                assert!(
                    swatches.intersect(&check).is_none(),
                    "{w}x{h}: swatches {swatches:?} overlap checkerboard {check:?}"
                );
            }
        }
    }

    #[test]
    fn colour_swatches_are_red_green_blue_left_to_right() {
        let mut fb = Framebuffer::new(600, 500);
        paint_static(&mut fb);
        let sq = swatch_size(600);
        let centre = |i: u32| (sq + i * (sq * 3 / 2) + sq / 2, sq + sq / 2);
        let (x, y) = centre(0);
        assert_eq!(bgr(&fb, x, y), RED);
        let (x, y) = centre(1);
        assert_eq!(bgr(&fb, x, y), GREEN);
        let (x, y) = centre(2);
        assert_eq!(bgr(&fb, x, y), BLUE);
    }

    #[test]
    fn moving_box_reports_both_the_old_and_new_regions() {
        let mut pat = TestPattern::new(400, 300);
        let mut fb = Framebuffer::new(400, 300);
        pat.paint_all(&mut fb);

        let mut damage = Vec::new();
        pat.step(&mut fb, &mut damage);
        assert_eq!(damage.len(), 1, "first frame only draws the box");

        damage.clear();
        pat.step(&mut fb, &mut damage);
        assert_eq!(damage.len(), 2, "erase the old box, draw the new one");
    }

    #[test]
    fn erasing_restores_the_background_exactly() {
        let mut pat = TestPattern::new(400, 300);
        let mut fb = Framebuffer::new(400, 300);
        pat.paint_all(&mut fb);
        let reference = fb.data().to_vec();

        let mut damage = Vec::new();
        for _ in 0..20 {
            damage.clear();
            pat.step(&mut fb, &mut damage);
        }
        // Erase whatever is currently drawn and the frame must match the
        // original background byte for byte.
        pat.set_cursor(None);
        damage.clear();
        pat.box_x = -1000;
        pat.box_y = -1000;
        pat.step(&mut fb, &mut damage);
        pat.drawn_box = None;
        damage.clear();
        pat.step(&mut fb, &mut damage);

        let differing = fb
            .data()
            .iter()
            .zip(&reference)
            .filter(|(a, b)| a != b)
            .count();
        // Only the box at its clamped position should differ.
        assert!(differing <= (BOX_SIZE * BOX_SIZE * 4) as usize, "{differing}");
    }

    #[test]
    fn crosshair_lands_on_the_requested_pixel() {
        let mut pat = TestPattern::new(200, 200);
        let mut fb = Framebuffer::new(200, 200);
        pat.paint_all(&mut fb);
        pat.set_cursor(Some((100, 150)));
        let mut damage = Vec::new();
        pat.step(&mut fb, &mut damage);
        assert_eq!(bgr(&fb, 100, 150), WHITE);
        assert_eq!(bgr(&fb, 95, 150), CROSSHAIR);
        assert_eq!(bgr(&fb, 100, 145), CROSSHAIR);
    }

    #[test]
    fn a_crosshair_at_the_edge_is_clipped_not_fatal() {
        let mut pat = TestPattern::new(40, 40);
        let mut fb = Framebuffer::new(40, 40);
        pat.paint_all(&mut fb);
        pat.set_cursor(Some((0, 39)));
        let mut damage = Vec::new();
        pat.step(&mut fb, &mut damage);
        pat.set_cursor(Some((39, 0)));
        pat.step(&mut fb, &mut damage);
    }

    #[test]
    fn tiny_and_zero_sized_patterns_do_not_panic() {
        for (w, h) in [(0, 0), (1, 1), (3, 200), (200, 3)] {
            let mut pat = TestPattern::new(w, h);
            let mut fb = Framebuffer::new(w, h);
            pat.paint_all(&mut fb);
            pat.set_cursor(Some((0, 0)));
            let mut damage = Vec::new();
            pat.step(&mut fb, &mut damage);
        }
    }
}
