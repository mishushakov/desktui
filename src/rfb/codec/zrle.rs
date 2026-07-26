use crate::remote::Rect;
use crate::rfb::{PixelFormat, VncError, VncEvent};
use std::future::Future;
use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::error;

use super::{checked_len, palette_entry, zeroed_vec, zlib::ZlibReader};

fn read_run_length(reader: &mut ZlibReader) -> Result<usize, VncError> {
    let mut run_length_part;
    let mut run_length = 1;
    loop {
        run_length_part = reader.read_u8()?;
        run_length += run_length_part as usize;
        if 255 != run_length_part {
            break;
        }
    }
    Ok(run_length)
}

fn copy_true_color(
    reader: &mut ZlibReader,
    pixels: &mut Vec<u8>,
    pad: bool,
    compressed_bpp: usize,
    bpp: usize,
) -> Result<(), VncError> {
    let mut buf = [255; 4];
    std::io::Read::read_exact(
        reader,
        &mut buf[pad as usize..pad as usize + compressed_bpp],
    )?;
    pixels.extend_from_slice(&buf[..bpp]);
    Ok(())
}

/// Append a palette entry, refusing an index the palette does not have.
///
/// An index off the wire goes up to 127 no matter how many colours were declared,
/// and upstream sliced the palette with it unchecked.
fn copy_indexed(
    palette: &[u8],
    pixels: &mut Vec<u8>,
    bpp: usize,
    index: u8,
) -> Result<(), VncError> {
    pixels.extend_from_slice(palette_entry(palette, index as usize, bpp)?);
    Ok(())
}

pub struct Decoder {
    decompressor: Option<flate2::Decompress>,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            decompressor: Some(flate2::Decompress::new(true)),
        }
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
        let data_len = checked_len(input.read_u32().await? as usize, "ZRLE payload")?;
        let mut zlib_data = zeroed_vec(data_len);
        input.read_exact(&mut zlib_data).await?;
        let decompressor = self
            .decompressor
            .take()
            .ok_or_else(|| VncError::General("ZRLE zlib stream is gone".into()))?;
        let mut reader = ZlibReader::new(decompressor, &zlib_data);

        let bpp = format.bits_per_pixel as usize / 8;
        let pixel_mask = ((format.red_max as u32) << format.red_shift)
            | ((format.green_max as u32) << format.green_shift)
            | ((format.blue_max as u32) << format.blue_shift);

        let (compressed_bpp, alpha_at_first) =
            if format.bits_per_pixel == 32 && format.true_color_flag > 0 && format.depth <= 24 {
                if pixel_mask & 0x000000ff == 0 {
                    // rgb at the most significant bits
                    // if format.big_endian_flag is set
                    // then decompressed data is excepted to be [rgb.0, rgb.1, rgb.2, alpha]
                    // otherwise the decompressed data should be [alpha, rgb.0, rgb.1, rgb.2]
                    (3, format.big_endian_flag == 0)
                } else if pixel_mask & 0xff000000 == 0 {
                    // rgb at the least significant bits
                    // if format.big_endian_flag is set
                    // then decompressed data should be [alpha, rgb.0, rgb.1, rgb.2]
                    // otherwise the decompressed data should be [rgb.0, rgb.1, rgb.2, alpha]
                    (3, format.big_endian_flag > 0)
                } else {
                    (4, false)
                }
            } else {
                (bpp, false)
            };
        // The tiles are decoded out of line so that the stream can be put back
        // whatever happens in there. Every `?` below used to return straight out of
        // this function, past the restore at the bottom, which left the decompressor
        // taken for good: after one malformed rectangle every later one in the session
        // failed with "the stream is gone". A palette index off the end or a
        // subencoding that does not exist is a single byte the server chooses, so that
        // was reachable from the network.
        let tiles = Self::decode_tiles(
            rect,
            &mut reader,
            output_func,
            bpp,
            compressed_bpp,
            alpha_at_first,
        )
        .await;

        // Restore the stream before propagating anything, so a failed rectangle
        // does not poison the decoder for the next one.
        match reader.into_inner() {
            Ok(decompressor) => self.decompressor = Some(decompressor),
            Err(err) => {
                self.decompressor = Some(flate2::Decompress::new(true));
                // Why the tile failed is more use than "leftover zlib byte data",
                // which is only a consequence of having stopped early.
                tiles?;
                return Err(err.into());
            }
        }

        tiles
    }

    async fn decode_tiles<F, Fut>(
        rect: &Rect,
        reader: &mut ZlibReader<'_>,
        output_func: &F,
        bpp: usize,
        compressed_bpp: usize,
        alpha_at_first: bool,
    ) -> Result<(), VncError>
    where
        F: Fn(VncEvent) -> Fut,
        Fut: Future<Output = Result<(), VncError>>,
    {
        let mut palette = Vec::with_capacity(128 * bpp);

        let mut y = 0;
        while y < rect.height {
            let height = if y + 64 > rect.height {
                rect.height - y
            } else {
                64
            };
            let mut x = 0;
            while x < rect.width {
                let width = if x + 64 > rect.width {
                    rect.width - x
                } else {
                    64
                };
                let pixel_count = height as usize * width as usize;

                let control = reader.read_u8()?;
                let is_rle = control & 0x80 > 0;
                let palette_size = control & 0x7f;
                palette.clear();

                for _ in 0..palette_size {
                    copy_true_color(reader, &mut palette, alpha_at_first, compressed_bpp, bpp)?
                }

                let mut pixels = Vec::with_capacity(pixel_count * bpp);
                match (is_rle, palette_size) {
                    (false, 0) => {
                        // True Color pixels
                        for _ in 0..pixel_count {
                            copy_true_color(
                                reader,
                                &mut pixels,
                                alpha_at_first,
                                compressed_bpp,
                                bpp,
                            )?
                        }
                    }
                    (false, 1) => {
                        // Color fill
                        for _ in 0..pixel_count {
                            copy_indexed(&palette, &mut pixels, bpp, 0)?
                        }
                    }
                    (false, 2..=16) => {
                        // Indexed pixels
                        let bits_per_index: i32 = match palette_size {
                            2 => 1,
                            3..=4 => 2,
                            5..=16 => 4,
                            // Unreachable given the match arm above, but this is a
                            // value off the wire and a panic here would be remotely
                            // triggerable if that arm ever changed.
                            _ => return Err(VncError::InvalidImageData),
                        };
                        let mut encoded = reader.read_u8()?;
                        let mask = (1 << bits_per_index) - 1;

                        for y in 0..height {
                            let mut shift = 8 - bits_per_index;
                            for _ in 0..width {
                                if shift < 0 {
                                    shift = 8 - bits_per_index;
                                    encoded = reader.read_u8()?;
                                }
                                let idx = (encoded >> shift) & mask;

                                copy_indexed(&palette, &mut pixels, bpp, idx)?;
                                shift -= bits_per_index;
                            }
                            if shift < 8 - bits_per_index && y < height - 1 {
                                encoded = reader.read_u8()?;
                            }
                        }
                    }
                    (true, 0) => {
                        // True Color RLE
                        let mut count = 0;
                        let mut pixel = Vec::new();
                        while count < pixel_count {
                            pixel.clear();
                            copy_true_color(
                                reader,
                                &mut pixel,
                                alpha_at_first,
                                compressed_bpp,
                                bpp,
                            )?;
                            let run_length = read_run_length(reader)?;
                            for _ in 0..run_length {
                                pixels.extend(&pixel)
                            }
                            count += run_length;
                        }
                    }
                    (true, 2..=127) => {
                        // Indexed RLE
                        let mut count = 0;
                        while count < pixel_count {
                            let control = reader.read_u8()?;
                            let longer_than_one = control & 0x80 > 0;
                            let index = control & 0x7f;
                            let run_length = if longer_than_one {
                                read_run_length(reader)?
                            } else {
                                1
                            };
                            for _ in 0..run_length {
                                copy_indexed(&palette, &mut pixels, bpp, index)?;
                            }
                            count += run_length;
                        }
                    }
                    (x, y) => {
                        error!("ZRLE subencoding error {:?}", (x, y));
                        return Err(VncError::InvalidImageData);
                    }
                }
                output_func(VncEvent::RawImage(
                    Rect {
                        x: rect.x + x,
                        y: rect.y + y,
                        width,
                        height,
                    },
                    pixels,
                ))
                .await?;
                x += width;
            }
            y += height;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rfb::codec::testing::{Sink, bgra, deflate, format, rect};

    /// ZRLE's own framing: a big-endian byte count, then that many bytes of the
    /// session-long zlib stream.
    fn framed(tile_data: &[u8]) -> Vec<u8> {
        let compressed = deflate(tile_data);
        let mut bytes = (compressed.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&compressed);
        bytes
    }

    /// A true-colour pixel as ZRLE puts it on the wire.
    ///
    /// With the format this client negotiates, the three bytes are used as the low
    /// three bytes of the pixel directly and alpha is filled in as 255 -- so the wire
    /// order is blue, green, red, not red, green, blue as Tight's copy filter uses.
    fn wire(r: u8, g: u8, b: u8) -> [u8; 3] {
        [b, g, r]
    }

    async fn decode(tile_data: &[u8], width: u16, height: u16) -> Result<Sink, VncError> {
        let sink = Sink::default();
        let bytes = framed(tile_data);
        Decoder::new()
            .decode(
                &format(),
                &rect(width, height),
                &mut &bytes[..],
                &sink.collector(),
            )
            .await?;
        Ok(sink)
    }

    #[tokio::test]
    async fn a_raw_tile_is_three_bytes_a_pixel() {
        // Subencoding 0: no palette, no runs, just pixels.
        let mut tile = vec![0x00];
        for (r, g, b) in [(1, 2, 3), (4, 5, 6), (7, 8, 9), (10, 11, 12)] {
            tile.extend_from_slice(&wire(r, g, b));
        }

        let sink = decode(&tile, 2, 2).await.unwrap();

        let (got, pixels) = sink.image();
        assert_eq!(got, rect(2, 2));
        let expected: Vec<u8> = [(1, 2, 3), (4, 5, 6), (7, 8, 9), (10, 11, 12)]
            .iter()
            .flat_map(|&(r, g, b)| bgra(r, g, b))
            .collect();
        assert_eq!(pixels, expected);
    }

    #[tokio::test]
    async fn a_single_colour_palette_fills_the_tile() {
        // Subencoding 1: one palette entry and no data at all, which is how ZRLE sends
        // a flat area.
        let mut tile = vec![0x01];
        tile.extend_from_slice(&wire(200, 100, 50));

        let sink = decode(&tile, 2, 2).await.unwrap();

        assert_eq!(sink.image().1, bgra(200, 100, 50).repeat(4));
    }

    #[tokio::test]
    async fn a_two_colour_palette_packs_one_bit_per_pixel_and_realigns_each_row() {
        // Two colours means one bit of index per pixel, and a row always starts on a
        // fresh byte however few pixels the last one used. Carrying the bit position
        // across the row boundary would shear the image.
        let mut tile = vec![0x02];
        tile.extend_from_slice(&wire(255, 0, 0));
        tile.extend_from_slice(&wire(0, 0, 255));
        // Four pixels a row, taken from the top of each byte.
        tile.extend_from_slice(&[0b0101_0000, 0b1010_0000]);

        let sink = decode(&tile, 4, 2).await.unwrap();

        let (red, blue) = (bgra(255, 0, 0), bgra(0, 0, 255));
        let mut expected = Vec::new();
        for colour in [red, blue, red, blue, blue, red, blue, red] {
            expected.extend_from_slice(&colour);
        }
        assert_eq!(sink.image().1, expected);
    }

    #[tokio::test]
    async fn a_sixteen_colour_palette_packs_four_bits_per_pixel() {
        // The widest packed index ZRLE has. Palettes of 5 to 16 all use four bits, so
        // the size of the palette does not tell you the packing.
        let mut tile = vec![0x10];
        for i in 0..16u8 {
            tile.extend_from_slice(&wire(i * 16, i, 255 - i));
        }
        // Two pixels in one byte: index 3 then index 12.
        tile.push(0x3c);

        let sink = decode(&tile, 2, 1).await.unwrap();

        let mut expected = Vec::new();
        expected.extend(bgra(3 * 16, 3, 255 - 3));
        expected.extend(bgra(12 * 16, 12, 255 - 12));
        assert_eq!(sink.image().1, expected);
    }

    #[tokio::test]
    async fn true_colour_runs_repeat_a_pixel() {
        // Subencoding 128: a pixel, then how many times it repeats. The length byte is
        // one less than the run, so zero means one pixel.
        let mut tile = vec![0x80];
        tile.extend_from_slice(&wire(1, 2, 3));
        tile.push(0); // run of 1
        tile.extend_from_slice(&wire(9, 8, 7));
        tile.push(2); // run of 3

        let sink = decode(&tile, 2, 2).await.unwrap();

        let mut expected = Vec::new();
        expected.extend(bgra(1, 2, 3));
        for _ in 0..3 {
            expected.extend(bgra(9, 8, 7));
        }
        assert_eq!(sink.image().1, expected);
    }

    #[tokio::test]
    async fn a_run_longer_than_255_continues_into_another_byte() {
        // 255 means "add 255 and read another", so a full 64x4 tile of one colour is
        // two length bytes. Stopping at the first would leave most of the tile
        // undecoded and the rest of the stream misread.
        let mut tile = vec![0x80];
        tile.extend_from_slice(&wire(7, 7, 7));
        tile.extend_from_slice(&[255, 0]); // 1 + 255 + 0 = 256

        let sink = decode(&tile, 64, 4).await.unwrap();

        let (got, pixels) = sink.image();
        assert_eq!(got, rect(64, 4));
        assert_eq!(pixels, bgra(7, 7, 7).repeat(256));
    }

    #[tokio::test]
    async fn indexed_runs_carry_the_index_in_the_control_byte() {
        // Subencoding 130: the top bit of each control byte says whether a length
        // follows, and the rest is the palette index.
        let mut tile = vec![0x82];
        tile.extend_from_slice(&wire(10, 20, 30));
        tile.extend_from_slice(&wire(40, 50, 60));
        tile.push(0x00); // index 0, run of 1
        tile.push(0x81); // index 1, length follows
        tile.push(2); // run of 3

        let sink = decode(&tile, 2, 2).await.unwrap();

        let mut expected = Vec::new();
        expected.extend(bgra(10, 20, 30));
        for _ in 0..3 {
            expected.extend(bgra(40, 50, 60));
        }
        assert_eq!(sink.image().1, expected);
    }

    #[tokio::test]
    async fn a_rectangle_wider_than_a_tile_is_emitted_in_pieces() {
        // ZRLE works in 64x64 tiles and the edge ones are short. Each arrives as its
        // own image at its own offset, so a wrong offset puts the right pixels in the
        // wrong place -- which no "did it draw" check would notice.
        let mut tile = Vec::new();
        for _ in 0..2 {
            tile.push(0x01);
            tile.extend_from_slice(&wire(5, 5, 5));
        }

        let sink = decode(&tile, 65, 1).await.unwrap();

        let rects: Vec<Rect> = sink.images().iter().map(|(r, _)| *r).collect();
        assert_eq!(
            rects,
            vec![
                Rect {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 1
                },
                Rect {
                    x: 64,
                    y: 0,
                    width: 1,
                    height: 1
                },
            ]
        );
        let sizes: Vec<usize> = sink.images().iter().map(|(_, p)| p.len()).collect();
        assert_eq!(sizes, vec![64 * 4, 4]);
    }

    #[tokio::test]
    async fn a_tall_rectangle_is_split_by_row_as_well() {
        let mut tile = Vec::new();
        for _ in 0..2 {
            tile.push(0x01);
            tile.extend_from_slice(&wire(1, 1, 1));
        }

        let sink = decode(&tile, 1, 65).await.unwrap();

        let rects: Vec<Rect> = sink.images().iter().map(|(r, _)| *r).collect();
        assert_eq!(
            rects,
            vec![
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 64
                },
                Rect {
                    x: 0,
                    y: 64,
                    width: 1,
                    height: 1
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_palette_index_past_the_end_is_refused() {
        // An indexed-RLE control byte carries seven bits of index, so it reaches 127
        // whatever the palette holds. Upstream sliced the palette with it and panicked.
        let mut tile = vec![0x82];
        tile.extend_from_slice(&wire(10, 20, 30));
        tile.extend_from_slice(&wire(40, 50, 60));
        tile.push(0x05); // index 5 into a palette of 2

        let result = decode(&tile, 2, 2).await;

        assert!(
            matches!(result, Err(VncError::InvalidImageData)),
            "expected a refusal, got {result:?}"
        );
    }

    #[tokio::test]
    async fn an_unknown_subencoding_is_refused() {
        // A palette of 17 with no run-length bit is not a subencoding ZRLE defines.
        // There is no length to skip, so the only safe answer is to stop.
        let mut tile = vec![0x11];
        for i in 0..17u8 {
            tile.extend_from_slice(&wire(i, i, i));
        }

        let result = decode(&tile, 2, 2).await;

        assert!(
            matches!(result, Err(VncError::InvalidImageData)),
            "expected a refusal, got {result:?}"
        );
    }

    #[tokio::test]
    async fn a_truncated_tile_is_an_error() {
        // The tile promises four pixels and the stream holds one.
        let mut tile = vec![0x00];
        tile.extend_from_slice(&wire(1, 2, 3));

        let result = decode(&tile, 2, 2).await;

        assert!(result.is_err(), "a short tile should not decode");
    }

    #[tokio::test]
    async fn an_implausible_payload_is_refused_before_allocating() {
        // The length is four bytes of the server's word. Believing it means a 4GB
        // allocation on request.
        let sink = Sink::default();
        let bytes = u32::MAX.to_be_bytes();
        let result = Decoder::new()
            .decode(&format(), &rect(2, 2), &mut &bytes[..], &sink.collector())
            .await;

        assert!(matches!(result, Err(VncError::General(_))), "{result:?}");
    }

    #[tokio::test]
    async fn the_zlib_stream_persists_across_rectangles() {
        // One stream for the whole session, so the second rectangle can only be read in
        // the state the first left behind.
        let mut compress = flate2::Compress::new(flate2::Compression::default(), true);
        let mut decoder = Decoder::new();

        for colour in [(1u8, 2u8, 3u8), (200, 100, 50)] {
            let mut tile = vec![0x01];
            tile.extend_from_slice(&wire(colour.0, colour.1, colour.2));

            let mut compressed = Vec::with_capacity(tile.len() + 128);
            compress
                .compress_vec(&tile, &mut compressed, flate2::FlushCompress::Sync)
                .unwrap();
            let mut bytes = (compressed.len() as u32).to_be_bytes().to_vec();
            bytes.extend_from_slice(&compressed);

            let sink = Sink::default();
            decoder
                .decode(&format(), &rect(2, 2), &mut &bytes[..], &sink.collector())
                .await
                .expect("a rectangle continuing the same zlib stream");
            assert_eq!(sink.image().1, bgra(colour.0, colour.1, colour.2).repeat(4));
        }
    }

    #[tokio::test]
    async fn a_failed_rectangle_leaves_the_stream_usable() {
        // A rectangle can fail part-way through its tiles for reasons that say nothing
        // about the stream -- a bad palette index, an unknown subencoding. The
        // decompressor has to go back in its slot regardless, or every later ZRLE
        // rectangle in the session fails with "the stream is gone" and the desktop
        // stops updating until the connection is remade.
        let mut compress = flate2::Compress::new(flate2::Compression::default(), true);
        let mut decoder = Decoder::new();

        // A tile that fails inside the loop: index 5 into a palette of two.
        let mut bad = vec![0x82];
        bad.extend_from_slice(&wire(1, 2, 3));
        bad.extend_from_slice(&wire(4, 5, 6));
        bad.push(0x05);

        let mut good = vec![0x01];
        good.extend_from_slice(&wire(9, 9, 9));

        let mut framed = Vec::new();
        for tile in [&bad, &good] {
            let mut compressed = Vec::with_capacity(tile.len() + 128);
            compress
                .compress_vec(tile, &mut compressed, flate2::FlushCompress::Sync)
                .unwrap();
            let mut bytes = (compressed.len() as u32).to_be_bytes().to_vec();
            bytes.extend_from_slice(&compressed);
            framed.push(bytes);
        }

        let sink = Sink::default();
        let result = decoder
            .decode(
                &format(),
                &rect(2, 2),
                &mut &framed[0][..],
                &sink.collector(),
            )
            .await;
        assert!(
            matches!(result, Err(VncError::InvalidImageData)),
            "the bad index should be refused, got {result:?}"
        );

        let sink = Sink::default();
        decoder
            .decode(
                &format(),
                &rect(2, 2),
                &mut &framed[1][..],
                &sink.collector(),
            )
            .await
            .expect("the decoder was poisoned by the failed rectangle");
        assert_eq!(sink.image().1, bgra(9, 9, 9).repeat(4));
    }
}
