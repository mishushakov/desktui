use crate::rfb::{MAX_CUT_TEXT, MAX_PAYLOAD, PixelFormat, Rect, ScreenInfo, VncEncoding, VncError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug)]
pub(super) enum ClientMsg {
    SetPixelFormat(PixelFormat),
    SetEncodings(Vec<VncEncoding>),
    FramebufferUpdateRequest(Rect, u8),
    KeyEvent(u32, bool),
    PointerEvent(u16, u16, u8),
    ClientCutText(String),
    SetDesktopSize {
        width: u16,
        height: u16,
        screens: Vec<ScreenInfo>,
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
            ClientMsg::SetEncodings(encodings) => {
                //  +--------------+--------------+---------------------+
                // | No. of bytes | Type [Value] | Description         |
                // +--------------+--------------+---------------------+
                // | 1            | U8 [2]       | message-type        |
                // | 1            |              | padding             |
                // | 2            | U16          | number-of-encodings |
                // +--------------+--------------+---------------------+

                // This is followed by number-of-encodings repetitions of the following:
                // +--------------+--------------+---------------+
                // | No. of bytes | Type [Value] | Description   |
                // +--------------+--------------+---------------+
                // | 4            | S32          | encoding-type |
                // +--------------+--------------+---------------+
                let mut payload = vec![2, 0];
                payload.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
                for e in encodings {
                    payload.write_u32(e.into()).await?;
                }
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
                let mut payload = vec![6_u8, 0, 0, 0];
                payload.write_u32(s.len() as u32).await?;
                payload.write_all(s.as_bytes()).await?;
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
    async fn key_and_pointer_events_match_the_spec() {
        let key = encode(ClientMsg::KeyEvent(0xffe1, true)).await;
        assert_eq!(key, vec![4, 1, 0, 0, 0x00, 0x00, 0xff, 0xe1]);
        let up = encode(ClientMsg::KeyEvent(0x61, false)).await;
        assert_eq!(up, vec![4, 0, 0, 0, 0x00, 0x00, 0x00, 0x61]);
        let pointer = encode(ClientMsg::PointerEvent(1599, 832, 0b101)).await;
        assert_eq!(pointer, vec![5, 0b101, 0x06, 0x3f, 0x03, 0x40]);
    }
}

#[derive(Debug)]
pub(super) enum ServerMsg {
    FramebufferUpdate(u16),
    // SetColorMapEntries,
    Bell,
    ServerCutText(String),
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
                // entirely. Reading it as unsigned -- which is the obvious
                // mistake, and the one upstream made -- turns -8 into four
                // billion and then spends the rest of the session trying to skip
                // that many bytes. We never request that extension, so a server
                // sending it is out of contract.
                let len = reader.read_i32().await?;
                if len < 0 {
                    return Err(VncError::General(
                        "server sent an extended clipboard message, which was never \
                         requested"
                            .into(),
                    ));
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
            _ => Err(VncError::WrongServerMessage),
        }
    }
}
