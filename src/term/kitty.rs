//! Kitty graphics protocol encoder.
//!
//! We only need a narrow slice of the protocol, but that slice has to be exactly
//! right:
//!
//! * `a=T` transmits and places in one command. Re-transmitting an id deletes
//!   the previous image *and* its placements, so a stable id per tile gives
//!   flicker-free partial updates without any explicit delete traffic.
//! * `f=24` sends packed RGB. That is 25% less data through zlib than RGBA, and
//!   the alpha channel would be constant anyway.
//! * `o=z` compresses the payload. Screen content is highly compressible, and
//!   this is the difference between a usable and an unusable frame rate.
//! * `C=1` stops the cursor from moving, so a placement cannot scroll the
//!   screen and shift every other placement with it.
//! * `q=2` suppresses both success and error replies. Without it the terminal
//!   would answer every tile and we would have to drain those answers out of
//!   the input stream.
//! * `z=-1` puts the image below text but above the cell background, so the
//!   status line and the help overlay stay legible on top of the remote screen.
//!   At the default `z=0` the image would cover them.
//!
//! Animation frames (`a=f`) are deliberately unused: they exist for
//! terminal-paced playback of frames known ahead of time, and Ghostty does not
//! implement them. Tile retransmit covers the same ground.

use std::io::Write;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::Compression;
use flate2::write::ZlibEncoder;

/// Maximum base64 payload per escape sequence, from the protocol spec. Chunks
/// before the last must also be a multiple of four, which this is.
const CHUNK: usize = 4096;

/// zlib level 1. Level 6 costs several times the CPU for a few percent on
/// screen content, and we are spending that CPU every frame.
const ZLIB_LEVEL: Compression = Compression::new(1);

/// First image id we use. Tiles take `IMAGE_ID_BASE + index`, well away from
/// the low ids other programs tend to pick.
pub const IMAGE_ID_BASE: u32 = 0x7600;

/// Where a tile goes and how big it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Image id. Re-using one replaces the image and its placements atomically.
    pub id: u32,
    /// Zero-based cell the top-left corner lands on.
    pub col: u16,
    pub row: u16,
    pub w: u32,
    pub h: u32,
}

pub struct KittyEncoder {
    compress: bool,
    zbuf: Vec<u8>,
    b64: String,
}

impl KittyEncoder {
    pub fn new(compress: bool) -> Self {
        Self {
            compress,
            zbuf: Vec::with_capacity(256 * 1024),
            b64: String::with_capacity(512 * 1024),
        }
    }

    /// Transmit `rgb` and place it with its top-left corner at cell
    /// `(col, row)`, both zero-based.
    ///
    /// `rgb` must be exactly `w * h * 3` bytes of packed RGB.
    pub fn place_rgb(&mut self, out: &mut Vec<u8>, at: Placement, rgb: &[u8]) {
        let Placement { id, col, row, w, h } = at;
        debug_assert_eq!(rgb.len(), (w as usize) * (h as usize) * 3);
        if w == 0 || h == 0 || rgb.is_empty() {
            return;
        }

        // Destructured so the compression buffer and the base64 buffer are
        // borrowed independently.
        let Self {
            compress,
            zbuf,
            b64,
        } = self;

        let data: &[u8] = if *compress {
            zbuf.clear();
            let mut enc = ZlibEncoder::new(&mut *zbuf, ZLIB_LEVEL);
            // Writing into a Vec cannot fail.
            enc.write_all(rgb).expect("zlib write to Vec");
            enc.finish().expect("zlib finish to Vec");
            zbuf
        } else {
            rgb
        };

        b64.clear();
        BASE64.encode_string(data, b64);

        // Place at the cursor, so put the cursor where the tile goes. CUP is
        // one-based.
        let _ = write!(out, "\x1b[{};{}H", row as u32 + 1, col as u32 + 1);

        let bytes = b64.as_bytes();
        let mut chunks = bytes.chunks(CHUNK);
        let first = chunks.next().unwrap_or(b"");
        let mut remaining = bytes.len().saturating_sub(first.len());

        out.extend_from_slice(b"\x1b_Ga=T,q=2,C=1,z=-1,f=24,i=");
        let _ = write!(out, "{id},s={w},v={h}");
        if *compress {
            out.extend_from_slice(b",o=z");
        }
        if remaining > 0 {
            out.extend_from_slice(b",m=1");
        }
        out.push(b';');
        out.extend_from_slice(first);
        out.extend_from_slice(b"\x1b\\");

        // Continuation chunks carry only `m` and `q`, as the spec requires.
        for chunk in chunks {
            remaining -= chunk.len();
            let more = if remaining > 0 { 1 } else { 0 };
            let _ = write!(out, "\x1b_Gm={more},q=2;");
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\x1b\\");
        }
    }

    /// Place an image whose pixels are already in a shared memory object.
    ///
    /// The payload is the object's name rather than the data, which is the whole
    /// point: nothing is base64-encoded but a short string. The `m` key is
    /// meaningless for a local medium, so there is never more than one command.
    pub fn place_shm(&mut self, out: &mut Vec<u8>, at: Placement, name: &str) {
        let Placement { id, col, row, w, h } = at;
        if w == 0 || h == 0 {
            return;
        }
        let _ = write!(out, "\x1b[{};{}H", row as u32 + 1, col as u32 + 1);
        self.b64.clear();
        BASE64.encode_string(name.as_bytes(), &mut self.b64);
        let _ = write!(out, "\x1b_Ga=T,q=2,C=1,z=-1,f=24,t=s,i={id},s={w},v={h};");
        out.extend_from_slice(self.b64.as_bytes());
        out.extend_from_slice(b"\x1b\\");
    }

    /// Delete every image and free the data behind it.
    ///
    /// Used when the layout changes: every tile is about to be retransmitted, so
    /// the old ones are only in the way. The teardown sequence in `term::mod` spells
    /// the same command out by hand, because it has to work from a panic handler
    /// where there is no encoder to call.
    pub fn delete_all(out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2\x1b\\");
    }
}

/// Begin a synchronised update: the terminal shows nothing until it ends, so a
/// frame made of many tiles commits at once instead of tearing.
pub fn begin_sync(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[?2026h");
}

pub fn end_sync(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[?2026l");
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::ZlibDecoder;
    use std::io::Read;

    fn place(id: u32, col: u16, row: u16, w: u32, h: u32) -> Placement {
        Placement { id, col, row, w, h }
    }

    /// Pull the payloads out of a stream of graphics commands, returning the
    /// first command's keys alongside the concatenated base64.
    fn split(out: &[u8]) -> (String, String, usize) {
        let text = String::from_utf8(out.to_vec()).unwrap();
        let mut keys = String::new();
        let mut payload = String::new();
        let mut count = 0;
        for cmd in text.split("\x1b_G").skip(1) {
            let cmd = cmd.strip_suffix("\x1b\\").unwrap_or(cmd);
            let cmd = cmd.split("\x1b\\").next().unwrap();
            let (k, p) = cmd.split_once(';').unwrap();
            if count == 0 {
                keys = k.to_string();
            }
            payload.push_str(p);
            count += 1;
        }
        (keys, payload, count)
    }

    #[test]
    fn small_tile_is_one_command_with_all_the_keys() {
        let mut enc = KittyEncoder::new(false);
        let mut out = Vec::new();
        let rgb = vec![0xab; 4 * 3];
        enc.place_rgb(&mut out, place(IMAGE_ID_BASE, 0, 0, 4, 1), &rgb);

        let (keys, payload, count) = split(&out);
        assert_eq!(count, 1);
        assert!(keys.contains("a=T"), "{keys}");
        assert!(keys.contains("f=24"), "{keys}");
        assert!(keys.contains("C=1"), "{keys}");
        assert!(keys.contains("q=2"), "{keys}");
        // Below text, so overlays remain readable over the remote screen.
        assert!(keys.contains("z=-1"), "{keys}");
        assert!(keys.contains("s=4,v=1"), "{keys}");
        assert!(
            !keys.contains("m="),
            "a single chunk must not carry m: {keys}"
        );
        assert_eq!(BASE64.decode(payload).unwrap(), rgb);
    }

    #[test]
    fn cursor_is_positioned_one_based_before_the_command() {
        let mut enc = KittyEncoder::new(false);
        let mut out = Vec::new();
        enc.place_rgb(&mut out, place(IMAGE_ID_BASE, 7, 3, 1, 1), &[1, 2, 3]);
        assert!(
            out.starts_with(b"\x1b[4;8H"),
            "{:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn large_tile_chunks_at_4096_and_round_trips() {
        let mut enc = KittyEncoder::new(false);
        let mut out = Vec::new();
        // 64x64 RGB base64-encodes to 16384 bytes: exactly four chunks.
        let w = 64;
        let h = 64;
        let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i % 251) as u8).collect();
        enc.place_rgb(
            &mut out,
            place(IMAGE_ID_BASE, 0, 0, w as u32, h as u32),
            &rgb,
        );

        let (keys, payload, count) = split(&out);
        assert_eq!(count, 4);
        assert!(keys.contains("m=1"), "{keys}");
        assert_eq!(payload.len(), 16384);
        assert_eq!(BASE64.decode(payload).unwrap(), rgb);

        // Only the last chunk says m=0, and continuation chunks carry nothing
        // but m and q.
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("m=0").count(), 1);
        for cmd in text.split("\x1b_G").skip(2) {
            let keys = cmd.split(';').next().unwrap();
            assert!(
                keys == "m=1,q=2" || keys == "m=0,q=2",
                "continuation keys must be m and q only, got {keys}"
            );
        }
    }

    #[test]
    fn chunk_boundary_is_exact_for_a_payload_just_over_the_limit() {
        let mut enc = KittyEncoder::new(false);
        let mut out = Vec::new();
        // 1025 pixels -> 3075 raw bytes -> 4100 base64 bytes: two chunks, the
        // second holding 4 bytes.
        let rgb = vec![0u8; 1025 * 3];
        enc.place_rgb(&mut out, place(IMAGE_ID_BASE, 0, 0, 1025, 1), &rgb);
        let (_, payload, count) = split(&out);
        assert_eq!(count, 2);
        assert_eq!(payload.len(), 4100);
    }

    #[test]
    fn compressed_payload_declares_and_survives_zlib() {
        let mut enc = KittyEncoder::new(true);
        let mut out = Vec::new();
        let rgb = vec![0x5a; 128 * 128 * 3];
        enc.place_rgb(&mut out, place(IMAGE_ID_BASE + 9, 2, 5, 128, 128), &rgb);

        let (keys, payload, _) = split(&out);
        assert!(keys.contains("o=z"), "{keys}");
        assert!(keys.contains(&format!("i={}", IMAGE_ID_BASE + 9)), "{keys}");

        let compressed = BASE64.decode(payload).unwrap();
        assert!(
            compressed.len() < rgb.len() / 10,
            "flat colour should compress hard, got {} from {}",
            compressed.len(),
            rgb.len()
        );
        let mut decoded = Vec::new();
        ZlibDecoder::new(&compressed[..])
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, rgb);
    }

    #[test]
    fn empty_tile_emits_nothing() {
        let mut enc = KittyEncoder::new(true);
        let mut out = Vec::new();
        enc.place_rgb(&mut out, place(IMAGE_ID_BASE, 0, 0, 0, 0), &[]);
        assert!(out.is_empty());
    }
}
