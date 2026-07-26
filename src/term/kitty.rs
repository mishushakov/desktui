//! Kitty graphics protocol encoder.
//!
//! We only need a narrow slice of the protocol, but that slice has to be exactly
//! right:
//!
//! * `a=T` transmits and places in one command, and `p=` pins the placement it
//!   makes. Both halves matter. A terminal keys a placement by image id *and*
//!   placement id, and a command that omits `p=` asks for a brand new placement
//!   rather than a replacement -- so a stable image id alone is not enough to
//!   replace anything. Every tile of every frame would add a placement that
//!   nothing ever removes.
//! * `a=d,d=i` drops the previous placement just before the new one is made. The
//!   spec says re-transmitting an image id deletes its placements for us, and
//!   this code used to rely on that, but Ghostty's `addPlacement` overwrites the
//!   map entry without releasing the pin the old placement held in the screen.
//!   With `p=` alone the placement count stays flat and the pins still pile up,
//!   which is a leak in the terminal that only an explicit delete collects. The
//!   `d` is lower case so the image data survives to be overwritten by the `a=T`
//!   that follows.
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
//!   status line and the command menu stay legible on top of the remote screen.
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

/// First image id we use, well away from the low ids other programs tend to pick.
///
/// A tile takes `IMAGE_ID_BASE` plus an offset for where it sits in the grid, which
/// `render::TileGrid` works out. Its stride is what the distance to
/// [`OVERLAY_IMAGE_ID`] has to clear.
pub const IMAGE_ID_BASE: u32 = 0x7600;

/// Placement id carried by every image we place.
///
/// One constant covers every tile: a placement is keyed by the pair, and each
/// tile already owns an image id, so nothing collides. Any non-zero value would
/// do -- zero is the one that must be avoided, being the protocol's way of
/// asking for an additional placement instead of a replacement.
const PLACEMENT_ID: u32 = 1;

/// Image id for an overlay's backdrop, above every id a tile can take.
///
/// The distance is the point: at equal z-index the higher id is composited on
/// top, which is the only way an overlay can cover the remote screen. A cell
/// background cannot, being painted below the image.
///
/// A tile is addressed by its place in the grid rather than by a running count, so
/// what has to be cleared is the whole coordinate space and not the number of tiles
/// on screen -- `render::TILE_ID_STRIDE` squared, which this is comfortably past.
pub const OVERLAY_IMAGE_ID: u32 = IMAGE_ID_BASE + 0x40000;

/// Image id for the menu row under the pointer, one above the backdrop it is drawn
/// on so it is composited over it rather than under.
pub const MENU_HIGHLIGHT_IMAGE_ID: u32 = OVERLAY_IMAGE_ID + 1;

/// Image id for the notification popup's backdrop, above both of the menu's: a note
/// that arrives while the menu is open belongs on top of it, not under it.
pub const TOAST_IMAGE_ID: u32 = MENU_HIGHLIGHT_IMAGE_ID + 1;

/// Where a tile goes and how big it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Image id. Re-using one replaces the image data; the placement on top of it
    /// is replaced by the fixed `p=` every command carries.
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

        release_placement(out, id);

        // Place at the cursor, so put the cursor where the tile goes. CUP is
        // one-based.
        let _ = write!(out, "\x1b[{};{}H", row as u32 + 1, col as u32 + 1);

        let bytes = b64.as_bytes();
        let mut chunks = bytes.chunks(CHUNK);
        let first = chunks.next().unwrap_or(b"");
        let mut remaining = bytes.len().saturating_sub(first.len());

        out.extend_from_slice(b"\x1b_Ga=T,q=2,C=1,z=-1,f=24,i=");
        let _ = write!(out, "{id},p={PLACEMENT_ID},s={w},v={h}");
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
    ///
    /// The object holds exactly this tile and nothing else, so there are no `O=`/`S=`
    /// keys. The protocol has them, and one object per frame with an offset per tile
    /// would be five system calls a frame rather than five a tile -- but Ghostty draws
    /// nothing for a placement carrying them, and with `q=2` it does not say why. Until
    /// that is understood, a frame is an object per tile.
    pub fn place_shm(&mut self, out: &mut Vec<u8>, at: Placement, name: &str) {
        let Placement { id, col, row, w, h } = at;
        if w == 0 || h == 0 {
            return;
        }
        release_placement(out, id);
        let _ = write!(out, "\x1b[{};{}H", row as u32 + 1, col as u32 + 1);
        self.b64.clear();
        BASE64.encode_string(name.as_bytes(), &mut self.b64);
        let _ = write!(
            out,
            "\x1b_Ga=T,q=2,C=1,z=-1,f=24,t=s,i={id},p={PLACEMENT_ID},s={w},v={h};"
        );
        out.extend_from_slice(self.b64.as_bytes());
        out.extend_from_slice(b"\x1b\\");
    }
}

/// Let go of an image's current placement, keeping the image data.
///
/// Emitted immediately before every placement, which is not the tidiness it looks
/// like. Replacing a placement is meant to release whatever the old one held, and
/// Ghostty does not: the entry in its placement map is overwritten in place while
/// the tracked pin the old placement had taken in the screen is left behind. At a
/// placement per tile per frame those pins are thousands a second, so the delete
/// is what keeps a long session from slowing to a crawl.
///
/// `d=i` rather than `d=I`: the upper case form frees the image data too, and the
/// `a=T` that follows is about to replace it anyway.
fn release_placement(out: &mut Vec<u8>, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=i,i={id},p={PLACEMENT_ID},q=2\x1b\\");
}

/// Put an image the terminal already holds on different cells, without sending a pixel.
///
/// A layout can move the picture without changing it: centring an image in a window of
/// another width shifts every tile by a cell and leaves every pixel where it was. `a=p`
/// is the protocol's word for that -- display image `i` at the cursor, no payload -- and
/// the docs name replacing a placement as the way to "resize or move placements around
/// the screen without flicker".
///
/// No dimensions: the image has its own, from the transmission that created it, and the
/// natural size is the size it was drawn at before.
///
/// The release beforehand is the discipline every placement here follows, for the reason
/// [`release_placement`] gives.
pub fn place_existing(out: &mut Vec<u8>, id: u32, col: u16, row: u16) {
    release_placement(out, id);
    let _ = write!(out, "\x1b[{};{}H", row as u32 + 1, col as u32 + 1);
    let _ = write!(out, "\x1b_Ga=p,q=2,C=1,z=-1,i={id},p={PLACEMENT_ID}\x1b\\");
}

/// Fill a block of cells with one colour, as an image at the tiles' own z-index.
///
/// An overlay cannot be given a background with SGR: the remote screen is
/// composited above the cell background, so a colour set there is never seen and
/// only the glyphs come out on top. An image of our own, at the same `z=-1` but a
/// higher id than any tile, is drawn over them instead -- and the text still lands
/// on top of that.
///
/// The source is two pixels square, stretched across the block with `c`/`r`, so
/// the payload is four pixels however large the box is. `col` and `row` are
/// one-based, as the cursor is.
pub fn place_solid(
    out: &mut Vec<u8>,
    id: u32,
    col: usize,
    row: usize,
    cols: usize,
    rows: usize,
    rgb: (u8, u8, u8),
) {
    if cols == 0 || rows == 0 {
        return;
    }
    let pixels = [rgb.0, rgb.1, rgb.2].repeat(4);
    let mut b64 = String::new();
    BASE64.encode_string(&pixels, &mut b64);

    // The overlay is redrawn every frame it is up, so it accumulates placements
    // exactly as a tile would without this.
    release_placement(out, id);
    let _ = write!(out, "\x1b[{row};{col}H");
    let _ = write!(
        out,
        "\x1b_Ga=T,q=2,C=1,z=-1,f=24,i={id},p={PLACEMENT_ID},s=2,v=2,c={cols},r={rows};"
    );
    out.extend_from_slice(b64.as_bytes());
    out.extend_from_slice(b"\x1b\\");
}

/// Remove one image and free the data behind it.
pub fn delete_image(out: &mut Vec<u8>, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
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
    ///
    /// Commands carrying no payload are skipped, so the count is the number of
    /// transmissions and the placement-releasing deletes that precede each one do
    /// not show up as chunks.
    fn split(out: &[u8]) -> (String, String, usize) {
        let text = String::from_utf8(out.to_vec()).unwrap();
        let mut keys = String::new();
        let mut payload = String::new();
        let mut count = 0;
        for cmd in text.split("\x1b_G").skip(1) {
            let cmd = cmd.strip_suffix("\x1b\\").unwrap_or(cmd);
            let cmd = cmd.split("\x1b\\").next().unwrap();
            let Some((k, p)) = cmd.split_once(';') else {
                continue;
            };
            if count == 0 {
                keys = k.to_string();
            }
            payload.push_str(p);
            count += 1;
        }
        (keys, payload, count)
    }

    /// The graphics commands in a stream, as their key strings.
    fn commands(out: &[u8]) -> Vec<String> {
        let text = String::from_utf8(out.to_vec()).unwrap();
        text.split("\x1b_G")
            .skip(1)
            .map(|cmd| {
                let cmd = cmd.split("\x1b\\").next().unwrap();
                cmd.split(';').next().unwrap().to_string()
            })
            .collect()
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
        let text = String::from_utf8(out).unwrap();
        // Immediately before the transmission, wherever the release ahead of it
        // ends: a placement lands wherever the cursor happens to be.
        assert!(
            text.contains("\x1b[4;8H\x1b_Ga=T"),
            "{}",
            text.escape_debug()
        );
    }

    /// The leak this guards against: a placement command with no `p=` asks the
    /// terminal for an *additional* placement rather than a replacement, so every
    /// tile of every frame left one behind. Ghostty rebuilds its whole placement
    /// list each frame, so a session's cost grew with its age until it crawled.
    #[test]
    fn every_placement_pins_its_placement_id() {
        let mut enc = KittyEncoder::new(true);
        let mut out = Vec::new();
        enc.place_rgb(&mut out, place(IMAGE_ID_BASE, 0, 0, 2, 2), &[7; 2 * 2 * 3]);
        enc.place_shm(&mut out, place(IMAGE_ID_BASE + 1, 0, 0, 2, 2), "/vt1-2");
        place_solid(&mut out, OVERLAY_IMAGE_ID, 1, 1, 4, 2, (0, 0, 0));

        let transmits: Vec<_> = commands(&out)
            .into_iter()
            .filter(|keys| keys.contains("a=T"))
            .collect();
        assert_eq!(transmits.len(), 3, "{transmits:?}");
        for keys in transmits {
            assert!(
                keys.contains(&format!("p={PLACEMENT_ID}")),
                "a placement without p= accumulates in the terminal: {keys}"
            );
        }
    }

    /// And the other half of it: replacing a placement is supposed to release what
    /// the old one held, but Ghostty keeps the pin, so the release has to be asked
    /// for. Lower-case `d` leaves the image data for the transmission to replace.
    #[test]
    fn each_placement_releases_the_one_it_replaces() {
        let mut enc = KittyEncoder::new(false);
        for (label, out) in [
            ("rgb", {
                let mut out = Vec::new();
                enc.place_rgb(&mut out, place(IMAGE_ID_BASE + 5, 0, 0, 2, 2), &[0; 12]);
                out
            }),
            ("shm", {
                let mut out = Vec::new();
                enc.place_shm(&mut out, place(IMAGE_ID_BASE + 5, 0, 0, 2, 2), "/vt1-2");
                out
            }),
        ] {
            let cmds = commands(&out);
            let release = format!("a=d,d=i,i={},p={PLACEMENT_ID},q=2", IMAGE_ID_BASE + 5);
            assert_eq!(
                cmds.first().map(String::as_str),
                Some(release.as_str()),
                "{label}: {cmds:?}"
            );
            assert!(
                cmds[1].contains("a=T"),
                "{label}: the release must sit directly ahead of the placement: {cmds:?}"
            );
        }
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
        // but m and q. The release and the first chunk come ahead of them.
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("m=0").count(), 1);
        let continuations = commands(text.as_bytes());
        let continuations = &continuations[2..];
        assert_eq!(continuations.len(), 3);
        for keys in continuations {
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
