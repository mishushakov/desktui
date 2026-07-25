//! Resampling, for when the remote resolution does not match the terminal.
//!
//! Alpha handling is switched off deliberately: our fourth byte is RFB padding,
//! not transparency. Left on, the resizer would premultiply by whatever the
//! server happened to put there -- and servers routinely send zero -- turning
//! the whole frame black.

use anyhow::{Context, Result};
use fast_image_resize::images::{TypedImage, TypedImageRef};
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

    /// Resample `crop` out of `src` into the whole of `dst`.
    pub fn resize(
        &mut self,
        src: &Framebuffer,
        crop: Rect,
        dst: &mut Framebuffer,
        filter: Filter,
    ) -> Result<()> {
        if dst.width() == 0 || dst.height() == 0 || crop.w == 0 || crop.h == 0 {
            return Ok(());
        }

        let src_view = TypedImageRef::<U8x4>::from_buffer(src.width(), src.height(), src.data())
            .context("source framebuffer is not a valid image")?;
        let (dw, dh) = (dst.width(), dst.height());
        let mut dst_view = TypedImage::<U8x4>::from_buffer(dw, dh, dst.data_mut())
            .context("destination framebuffer is not a valid image")?;

        let options = ResizeOptions::new()
            .resize_alg(filter.alg())
            .use_alpha(false)
            .crop(
                f64::from(crop.x),
                f64::from(crop.y),
                f64::from(crop.w),
                f64::from(crop.h),
            );

        self.resizer
            .resize_typed(&src_view, &mut dst_view, Some(&options))
            .context("resample failed")
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
