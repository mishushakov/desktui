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
