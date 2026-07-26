use crate::rfb::{PixelFormat, Rect, VncError, VncEvent};
use std::future::Future;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::{checked_len, zeroed_vec};

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
        // +----------------------------+--------------+-------------+
        // | No. of bytes               | Type [Value] | Description |
        // +----------------------------+--------------+-------------+
        // | width*height*bytesPerPixel | PIXEL array  | pixels      |
        // +----------------------------+--------------+-------------+
        let bpp = format.bits_per_pixel / 8;
        let buffer_size = checked_len(
            bpp as usize * rect.height as usize * rect.width as usize,
            "raw rectangle",
        )?;
        let mut pixels = zeroed_vec(buffer_size);
        input.read_exact(&mut pixels).await?;
        output_func(VncEvent::RawImage(*rect, pixels)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rfb::codec::testing::{Sink, format, rect};

    #[tokio::test]
    async fn pixels_are_passed_through_untouched() {
        // Raw is the mandatory encoding and the only one with no transformation: the
        // bytes on the wire are already in the negotiated format.
        let pixels: Vec<u8> = (0..2 * 2 * 4).collect();
        let sink = Sink::default();
        let mut input = &pixels[..];

        Decoder::new()
            .decode(&format(), &rect(2, 2), &mut input, &sink.collector())
            .await
            .expect("a complete raw rectangle should decode");

        let (got, data) = sink.image();
        assert_eq!(got, rect(2, 2));
        assert_eq!(data, pixels, "raw pixels must not be reinterpreted");
    }

    #[tokio::test]
    async fn the_rectangle_is_sized_from_the_pixel_format_not_the_data() {
        // Four bytes per pixel at 32bpp: a 3x1 rectangle is twelve bytes, and the
        // trailing byte here must be left on the stream for the next rectangle.
        let pixels = [9u8; 13];
        let sink = Sink::default();
        let mut input = &pixels[..];

        Decoder::new()
            .decode(&format(), &rect(3, 1), &mut input, &sink.collector())
            .await
            .unwrap();

        assert_eq!(sink.image().1.len(), 12);
        assert_eq!(input.len(), 1, "read past the end of the rectangle");
    }

    #[tokio::test]
    async fn a_truncated_rectangle_is_an_error_not_a_short_image() {
        // A server that promises 2x2 and sends three pixels' worth has to fail here.
        // Accepting it would hand the renderer a buffer shorter than the rect it is
        // told to draw into.
        let sink = Sink::default();
        let mut input = &[0u8; 12][..];

        let result = Decoder::new()
            .decode(&format(), &rect(2, 2), &mut input, &sink.collector())
            .await;

        assert!(
            matches!(result, Err(VncError::IoError(_))),
            "expected an unexpected-eof, got {result:?}"
        );
        assert!(sink.images().is_empty(), "emitted a partial image");
    }

    #[tokio::test]
    async fn an_implausible_rectangle_is_refused_before_allocating() {
        // 65535x65535 at four bytes a pixel is 17GB. The length is the server's word
        // alone, so it has to be checked before it becomes an allocation.
        let sink = Sink::default();
        let mut input = &[][..];

        let result = Decoder::new()
            .decode(
                &format(),
                &rect(u16::MAX, u16::MAX),
                &mut input,
                &sink.collector(),
            )
            .await;

        assert!(matches!(result, Err(VncError::General(_))), "{result:?}");
    }
}
