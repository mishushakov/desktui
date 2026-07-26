//! Terminal capability probing.
//!
//! Everything here talks to the terminal by hand, at the raw fd level, and must
//! run *before* the crossterm event stream starts: crossterm would parse the
//! replies as input events and we would never see them.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

/// How long to wait for the terminal to answer. The replies come back in a
/// single round trip, so this only has to cover terminal latency.
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// A distinctive image id so a stray graphics response from another program
/// cannot be mistaken for ours.
const PROBE_IMAGE_ID: u32 = 0x765;

/// A second id, for the shared memory question, so the two answers are told apart.
const PROBE_SHM_ID: u32 = 0x766;

/// What the terminal told us it can do.
#[derive(Debug, Clone, Default)]
pub struct Caps {
    /// Answered the Kitty graphics query with `OK`.
    pub kitty_graphics: bool,
    /// Mode 1016, SGR-pixel mouse reporting: pointer position in pixels.
    pub pixel_mouse: bool,
    /// Mode 2026, synchronised output: a frame commits without tearing.
    pub sync_output: bool,
    /// The Kitty keyboard protocol, which is what turns key releases into
    /// events. RFB needs a release for every press, and without this we have to
    /// synthesise one immediately after each press instead.
    pub kitty_keyboard: bool,
    /// The `t=s` transmission medium: pixels handed over in shared memory rather
    /// than base64.
    ///
    /// Worth asking about rather than assuming, because frames are sent with
    /// responses suppressed: a terminal that cannot map the object would fail
    /// silently and simply show nothing.
    pub shm_graphics: bool,
    /// Text area size in pixels, from `CSI 14 t`.
    pub text_area_px: Option<(u32, u32)>,
    /// Cell size in pixels, from `CSI 16 t`.
    pub cell_px: Option<(u32, u32)>,
    /// Primary device attributes, for the record.
    pub da1: Option<String>,
}

/// Ask the terminal everything we want to know, in one round trip.
///
/// The primary-DA query goes last and acts as a sentinel: terminals answer
/// queries in order, so once its reply arrives every earlier reply has too, and
/// a terminal that ignores a query costs us nothing.
pub fn probe() -> Result<Caps> {
    let payload = base64_3_zero_bytes();
    let mut req = Vec::new();
    // Kitty graphics support: query action, so nothing is stored. No `q=2` here
    // -- suppressing responses would defeat the point.
    write!(
        req,
        "\x1b_Gi={PROBE_IMAGE_ID},s=1,v=1,a=q,t=d,f=24;{payload}\x1b\\"
    )?;
    req.extend_from_slice(b"\x1b[14t"); // text area in pixels
    req.extend_from_slice(b"\x1b[16t"); // cell size in pixels
    req.extend_from_slice(b"\x1b[?1016$p"); // DECRQM: SGR-pixel mouse
    req.extend_from_slice(b"\x1b[?2026$p"); // DECRQM: synchronised output
    req.extend_from_slice(b"\x1b[?u"); // kitty keyboard: report current flags

    // Ask whether pixels can arrive through shared memory. The query action
    // validates the transmission without storing anything, so this costs one
    // 3-byte object that the terminal unlinks on the way out.
    let mut pool = super::shm::ShmPool::new();
    let shm_name = pool.publish(&[0u8, 0, 0]).ok();
    if let Some(name) = &shm_name {
        let encoded = BASE64.encode(name.as_bytes());
        write!(
            req,
            "\x1b_Gi={PROBE_SHM_ID},s=1,v=1,a=q,t=s,f=24;{encoded}\x1b\\"
        )?;
    }

    req.extend_from_slice(b"\x1b[c"); // sentinel

    let reply = round_trip(&req)?;
    let caps = parse_caps(&reply);
    // The pool's own cleanup would unlink the object, but the terminal has
    // normally done that already.
    drop(pool);
    Ok(caps)
}

/// Ask only for the pixel geometry. Used when `TIOCGWINSZ` reports no pixel
/// size, which is common under tmux.
pub fn query_pixel_geometry() -> Result<(u32, u32)> {
    let reply = round_trip(b"\x1b[14t\x1b[c")?;
    let caps = parse_caps(&reply);
    caps.text_area_px
        .context("terminal did not answer the CSI 14 t pixel size query")
}

/// Write a request to the terminal and collect what comes back, stopping at the
/// primary-DA reply or the timeout, whichever comes first.
fn round_trip(request: &[u8]) -> Result<Vec<u8>> {
    let mut out = io::stdout();
    out.write_all(request)?;
    out.flush()?;
    read_until_da1(PROBE_TIMEOUT)
}

fn read_until_da1(timeout: Duration) -> Result<Vec<u8>> {
    let start = Instant::now();
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 1024];

    while let Some(remaining) = timeout.checked_sub(start.elapsed()) {
        let ms = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);

        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: a single initialised pollfd, count matches, fd is stdin.
        let ready = unsafe { libc::poll(&mut pfd, 1, ms) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("poll on stdin failed while probing the terminal");
        }
        if ready == 0 {
            break; // terminal stayed quiet
        }

        // SAFETY: writing into a local buffer, length bounded by its size.
        let got = unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
        if got <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..got as usize]);
        if sequences(&buf).iter().any(is_da1) {
            break;
        }
    }
    Ok(buf)
}

/// An escape sequence pulled out of the terminal's reply stream.
#[derive(Debug, PartialEq, Eq)]
enum Seq<'a> {
    /// `CSI` params and intermediates, plus the final byte.
    Csi { body: &'a [u8], final_byte: u8 },
    /// `APC` payload, without the introducer or the string terminator.
    Apc(&'a [u8]),
}

fn is_da1(seq: &Seq<'_>) -> bool {
    matches!(seq, Seq::Csi { body, final_byte: b'c' } if body.starts_with(b"?"))
}

/// Split a reply stream into the sequences we care about, ignoring anything else
/// (stray keystrokes typed during startup, for instance).
fn sequences(buf: &[u8]) -> Vec<Seq<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if buf[i] != 0x1b || i + 1 >= buf.len() {
            i += 1;
            continue;
        }
        match buf[i + 1] {
            b'[' => {
                let start = i + 2;
                let mut j = start;
                // Parameter and intermediate bytes, then a final byte.
                while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
                    j += 1;
                }
                if j < buf.len() {
                    out.push(Seq::Csi {
                        body: &buf[start..j],
                        final_byte: buf[j],
                    });
                    i = j + 1;
                } else {
                    break; // truncated
                }
            }
            b'_' => {
                let start = i + 2;
                let mut j = start;
                let mut end = None;
                while j < buf.len() {
                    if buf[j] == 0x9c {
                        end = Some((j, j + 1));
                        break;
                    }
                    if buf[j] == 0x1b && j + 1 < buf.len() && buf[j + 1] == b'\\' {
                        end = Some((j, j + 2));
                        break;
                    }
                    j += 1;
                }
                match end {
                    Some((payload_end, next)) => {
                        out.push(Seq::Apc(&buf[start..payload_end]));
                        i = next;
                    }
                    None => break, // truncated
                }
            }
            _ => i += 2,
        }
    }
    out
}

fn parse_caps(buf: &[u8]) -> Caps {
    let mut caps = Caps::default();

    for seq in sequences(buf) {
        match seq {
            Seq::Apc(payload) => {
                // `Gi=<id>;OK` -- the id says which question is being answered.
                let ok = payload.starts_with(b"G")
                    && payload.windows(3).any(|w| w == b"=OK" || w == b";OK");
                if ok && contains_id(payload, PROBE_IMAGE_ID) {
                    caps.kitty_graphics = true;
                }
                if ok && contains_id(payload, PROBE_SHM_ID) {
                    caps.shm_graphics = true;
                }
            }
            Seq::Csi { body, final_byte } => match final_byte {
                b'c' if body.starts_with(b"?") => {
                    caps.da1 = Some(String::from_utf8_lossy(&body[1..]).into_owned());
                }
                b't' => {
                    // `CSI 4 ; height ; width t` and `CSI 6 ; cell_h ; cell_w t`.
                    let p = params(body);
                    if let [kind, h, w] = p[..] {
                        match kind {
                            4 => caps.text_area_px = Some((w, h)),
                            6 => caps.cell_px = Some((w, h)),
                            _ => {}
                        }
                    }
                }
                // `CSI ? <flags> u`, the kitty keyboard protocol reporting its
                // current flags. Only a terminal that implements it answers.
                b'u' if body.starts_with(b"?") => {
                    caps.kitty_keyboard = true;
                }
                b'y' if body.starts_with(b"?") => {
                    // DECRPM: `CSI ? <mode> ; <value> $ y`. Value 0 means the
                    // mode is unknown and 4 means it is permanently reset;
                    // anything else means we can use it.
                    let p = params(&body[1..]);
                    if let [mode, value] = p[..] {
                        let usable = matches!(value, 1..=3);
                        match mode {
                            1016 => caps.pixel_mouse = usable,
                            2026 => caps.sync_output = usable,
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
        }
    }
    caps
}

/// Numeric parameters of a CSI body, stopping at the first intermediate byte
/// (`$` in a DECRPM reply, for instance).
fn params(body: &[u8]) -> Vec<u32> {
    body.split(|&b| b == b';')
        .map(|part| {
            part.iter()
                .take_while(|b| b.is_ascii_digit())
                .fold(0u32, |acc, b| {
                    acc.saturating_mul(10).saturating_add(u32::from(b - b'0'))
                })
        })
        .collect()
}

fn contains_id(payload: &[u8], id: u32) -> bool {
    let needle = format!("i={id}");
    payload
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// Base64 of three zero bytes: a 1x1 black RGB pixel.
fn base64_3_zero_bytes() -> &'static str {
    "AAAA"
}

#[cfg(test)]
mod probe_payload_tests {
    #[test]
    fn the_inline_probe_payload_is_really_base64_of_three_zeros() {
        use base64::Engine as _;
        assert_eq!(
            super::BASE64.encode([0u8, 0, 0]),
            super::base64_3_zero_bytes()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_mixed_replies() {
        let buf = b"\x1b_Gi=1893;OK\x1b\\\x1b[4;850;1600t\x1b[?1016;2$y\x1b[?62;c";
        let seqs = sequences(buf);
        assert_eq!(seqs.len(), 4);
        assert!(matches!(seqs[0], Seq::Apc(p) if p == b"Gi=1893;OK"));
        assert!(seqs.iter().any(is_da1));
    }

    #[test]
    fn reads_ghostty_style_replies() {
        let buf = b"\x1b_Gi=1893;OK\x1b\\\x1b[4;850;1600t\x1b[6;17;8t\
                    \x1b[?1016;2$y\x1b[?2026;2$y\x1b[?0u\x1b_Gi=1894;OK\x1b\\\x1b[?62;22c";
        let caps = parse_caps(buf);
        assert!(caps.kitty_graphics);
        assert!(caps.shm_graphics);
        assert!(caps.pixel_mouse);
        assert!(caps.sync_output);
        assert!(caps.kitty_keyboard);
        assert_eq!(caps.text_area_px, Some((1600, 850)));
        assert_eq!(caps.cell_px, Some((8, 17)));
        assert_eq!(caps.da1.as_deref(), Some("62;22"));
    }

    #[test]
    fn silence_about_the_keyboard_protocol_means_no_key_releases() {
        let caps = parse_caps(b"\x1b[?1016;2$y\x1b[?62;22c");
        assert!(!caps.kitty_keyboard);
    }

    #[test]
    fn a_terminal_without_graphics_answers_only_the_sentinel() {
        let caps = parse_caps(b"\x1b[?6c");
        assert!(!caps.kitty_graphics);
        assert!(!caps.pixel_mouse);
        assert!(caps.text_area_px.is_none());
    }

    #[test]
    fn unknown_or_locked_modes_are_not_usable() {
        // 0 = not recognised, 4 = permanently reset.
        let caps = parse_caps(b"\x1b[?1016;0$y\x1b[?2026;4$y\x1b[?6c");
        assert!(!caps.pixel_mouse);
        assert!(!caps.sync_output);
    }

    #[test]
    fn shared_memory_is_only_claimed_when_it_is_answered() {
        // A terminal that can draw but cannot map shared memory answers the first
        // question and not the second. Assuming otherwise would mean frames sent
        // with responses suppressed and nothing appearing.
        let caps = parse_caps(b"\x1b_Gi=1893;OK\x1b\\\x1b[?62;22c");
        assert!(caps.kitty_graphics);
        assert!(!caps.shm_graphics);
    }

    #[test]
    fn an_error_reply_does_not_count_as_support() {
        let caps = parse_caps(b"\x1b_Gi=1894;EBADF:no shm\x1b\\\x1b[?62;22c");
        assert!(!caps.shm_graphics);
    }

    #[test]
    fn ignores_a_graphics_reply_for_someone_elses_image() {
        let caps = parse_caps(b"\x1b_Gi=99;OK\x1b\\\x1b[?6c");
        assert!(!caps.kitty_graphics);
    }

    #[test]
    fn tolerates_keystrokes_mixed_into_the_reply() {
        let caps = parse_caps(b"q\x1b_Gi=1893;OK\x1b\\x\x1b[6;17;8t\x1b[?6c");
        assert!(caps.kitty_graphics);
        assert_eq!(caps.cell_px, Some((8, 17)));
    }

    #[test]
    fn truncated_sequences_do_not_panic() {
        assert!(sequences(b"\x1b[4;850").is_empty());
        assert!(sequences(b"\x1b_Gi=1").is_empty());
        assert!(sequences(b"\x1b").is_empty());
    }
}
