//! Throughput of the compose path, which is what decides whether a moving picture
//! is watchable.
//!
//! Ignored by default, because timings are not assertions. Run them when the
//! question comes up:
//!
//! ```text
//! cargo test --release --test perf -- --ignored --nocapture
//! ```
//!
//! Measured: everything between "the framebuffer changed" and "bytes are ready for
//! the terminal" -- packing BGRA to RGB, then either compress-and-base64 or a
//! shared memory object, tile by tile exactly as the renderer does it. Not
//! measured: the server's own encoding, our JPEG decode, or what the terminal does
//! with the result.

use std::ffi::CString;
use std::time::Instant;

/// A full-screen update at the size a 200x50 Ghostty window negotiates, tiled the
/// way the renderer tiles it: 16x8 cells of 8x17 pixels.
const WIDTH: u32 = 1600;
const HEIGHT: u32 = 832;
const TILE_W: u32 = 128;
const TILE_H: u32 = 136;
const ROUNDS: u32 = 30;

/// Something photographic: smooth gradients with detail, which is what video looks
/// like to a compressor. Flat colour would flatter these numbers enormously.
fn photo_like(w: u32, h: u32, phase: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let fx = x.wrapping_add(phase);
            let b = ((fx / 3) ^ (y / 5)) as u8;
            let g = (fx.wrapping_mul(2).wrapping_add(y) / 7) as u8;
            let r = ((fx.wrapping_add(y.wrapping_mul(3))) / 4) as u8;
            data.extend_from_slice(&[b, g, r, 0xff]);
        }
    }
    data
}

/// Every tile of the screen, as (x, y, w, h).
fn tiles() -> Vec<(u32, u32, u32, u32)> {
    let mut out = Vec::new();
    let mut y = 0;
    while y < HEIGHT {
        let h = TILE_H.min(HEIGHT - y);
        let mut x = 0;
        while x < WIDTH {
            let w = TILE_W.min(WIDTH - x);
            out.push((x, y, w, h));
            x += TILE_W;
        }
        y += TILE_H;
    }
    out
}

/// Pack one tile out of a BGRA frame into packed RGB, as `Framebuffer::pack_rgb`
/// does.
fn pack_tile(frame: &[u8], tile: (u32, u32, u32, u32), out: &mut Vec<u8>) {
    let (tx, ty, tw, th) = tile;
    out.clear();
    for row in 0..th {
        let start = (((ty + row) * WIDTH + tx) * 4) as usize;
        let end = start + (tw * 4) as usize;
        for px in frame[start..end].chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
}

/// Publish a tile into a shared memory object and unlink it, which is the whole
/// per-tile cost of the `t=s` medium: create, size, map, copy, unmap, close,
/// unlink.
fn publish_shm(data: &[u8], counter: u64) -> usize {
    let name = format!("/vtperf{counter:x}");
    let cname = CString::new(name).unwrap();
    // SAFETY: a valid NUL-terminated name; O_EXCL so a collision is reported.
    let fd = unsafe {
        libc::shm_open(
            cname.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600 as libc::c_uint,
        )
    };
    assert!(fd >= 0, "shm_open: {}", std::io::Error::last_os_error());
    // SAFETY: sizing and mapping an object we just created and own.
    unsafe {
        assert_eq!(libc::ftruncate(fd, data.len() as libc::off_t), 0);
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            data.len(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        assert_ne!(ptr, libc::MAP_FAILED);
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.cast::<u8>(), data.len());
        libc::munmap(ptr, data.len());
        libc::close(fd);
        // The terminal would do this; here nobody is reading, so we clean up.
        libc::shm_unlink(cname.as_ptr());
    }
    data.len()
}

/// Pack a tile straight into a slice, as `Framebuffer::pack_rgb_into` does.
fn pack_tile_into(frame: &[u8], tile: (u32, u32, u32, u32), out: &mut [u8]) {
    let (tx, ty, tw, th) = tile;
    let stride = (tw * 3) as usize;
    for (row, line) in out.chunks_exact_mut(stride).enumerate().take(th as usize) {
        let start = (((ty + row as u32) * WIDTH + tx) * 4) as usize;
        let end = start + (tw * 4) as usize;
        for (px, rgb) in frame[start..end]
            .chunks_exact(4)
            .zip(line.chunks_exact_mut(3))
        {
            rgb.copy_from_slice(&[px[2], px[1], px[0]]);
        }
    }
}

/// One object for a whole frame's tiles, packed straight into the mapping: the whole
/// per-*frame* cost of the `t=s` medium, against `publish_shm`'s per-tile one.
fn publish_shm_frame(
    frame: &[u8],
    tiles: &[(u32, u32, u32, u32)],
    counter: u64,
    len: usize,
) -> usize {
    let name = format!("/vtperff{counter:x}");
    let cname = CString::new(name).unwrap();
    // SAFETY: a valid NUL-terminated name; O_EXCL so a collision is reported.
    let fd = unsafe {
        libc::shm_open(
            cname.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600 as libc::c_uint,
        )
    };
    assert!(fd >= 0, "shm_open: {}", std::io::Error::last_os_error());
    // SAFETY: sizing and mapping an object we just created and own; the slice is of the
    // length we mapped and is written a disjoint tile at a time.
    unsafe {
        assert_eq!(libc::ftruncate(fd, len as libc::off_t), 0);
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        assert_ne!(ptr, libc::MAP_FAILED);
        let map = std::slice::from_raw_parts_mut(ptr.cast::<u8>(), len);
        let mut at = 0;
        for &tile in tiles {
            let bytes = (tile.2 * tile.3 * 3) as usize;
            pack_tile_into(frame, tile, &mut map[at..at + bytes]);
            at += bytes;
        }
        libc::munmap(ptr, len);
        libc::close(fd);
        // The terminal would do this; here nobody is reading, so we clean up.
        libc::shm_unlink(cname.as_ptr());
    }
    len
}

fn report(label: &str, elapsed: std::time::Duration, rounds: u32, bytes: usize) {
    let ms = (elapsed / rounds).as_secs_f64() * 1000.0;
    println!(
        "{label:<30} {ms:>7.2} ms/frame  {:>6.0} fps ceiling  {:>7.0} KB/frame",
        1000.0 / ms,
        bytes as f64 / rounds as f64 / 1024.0
    );
}

#[test]
#[ignore = "timing, not a pass/fail assertion"]
fn full_screen_compose_throughput() {
    use base64::Engine as _;
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;

    let tiles = tiles();
    println!();
    println!(
        "full-screen update: {WIDTH}x{HEIGHT} ({:.1} MP) in {} tiles, {ROUNDS} frames",
        (WIDTH * HEIGHT) as f64 / 1e6,
        tiles.len()
    );
    println!();

    let frames: Vec<Vec<u8>> = (0..ROUNDS)
        .map(|i| photo_like(WIDTH, HEIGHT, i * 7))
        .collect();
    let mut scratch = Vec::with_capacity((TILE_W * TILE_H * 3) as usize);

    // The pack every path pays for.
    let start = Instant::now();
    let mut total = 0;
    for frame in &frames {
        for &tile in &tiles {
            pack_tile(frame, tile, &mut scratch);
            total += scratch.len();
        }
    }
    report("pack BGRA->RGB", start.elapsed(), ROUNDS, total);

    // The direct medium: compress and base64 every tile.
    let start = Instant::now();
    let mut total = 0;
    for frame in &frames {
        for &tile in &tiles {
            pack_tile(frame, tile, &mut scratch);
            let mut compressed = Vec::with_capacity(scratch.len() / 2);
            let mut enc = ZlibEncoder::new(&mut compressed, Compression::new(1));
            enc.write_all(&scratch).unwrap();
            enc.finish().unwrap();
            total += base64::engine::general_purpose::STANDARD
                .encode(&compressed)
                .len();
        }
    }
    report("direct: +zlib +base64", start.elapsed(), ROUNDS, total);

    // The shared memory medium, the way it used to be done: an object per tile, packed
    // into a buffer and copied in. Kept for the comparison below.
    let start = Instant::now();
    let mut total = 0;
    let mut counter = 0u64;
    for frame in &frames {
        for &tile in &tiles {
            pack_tile(frame, tile, &mut scratch);
            counter += 1;
            total += publish_shm(&scratch, counter);
        }
    }
    report("shm: object per tile", start.elapsed(), ROUNDS, total);

    // And the way it is done: one object for the frame, every tile packed straight into
    // the mapping at an offset of its own. Five system calls a frame rather than five a
    // tile, and one pass over the pixels rather than a pack and a copy.
    let frame_bytes: usize = tiles.iter().map(|&(_, _, w, h)| (w * h * 3) as usize).sum();
    let start = Instant::now();
    let mut total = 0;
    for frame in &frames {
        counter += 1;
        total += publish_shm_frame(frame, &tiles, counter, frame_bytes);
    }
    report("shm: one object per frame", start.elapsed(), ROUNDS, total);

    println!();
    println!("Ceilings are this stage alone, single threaded, with every tile dirty.");
    println!(
        "The pack row appends into a buffer, which is what the direct medium still does;\n\
         one object per frame packs into the mapping instead, so it comes out below it."
    );
    println!("A partial update costs proportionally less. Server-side encoding, JPEG");
    println!("decode and the terminal's own work all come on top.");
    println!();
}
