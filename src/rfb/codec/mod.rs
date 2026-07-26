mod cursor;
mod raw;
mod tight;
mod zlib;
mod zrle;
pub(crate) use cursor::Decoder as CursorDecoder;
pub(crate) use raw::Decoder as RawDecoder;
pub(crate) use tight::Decoder as TightDecoder;
pub(crate) use zrle::Decoder as ZrleDecoder;

use crate::rfb::{MAX_PAYLOAD, VncError};

/// A zeroed buffer of `len` bytes.
///
/// Upstream's version of this called `Vec::set_len` on uninitialised capacity and
/// handed the result out as `&mut [u8]`, which is undefined behaviour even when
/// the only thing that touches it is a `read_exact` that fills it. The memset is
/// not free, but it is a fraction of what decoding the pixels costs.
pub(super) fn zeroed_vec(len: usize) -> Vec<u8> {
    vec![0; len]
}

/// Check a length taken off the wire before allocating for it.
pub(super) fn checked_len(len: usize, what: &str) -> Result<usize, VncError> {
    if len > MAX_PAYLOAD {
        return Err(VncError::General(format!(
            "server declared an implausible {what} of {len} bytes"
        )));
    }
    Ok(len)
}

/// Look up a palette entry, refusing an index the palette does not have.
///
/// The index comes off the wire and upstream used it to slice the palette
/// directly, so an out-of-range one panicked the process. Both Tight and ZRLE can
/// carry indices far beyond the palette they just declared.
pub(super) fn palette_entry(
    palette: &[u8],
    index: usize,
    stride: usize,
) -> Result<&[u8], VncError> {
    let start = index * stride;
    palette
        .get(start..start + stride)
        .ok_or(VncError::InvalidImageData)
}

/// Shared by the decoder tests: somewhere to put events, and the two encodings a
/// test has to produce by hand to feed a decoder real bytes.
///
/// The decoders are the one part of this client that consumes attacker-controlled
/// bytes and turns them into pixels, and until these tests existed the only thing
/// covering them was the live suite -- which asserts that no decoder *gave up*, not
/// that the pixels are right, and is `#[ignore]`d besides.
#[cfg(test)]
pub(super) mod testing {
    use crate::remote::Rect;
    use crate::rfb::{PixelFormat, VncError, VncEvent};
    use std::cell::RefCell;
    use std::future::{Ready, ready};

    /// Collects what a decoder emits.
    ///
    /// `decode` takes `&F where F: Fn(VncEvent) -> Future`, so the collector cannot
    /// hold `&mut` -- hence the `RefCell`. Nothing is awaited while the borrow is
    /// live, so it cannot panic.
    #[derive(Default, Debug)]
    pub struct Sink(RefCell<Vec<VncEvent>>);

    impl Sink {
        /// Pass `&sink.collector()` as a decoder's `output_func`.
        pub fn collector(&self) -> impl Fn(VncEvent) -> Ready<Result<(), VncError>> + '_ {
            move |event| {
                self.0.borrow_mut().push(event);
                ready(Ok(()))
            }
        }

        /// Every `RawImage` emitted, as (rect, pixels).
        pub fn images(&self) -> Vec<(Rect, Vec<u8>)> {
            self.0
                .borrow()
                .iter()
                .filter_map(|e| match e {
                    VncEvent::RawImage(rect, data) => Some((*rect, data.clone())),
                    _ => None,
                })
                .collect()
        }

        /// The single image a decoder was expected to emit.
        #[track_caller]
        pub fn image(&self) -> (Rect, Vec<u8>) {
            let images = self.images();
            assert_eq!(
                images.len(),
                1,
                "expected exactly one image, got {:?}",
                self.0.borrow()
            );
            images.into_iter().next().unwrap()
        }

        /// The events themselves, for the decoders that emit something other than an
        /// image.
        pub fn events(&self) -> Vec<VncEvent> {
            self.0.borrow().clone()
        }
    }

    /// `PixelFormat::bgra` puts red at 16, green at 8, blue at 0 and leaves the top
    /// byte for alpha, so a wire pixel `(r, g, b)` decodes to these four bytes. Every
    /// expectation below is built with this rather than written out, so a test says
    /// which colour it means instead of which byte order.
    pub fn bgra(r: u8, g: u8, b: u8) -> [u8; 4] {
        [b, g, r, 255]
    }

    /// The same colour repeated, for a fill.
    pub fn bgra_repeated(r: u8, g: u8, b: u8, count: usize) -> Vec<u8> {
        bgra(r, g, b).repeat(count)
    }

    /// Compress with a sync flush rather than finishing the stream.
    ///
    /// Finishing it would emit zlib's final block, and `ZlibReader` treats
    /// `StreamEnd` as an error -- correctly, because an RFB server keeps one stream
    /// open across every rectangle of the session and never ends it.
    pub fn deflate(data: &[u8]) -> Vec<u8> {
        let mut compress = flate2::Compress::new(flate2::Compression::default(), true);
        let mut out = Vec::with_capacity(data.len() + 128);
        compress
            .compress_vec(data, &mut out, flate2::FlushCompress::Sync)
            .expect("compressing test data");
        out
    }

    /// Tight's compact length: 7 bits per byte, low group first, top bit meaning
    /// "another byte follows". The third byte carries a full 8 bits.
    pub fn tight_len(len: usize) -> Vec<u8> {
        if len < 0x80 {
            vec![len as u8]
        } else if len < 1 << 14 {
            vec![(len & 0x7f) as u8 | 0x80, ((len >> 7) & 0x7f) as u8]
        } else {
            vec![
                (len & 0x7f) as u8 | 0x80,
                ((len >> 7) & 0x7f) as u8 | 0x80,
                ((len >> 14) & 0xff) as u8,
            ]
        }
    }

    /// A rectangle at the origin.
    pub fn rect(width: u16, height: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// The format the client actually negotiates.
    pub fn format() -> PixelFormat {
        PixelFormat::bgra()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeroed_vec_is_actually_zeroed() {
        assert_eq!(zeroed_vec(4), vec![0, 0, 0, 0]);
        assert!(zeroed_vec(0).is_empty());
    }

    #[test]
    fn implausible_lengths_are_refused_before_allocating() {
        assert!(checked_len(1024, "rectangle").is_ok());
        assert!(checked_len(MAX_PAYLOAD, "rectangle").is_ok());
        assert!(checked_len(MAX_PAYLOAD + 1, "rectangle").is_err());
        assert!(checked_len(u32::MAX as usize, "rectangle").is_err());
    }

    #[test]
    fn palette_lookup_refuses_an_index_past_the_end() {
        let palette = vec![1, 2, 3, 4, 5, 6]; // two entries of three bytes
        assert_eq!(palette_entry(&palette, 0, 3).unwrap(), &[1, 2, 3]);
        assert_eq!(palette_entry(&palette, 1, 3).unwrap(), &[4, 5, 6]);
        // The wire can carry any index up to 127 regardless of palette size.
        assert!(palette_entry(&palette, 2, 3).is_err());
        assert!(palette_entry(&palette, 127, 3).is_err());
        assert!(palette_entry(&[], 0, 4).is_err());
    }
}
