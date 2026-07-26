//! Resampling, for when the remote resolution does not match the terminal.
//!
//! Alpha handling is switched off deliberately: our fourth byte is RFB padding,
//! not transparency. Left on, the resizer would premultiply by whatever the
//! server happened to put there -- and servers routinely send zero -- turning
//! the whole frame black.

use anyhow::{Context, Result};
use fast_image_resize::images::{TypedCroppedImageMut, TypedImage, TypedImageRef};
use fast_image_resize::pixels::U8x4;
use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};

use super::Rect;
use super::framebuffer::Framebuffer;

/// How to resample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// Pixel doubling. The right choice for whole-number scale factors: crisp,
    /// and cheaper than any convolution.
    Nearest,
    /// Good quality for arbitrary ratios.
    Lanczos3,
}

impl Filter {
    /// Extra destination pixels a single source pixel can influence. Damage has
    /// to be padded by this much or the edges of a changed region go stale.
    pub fn dst_padding(self) -> u32 {
        match self {
            Filter::Nearest => 1,
            Filter::Lanczos3 => 3,
        }
    }

    /// Destination pixels a region has to be grown by before being resampled on its own,
    /// at a given scale, so that its own pixels come out as they would have with the whole
    /// screen.
    ///
    /// [`Self::dst_padding`] is the reach at 1:1; magnifying spreads one source pixel over
    /// more destination pixels, so the reach grows with it. Plus a source pixel of slack,
    /// so the filter's window falls inside the crop rather than exactly on its boundary.
    ///
    /// This buys agreement to within one in the last place, not exactly: a convolution
    /// accumulated over a window at a different offset rounds differently in the final
    /// byte. One step of one channel is invisible, and
    /// `a_region_resampled_alone_matches_the_same_pixels_resampled_whole` holds the bound.
    pub fn dst_reach(self, scale: f64) -> u32 {
        let magnified = scale.max(1.0);
        ((f64::from(self.dst_padding()) + 1.0) * magnified).ceil() as u32
    }

    fn alg(self) -> ResizeAlg {
        match self {
            Filter::Nearest => ResizeAlg::Nearest,
            Filter::Lanczos3 => ResizeAlg::Convolution(FilterType::Lanczos3),
        }
    }
}

pub struct Scaler {
    resizer: Resizer,
}

impl Scaler {
    pub fn new() -> Self {
        Self {
            resizer: Resizer::new(),
        }
    }

    /// Resample `crop` out of `src` into `region` of `dst`, leaving the rest of `dst`
    /// alone.
    ///
    /// The crop is in fractional source pixels on purpose. A destination region maps back
    /// to a source rectangle that rarely lands on whole pixels, and rounding it would
    /// shift that part of the picture against the rest by a fraction of a pixel -- which
    /// on a still image is a visible seam.
    ///
    /// The caller is expected to have grown `region` past the damage by the filter's
    /// reach: a pixel at the edge of a region resampled on its own has only the source
    /// inside the crop to work from, where in a whole-screen resample it would have had
    /// the source beyond it too. Growing the region and throwing the edge away is what
    /// makes the two agree.
    pub fn resize_region(
        &mut self,
        src: &Framebuffer,
        crop: (f64, f64, f64, f64),
        dst: &mut Framebuffer,
        region: Rect,
        filter: Filter,
    ) -> Result<()> {
        if region.is_empty() || crop.2 <= 0.0 || crop.3 <= 0.0 {
            return Ok(());
        }
        let src_view = TypedImageRef::<U8x4>::from_buffer(src.width(), src.height(), src.data())
            .context("source framebuffer is not a valid image")?;
        let (dw, dh) = (dst.width(), dst.height());
        let whole = TypedImage::<U8x4>::from_buffer(dw, dh, dst.data_mut())
            .context("destination framebuffer is not a valid image")?;
        let mut view = TypedCroppedImageMut::new(whole, region.x, region.y, region.w, region.h)
            .context("region is outside the destination")?;

        let options = ResizeOptions::new()
            .resize_alg(filter.alg())
            .use_alpha(false)
            .crop(crop.0, crop.1, crop.2, crop.3);

        self.resizer
            .resize_typed(&src_view, &mut view, Some(&options))
            .context("resample failed")
    }

    /// Resample `crop` out of `src` into the whole of `dst`.
    ///
    /// The degenerate region, and only tests ask for it: a frame resamples what it is
    /// about to send, which is a whole screen only when a whole screen has changed.
    #[cfg(test)]
    pub fn resize(
        &mut self,
        src: &Framebuffer,
        crop: Rect,
        dst: &mut Framebuffer,
        filter: Filter,
    ) -> Result<()> {
        let whole = Rect::new(0, 0, dst.width(), dst.height());
        let crop = (
            f64::from(crop.x),
            f64::from(crop.y),
            f64::from(crop.w),
            f64::from(crop.h),
        );
        self.resize_region(src, crop, dst, whole, filter)
    }
}

impl Default for Scaler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(fb: &Framebuffer, x: u32, y: u32) -> [u8; 4] {
        let i = ((y as usize) * (fb.width() as usize) + x as usize) * 4;
        fb.data()[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn nearest_doubling_keeps_exact_colours() {
        let mut src = Framebuffer::new(2, 1);
        src.apply_bgra(Rect::new(0, 0, 2, 1), &[10, 20, 30, 0xff, 40, 50, 60, 0xff])
            .unwrap();

        let mut dst = Framebuffer::new(4, 2);
        Scaler::new()
            .resize(&src, src.rect(), &mut dst, Filter::Nearest)
            .unwrap();

        assert_eq!(px(&dst, 0, 0), [10, 20, 30, 0xff]);
        assert_eq!(px(&dst, 1, 1), [10, 20, 30, 0xff]);
        assert_eq!(px(&dst, 3, 1), [40, 50, 60, 0xff]);
    }

    #[test]
    fn zero_alpha_padding_does_not_black_out_the_frame() {
        // A server that leaves the padding byte at zero must not cost us the
        // image, which is exactly what alpha premultiplication would do.
        let mut src = Framebuffer::new(2, 2);
        for y in 0..2 {
            for x in 0..2 {
                src.apply_bgra(Rect::new(x, y, 1, 1), &[200, 100, 50, 0x00])
                    .unwrap();
            }
        }
        // apply_bgra forces opacity, so poke the padding back to zero by hand.
        for px in src.data_mut().chunks_exact_mut(4) {
            px[3] = 0;
        }

        let mut dst = Framebuffer::new(4, 4);
        Scaler::new()
            .resize(&src, src.rect(), &mut dst, Filter::Lanczos3)
            .unwrap();

        let mid = px(&dst, 2, 2);
        assert!(
            mid[0] > 150 && mid[1] > 60 && mid[2] > 20,
            "colour was lost to premultiplication: {mid:?}"
        );
    }

    #[test]
    fn crop_selects_the_requested_region() {
        let mut src = Framebuffer::new(2, 1);
        src.apply_bgra(Rect::new(0, 0, 2, 1), &[1, 1, 1, 0xff, 9, 9, 9, 0xff])
            .unwrap();

        let mut dst = Framebuffer::new(1, 1);
        Scaler::new()
            .resize(&src, Rect::new(1, 0, 1, 1), &mut dst, Filter::Nearest)
            .unwrap();
        assert_eq!(px(&dst, 0, 0), [9, 9, 9, 0xff]);
    }

    /// Something with detail at the pixel level, which is what a convolution filter needs
    /// to see to disagree with itself.
    fn detailed(side: u32) -> Framebuffer {
        let mut fb = Framebuffer::new(side, side);
        for y in 0..side {
            for x in 0..side {
                let v = ((x * 7) ^ (y * 11)) as u8;
                fb.apply_bgra(
                    Rect::new(x, y, 1, 1),
                    &[v, v.wrapping_add(60), 200u8.wrapping_sub(v), 0xff],
                )
                .unwrap();
            }
        }
        fb
    }

    #[test]
    fn a_region_resampled_alone_matches_the_same_pixels_resampled_whole() {
        // The property the reach exists for, and the reason a frame can resample only what
        // it is about to send: a region done on its own has to come out as the same region
        // of a whole-screen resample, or every partial frame would leave a seam where the
        // two met.
        //
        // To within one step of one channel. A convolution accumulated over a window at a
        // different offset rounds differently in the final byte, and no amount of reach
        // changes that -- but one step of 255 is invisible, and the bound is what matters:
        // a seam is only a seam if it can be seen.
        for (side, dst_side) in [(64u32, 256u32), (256, 64)] {
            let src = detailed(side);
            let filter = Filter::Lanczos3;
            let scale = f64::from(dst_side) / f64::from(side);

            let mut whole = Framebuffer::new(dst_side, dst_side);
            Scaler::new()
                .resize(&src, src.rect(), &mut whole, filter)
                .unwrap();

            // A quarter of the way in, so there is picture on every side of it.
            let region = Rect::new(dst_side / 4, dst_side / 4, dst_side / 4, dst_side / 4);
            let grown = region
                .expand(filter.dst_reach(scale))
                .intersect(&Rect::new(0, 0, dst_side, dst_side))
                .unwrap();
            let per = f64::from(side) / f64::from(dst_side);
            let mut part = Framebuffer::new(dst_side, dst_side);
            Scaler::new()
                .resize_region(
                    &src,
                    (
                        f64::from(grown.x) * per,
                        f64::from(grown.y) * per,
                        f64::from(grown.w) * per,
                        f64::from(grown.h) * per,
                    ),
                    &mut part,
                    grown,
                    filter,
                )
                .unwrap();

            let mut worst = 0;
            for y in region.y..region.bottom() {
                for x in region.x..region.right() {
                    for (a, b) in px(&part, x, y).iter().zip(px(&whole, x, y).iter()) {
                        worst = worst.max(a.abs_diff(*b));
                    }
                }
            }
            assert!(
                worst <= 1,
                "resampling {side} to {dst_side} in a region differs from the whole \
                 screen by {worst}"
            );
            // And nothing outside the region it was given was touched: still whatever a
            // fresh framebuffer holds.
            let untouched = Framebuffer::new(dst_side, dst_side);
            assert_eq!(px(&part, 0, 0), px(&untouched, 0, 0));
        }
    }

    #[test]
    fn degenerate_sizes_are_a_no_op() {
        let src = Framebuffer::new(4, 4);
        let mut dst = Framebuffer::new(0, 0);
        assert!(
            Scaler::new()
                .resize(&src, src.rect(), &mut dst, Filter::Nearest)
                .is_ok()
        );
    }
}
