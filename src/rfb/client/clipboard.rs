//! The `ExtendedClipboard` pseudo-encoding: the clipboard, in UTF-8.
//!
//! Legacy `ClientCutText` and `ServerCutText` carry Latin-1 and nothing else, so
//! half the world's text cannot survive the trip -- Cyrillic, Greek, CJK and every
//! emoji come out as question marks. This extension replaces the payload of both
//! messages with UTF-8, and replaces the eager push with a handshake: the side whose
//! clipboard changed sends a *notify*, and the other side sends a *request* when it
//! actually wants the text.
//!
//! Both messages keep their type byte. What marks the new format is a *negative*
//! length, whose magnitude is the byte count that follows -- which is why reading
//! that length as unsigned is such a destructive bug, and why the decoder in
//! `messages.rs` reads it as `i32`.
//!
//! Only the *text* format is handled here. RTF, HTML, images and files are formats a
//! terminal has nowhere to put, so they are declined in our capabilities and skipped
//! if a server sends them anyway.
//!
//! See the `ExtendedClipboard` section of the RFB protocol document (TigerVNC's
//! `rfbproto.rst`), which is where this extension is specified.

use crate::rfb::{MAX_CUT_TEXT, VncError};
use std::io::{Read, Write};

/// Format and action bits of the flags word.
pub mod flag {
    /// Plain text: UTF-8, CRLF line endings, terminated by a null byte.
    pub const TEXT: u32 = 1 << 0;
    #[allow(dead_code)]
    pub const RTF: u32 = 1 << 1;
    #[allow(dead_code)]
    pub const HTML: u32 = 1 << 2;
    #[allow(dead_code)]
    pub const DIB: u32 = 1 << 3;
    #[allow(dead_code)]
    pub const FILES: u32 = 1 << 4;
    /// "Here is what I am willing to receive." Carries a size per format.
    pub const CAPS: u32 = 1 << 24;
    /// "Send me the formats named in these flags."
    pub const REQUEST: u32 = 1 << 25;
    /// "Tell me again which formats you have."
    pub const PEEK: u32 = 1 << 26;
    /// "My clipboard changed, and holds these formats." Carries no data.
    pub const NOTIFY: u32 = 1 << 27;
    /// "Here is the data", zlib compressed.
    pub const PROVIDE: u32 = 1 << 28;
    /// Bits 24-31 are actions. Exactly one may be set unless `CAPS` is.
    pub const ACTION_MASK: u32 = 0xff00_0000;
}

/// What this client is willing to receive, and what it can do.
///
/// Text only, and no `PEEK`: a peek asks for a fresh notify listing what we hold,
/// and what we hold is whatever the user last pasted -- which the server learns
/// from the notify we already send.
pub const OUR_CAPS: u32 = flag::TEXT | flag::CAPS | flag::REQUEST | flag::NOTIFY | flag::PROVIDE;

/// The unsolicited size we advertise for text, in bytes.
///
/// Zero, which the spec recommends: it tells the server never to push clipboard data
/// unasked, but to send a `notify` and wait for a `request`. Anything else leaves it
/// ambiguous whether an arriving message is a new clipboard or the rest of one that
/// was too large last time, and a size limit is a poor way to discover that.
pub const OUR_UNSOLICITED_SIZE: u32 = 0;

/// What the other side will accept, from its `caps` message.
///
/// The format and action bits of a `caps` message say what its sender is willing to
/// *receive*, so every question here is about what we are allowed to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    flags: u32,
    text_size: u32,
}

impl Caps {
    /// Whether text can be sent at all. Nothing else a terminal has fits.
    pub fn takes_text(&self) -> bool {
        self.flags & flag::TEXT != 0
    }

    /// Whether a `provide` carrying data is accepted.
    pub fn takes_provide(&self) -> bool {
        self.flags & flag::PROVIDE != 0
    }

    /// Whether a `notify` announcing what we hold is accepted.
    pub fn takes_notify(&self) -> bool {
        self.flags & flag::NOTIFY != 0
    }

    /// The most text, in bytes, that may be sent in a `provide` nobody asked for.
    ///
    /// Zero -- which is what the spec recommends both sides advertise -- means every
    /// transfer has to start with a `notify` and wait to be asked.
    pub fn unsolicited_text(&self) -> u32 {
        self.text_size
    }

    /// Caps as a server that takes everything would report them, with a ceiling on
    /// unsolicited text. Lets the backend's paste decisions be tested without a
    /// handshake to build them from.
    #[cfg(test)]
    pub fn taking_text_up_to(text_size: u32) -> Self {
        Self {
            flags: flag::TEXT | flag::PROVIDE | flag::NOTIFY,
            text_size,
        }
    }
}

/// A decoded extended clipboard message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// The peer's capabilities, and its unsolicited size limit for text.
    ///
    /// A server must send this in answer to a `SetEncodings` naming the
    /// pseudo-encoding, so receiving it is also how support is confirmed.
    Caps(Caps),
    /// The peer wants our clipboard.
    Request,
    /// The peer wants a fresh `notify` from us.
    Peek,
    /// The peer's clipboard changed. `text` is false when it went empty.
    Notify { text: bool },
    /// The peer's clipboard, as text.
    Provide(String),
    /// Well formed, but nothing this client acts on: an action we do not implement,
    /// or data in formats a terminal cannot hold.
    Ignored,
}

/// Decode the body of an extended message -- the flags word and everything after it.
///
/// The caller has already read the payload and checked its length against
/// [`MAX_CUT_TEXT`], because a length off the wire is not permission to allocate.
pub fn decode(payload: &[u8]) -> Result<Message, VncError> {
    if payload.len() < 4 {
        return Err(VncError::General(
            "extended clipboard message shorter than its flags word".into(),
        ));
    }
    let flags = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let body = &payload[4..];

    // Caps is tested first and on its own: it is the one action that may share the
    // flags word with format bits that mean something else -- there, they say what
    // the peer accepts rather than what it is sending.
    if flags & flag::CAPS != 0 {
        return decode_caps(flags, body);
    }

    match flags & flag::ACTION_MASK {
        flag::REQUEST => Ok(Message::Request),
        flag::PEEK => Ok(Message::Peek),
        flag::NOTIFY => Ok(Message::Notify {
            text: flags & flag::TEXT != 0,
        }),
        flag::PROVIDE => decode_provide(flags, body),
        // Either no action bit or several. Both are out of contract, and neither
        // leaves anything sensible to do with the payload -- but the stream is still
        // aligned, since the length told us where this message ends.
        other => {
            tracing::debug!("ignoring extended clipboard action {other:#x}");
            Ok(Message::Ignored)
        }
    }
}

/// `caps`: the flags word, then one `U32` size per format bit set in it.
fn decode_caps(flags: u32, body: &[u8]) -> Result<Message, VncError> {
    let mut text_size = 0;
    let mut rest = body;
    // Bits 0-15 are the formats, and there is one size for each one set, in
    // increasing bit order. Text is bit 0, but the walk still has to be a walk: the
    // sizes for formats we do not want are what tell us where ours is.
    for bit in 0..16 {
        let format = 1u32 << bit;
        if flags & format == 0 {
            continue;
        }
        let (size, tail) = rest.split_at_checked(4).ok_or_else(|| {
            VncError::General("extended clipboard caps ended mid-size".to_string())
        })?;
        if format == flag::TEXT {
            text_size = u32::from_be_bytes([size[0], size[1], size[2], size[3]]);
        }
        rest = tail;
    }
    Ok(Message::Caps(Caps { flags, text_size }))
}

/// `provide`: the flags word, then a zlib stream of `size`-and-`data` pairs, one per
/// format bit set, in increasing bit order.
fn decode_provide(flags: u32, body: &[u8]) -> Result<Message, VncError> {
    if flags & flag::TEXT == 0 {
        // Some other format, and the pairs we would have to walk past to find text
        // are inside the compressed stream. Nothing here is worth inflating.
        tracing::debug!("ignoring extended clipboard provide with formats {flags:#x}");
        return Ok(Message::Ignored);
    }

    // Inflate with a ceiling rather than to completion: the compressed payload was
    // bounded by the message length, but its expansion is not, and zlib will happily
    // turn a kilobyte into a gigabyte. Text is the only format we asked for, so the
    // cap that applies to it applies to the whole stream.
    let mut inflated = Vec::new();
    let limit = (MAX_CUT_TEXT as u64) + 4;
    flate2::read::ZlibDecoder::new(body)
        .take(limit)
        .read_to_end(&mut inflated)
        .map_err(|err| VncError::General(format!("extended clipboard was not zlib: {err}")))?;
    if inflated.len() as u64 > MAX_CUT_TEXT as u64 {
        return Err(VncError::General(format!(
            "extended clipboard inflated past the {MAX_CUT_TEXT}-byte cap"
        )));
    }

    // Text is bit 0, so it is the first pair in the stream whatever else is set.
    let (size, data) = inflated.split_at_checked(4).ok_or_else(|| {
        VncError::General("extended clipboard provide ended mid-size".to_string())
    })?;
    let size = u32::from_be_bytes([size[0], size[1], size[2], size[3]]) as usize;
    if size > data.len() {
        return Err(VncError::General(format!(
            "extended clipboard declared {size} bytes of text and sent {}",
            data.len()
        )));
    }
    Ok(Message::Provide(decode_text(&data[..size])))
}

/// Turn the wire form of text into what a local clipboard wants.
///
/// Three things to undo: the terminating null, which is counted in the size; CRLF
/// line endings, which the spec mandates and no terminal wants; and the possibility
/// that the sender's UTF-8 was not.
fn decode_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    // Lossy rather than an error: a server that sends one bad byte has still sent a
    // clipboard the user wants, and refusing the whole transfer -- or worse, the
    // session -- is a poor trade for a replacement character.
    String::from_utf8_lossy(bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Build a `caps` message body: what we accept, and how much of it unasked.
pub fn caps() -> Vec<u8> {
    let mut body = OUR_CAPS.to_be_bytes().to_vec();
    // One size per format bit in OUR_CAPS, in increasing bit order. Text is the only
    // one, so this is a single entry.
    body.extend_from_slice(&OUR_UNSOLICITED_SIZE.to_be_bytes());
    body
}

/// Build a `request` message body: send us the text.
pub fn request() -> Vec<u8> {
    (flag::REQUEST | flag::TEXT).to_be_bytes().to_vec()
}

/// Build a `notify` message body: our clipboard changed, and holds text.
pub fn notify() -> Vec<u8> {
    (flag::NOTIFY | flag::TEXT).to_be_bytes().to_vec()
}

/// Build a `provide` message body carrying `text`.
pub fn provide(text: &str) -> Vec<u8> {
    // CRLF and a terminating null, both of which the spec asks for, and the null is
    // counted in the size.
    let mut payload = text.replace('\n', "\r\n").into_bytes();
    payload.push(0);

    let mut deflated = Vec::new();
    {
        // Level 1: this is text on its way to a clipboard, where the difference
        // between compression levels is measured in bytes and the time is not.
        let mut encoder =
            flate2::write::ZlibEncoder::new(&mut deflated, flate2::Compression::new(1));
        // A Vec never fails to be written to, and flate2 only surfaces the writer's
        // errors, so neither of these can fail in practice.
        let _ = encoder.write_all(&(payload.len() as u32).to_be_bytes());
        let _ = encoder.write_all(&payload);
        let _ = encoder.finish();
    }

    let mut body = (flag::PROVIDE | flag::TEXT).to_be_bytes().to_vec();
    body.extend_from_slice(&deflated);
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire form of a `provide` body, built the way a server would.
    fn provided(flags: u32, pairs: &[&[u8]]) -> Vec<u8> {
        let mut plain = Vec::new();
        for data in pairs {
            plain.extend_from_slice(&(data.len() as u32).to_be_bytes());
            plain.extend_from_slice(data);
        }
        let mut deflated = Vec::new();
        {
            let mut encoder =
                flate2::write::ZlibEncoder::new(&mut deflated, flate2::Compression::new(6));
            encoder.write_all(&plain).unwrap();
            encoder.finish().unwrap();
        }
        let mut body = flags.to_be_bytes().to_vec();
        body.extend_from_slice(&deflated);
        body
    }

    #[test]
    fn our_caps_body_carries_one_size_per_format_bit() {
        // Text is the only format we accept, so: the flags word, then one size.
        assert_eq!(
            caps(),
            vec![
                0x1b, 0x00, 0x00, 0x01, // provide | notify | request | caps | text
                0x00, 0x00, 0x00, 0x00, // and no unsolicited text, so notify first
            ]
        );
    }

    #[test]
    fn request_and_notify_are_flags_and_nothing_else() {
        assert_eq!(request(), vec![0x02, 0x00, 0x00, 0x01]);
        assert_eq!(notify(), vec![0x08, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn cyrillic_survives_a_round_trip() {
        // The whole point of the extension: Latin-1 cut text turns every one of these
        // into a question mark.
        let text = "Привет, мир";
        let Message::Provide(back) = decode(&provide(text)).unwrap() else {
            panic!("a provide did not decode as one");
        };
        assert_eq!(back, text);
    }

    #[test]
    fn a_provide_is_null_terminated_and_crlf_and_the_null_is_counted() {
        // Both are required by the spec, and the size includes the null.
        let body = provide("a\nb");
        let mut inflated = Vec::new();
        flate2::read::ZlibDecoder::new(&body[4..])
            .read_to_end(&mut inflated)
            .unwrap();
        assert_eq!(inflated, vec![0, 0, 0, 5, b'a', b'\r', b'\n', b'b', 0]);
    }

    #[test]
    fn line_endings_come_back_as_newlines() {
        // CRLF is what the wire uses and a lone CR is what a careless sender uses;
        // neither belongs in a terminal's clipboard.
        let body = provided(flag::PROVIDE | flag::TEXT, &[b"one\r\ntwo\rthree\0"]);
        assert_eq!(
            decode(&body).unwrap(),
            Message::Provide("one\ntwo\nthree".to_string())
        );
    }

    #[test]
    fn text_that_is_not_utf8_is_substituted_rather_than_refused() {
        // A bad byte should cost that byte, not the transfer: the rest of the
        // clipboard is still what the user asked for.
        let body = provided(flag::PROVIDE | flag::TEXT, &[b"ok \xff\0"]);
        assert_eq!(
            decode(&body).unwrap(),
            Message::Provide("ok \u{fffd}".to_string())
        );
    }

    #[test]
    fn caps_reports_the_text_size_from_the_right_slot() {
        // Sizes arrive in increasing bit order with one entry per format bit, so
        // reading the first word regardless would pick up RTF's size here.
        let mut body = (flag::CAPS | flag::RTF | flag::TEXT | flag::PROVIDE)
            .to_be_bytes()
            .to_vec();
        body.extend_from_slice(&20_000u32.to_be_bytes()); // text, bit 0
        body.extend_from_slice(&99u32.to_be_bytes()); // rtf, bit 1
        let Message::Caps(caps) = decode(&body).unwrap() else {
            panic!("caps did not decode as caps");
        };
        assert_eq!(caps.unsolicited_text(), 20_000);
        assert!(caps.takes_text() && caps.takes_provide());
        // Notify was not offered, so announcing a clipboard would be out of contract.
        assert!(!caps.takes_notify());

        // And with text absent there is no size to find, whatever else is listed.
        let mut body = (flag::CAPS | flag::RTF).to_be_bytes().to_vec();
        body.extend_from_slice(&99u32.to_be_bytes());
        let Message::Caps(caps) = decode(&body).unwrap() else {
            panic!("caps did not decode as caps");
        };
        assert!(!caps.takes_text());
        assert_eq!(caps.unsolicited_text(), 0);
    }

    #[test]
    fn the_actions_that_carry_nothing_decode_from_flags_alone() {
        assert_eq!(
            decode(&(flag::REQUEST | flag::TEXT).to_be_bytes()).unwrap(),
            Message::Request
        );
        assert_eq!(decode(&flag::PEEK.to_be_bytes()).unwrap(), Message::Peek);
        assert_eq!(
            decode(&(flag::NOTIFY | flag::TEXT).to_be_bytes()).unwrap(),
            Message::Notify { text: true }
        );
        // An empty clipboard is a notify with no format bits, not a missing message.
        assert_eq!(
            decode(&flag::NOTIFY.to_be_bytes()).unwrap(),
            Message::Notify { text: false }
        );
    }

    #[test]
    fn formats_a_terminal_cannot_hold_are_ignored_not_refused() {
        // An image on the clipboard is not an error, it is just not for us -- and the
        // length has already told the reader where the message ends, so ignoring it
        // leaves the stream aligned.
        let body = provided(flag::PROVIDE | flag::DIB, &[b"\x89PNG"]);
        assert_eq!(decode(&body).unwrap(), Message::Ignored);
        // Same for an action we never advertised.
        assert_eq!(
            decode(&(1u32 << 29).to_be_bytes()).unwrap(),
            Message::Ignored
        );
        // And for a message with no action bit at all.
        assert_eq!(decode(&flag::TEXT.to_be_bytes()).unwrap(), Message::Ignored);
    }

    #[test]
    fn a_truncated_message_is_refused_rather_than_guessed_at() {
        assert!(decode(&[0, 0, 0]).is_err());
        // Caps that promises two sizes and sends one.
        let mut body = (flag::CAPS | flag::TEXT | flag::RTF).to_be_bytes().to_vec();
        body.extend_from_slice(&1u32.to_be_bytes());
        assert!(decode(&body).is_err());
    }

    #[test]
    fn a_provide_that_is_not_zlib_is_refused() {
        let mut body = (flag::PROVIDE | flag::TEXT).to_be_bytes().to_vec();
        body.extend_from_slice(b"not a zlib stream at all");
        assert!(decode(&body).is_err());
    }

    #[test]
    fn a_provide_longer_than_its_declared_size_is_refused() {
        // The size is the client's only bound on the text inside the stream, so a
        // size past the end of the data cannot be trusted to be a rounding error.
        let mut plain = 64u32.to_be_bytes().to_vec();
        plain.extend_from_slice(b"four");
        let mut deflated = Vec::new();
        {
            let mut encoder =
                flate2::write::ZlibEncoder::new(&mut deflated, flate2::Compression::new(6));
            encoder.write_all(&plain).unwrap();
            encoder.finish().unwrap();
        }
        let mut body = (flag::PROVIDE | flag::TEXT).to_be_bytes().to_vec();
        body.extend_from_slice(&deflated);
        assert!(decode(&body).is_err());
    }

    #[test]
    fn a_payload_that_inflates_past_the_cap_is_refused_before_it_is_kept() {
        // A zip bomb is cheap to send and expensive to hold: a megabyte of zeroes
        // compresses to about a kilobyte, and the message length -- which is all the
        // reader checked -- says nothing about the size after inflation.
        let bomb = vec![0u8; MAX_CUT_TEXT + 4096];
        let mut deflated = Vec::new();
        {
            let mut encoder =
                flate2::write::ZlibEncoder::new(&mut deflated, flate2::Compression::new(9));
            encoder.write_all(&bomb).unwrap();
            encoder.finish().unwrap();
        }
        assert!(
            deflated.len() < 8192,
            "the fixture is meant to be small compressed, was {}",
            deflated.len()
        );
        let mut body = (flag::PROVIDE | flag::TEXT).to_be_bytes().to_vec();
        body.extend_from_slice(&deflated);
        assert!(decode(&body).is_err());
    }
}
