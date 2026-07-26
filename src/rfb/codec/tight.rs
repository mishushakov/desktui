use crate::remote::Rect;
use crate::rfb::{PixelFormat, VncError, VncEvent};
use std::future::Future;
use std::io::Read;
use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::error;

use super::{checked_len, palette_entry, zeroed_vec, zlib::ZlibReader};

const MAX_PALETTE: usize = 256;

#[derive(Default)]
pub struct Decoder {
    zlibs: [Option<flate2::Decompress>; 4],
    ctrl: u8,
    filter: u8,
    palette: Vec<u8>,
    alpha_shift: u32,
}

impl Decoder {
    pub fn new() -> Self {
        let mut new = Self {
            palette: Vec::with_capacity(MAX_PALETTE * 4),
            ..Default::default()
        };
        for i in 0..4 {
            let decompressor = flate2::Decompress::new(true);
            new.zlibs[i] = Some(decompressor);
        }
        new
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
        let pixel_mask = ((format.red_max as u32) << format.red_shift)
            | ((format.green_max as u32) << format.green_shift)
            | ((format.blue_max as u32) << format.blue_shift);

        // Which byte of the pixel the colour channels leave free. Upstream had
        // `unreachable!()` for anything else, which let a server that ignored our
        // SetPixelFormat panic the process.
        self.alpha_shift = match pixel_mask {
            0xff_ff_ff_00 => 0,
            0xff_ff_00_ff => 8,
            0xff_00_ff_ff => 16,
            0x00_ff_ff_ff => 24,
            _ => {
                error!("Tight rect in an unsupported pixel format (mask {pixel_mask:#010x})");
                return Err(VncError::WrongPixelFormat);
            }
        };

        let ctrl = input.read_u8().await?;
        for i in 0..4 {
            if (ctrl >> i) & 1 == 1 {
                self.zlibs[i].as_mut().unwrap().reset(true);
            }
        }

        // Figure out filter
        self.ctrl = ctrl >> 4;

        match self.ctrl {
            8 => {
                // fill Rect
                self.fill_rect(format, rect, input, output_func).await
            }
            9 => {
                // jpeg Rect
                self.jpeg_rect(format, rect, input, output_func).await
            }
            10 => {
                // png Rect
                error!("PNG received in standard Tight rect");
                Err(VncError::InvalidImageData)
            }
            x if x & 0x8 == 0 => {
                // basic Rect
                self.basic_rect(format, rect, input, output_func).await
            }
            _ => {
                error!("Illegal tight compression received ({})", self.ctrl);
                Err(VncError::InvalidImageData)
            }
        }
    }

    async fn read_data<S>(&mut self, input: &mut S) -> Result<Vec<u8>, VncError>
    where
        S: AsyncRead + Unpin,
    {
        let len = {
            let mut len;
            let mut byte = input.read_u8().await? as usize;
            len = byte & 0x7f;
            if byte & 0x80 == 0x80 {
                byte = input.read_u8().await? as usize;
                len |= (byte & 0x7f) << 7;

                if byte & 0x80 == 0x80 {
                    byte = input.read_u8().await? as usize;
                    len |= byte << 14;
                }
            }
            len
        };
        let mut data = zeroed_vec(checked_len(len, "Tight payload")?);
        input.read_exact(&mut data).await?;
        Ok(data)
    }

    async fn fill_rect<S, F, Fut>(
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
        let mut color = [0; 3];
        input.read_exact(&mut color).await?;
        let bpp = format.bits_per_pixel as usize / 8;
        let mut image = Vec::with_capacity(rect.width as usize * rect.height as usize * bpp);

        let true_color = self.to_true_color(format, &color);

        for _ in 0..rect.width {
            for _ in 0..rect.height {
                image.extend_from_slice(&true_color);
            }
        }
        output_func(VncEvent::RawImage(*rect, image)).await?;
        Ok(())
    }

    async fn jpeg_rect<S, F, Fut>(
        &mut self,
        _format: &PixelFormat,
        rect: &Rect,
        input: &mut S,
        output_func: &F,
    ) -> Result<(), VncError>
    where
        S: AsyncRead + Unpin,
        F: Fn(VncEvent) -> Fut,
        Fut: Future<Output = Result<(), VncError>>,
    {
        let data = self.read_data(input).await?;
        output_func(VncEvent::JpegImage(*rect, data)).await?;
        Ok(())
    }

    async fn basic_rect<S, F, Fut>(
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
        self.filter = {
            if self.ctrl & 0x4 == 4 {
                input.read_u8().await?
            } else {
                0
            }
        };

        let stream_id = self.ctrl & 0x3;
        match self.filter {
            0 => {
                // copy filter
                self.copy_filter(stream_id, format, rect, input, output_func)
                    .await
            }
            1 => {
                // palette
                self.palette_filter(stream_id, format, rect, input, output_func)
                    .await
            }
            2 => {
                // gradient
                self.gradient_filter(stream_id, format, rect, input, output_func)
                    .await
            }
            _ => {
                error!("Illegal tight filter received (filter: {})", self.filter);
                Err(VncError::InvalidImageData)
            }
        }
    }

    async fn copy_filter<S, F, Fut>(
        &mut self,
        stream: u8,
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
        let uncompressed_size = rect.width as usize * rect.height as usize * 3;
        if uncompressed_size == 0 {
            return Ok(());
        };

        let data = self
            .read_tight_data(stream, input, uncompressed_size)
            .await?;
        let mut image = Vec::with_capacity(uncompressed_size / 3 * 4);
        let mut j = 0;
        while j < uncompressed_size {
            image.extend_from_slice(&self.to_true_color(format, &data[j..j + 3]));
            j += 3;
        }

        output_func(VncEvent::RawImage(*rect, image)).await?;

        Ok(())
    }

    async fn palette_filter<S, F, Fut>(
        &mut self,
        stream: u8,
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
        let num_colors = input.read_u8().await? as usize + 1;
        let palette_size = num_colors * 3;

        self.palette = zeroed_vec(palette_size);
        input.read_exact(&mut self.palette).await?;

        let bpp = if num_colors <= 2 { 1 } else { 8 };
        let row_size = (rect.width as usize * bpp).div_ceil(8);
        let uncompressed_size = rect.height as usize * row_size;

        if uncompressed_size == 0 {
            return Ok(());
        }

        let data = self
            .read_tight_data(stream, input, uncompressed_size)
            .await?;

        if num_colors == 2 {
            self.mono_rect(data, rect, format, output_func).await?
        } else {
            self.palette_rect(data, rect, format, output_func).await?
        }

        Ok(())
    }

    async fn mono_rect<F, Fut>(
        &mut self,
        data: Vec<u8>,
        rect: &Rect,
        format: &PixelFormat,
        output_func: &F,
    ) -> Result<(), VncError>
    where
        F: Fn(VncEvent) -> Fut,
        Fut: Future<Output = Result<(), VncError>>,
    {
        // Convert indexed (palette based) image data to RGB
        let total = rect.width as usize * rect.height as usize;
        let mut image = zeroed_vec(total * 4);
        let mut offset = 8_usize;
        let mut index = -1_isize;
        let mut dp = 0;
        for i in 0..total {
            if offset == 0 || i % rect.width as usize == 0 {
                offset = 8;
                index += 1;
            }
            offset -= 1;
            let byte = *data.get(index as usize).ok_or(VncError::InvalidImageData)?;
            let entry = palette_entry(&self.palette, ((byte >> offset) & 0x01) as usize, 3)?;
            let true_color = self.to_true_color(format, entry);
            image[dp..dp + 4].copy_from_slice(&true_color);
            dp += 4;
        }
        output_func(VncEvent::RawImage(*rect, image)).await?;
        Ok(())
    }

    async fn palette_rect<F, Fut>(
        &mut self,
        data: Vec<u8>,
        rect: &Rect,
        format: &PixelFormat,
        output_func: &F,
    ) -> Result<(), VncError>
    where
        F: Fn(VncEvent) -> Fut,
        Fut: Future<Output = Result<(), VncError>>,
    {
        // Convert indexed (palette based) image data to RGB
        let total = rect.width as usize * rect.height as usize;
        let mut image = zeroed_vec(total * 4);
        let mut dp = 0;
        for i in 0..total {
            // The index is a whole byte, so it reaches 255 whatever the palette
            // actually holds. Upstream sliced the palette with it unchecked.
            let index = *data.get(i).ok_or(VncError::InvalidImageData)? as usize;
            let entry = palette_entry(&self.palette, index, 3)?;
            let true_color = self.to_true_color(format, entry);
            image[dp..dp + 4].copy_from_slice(&true_color);
            dp += 4;
        }
        output_func(VncEvent::RawImage(*rect, image)).await?;
        Ok(())
    }

    async fn gradient_filter<S, F, Fut>(
        &mut self,
        stream: u8,
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
        let uncompressed_size = rect.width as usize * rect.height as usize * 3;
        if uncompressed_size == 0 {
            return Ok(());
        };
        let data = self
            .read_tight_data(stream, input, uncompressed_size)
            .await?;
        let mut image = zeroed_vec(rect.width as usize * rect.height as usize * 4);

        let row_len = rect.width as usize * 3 + 3;
        let mut row_0 = vec![0_u16; row_len];
        let mut row_1 = vec![0_u16; row_len];
        let max = [format.red_max, format.green_max, format.blue_max];
        let shift = [format.red_shift, format.green_shift, format.blue_shift];
        let mut sp = 0;
        let mut dp = 0;

        for y in 0..rect.height as usize {
            let (this_row, prev_row) = match y & 1 {
                0 => (&mut row_0, &mut row_1),
                1 => (&mut row_1, &mut row_0),
                _ => unreachable!(),
            };
            let mut x = 3;
            while x < row_len {
                let rgb = &data[sp..sp + 3];
                let mut color = 0;
                for index in 0..3 {
                    let d = prev_row[index + x] as i32 + this_row[index + x - 3] as i32
                        - prev_row[index + x - 3] as i32;
                    let converted = if d < 0 {
                        0
                    } else if d > max[index] as i32 {
                        max[index]
                    } else {
                        d as u16
                    };
                    this_row[index + x] = (converted + rgb[index] as u16) & max[index];
                    color |= (this_row[x + index] as u32 & max[index] as u32) << shift[index];
                }
                image[dp..dp + 4].copy_from_slice(&color.to_le_bytes());
                dp += 4;
                sp += 3;
                x += 3;
            }
        }

        output_func(VncEvent::RawImage(*rect, image)).await?;
        Ok(())
    }

    async fn read_tight_data<S>(
        &mut self,
        stream: u8,
        input: &mut S,
        uncompressed_size: usize,
    ) -> Result<Vec<u8>, VncError>
    where
        S: AsyncRead + Unpin,
    {
        let mut data;
        if uncompressed_size < 12 {
            data = zeroed_vec(uncompressed_size);
            input.read_exact(&mut data).await?;
        } else {
            let idx = (stream & 0x3) as usize;
            let d = self.read_data(input).await?;
            let decompressor = self.zlibs[idx]
                .take()
                .ok_or_else(|| VncError::General(format!("Tight zlib stream {idx} is gone")))?;
            let mut reader = ZlibReader::new(decompressor, &d);
            data = zeroed_vec(uncompressed_size);
            let read = reader.read_exact(&mut data).map_err(VncError::from);
            // Put the stream back whatever happened. Upstream left the slot empty
            // on any error, so the next rectangle on the same stream unwrapped a
            // None and panicked.
            match reader.into_inner() {
                Ok(decompressor) => {
                    self.zlibs[idx] = Some(decompressor);
                    read?;
                }
                Err(err) => {
                    self.zlibs[idx] = Some(flate2::Decompress::new(true));
                    read?;
                    return Err(err.into());
                }
            }
        };
        Ok(data)
    }

    fn to_true_color(&self, format: &PixelFormat, color: &[u8]) -> [u8; 4] {
        let alpha = 255;
        // always rgb
        (((color[0] as u32 & format.red_max as u32) << format.red_shift)
            | ((color[1] as u32 & format.green_max as u32) << format.green_shift)
            | ((color[2] as u32 & format.blue_max as u32) << format.blue_shift)
            | ((alpha as u32) << self.alpha_shift))
            .to_le_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rfb::VncEvent;
    use crate::rfb::codec::testing::{Sink, bgra, bgra_repeated, deflate, format, rect, tight_len};

    /// The control byte: the low nibble resets zlib streams, the high nibble picks the
    /// compression. Written out here because every test below starts with one and the
    /// two halves are easy to transpose.
    fn ctrl(compression: u8, resets: u8) -> u8 {
        (compression << 4) | resets
    }

    const FILL: u8 = 8;
    const JPEG: u8 = 9;
    const PNG: u8 = 10;
    /// Basic compression, stream 0, with an explicit filter byte following (bit 2).
    const BASIC_FILTERED: u8 = 4;
    /// Basic compression, stream 0, filter implied to be `copy`.
    const BASIC_COPY: u8 = 0;

    /// Successive chunks compressed through one stream, as a server does across the
    /// rectangles of a session. Each is sync-flushed, never finished.
    fn deflate_stream(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut compress = flate2::Compress::new(flate2::Compression::default(), true);
        chunks
            .iter()
            .map(|chunk| {
                let mut out = Vec::with_capacity(chunk.len() + 128);
                compress
                    .compress_vec(chunk, &mut out, flate2::FlushCompress::Sync)
                    .expect("compressing test data");
                out
            })
            .collect()
    }

    async fn decode(bytes: &[u8], width: u16, height: u16) -> Result<Sink, VncError> {
        let sink = Sink::default();
        let mut input = bytes;
        Decoder::new()
            .decode(
                &format(),
                &rect(width, height),
                &mut input,
                &sink.collector(),
            )
            .await?;
        Ok(sink)
    }

    // ------------------------------------------------------------------ fill

    #[tokio::test]
    async fn a_fill_rectangle_is_one_colour_everywhere() {
        // Three bytes of colour stand for the whole rectangle: the cheapest thing
        // Tight can send, and what a flat desktop background arrives as.
        let sink = decode(&[ctrl(FILL, 0), 10, 20, 30], 2, 3).await.unwrap();

        let (got, pixels) = sink.image();
        assert_eq!(got, rect(2, 3));
        assert_eq!(pixels, bgra_repeated(10, 20, 30, 6));
    }

    // ------------------------------------------------------------------ jpeg

    #[tokio::test]
    async fn a_jpeg_rectangle_is_handed_on_whole() {
        // The Tight decoder does not decode JPEG itself; it hands the payload to the
        // renderer, so what matters is that the length prefix is read exactly right and
        // no byte is added or dropped.
        let payload: Vec<u8> = (0..40u8).collect();
        let mut bytes = vec![ctrl(JPEG, 0)];
        bytes.extend(tight_len(payload.len()));
        bytes.extend_from_slice(&payload);

        let sink = decode(&bytes, 4, 4).await.unwrap();

        match sink.events().as_slice() {
            [VncEvent::JpegImage(got, data)] => {
                assert_eq!(*got, rect(4, 4));
                assert_eq!(*data, payload);
            }
            other => panic!("expected one JpegImage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_compact_length_is_read_in_all_three_widths() {
        // Tight writes a length as 7 bits per byte, low group first, the top bit
        // meaning "another follows". One byte up to 127, two to 16383, three beyond --
        // and getting the third wrong desynchronises every later rectangle rather than
        // failing here.
        for len in [1usize, 127, 128, 200, 16383, 16384, 20000] {
            let payload = vec![0xab; len];
            let mut bytes = vec![ctrl(JPEG, 0)];
            bytes.extend(tight_len(len));
            bytes.extend_from_slice(&payload);
            // A trailing byte that must still be on the stream afterwards.
            bytes.push(0xff);

            let sink = Sink::default();
            let mut input = &bytes[..];
            Decoder::new()
                .decode(&format(), &rect(4, 4), &mut input, &sink.collector())
                .await
                .unwrap_or_else(|e| panic!("length {len} failed: {e}"));

            match sink.events().as_slice() {
                [VncEvent::JpegImage(_, data)] => {
                    assert_eq!(data.len(), len, "wrong payload length for {len}")
                }
                other => panic!("length {len}: {other:?}"),
            }
            assert_eq!(input, &[0xff], "length {len} left the stream misaligned");
        }
    }

    // ------------------------------------------------------------- copy filter

    #[tokio::test]
    async fn a_short_copy_rectangle_is_sent_uncompressed() {
        // Under twelve bytes Tight skips zlib, because the header would cost more than
        // the pixels. Three bytes per pixel on the wire become four in the framebuffer.
        let sink = decode(&[ctrl(BASIC_COPY, 0), 1, 2, 3, 4, 5, 6, 7, 8, 9], 3, 1)
            .await
            .unwrap();

        let (_, pixels) = sink.image();
        let mut expected = Vec::new();
        expected.extend(bgra(1, 2, 3));
        expected.extend(bgra(4, 5, 6));
        expected.extend(bgra(7, 8, 9));
        assert_eq!(pixels, expected);
    }

    #[tokio::test]
    async fn a_longer_copy_rectangle_comes_through_zlib() {
        // Twelve bytes and up is the compressed path, with its own length prefix.
        let rgb: Vec<u8> = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let compressed = deflate(&rgb);
        let mut bytes = vec![ctrl(BASIC_COPY, 0)];
        bytes.extend(tight_len(compressed.len()));
        bytes.extend_from_slice(&compressed);

        let sink = decode(&bytes, 2, 2).await.unwrap();

        let (_, pixels) = sink.image();
        let mut expected = Vec::new();
        for chunk in rgb.chunks(3) {
            expected.extend(bgra(chunk[0], chunk[1], chunk[2]));
        }
        assert_eq!(pixels, expected);
    }

    // ---------------------------------------------------------- palette filter

    #[tokio::test]
    async fn a_two_colour_rectangle_is_one_bit_per_pixel() {
        // The mono case packs eight pixels into a byte, most significant bit first, and
        // starts a fresh byte on every row. Getting the bit order backwards mirrors
        // every glyph on the screen, which is what this pins down.
        let red = [255, 0, 0];
        let blue = [0, 0, 255];
        let mut bytes = vec![ctrl(BASIC_FILTERED, 0), 1, 1];
        bytes.extend_from_slice(&red);
        bytes.extend_from_slice(&blue);
        // Row 0 alternating from red, row 1 alternating from blue.
        bytes.extend_from_slice(&[0b0101_0101, 0b1010_1010]);

        let sink = decode(&bytes, 8, 2).await.unwrap();

        let (_, pixels) = sink.image();
        let mut expected = Vec::new();
        for _ in 0..4 {
            expected.extend(bgra(255, 0, 0));
            expected.extend(bgra(0, 0, 255));
        }
        for _ in 0..4 {
            expected.extend(bgra(0, 0, 255));
            expected.extend(bgra(255, 0, 0));
        }
        assert_eq!(pixels, expected);
    }

    #[tokio::test]
    async fn a_small_palette_is_one_byte_per_pixel() {
        // Three colours and up: a whole byte of index per pixel, however few colours
        // there are.
        let mut bytes = vec![ctrl(BASIC_FILTERED, 0), 1, 2];
        bytes.extend_from_slice(&[10, 11, 12]); // index 0
        bytes.extend_from_slice(&[20, 21, 22]); // index 1
        bytes.extend_from_slice(&[30, 31, 32]); // index 2
        bytes.extend_from_slice(&[2, 0, 1, 0]);

        let sink = decode(&bytes, 4, 1).await.unwrap();

        let (_, pixels) = sink.image();
        let mut expected = Vec::new();
        expected.extend(bgra(30, 31, 32));
        expected.extend(bgra(10, 11, 12));
        expected.extend(bgra(20, 21, 22));
        expected.extend(bgra(10, 11, 12));
        assert_eq!(pixels, expected);
    }

    #[tokio::test]
    async fn a_palette_index_past_the_end_is_refused() {
        // The index is a whole byte, so it reaches 255 whatever the palette holds.
        // Upstream sliced the palette with it and panicked the process; a server only
        // has to send one byte to do that, so it is worth a test of its own.
        let mut bytes = vec![ctrl(BASIC_FILTERED, 0), 1, 2];
        bytes.extend_from_slice(&[10, 11, 12]);
        bytes.extend_from_slice(&[20, 21, 22]);
        bytes.extend_from_slice(&[30, 31, 32]);
        bytes.extend_from_slice(&[0, 200, 0, 0]);

        let result = decode(&bytes, 4, 1).await;

        assert!(
            matches!(result, Err(VncError::InvalidImageData)),
            "expected a refusal, got {:?}",
            result.map(|s| s.images())
        );
    }

    // --------------------------------------------------------- gradient filter

    #[tokio::test]
    async fn the_gradient_filter_predicts_from_the_neighbours() {
        // Each channel arrives as a difference from a prediction made out of the pixel
        // to the left, the one above, and the one above-left. On the first row the
        // prediction is just the pixel to the left, so these three pixels accumulate.
        let mut bytes = vec![ctrl(BASIC_FILTERED, 0), 2];
        bytes.extend_from_slice(&[10, 20, 30, 1, 2, 3, 5, 5, 5]);

        let sink = decode(&bytes, 3, 1).await.unwrap();

        let (_, pixels) = sink.image();
        // 10,20,30 then +1,+2,+3 then +5,+5,+5.
        let channels: Vec<[u8; 3]> = pixels
            .chunks_exact(4)
            .map(|px| [px[2], px[1], px[0]])
            .collect();
        assert_eq!(channels, vec![[10, 20, 30], [11, 22, 33], [16, 27, 38]]);

        // Unlike every other filter, the gradient path never sets the alpha byte. That
        // is harmless only because `Framebuffer::pack_rgb` reads three bytes of four
        // and ignores this one -- asserted here so that if the renderer ever starts
        // reading it, this fails rather than the picture going transparent.
        assert!(
            pixels.chunks_exact(4).all(|px| px[3] == 0),
            "the gradient filter now sets alpha; pack_rgb has to be checked"
        );
    }

    // ---------------------------------------------------------- malformed input

    #[tokio::test]
    async fn png_in_a_standard_tight_rectangle_is_refused() {
        // PNG belongs to TightPNG, which this client never negotiates. Upstream fell
        // through to the basic path and desynchronised the stream.
        let result = decode(&[ctrl(PNG, 0)], 2, 2).await;
        assert!(
            matches!(result, Err(VncError::InvalidImageData)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn an_unknown_compression_is_refused() {
        // Compression 11 has the high bit of the nibble set but is not fill, jpeg or
        // png, so there is nothing to do but refuse it.
        let result = decode(&[ctrl(11, 0)], 2, 2).await;
        assert!(
            matches!(result, Err(VncError::InvalidImageData)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn an_unknown_filter_is_refused() {
        // Filters are copy, palette and gradient. A fourth would leave us guessing at
        // the length of what follows.
        let result = decode(&[ctrl(BASIC_FILTERED, 0), 3], 2, 2).await;
        assert!(
            matches!(result, Err(VncError::InvalidImageData)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn an_unsupported_pixel_format_is_refused_rather_than_panicking() {
        // The colour channels have to leave one byte of the pixel free for alpha.
        // Upstream had `unreachable!()` here, reachable from a server that ignored our
        // SetPixelFormat.
        let mut odd = format();
        odd.red_max = 31;
        odd.green_max = 63;
        odd.blue_max = 31;
        odd.red_shift = 11;
        odd.green_shift = 5;
        odd.blue_shift = 0;

        let sink = Sink::default();
        let mut input = &[ctrl(FILL, 0), 1, 2, 3][..];
        let result = Decoder::new()
            .decode(&odd, &rect(2, 2), &mut input, &sink.collector())
            .await;

        assert!(
            matches!(result, Err(VncError::WrongPixelFormat)),
            "{result:?}"
        );
    }

    // -------------------------------------------------------------- zlib streams

    #[tokio::test]
    async fn a_zlib_stream_persists_across_rectangles() {
        // The four streams stay open for the whole session, so a rectangle can only be
        // decompressed in the context the previous one left behind. Decoding each
        // rectangle with a fresh decompressor would work on the first and fail on the
        // second, which is why both are checked here on one decoder.
        let first: Vec<u8> = (0..12u8).collect();
        let second: Vec<u8> = (100..112u8).collect();
        let chunks = deflate_stream(&[&first, &second]);

        let mut decoder = Decoder::new();
        for (rgb, compressed) in [(&first, &chunks[0]), (&second, &chunks[1])] {
            let mut bytes = vec![ctrl(BASIC_COPY, 0)];
            bytes.extend(tight_len(compressed.len()));
            bytes.extend_from_slice(compressed);

            let sink = Sink::default();
            let mut input = &bytes[..];
            decoder
                .decode(&format(), &rect(2, 2), &mut input, &sink.collector())
                .await
                .expect("a rectangle continuing the same zlib stream");

            let expected: Vec<u8> = rgb.chunks(3).flat_map(|c| bgra(c[0], c[1], c[2])).collect();
            assert_eq!(sink.image().1, expected);
        }
    }

    #[tokio::test]
    async fn the_reset_bit_starts_the_stream_over() {
        // A server that resets a stream sets the matching bit in the control byte, and
        // what follows is a new zlib stream with its own header. Without honouring the
        // reset the decompressor would still be mid-stream and reject it.
        let first: Vec<u8> = (0..12u8).collect();
        let second: Vec<u8> = (50..62u8).collect();

        let mut decoder = Decoder::new();

        let compressed = deflate(&first);
        let mut bytes = vec![ctrl(BASIC_COPY, 0)];
        bytes.extend(tight_len(compressed.len()));
        bytes.extend_from_slice(&compressed);
        let sink = Sink::default();
        decoder
            .decode(&format(), &rect(2, 2), &mut &bytes[..], &sink.collector())
            .await
            .unwrap();

        // An independent stream, announced by resetting stream 0.
        let compressed = deflate(&second);
        let mut bytes = vec![ctrl(BASIC_COPY, 1)];
        bytes.extend(tight_len(compressed.len()));
        bytes.extend_from_slice(&compressed);
        let sink = Sink::default();
        decoder
            .decode(&format(), &rect(2, 2), &mut &bytes[..], &sink.collector())
            .await
            .expect("a reset stream should start clean");

        let expected: Vec<u8> = second
            .chunks(3)
            .flat_map(|c| bgra(c[0], c[1], c[2]))
            .collect();
        assert_eq!(sink.image().1, expected);
    }

    #[tokio::test]
    async fn a_failed_rectangle_leaves_the_stream_usable() {
        // Upstream took the decompressor out of its slot to use it and only put it back
        // on success, so one corrupt rectangle made the next one unwrap a `None` and
        // panic. The corrupt rectangle has to fail; the one after it has to not.
        let mut decoder = Decoder::new();

        let mut bytes = vec![ctrl(BASIC_COPY, 0)];
        bytes.extend(tight_len(4));
        bytes.extend_from_slice(&[0, 0, 0, 0]); // not a zlib stream
        let sink = Sink::default();
        let result = decoder
            .decode(&format(), &rect(2, 2), &mut &bytes[..], &sink.collector())
            .await;
        assert!(result.is_err(), "corrupt zlib data should fail");

        // Now a good rectangle on the same stream.
        let rgb: Vec<u8> = (0..12u8).collect();
        let compressed = deflate(&rgb);
        let mut bytes = vec![ctrl(BASIC_COPY, 1)];
        bytes.extend(tight_len(compressed.len()));
        bytes.extend_from_slice(&compressed);
        let sink = Sink::default();
        decoder
            .decode(&format(), &rect(2, 2), &mut &bytes[..], &sink.collector())
            .await
            .expect("the decoder was poisoned by the failed rectangle");

        let expected: Vec<u8> = rgb.chunks(3).flat_map(|c| bgra(c[0], c[1], c[2])).collect();
        assert_eq!(sink.image().1, expected);
    }

    #[tokio::test]
    async fn a_truncated_payload_is_an_error() {
        // The length prefix promises more than the stream holds, which is what a
        // rectangle cut short by a dropped connection looks like.
        let mut bytes = vec![ctrl(JPEG, 0)];
        bytes.extend(tight_len(100));
        bytes.extend_from_slice(&[0; 10]);

        let result = decode(&bytes, 4, 4).await;
        assert!(matches!(result, Err(VncError::IoError(_))), "{result:?}");
    }
}
