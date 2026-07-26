use super::clipboard;
use crate::remote::{Rect, ScreenInfo};
use crate::rfb::{MAX_CUT_TEXT, MAX_PAYLOAD, PixelFormat, VncError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug)]
pub(super) enum ClientMsg {
    SetPixelFormat(PixelFormat),

    FramebufferUpdateRequest(Rect, u8),
    KeyEvent(u32, bool),
    PointerEvent(u16, u16, u8),
    ClientCutText(String),
    /// A `ClientCutText` in the extended form: the body from [`clipboard`], sent
    /// under a negative length. Only legal once the server has answered our
    /// `SetEncodings` with a `caps` message.
    ExtendedCutText(Vec<u8>),
    SetDesktopSize {
        width: u16,
        height: u16,
        screens: Vec<ScreenInfo>,
    },
    /// Raw encoding numbers appended to `SetEncodings`, for hints that are a number
    /// and nothing else: quality and compression levels.
    SetEncodingsRaw(Vec<i32>),
    EnableContinuousUpdates {
        enable: bool,
        rect: Rect,
    },
    Fence {
        flags: u32,
        payload: Vec<u8>,
    },
}

impl ClientMsg {
    pub(super) async fn write<S>(self, writer: &mut S) -> Result<(), VncError>
    where
        S: AsyncWrite + Unpin,
    {
        match self {
            ClientMsg::SetPixelFormat(pf) => {
                // +--------------+--------------+--------------+
                // | No. of bytes | Type [Value] | Description  |
                // +--------------+--------------+--------------+
                // | 1            | U8 [0]       | message-type |
                // | 3            |              | padding      |
                // | 16           | PIXEL_FORMAT | pixel-format |
                // +--------------+--------------+--------------+
                let mut payload = vec![0_u8, 0, 0, 0];
                payload.extend(<PixelFormat as Into<Vec<u8>>>::into(pf));
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::SetEncodingsRaw(ids) => {
                // Same message as SetEncodings, but the caller has already reduced
                // everything to numbers -- which is all a quality or compression hint
                // is.
                let mut payload = vec![2, 0];
                payload.extend_from_slice(&(ids.len() as u16).to_be_bytes());
                for id in ids {
                    payload.extend_from_slice(&id.to_be_bytes());
                }
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::EnableContinuousUpdates { enable, rect } => {
                // +--------------+--------------+--------------+
                // | 1            | U8 [150]     | message-type |
                // | 1            | U8           | enable-flag  |
                // | 2            | U16          | x-position   |
                // | 2            | U16          | y-position   |
                // | 2            | U16          | width        |
                // | 2            | U16          | height       |
                // +--------------+--------------+--------------+
                let mut payload = vec![150_u8, enable as u8];
                payload.extend_from_slice(&rect.x.to_be_bytes());
                payload.extend_from_slice(&rect.y.to_be_bytes());
                payload.extend_from_slice(&rect.width.to_be_bytes());
                payload.extend_from_slice(&rect.height.to_be_bytes());
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::Fence {
                flags,
                payload: body,
            } => {
                // +--------------+--------------+--------------+
                // | 1            | U8 [248]     | message-type |
                // | 3            |              | padding      |
                // | 4            | U32          | flags        |
                // | 1            | U8           | length       |
                // | length       | U8 array     | payload      |
                // +--------------+--------------+--------------+
                //
                // The payload is capped at 64 bytes by the spec, and echoing more
                // than we were sent would be a protocol error of our own making.
                let body = if body.len() > 64 {
                    &body[..64]
                } else {
                    &body[..]
                };
                let mut payload = vec![248_u8, 0, 0, 0];
                payload.extend_from_slice(&flags.to_be_bytes());
                payload.push(body.len() as u8);
                payload.extend_from_slice(body);
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::FramebufferUpdateRequest(rect, incremental) => {
                // +--------------+--------------+--------------+
                // | No. of bytes | Type [Value] | Description  |
                // +--------------+--------------+--------------+
                // | 1            | U8 [3]       | message-type |
                // | 1            | U8           | incremental  |
                // | 2            | U16          | x-position   |
                // | 2            | U16          | y-position   |
                // | 2            | U16          | width        |
                // | 2            | U16          | height       |
                // +--------------+--------------+--------------+
                let mut payload = vec![3, incremental];
                payload.extend_from_slice(&rect.x.to_be_bytes());
                payload.extend_from_slice(&rect.y.to_be_bytes());
                payload.extend_from_slice(&rect.width.to_be_bytes());
                payload.extend_from_slice(&rect.height.to_be_bytes());
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::KeyEvent(keycode, down) => {
                // +--------------+--------------+--------------+
                // | No. of bytes | Type [Value] | Description  |
                // +--------------+--------------+--------------+
                // | 1            | U8 [4]       | message-type |
                // | 1            | U8           | down-flag    |
                // | 2            |              | padding      |
                // | 4            | U32          | key          |
                // +--------------+--------------+--------------+
                let mut payload = vec![4, down as u8, 0, 0];
                payload.write_u32(keycode).await?;
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::PointerEvent(x, y, mask) => {
                // +--------------+--------------+--------------+
                // | No. of bytes | Type [Value] | Description  |
                // +--------------+--------------+--------------+
                // | 1            | U8 [5]       | message-type |
                // | 1            | U8           | button-mask  |
                // | 2            | U16          | x-position   |
                // | 2            | U16          | y-position   |
                // +--------------+--------------+--------------+
                let mut payload = vec![5, mask];
                payload.write_u16(x).await?;
                payload.write_u16(y).await?;
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::ClientCutText(s) => {
                //   +--------------+--------------+--------------+
                //   | No. of bytes | Type [Value] | Description  |
                //   +--------------+--------------+--------------+
                //   | 1            | U8 [6]       | message-type |
                //   | 3            |              | padding      |
                //   | 4            | U32          | length       |
                //   | length       | U8 array     | text         |
                //   +--------------+--------------+--------------+
                // The text is Latin-1: one byte per character, not UTF-8. Writing
                // `as_bytes()` sends U+0080..U+00FF as their two-byte UTF-8 form,
                // which a server reading Latin-1 turns "café" into "cafÃ©" -- and
                // the length no longer matches the character count either. Anything
                // above U+00FF cannot be sent at all; the caller substitutes it
                // already, and doing so here too keeps the wire well formed for
                // callers that did not rather than truncating into a stray byte.
                let latin1: Vec<u8> = s
                    .chars()
                    .map(|c| if (c as u32) > 0xff { b'?' } else { c as u8 })
                    .collect();
                let mut payload = vec![6_u8, 0, 0, 0];
                payload.write_u32(latin1.len() as u32).await?;
                payload.write_all(&latin1).await?;
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::ExtendedCutText(body) => {
                // The same message, with the length negated to say the payload is the
                // extended form rather than Latin-1 text. The magnitude is the whole
                // body including its flags word, so it is just the body's length.
                // Negating a length that does not fit in an `i32` would send a
                // *positive* one and hand the server our zlib stream as if it were
                // Latin-1 text. Nothing we build comes close, so this is a guard, not
                // a case to handle.
                let len = i32::try_from(body.len()).map_err(|_| {
                    VncError::General(format!("extended clipboard body of {} bytes", body.len()))
                })?;
                let mut payload = vec![6_u8, 0, 0, 0];
                payload.write_i32(-len).await?;
                payload.write_all(&body).await?;
                writer.write_all(&payload).await?;
                Ok(())
            }
            ClientMsg::SetDesktopSize {
                width,
                height,
                screens,
            } => {
                // RFB community extension, message type 251:
                //
                //   +--------------------------+----------+--------------------+
                //   | No. of bytes             | Type     | Description        |
                //   +--------------------------+----------+--------------------+
                //   | 1                        | U8 [251] | message-type       |
                //   | 1                        |          | padding            |
                //   | 2                        | U16      | width              |
                //   | 2                        | U16      | height             |
                //   | 1                        | U8       | number-of-screens  |
                //   | 1                        |          | padding            |
                //   | number-of-screens * 16   | SCREEN[] | screens            |
                //   +--------------------------+----------+--------------------+
                //
                // Only legal after an ExtendedDesktopSize rectangle has been
                // received; the caller is responsible for that, and for carrying
                // over the screen ids the server gave us.
                let count = u8::try_from(screens.len()).map_err(|_| {
                    VncError::General("too many screens in a SetDesktopSize request".into())
                })?;
                let mut payload = vec![251_u8, 0];
                payload.extend_from_slice(&width.to_be_bytes());
                payload.extend_from_slice(&height.to_be_bytes());
                payload.push(count);
                payload.push(0);
                for screen in &screens {
                    screen.encode(&mut payload);
                }
                writer.write_all(&payload).await?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod client_msg_tests {
    use super::*;

    async fn encode(msg: ClientMsg) -> Vec<u8> {
        let mut out = Vec::new();
        msg.write(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn set_desktop_size_matches_the_wire_format() {
        // RFC community extension, message 251: type, padding, width, height,
        // screen count, padding, then 16 bytes per screen.
        let bytes = encode(ClientMsg::SetDesktopSize {
            width: 1600,
            height: 833,
            screens: vec![ScreenInfo {
                id: 0x0000_0001,
                x: 0,
                y: 0,
                width: 1600,
                height: 833,
                flags: 0,
            }],
        })
        .await;

        assert_eq!(
            bytes,
            vec![
                251, // message-type
                0,   // padding
                0x06, 0x40, // width  = 1600
                0x03, 0x41, // height = 833
                1,    // number-of-screens
                0,    // padding
                0x00, 0x00, 0x00, 0x01, // screen id
                0x00, 0x00, // screen x
                0x00, 0x00, // screen y
                0x06, 0x40, // screen width
                0x03, 0x41, // screen height
                0x00, 0x00, 0x00, 0x00, // screen flags
            ]
        );
        assert_eq!(bytes.len(), 8 + ScreenInfo::WIRE_LEN);
    }

    #[tokio::test]
    async fn set_desktop_size_preserves_screen_ids_and_flags() {
        // The id is how a server tells a moved screen from a new one, and unknown
        // flag bits have to survive the round trip.
        let bytes = encode(ClientMsg::SetDesktopSize {
            width: 800,
            height: 600,
            screens: vec![
                ScreenInfo {
                    id: 0xdead_beef,
                    x: 0,
                    y: 0,
                    width: 400,
                    height: 600,
                    flags: 0xa5a5_a5a5,
                },
                ScreenInfo {
                    id: 0x0bad_c0de,
                    x: 400,
                    y: 0,
                    width: 400,
                    height: 600,
                    flags: 0,
                },
            ],
        })
        .await;

        assert_eq!(bytes[6], 2, "screen count");
        assert_eq!(&bytes[8..12], &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(&bytes[20..24], &[0xa5, 0xa5, 0xa5, 0xa5]);
        assert_eq!(&bytes[24..28], &[0x0b, 0xad, 0xc0, 0xde]);
    }

    #[tokio::test]
    async fn a_framebuffer_update_request_carries_the_incremental_flag() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 1600,
            height: 833,
        };
        let incremental = encode(ClientMsg::FramebufferUpdateRequest(rect, 1)).await;
        assert_eq!(incremental[0..2], [3, 1]);
        let full = encode(ClientMsg::FramebufferUpdateRequest(rect, 0)).await;
        assert_eq!(full[0..2], [3, 0]);
        assert_eq!(&full[2..10], &[0, 0, 0, 0, 0x06, 0x40, 0x03, 0x41]);
    }

    #[tokio::test]
    async fn enable_continuous_updates_matches_the_wire_format() {
        let bytes = encode(ClientMsg::EnableContinuousUpdates {
            enable: true,
            rect: Rect {
                x: 0,
                y: 0,
                width: 1600,
                height: 832,
            },
        })
        .await;
        assert_eq!(
            bytes,
            vec![
                150, // message-type
                1,   // enable
                0x00, 0x00, // x
                0x00, 0x00, // y
                0x06, 0x40, // width  = 1600
                0x03, 0x40, // height = 832
            ]
        );

        let off = encode(ClientMsg::EnableContinuousUpdates {
            enable: false,
            rect: Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
        })
        .await;
        assert_eq!(off[1], 0, "the enable flag has to clear");
    }

    #[tokio::test]
    async fn a_fence_response_matches_the_wire_format() {
        let bytes = encode(ClientMsg::Fence {
            flags: 0b11,
            payload: b"sync".to_vec(),
        })
        .await;
        assert_eq!(
            bytes,
            vec![
                248, // message-type
                0, 0, 0, // padding
                0x00, 0x00, 0x00, 0x03, // flags
                4,    // length
                b's', b'y', b'n', b'c',
            ]
        );
    }

    #[tokio::test]
    async fn an_oversized_fence_payload_is_clipped_to_the_limit() {
        // The spec caps a fence payload at 64 bytes; echoing more than that back would
        // be a protocol error of our own making.
        let bytes = encode(ClientMsg::Fence {
            flags: 0,
            payload: vec![0xab; 200],
        })
        .await;
        assert_eq!(bytes[8], 64, "declared length");
        assert_eq!(bytes.len(), 9 + 64);
    }

    #[tokio::test]
    async fn the_hint_encodings_go_out_with_the_rest() {
        // Quality and compression are encoding numbers with no rectangle behind them,
        // and they have to share the one SetEncodings message: a second would replace
        // the first rather than adding to it.
        let bytes = encode(ClientMsg::SetEncodingsRaw(vec![7, 16, -23, -254])).await;
        assert_eq!(bytes[0], 2, "message-type");
        assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 4, "count");
        let ids: Vec<i32> = bytes[4..]
            .chunks_exact(4)
            .map(|c| i32::from_be_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(ids, vec![7, 16, -23, -254]);
    }

    #[tokio::test]
    async fn key_and_pointer_events_match_the_spec() {
        let key = encode(ClientMsg::KeyEvent(0xffe1, true)).await;
        assert_eq!(key, vec![4, 1, 0, 0, 0x00, 0x00, 0xff, 0xe1]);
        let up = encode(ClientMsg::KeyEvent(0x61, false)).await;
        assert_eq!(up, vec![4, 0, 0, 0, 0x00, 0x00, 0x00, 0x61]);
        let pointer = encode(ClientMsg::PointerEvent(1599, 832, 0b101)).await;
        assert_eq!(pointer, vec![5, 0b101, 0x06, 0x3f, 0x03, 0x40]);
    }

    #[tokio::test]
    async fn cut_text_goes_out_as_latin1_not_utf8() {
        // The e-acute is one byte, 0xe9 -- not the 0xc3 0xa9 that UTF-8 would make of
        // it, which a server decoding Latin-1 would show as two characters.
        let bytes = encode(ClientMsg::ClientCutText("caf\u{e9}".into())).await;
        assert_eq!(bytes, vec![6, 0, 0, 0, 0, 0, 0, 4, b'c', b'a', b'f', 0xe9]);
    }

    #[tokio::test]
    async fn an_extended_cut_text_goes_out_under_a_negative_length() {
        // Same message type as the Latin-1 form; the sign of the length is the only
        // thing telling the server which one it is reading.
        let bytes = encode(ClientMsg::ExtendedCutText(vec![1, 2, 3, 4, 5])).await;
        assert_eq!(bytes[..4], [6, 0, 0, 0]);
        assert_eq!(i32::from_be_bytes(bytes[4..8].try_into().unwrap()), -5);
        assert_eq!(bytes[8..], [1, 2, 3, 4, 5]);
    }

    #[tokio::test]
    async fn cut_text_outside_latin1_is_substituted_rather_than_truncated() {
        // Callers substitute before they get here so they can report how much they
        // changed; this is the floor under them. Truncating `c as u8` would send the
        // coffee cup as 0x15, an unrelated control character.
        let bytes = encode(ClientMsg::ClientCutText("\u{2615}".into())).await;
        assert_eq!(bytes, vec![6, 0, 0, 0, 0, 0, 0, 1, b'?']);
    }
}

#[derive(Debug)]
pub(super) enum ServerMsg {
    FramebufferUpdate(u16),
    // SetColorMapEntries,
    Bell,
    ServerCutText(String),
    /// A `ServerCutText` in the extended form, marked by a negative length.
    ExtendedClipboard(clipboard::Message),
    /// The server has stopped pushing updates. Also its way of saying the extension
    /// exists, the first time it arrives.
    EndOfContinuousUpdates,
    ServerFence {
        flags: u32,
        payload: Vec<u8>,
    },
}

impl ServerMsg {
    pub(super) async fn read<S>(reader: &mut S) -> Result<Self, VncError>
    where
        S: AsyncRead + Unpin,
    {
        let server_msg = reader.read_u8().await?;

        match server_msg {
            0 => {
                // FramebufferUpdate
                //   +--------------+--------------+----------------------+
                //   | No. of bytes | Type [Value] | Description          |
                //   +--------------+--------------+----------------------+
                //   | 1            | U8 [0]       | message-type         |
                //   | 1            |              | padding              |
                //   | 2            | U16          | number-of-rectangles |
                //   +--------------+--------------+----------------------+
                let _padding = reader.read_u8().await?;
                let rects = reader.read_u16().await?;
                Ok(ServerMsg::FramebufferUpdate(rects))
            }
            1 => {
                // SetColorMapEntries
                // +--------------+--------------+------------------+
                // | No. of bytes | Type [Value] | Description      |
                // +--------------+--------------+------------------+
                // | 1            | U8 [1]       | message-type     |
                // | 1            |              | padding          |
                // | 2            | U16          | first-color      |
                // | 2            | U16          | number-of-colors |
                // +--------------+--------------+------------------+
                //
                // We always ask for a true-colour pixel format, so a colour map
                // means the server ignored that and nothing that follows can be
                // interpreted. Upstream had `unimplemented!()` here, which let a
                // server panic the client; drain the message and report instead.
                let mut padding = [0; 1];
                reader.read_exact(&mut padding).await?;
                let _first_colour = reader.read_u16().await?;
                let colours = reader.read_u16().await?;
                let mut sink = vec![0; usize::from(colours) * 6];
                reader.read_exact(&mut sink).await?;
                Err(VncError::General(
                    "server sent a colour map, but a true-colour format was negotiated".into(),
                ))
            }
            2 => {
                // Bell
                //   +--------------+--------------+--------------+
                //   | No. of bytes | Type [Value] | Description  |
                //   +--------------+--------------+--------------+
                //   | 1            | U8 [2]       | message-type |
                //   +--------------+--------------+--------------+
                Ok(ServerMsg::Bell)
            }
            3 => {
                // ServerCutText
                // +--------------+--------------+--------------+
                // | No. of bytes | Type [Value] | Description  |
                // +--------------+--------------+--------------+
                // | 1            | U8 [3]       | message-type |
                // | 3            |              | padding      |
                // | 4            | U32          | length       |
                // | length       | U8 array     | text         |
                // +--------------+--------------+--------------+
                let mut padding = [0; 3];
                reader.read_exact(&mut padding).await?;

                // The length is *signed*: a negative one means the extended
                // clipboard extension, whose payload is a different shape
                // entirely, and whose magnitude is the byte count. Reading it as
                // unsigned -- which is the obvious mistake, and the one upstream
                // made -- turns -8 into four billion and then spends the rest of
                // the session trying to skip that many bytes.
                let len = reader.read_i32().await?;
                if len < 0 {
                    return read_extended_clipboard(reader, len.unsigned_abs() as usize).await;
                }
                let len = len as usize;
                if len > MAX_PAYLOAD {
                    // Skipping this would mean reading tens of megabytes to get
                    // back to a message boundary; refusing says so immediately.
                    return Err(VncError::General(format!(
                        "server declared a {len}-byte clipboard payload"
                    )));
                }
                // A length is not permission to allocate. Read past the excess so
                // the stream stays usable, and keep what could plausibly be text.
                let keep = len.min(MAX_CUT_TEXT);
                let mut buffer_str = vec![0; keep];
                reader.read_exact(&mut buffer_str).await?;
                let mut discard = len - keep;
                let mut sink = [0u8; 4096];
                while discard > 0 {
                    let n = discard.min(sink.len());
                    reader.read_exact(&mut sink[..n]).await?;
                    discard -= n;
                }
                Ok(Self::ServerCutText(
                    String::from_utf8_lossy(&buffer_str).to_string(),
                ))
            }
            150 => {
                // EndOfContinuousUpdates carries nothing but its type.
                Ok(ServerMsg::EndOfContinuousUpdates)
            }
            248 => {
                // ServerFence: padding, flags, length, payload.
                let mut padding = [0; 3];
                reader.read_exact(&mut padding).await?;
                let flags = reader.read_u32().await?;
                let len = reader.read_u8().await? as usize;
                // The spec caps this at 64. A longer one is a broken server, and
                // reading it anyway keeps the stream aligned.
                let mut payload = vec![0; len];
                reader.read_exact(&mut payload).await?;
                if len > 64 {
                    payload.truncate(64);
                }
                Ok(ServerMsg::ServerFence { flags, payload })
            }
            _ => Err(VncError::WrongServerMessage),
        }
    }
}

/// Read the body of an extended `ServerCutText`, whose length arrived negative.
///
/// `len` is that length's magnitude: the whole body, flags word included.
async fn read_extended_clipboard<S>(reader: &mut S, len: usize) -> Result<ServerMsg, VncError>
where
    S: AsyncRead + Unpin,
{
    if len < 4 {
        return Err(VncError::General(format!(
            "server declared a {len}-byte extended clipboard message, which cannot \
             hold its own flags"
        )));
    }
    if len > MAX_PAYLOAD {
        // As with the legacy form: skipping this would mean reading tens of megabytes
        // to find the next message boundary, so refusing is the kinder answer.
        return Err(VncError::General(format!(
            "server declared a {len}-byte extended clipboard payload"
        )));
    }
    // Read it whole -- the length is bounded above and the flags cannot be understood
    // in pieces -- but do not let a length past the cap turn into an allocation.
    let keep = len.min(MAX_CUT_TEXT);
    let mut body = vec![0; keep];
    reader.read_exact(&mut body).await?;
    let mut discard = len - keep;
    let mut sink = [0u8; 4096];
    while discard > 0 {
        let n = discard.min(sink.len());
        reader.read_exact(&mut sink[..n]).await?;
        discard -= n;
    }
    if len > keep {
        // A clipboard this large is not one a terminal can do anything with, and the
        // truncated remainder would decode as a corrupt zlib stream. The stream is
        // aligned again, so the session continues.
        tracing::warn!("dropping a {len}-byte remote clipboard");
        return Ok(ServerMsg::ExtendedClipboard(clipboard::Message::Ignored));
    }
    Ok(ServerMsg::ExtendedClipboard(clipboard::decode(&body)?))
}

#[cfg(test)]
mod server_msg_tests {
    use super::*;

    /// Parse one message, and hand back whatever is left on the stream.
    ///
    /// The leftovers are half the point: several of these messages carry a length the
    /// server chose, and the contract is that the whole message is consumed even when
    /// it is refused. A parser that stops early leaves the next read starting
    /// mid-message, and from there every later message is garbage -- which looks
    /// nothing like the bug that caused it.
    async fn read(bytes: &[u8]) -> (Result<ServerMsg, VncError>, Vec<u8>) {
        let mut input = bytes;
        let result = ServerMsg::read(&mut input).await;
        (result, input.to_vec())
    }

    /// A `Bell`, as something recognisable to put after a message under test.
    const NEXT_MESSAGE: [u8; 1] = [2];

    #[tokio::test]
    async fn a_framebuffer_update_carries_its_rectangle_count() {
        let (msg, rest) = read(&[0, 0, 0x01, 0x2c]).await;

        assert!(
            matches!(msg, Ok(ServerMsg::FramebufferUpdate(300))),
            "{msg:?}"
        );
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn a_bell_is_just_its_type_byte() {
        let (msg, rest) = read(&[2, 0xff]).await;

        assert!(matches!(msg, Ok(ServerMsg::Bell)), "{msg:?}");
        assert_eq!(rest, vec![0xff], "read past the end of a Bell");
    }

    #[tokio::test]
    async fn end_of_continuous_updates_is_just_its_type_byte() {
        let (msg, rest) = read(&[150, 0xff]).await;

        assert!(
            matches!(msg, Ok(ServerMsg::EndOfContinuousUpdates)),
            "{msg:?}"
        );
        assert_eq!(rest, vec![0xff]);
    }

    #[tokio::test]
    async fn an_unknown_message_type_is_refused() {
        // There is no length to skip, so the stream cannot be recovered and saying so
        // is all that is left.
        let (msg, _) = read(&[99]).await;

        assert!(matches!(msg, Err(VncError::WrongServerMessage)), "{msg:?}");
    }

    #[tokio::test]
    async fn cut_text_arrives_as_a_string() {
        let mut bytes = vec![3, 0, 0, 0];
        bytes.extend_from_slice(&5i32.to_be_bytes());
        bytes.extend_from_slice(b"hello");
        bytes.extend_from_slice(&NEXT_MESSAGE);

        let (msg, rest) = read(&bytes).await;

        match msg {
            Ok(ServerMsg::ServerCutText(text)) => assert_eq!(text, "hello"),
            other => panic!("{other:?}"),
        }
        assert_eq!(rest, NEXT_MESSAGE.to_vec());
    }

    #[tokio::test]
    async fn cut_text_that_is_not_utf8_is_substituted_rather_than_refused() {
        // RFB says Latin-1 and servers send whatever they like. Losing the clipboard is
        // better than losing the session.
        let mut bytes = vec![3, 0, 0, 0];
        bytes.extend_from_slice(&3i32.to_be_bytes());
        bytes.extend_from_slice(&[0xff, 0xfe, b'a']);

        let (msg, _) = read(&bytes).await;

        match msg {
            Ok(ServerMsg::ServerCutText(text)) => {
                assert!(text.ends_with('a'), "got {text:?}");
                assert!(text.contains('\u{fffd}'), "expected replacements: {text:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_negative_cut_text_length_is_the_extension_not_four_billion_bytes() {
        // The length is signed, and a negative one announces the extended clipboard
        // form with abs(length) bytes to follow. Read as unsigned, -8 becomes
        // 4294967288 and the client spends the rest of the session trying to skip that
        // many bytes.
        let body = crate::rfb::client::clipboard::notify();
        let mut bytes = vec![3, 0, 0, 0];
        bytes.extend_from_slice(&(-(body.len() as i32)).to_be_bytes());
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(&NEXT_MESSAGE);

        let (msg, rest) = read(&bytes).await;

        match msg {
            Ok(ServerMsg::ExtendedClipboard(clipboard::Message::Notify { text })) => assert!(text),
            other => panic!("{other:?}"),
        }
        // And the length is what found the next message, so the stream is still aligned.
        assert_eq!(rest, NEXT_MESSAGE.to_vec());
    }

    #[tokio::test]
    async fn an_extended_message_too_short_for_its_flags_is_refused() {
        // Four bytes of flags is the minimum any of these can be. Less than that and
        // there is nothing to dispatch on, so guessing would mean reading into
        // whatever follows.
        let mut bytes = vec![3, 0, 0, 0];
        bytes.extend_from_slice(&(-3i32).to_be_bytes());
        bytes.extend_from_slice(&[0, 0, 0]);

        let (msg, _) = read(&bytes).await;

        match msg {
            Err(VncError::General(text)) => assert!(text.contains("flags"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn an_extended_message_past_the_text_cap_is_dropped_but_still_consumed() {
        // Too large to be a clipboard a terminal can use, and the part we would keep
        // is half a zlib stream. Drop it, but read all of it: the session survives a
        // clipboard it cannot hold.
        let len = MAX_CUT_TEXT + 32;
        let mut bytes = vec![3, 0, 0, 0];
        bytes.extend_from_slice(&(-(len as i32)).to_be_bytes());
        bytes.extend_from_slice(&vec![0x11; len]);
        bytes.extend_from_slice(&NEXT_MESSAGE);

        let (msg, rest) = read(&bytes).await;

        assert!(matches!(
            msg,
            Ok(ServerMsg::ExtendedClipboard(clipboard::Message::Ignored))
        ));
        assert_eq!(rest, NEXT_MESSAGE.to_vec());
    }

    #[tokio::test]
    async fn an_implausible_cut_text_length_is_refused_before_allocating() {
        // Past MAX_PAYLOAD there is no way back to a message boundary that does not mean
        // reading tens of megabytes, so this one gives up on the stream deliberately.
        let mut bytes = vec![3, 0, 0, 0];
        bytes.extend_from_slice(&((MAX_PAYLOAD + 1) as i32).to_be_bytes());

        let (msg, _) = read(&bytes).await;

        match msg {
            Err(VncError::General(text)) => {
                assert!(text.contains(&(MAX_PAYLOAD + 1).to_string()), "{text}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn cut_text_past_the_text_cap_is_clipped_but_still_consumed() {
        // Between MAX_CUT_TEXT and MAX_PAYLOAD the payload is too big to be clipboard
        // text but small enough to skip, so the excess is read past rather than
        // allocated -- and the next message has to still line up afterwards.
        let extra = 100;
        let len = MAX_CUT_TEXT + extra;
        let mut bytes = vec![3, 0, 0, 0];
        bytes.extend_from_slice(&(len as i32).to_be_bytes());
        bytes.extend(std::iter::repeat_n(b'x', len));
        bytes.extend_from_slice(&NEXT_MESSAGE);

        let (msg, rest) = read(&bytes).await;

        match msg {
            Ok(ServerMsg::ServerCutText(text)) => {
                assert_eq!(text.len(), MAX_CUT_TEXT, "should keep exactly the cap");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            rest,
            NEXT_MESSAGE.to_vec(),
            "the skipped tail left the stream misaligned"
        );
    }

    #[tokio::test]
    async fn a_colour_map_is_drained_before_it_is_refused() {
        // We always ask for true colour, so a colour map means the server ignored that
        // and nothing after it can be interpreted. Upstream had `unimplemented!()` here.
        // The message is still drained, because the error is reported to the user rather
        // than ending the session outright.
        let colours = 3u16;
        let mut bytes = vec![1, 0];
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&colours.to_be_bytes());
        bytes.extend(std::iter::repeat_n(0u8, usize::from(colours) * 6));
        bytes.extend_from_slice(&NEXT_MESSAGE);

        let (msg, rest) = read(&bytes).await;

        match msg {
            Err(VncError::General(text)) => assert!(text.contains("colour map"), "{text}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            rest,
            NEXT_MESSAGE.to_vec(),
            "six bytes per colour have to be drained or the stream desynchronises"
        );
    }

    #[tokio::test]
    async fn a_fence_carries_its_flags_and_payload() {
        let mut bytes = vec![248, 0, 0, 0];
        bytes.extend_from_slice(&0b11u32.to_be_bytes());
        bytes.push(4);
        bytes.extend_from_slice(b"hail");
        bytes.extend_from_slice(&NEXT_MESSAGE);

        let (msg, rest) = read(&bytes).await;

        match msg {
            Ok(ServerMsg::ServerFence { flags, payload }) => {
                assert_eq!(flags, 0b11);
                assert_eq!(payload, b"hail");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(rest, NEXT_MESSAGE.to_vec());
    }

    #[tokio::test]
    async fn an_oversized_fence_payload_is_clipped_but_still_consumed() {
        // The spec caps a fence payload at 64 bytes. A server sending more is broken,
        // but the bytes are on the stream either way and have to come off it.
        let mut bytes = vec![248, 0, 0, 0];
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.push(200);
        bytes.extend(std::iter::repeat_n(b'z', 200));
        bytes.extend_from_slice(&NEXT_MESSAGE);

        let (msg, rest) = read(&bytes).await;

        match msg {
            Ok(ServerMsg::ServerFence { payload, .. }) => {
                assert_eq!(payload.len(), 64, "should be clipped to the spec's cap")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            rest,
            NEXT_MESSAGE.to_vec(),
            "all 200 bytes have to be read even though only 64 are kept"
        );
    }

    #[tokio::test]
    async fn a_truncated_message_is_an_error() {
        // A fence that promises four bytes of payload and supplies one, which is what a
        // connection dropped mid-message looks like.
        let mut bytes = vec![248, 0, 0, 0];
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.push(4);
        bytes.push(b'x');

        let (msg, _) = read(&bytes).await;

        assert!(matches!(msg, Err(VncError::IoError(_))), "{msg:?}");
    }
}
