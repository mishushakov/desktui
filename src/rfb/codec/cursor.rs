use crate::remote::Rect;
use crate::rfb::{PixelFormat, VncError, VncEvent};
use std::future::Future;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::zeroed_vec;

pub struct Decoder {}

impl Decoder {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn decode<S, F, Fut>(
        &mut self,
        format: &PixelFormat,
        rect: &Rect,
        input: &mut S,
        output_func: &F,
    ) -> Result<(), VncError>
    where
        S: AsyncRead + Unpin,
        F: Fn(VncEvent) -> Fut,
        Fut: Future<Output = Result<(), VncError>>,
    {
        let _hotx = rect.x;
        let _hoty = rect.y;
        let w = rect.width;
        let h = rect.height;

        // The loop below writes four bytes per pixel, so anything else would run
        // off the end of the buffer. Upstream indexed `pix_idx + 3` regardless of
        // the negotiated depth.
        if format.bits_per_pixel != 32 {
            return Err(VncError::WrongPixelFormat);
        }

        let pixels_length = w as usize * h as usize * format.bits_per_pixel as usize / 8;
        let mask_length = (w as usize).div_ceil(8) * h as usize;

        let mut pixels = zeroed_vec(pixels_length);
        input.read_exact(&mut pixels).await?;
        let mut mask = zeroed_vec(mask_length);
        input.read_exact(&mut mask).await?;
        let mut image = zeroed_vec(pixels_length);
        let mut pix_idx = 0;

        let pixel_mask = ((format.red_max as u32) << format.red_shift)
            | ((format.green_max as u32) << format.green_shift)
            | ((format.blue_max as u32) << format.blue_shift);

        let mut alpha_idx = match pixel_mask {
            0xff_ff_ff_00 => 3,
            0xff_ff_00_ff => 2,
            0xff_00_ff_ff => 1,
            0x00_ff_ff_ff => 0,
            // A pixel format we cannot place the alpha byte in. Upstream panicked.
            _ => return Err(VncError::WrongPixelFormat),
        };
        if format.big_endian_flag == 0 {
            alpha_idx = 3 - alpha_idx;
        }
        for y in 0..h as usize {
            for x in 0..w as usize {
                let mask_idx = y * (w as usize).div_ceil(8) + (x / 8);
                let alpha = if (mask[mask_idx] << (x % 8)) & 0x80 > 0 {
                    255
                } else {
                    0
                };
                image[pix_idx] = pixels[pix_idx];
                image[pix_idx + 1] = pixels[pix_idx + 1];
                image[pix_idx + 2] = pixels[pix_idx + 2];
                image[pix_idx + 3] = pixels[pix_idx + 3];

                // use alpha from the bitmask to cover it.
                image[pix_idx + alpha_idx] = alpha;
                pix_idx += 4;
            }
        }

        output_func(VncEvent::SetCursor(*rect, image)).await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rfb::VncEvent;
    use crate::rfb::codec::testing::{Sink, format, rect};

    /// The cursor arrives as pixels followed by a one-bit-per-pixel mask, so it is the
    /// only decoder whose output depends on two separate runs of bytes lining up.
    async fn decode(
        pixels: &[u8],
        mask: &[u8],
        width: u16,
        height: u16,
    ) -> Result<Vec<u8>, VncError> {
        let mut bytes = pixels.to_vec();
        bytes.extend_from_slice(mask);
        let sink = Sink::default();
        Decoder::new()
            .decode(
                &format(),
                &rect(width, height),
                &mut &bytes[..],
                &sink.collector(),
            )
            .await?;
        match sink.events().as_slice() {
            [VncEvent::SetCursor(_, image)] => Ok(image.clone()),
            other => panic!("expected one SetCursor, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_mask_becomes_the_alpha_channel() {
        // A set bit means opaque. The bits run from the most significant down, so the
        // leftmost pixel of a row is the top bit -- reversing that would mirror the
        // pointer's transparency against its picture.
        let pixels = [1, 2, 3, 99, 4, 5, 6, 99];
        let image = decode(&pixels, &[0b1000_0000], 2, 1).await.unwrap();

        assert_eq!(
            image,
            vec![1, 2, 3, 255, 4, 5, 6, 0],
            "the first pixel should be opaque and the second fully transparent"
        );
    }

    #[tokio::test]
    async fn the_colour_bytes_are_left_as_they_arrived() {
        // Only the alpha byte is written; the other three are the server's.
        let pixels = [10, 20, 30, 0];
        let image = decode(&pixels, &[0b1000_0000], 1, 1).await.unwrap();

        assert_eq!(&image[..3], &[10, 20, 30]);
    }

    #[tokio::test]
    async fn each_mask_row_starts_on_a_fresh_byte() {
        // Nine pixels a row needs two mask bytes, and the second holds one bit and
        // seven of padding. Packing the rows continuously instead would shift the
        // transparency of every row after the first.
        let pixels = vec![7u8; 9 * 2 * 4];
        // Row 0: first pixel opaque. Row 1: ninth pixel opaque.
        let mask = [0b1000_0000, 0b0000_0000, 0b0000_0000, 0b1000_0000];

        let image = decode(&pixels, &mask, 9, 2).await.unwrap();

        let alpha: Vec<u8> = image.chunks_exact(4).map(|px| px[3]).collect();
        let mut expected = vec![0u8; 9 * 2];
        expected[0] = 255; // row 0, first pixel
        expected[9 + 8] = 255; // row 1, ninth pixel -- the one in the padded byte
        assert_eq!(alpha, expected);
    }

    #[tokio::test]
    async fn a_cursor_at_any_other_depth_is_refused() {
        // The loop writes four bytes a pixel, so a 16-bit cursor would run off the end
        // of the buffer. Upstream indexed `pix_idx + 3` whatever the depth had been
        // negotiated as, which a server could turn into a panic by sending one.
        let mut shallow = format();
        shallow.bits_per_pixel = 16;

        let sink = Sink::default();
        let result = Decoder::new()
            .decode(
                &shallow,
                &rect(2, 2),
                &mut &[0u8; 32][..],
                &sink.collector(),
            )
            .await;

        assert!(
            matches!(result, Err(VncError::WrongPixelFormat)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_pixel_format_with_no_room_for_alpha_is_refused() {
        // Upstream panicked here too. There has to be a spare byte in the pixel for the
        // mask to become.
        let mut packed = format();
        packed.red_max = 31;
        packed.green_max = 63;
        packed.blue_max = 31;
        packed.red_shift = 11;
        packed.green_shift = 5;
        packed.blue_shift = 0;

        let sink = Sink::default();
        let result = Decoder::new()
            .decode(&packed, &rect(1, 1), &mut &[0u8; 8][..], &sink.collector())
            .await;

        assert!(
            matches!(result, Err(VncError::WrongPixelFormat)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_cursor_missing_its_mask_is_an_error() {
        // The pixels arrive and the mask does not, which is what a cursor cut short by
        // a dropped connection looks like.
        let sink = Sink::default();
        let result = Decoder::new()
            .decode(
                &format(),
                &rect(2, 2),
                &mut &[0u8; 16][..],
                &sink.collector(),
            )
            .await;

        assert!(matches!(result, Err(VncError::IoError(_))), "{result:?}");
    }
}
