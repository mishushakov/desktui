use crate::rfb::VncError;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// All supported vnc encodings
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum VncEncoding {
    Raw = 0,
    CopyRect = 1,
    // Rre = 2,
    // Hextile = 5,
    Tight = 7,
    // Trle = 15 is deliberately absent; see the note in this module's parent.
    Zrle = 16,
    CursorPseudo = -239,
    DesktopSizePseudo = -223,
    LastRectPseudo = -224,
    /// The client can cope with the framebuffer changing size, and can ask for a
    /// size of its own with `SetDesktopSize`.
    ExtendedDesktopSizePseudo = -308,
    /// The server reports the state of the lock keys, so the client can tell when
    /// its own caps lock disagrees with the remote one.
    QemuLedStatePseudo = -261,
    /// The client understands `ServerFence` and will answer it. Requesting this is
    /// how a client discovers the extension: the server sends a fence in reply.
    FencePseudo = -312,
    /// The client wants updates pushed rather than requested. Requesting this is how
    /// support is discovered: the server answers with `EndOfContinuousUpdates`.
    ContinuousUpdatesPseudo = -313,
}

/// Encoding numbers for the quality and compression hints.
///
/// These are ranges rather than single values, and carry no rectangles: the number
/// itself is the message. Not sending a quality level matters -- the spec says
/// `JpegCompression` is not used in Tight encoding unless one is given, which is the
/// only way to ask a server for a lossless picture.
pub mod hint {
    /// -32 is the lowest quality, -23 the highest.
    pub const QUALITY_BASE: i32 = -32;
    /// -256 is the least compression, -247 the most.
    pub const COMPRESSION_BASE: i32 = -256;

    /// The encoding number for a JPEG quality level, 0 (lowest) to 9 (highest).
    pub fn quality(level: u8) -> i32 {
        QUALITY_BASE + i32::from(level.min(9))
    }

    /// The encoding number for a compression level, 0 (least) to 9 (most).
    pub fn compression(level: u8) -> i32 {
        COMPRESSION_BASE + i32::from(level.min(9))
    }
}

#[cfg(test)]
mod hint_tests {
    use super::hint;

    #[test]
    fn quality_and_compression_map_to_the_documented_ranges() {
        // -32 is the worst quality and -23 the best; -256 the least compression and
        // -247 the most. Getting these backwards would quietly ask for the opposite of
        // what the user wanted.
        assert_eq!(hint::quality(0), -32);
        assert_eq!(hint::quality(9), -23);
        assert_eq!(hint::compression(0), -256);
        assert_eq!(hint::compression(9), -247);
        // Out of range is clamped rather than wrapping into another encoding's number.
        assert_eq!(hint::quality(200), -23);
        assert_eq!(hint::compression(200), -247);
    }
}

/// An encoding the server used that this client did not ask for.
///
/// Servers do occasionally send rectangles a client never requested, and there
/// is no way to skip one safely: without knowing its length the stream cannot be
/// resynchronised. So this is reported rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownEncoding(pub i32);

impl TryFrom<u32> for VncEncoding {
    type Error = UnknownEncoding;

    fn try_from(num: u32) -> Result<Self, UnknownEncoding> {
        // A match rather than a transmute: upstream reinterpreted arbitrary
        // network integers as this enum, which is undefined behaviour for any
        // value that is not a valid discriminant. Falling back to `Raw` would be
        // just as bad in practice, since it would read a rectangle's worth of
        // pixels out of a stream that holds something else.
        match num as i32 {
            0 => Ok(VncEncoding::Raw),
            1 => Ok(VncEncoding::CopyRect),
            7 => Ok(VncEncoding::Tight),
            16 => Ok(VncEncoding::Zrle),
            -239 => Ok(VncEncoding::CursorPseudo),
            -223 => Ok(VncEncoding::DesktopSizePseudo),
            -224 => Ok(VncEncoding::LastRectPseudo),
            -308 => Ok(VncEncoding::ExtendedDesktopSizePseudo),
            -261 => Ok(VncEncoding::QemuLedStatePseudo),
            -312 => Ok(VncEncoding::FencePseudo),
            -313 => Ok(VncEncoding::ContinuousUpdatesPseudo),
            other => Err(UnknownEncoding(other)),
        }
    }
}

impl From<VncEncoding> for u32 {
    fn from(e: VncEncoding) -> Self {
        e as u32
    }
}

/// All supported vnc versions
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq)]
#[repr(u8)]
pub enum VncVersion {
    RFB33,
    RFB37,
    RFB38,
}

impl From<[u8; 12]> for VncVersion {
    fn from(version: [u8; 12]) -> Self {
        match &version {
            b"RFB 003.003\n" => VncVersion::RFB33,
            b"RFB 003.007\n" => VncVersion::RFB37,
            b"RFB 003.008\n" => VncVersion::RFB38,
            // https://www.rfc-editor.org/rfc/rfc6143#section-7.1.1
            //  Other version numbers are reported by some servers and clients,
            //  but should be interpreted as 3.3 since they do not implement the
            //  different handshake in 3.7 or 3.8.
            _ => VncVersion::RFB33,
        }
    }
}

impl From<VncVersion> for &[u8; 12] {
    fn from(version: VncVersion) -> Self {
        match version {
            VncVersion::RFB33 => b"RFB 003.003\n",
            VncVersion::RFB37 => b"RFB 003.007\n",
            VncVersion::RFB38 => b"RFB 003.008\n",
        }
    }
}

impl VncVersion {
    pub(crate) async fn read<S>(reader: &mut S) -> Result<Self, VncError>
    where
        S: AsyncRead + Unpin,
    {
        let mut buffer = [0_u8; 12];
        reader.read_exact(&mut buffer).await?;
        Ok(buffer.into())
    }

    pub(crate) async fn write<S>(self, writer: &mut S) -> Result<(), VncError>
    where
        S: AsyncWrite + Unpin,
    {
        writer
            .write_all(&<VncVersion as Into<&[u8; 12]>>::into(self)[..])
            .await?;
        Ok(())
    }
}

///  Pixel Format Data Structure according to [RFC6143](https://www.rfc-editor.org/rfc/rfc6143.html#section-7.4)
///
/// ```text
/// +--------------+--------------+-----------------+
/// | No. of bytes | Type [Value] | Description     |
/// +--------------+--------------+-----------------+
/// | 1            | U8           | bits-per-pixel  |
/// | 1            | U8           | depth           |
/// | 1            | U8           | big-endian-flag |
/// | 1            | U8           | true-color-flag |
/// | 2            | U16          | red-max         |
/// | 2            | U16          | green-max       |
/// | 2            | U16          | blue-max        |
/// | 1            | U8           | red-shift       |
/// | 1            | U8           | green-shift     |
/// | 1            | U8           | blue-shift      |
/// | 3            |              | padding         |
/// +--------------+--------------+-----------------+
/// ```
#[derive(Debug, Clone, Copy)]
pub struct PixelFormat {
    /// the number of bits used for each pixel value on the wire
    ///
    /// 8, 16, 32(usually) only
    ///
    pub bits_per_pixel: u8,
    /// Although the depth should
    ///
    /// be consistent with the bits-per-pixel and the various -max values,
    ///
    /// clients do not use it when interpreting pixel data.
    ///
    pub depth: u8,
    /// true if multi-byte pixels are interpreted as big endian
    ///
    pub big_endian_flag: u8,
    /// true then the last six items specify how to extract the red, green and blue intensities from the pixel value
    ///
    pub true_color_flag: u8,
    /// the next three always in big-endian order
    /// no matter how the `big_endian_flag` is set
    ///
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    /// the number of shifts needed to get the red value in a pixel to the least significant bit
    ///
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
    _padding_1: u8,
    _padding_2: u8,
    _padding_3: u8,
}

impl From<PixelFormat> for Vec<u8> {
    fn from(pf: PixelFormat) -> Vec<u8> {
        vec![
            pf.bits_per_pixel,
            pf.depth,
            pf.big_endian_flag,
            pf.true_color_flag,
            (pf.red_max >> 8) as u8,
            pf.red_max as u8,
            (pf.green_max >> 8) as u8,
            pf.green_max as u8,
            (pf.blue_max >> 8) as u8,
            pf.blue_max as u8,
            pf.red_shift,
            pf.green_shift,
            pf.blue_shift,
            pf._padding_1,
            pf._padding_2,
            pf._padding_3,
        ]
    }
}

impl TryFrom<[u8; 16]> for PixelFormat {
    type Error = VncError;

    fn try_from(pf: [u8; 16]) -> Result<Self, Self::Error> {
        let bits_per_pixel = pf[0];
        if bits_per_pixel != 8 && bits_per_pixel != 16 && bits_per_pixel != 32 {
            return Err(VncError::WrongPixelFormat);
        }
        let depth = pf[1];
        let big_endian_flag = pf[2];
        let true_color_flag = pf[3];
        let red_max = u16::from_be_bytes(pf[4..6].try_into().unwrap());
        let green_max = u16::from_be_bytes(pf[6..8].try_into().unwrap());
        let blue_max = u16::from_be_bytes(pf[8..10].try_into().unwrap());
        let red_shift = pf[10];
        let green_shift = pf[11];
        let blue_shift = pf[12];
        let _padding_1 = pf[13];
        let _padding_2 = pf[14];
        let _padding_3 = pf[15];
        Ok(PixelFormat {
            bits_per_pixel,
            depth,
            big_endian_flag,
            true_color_flag,
            red_max,
            green_max,
            blue_max,
            red_shift,
            green_shift,
            blue_shift,
            _padding_1,
            _padding_2,
            _padding_3,
        })
    }
}

impl Default for PixelFormat {
    // by default the pixel transformed is (a << 24 | r << 16 || g << 8 | b) in le
    // which is [b, g, r, a] in network
    fn default() -> Self {
        Self {
            bits_per_pixel: 32,
            depth: 24,
            big_endian_flag: 0,
            true_color_flag: 1,
            red_max: 255,
            green_max: 255,
            blue_max: 255,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
            _padding_1: 0,
            _padding_2: 0,
            _padding_3: 0,
        }
    }
}

impl PixelFormat {
    // (a << 24 | r << 16 || g << 8 | b) in le
    // [b, g, r, a] in network
    pub fn bgra() -> PixelFormat {
        PixelFormat::default()
    }

    // (a << 24 | b << 16 | g << 8 | r) in le
    // which is [r, g, b, a] in network
    pub fn rgba() -> PixelFormat {
        Self {
            red_shift: 0,
            blue_shift: 16,
            ..Default::default()
        }
    }

    pub(crate) async fn read<S>(reader: &mut S) -> Result<Self, VncError>
    where
        S: AsyncRead + Unpin,
    {
        let mut pixel_buffer = [0_u8; 16];
        reader.read_exact(&mut pixel_buffer).await?;
        pixel_buffer.try_into()
    }
}
