use super::security;
use crate::rfb::{VncError, VncVersion};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum SecurityType {
    Invalid = 0,
    None = 1,
    VncAuth = 2,
    RA2 = 5,
    RA2ne = 6,
    Tight = 16,
    Ultra = 17,
    Tls = 18,
    VeNCrypt = 19,
    GtkVncSasl = 20,
    Md5Hash = 21,
    ColinDeanXvp = 22,
}

impl TryFrom<u8> for SecurityType {
    type Error = VncError;
    /// An explicit match, not a transmute.
    ///
    /// Upstream reinterpreted the byte as this enum after checking it against a
    /// list, which is correct only as long as the list and the discriminants stay
    /// in step -- and undefined behaviour the moment they do not.
    fn try_from(num: u8) -> Result<Self, Self::Error> {
        match num {
            0 => Ok(SecurityType::Invalid),
            1 => Ok(SecurityType::None),
            2 => Ok(SecurityType::VncAuth),
            5 => Ok(SecurityType::RA2),
            6 => Ok(SecurityType::RA2ne),
            16 => Ok(SecurityType::Tight),
            17 => Ok(SecurityType::Ultra),
            18 => Ok(SecurityType::Tls),
            19 => Ok(SecurityType::VeNCrypt),
            20 => Ok(SecurityType::GtkVncSasl),
            21 => Ok(SecurityType::Md5Hash),
            22 => Ok(SecurityType::ColinDeanXvp),
            invalid => Err(VncError::InvalidSecurityType(invalid)),
        }
    }
}

impl From<SecurityType> for u8 {
    fn from(e: SecurityType) -> Self {
        e as u8
    }
}

impl SecurityType {
    pub(super) async fn read<S>(reader: &mut S, version: &VncVersion) -> Result<Vec<Self>, VncError>
    where
        S: AsyncRead + Unpin,
    {
        match version {
            VncVersion::RFB33 => {
                let security_type = reader.read_u32().await?;
                let security_type = (security_type as u8).try_into()?;
                if let SecurityType::Invalid = security_type {
                    let _ = reader.read_u32().await?;
                    let mut err_msg = String::new();
                    reader.read_to_string(&mut err_msg).await?;
                    return Err(VncError::General(err_msg));
                }
                Ok(vec![security_type])
            }
            _ => {
                // +--------------------------+-------------+--------------------------+
                // | No. of bytes             | Type        | Description              |
                // |                          | [Value]     |                          |
                // +--------------------------+-------------+--------------------------+
                // | 1                        | U8          | number-of-security-types |
                // | number-of-security-types | U8 array    | security-types           |
                // +--------------------------+-------------+--------------------------+
                let num = reader.read_u8().await?;

                if num == 0 {
                    let _ = reader.read_u32().await?;
                    let mut err_msg = String::new();
                    reader.read_to_string(&mut err_msg).await?;
                    return Err(VncError::General(err_msg));
                }
                let mut sec_types = vec![];
                for _ in 0..num {
                    sec_types.push(reader.read_u8().await?.try_into()?);
                }
                tracing::trace!("Server supported security type: {:?}", sec_types);
                Ok(sec_types)
            }
        }
    }

    pub(super) async fn write<S>(&self, writer: &mut S) -> Result<(), VncError>
    where
        S: AsyncWrite + Unpin,
    {
        writer.write_all(&[(*self).into()]).await?;
        Ok(())
    }
}

#[allow(dead_code)]
#[repr(u32)]
pub(super) enum AuthResult {
    Ok = 0,
    Failed = 1,
}

impl From<u32> for AuthResult {
    /// Anything that is not a documented success is a failure.
    ///
    /// Upstream transmuted the word straight into this two-variant enum, so a
    /// server returning 2 produced a value that was neither variant.
    fn from(num: u32) -> Self {
        match num {
            0 => AuthResult::Ok,
            _ => AuthResult::Failed,
        }
    }
}

impl From<AuthResult> for u32 {
    fn from(e: AuthResult) -> Self {
        e as u32
    }
}

pub(super) struct AuthHelper {
    challenge: [u8; 16],
    key: [u8; 8],
}

impl AuthHelper {
    pub(super) async fn read<S>(reader: &mut S, credential: &str) -> Result<Self, VncError>
    where
        S: AsyncRead + Unpin,
    {
        let mut challenge = [0; 16];
        reader.read_exact(&mut challenge).await?;

        let credential_len = credential.len();
        let mut key = [0u8; 8];
        for (i, key_i) in key.iter_mut().enumerate() {
            let c = if i < credential_len {
                credential.as_bytes()[i]
            } else {
                0
            };
            let mut cs = 0u8;
            for j in 0..8 {
                cs |= ((c >> j) & 1) << (7 - j)
            }
            *key_i = cs;
        }

        Ok(Self { challenge, key })
    }

    pub(super) async fn write<S>(&self, writer: &mut S) -> Result<(), VncError>
    where
        S: AsyncWrite + Unpin,
    {
        let encrypted = security::des::encrypt(&self.challenge, &self.key);
        writer.write_all(&encrypted).await?;
        Ok(())
    }

    pub(super) async fn finish<S>(self, reader: &mut S) -> Result<AuthResult, VncError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let result = reader.read_u32().await?;
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------- the security type

    #[test]
    fn every_documented_security_type_maps_to_itself() {
        for (byte, expected) in [
            (0u8, SecurityType::Invalid),
            (1, SecurityType::None),
            (2, SecurityType::VncAuth),
            (5, SecurityType::RA2),
            (6, SecurityType::RA2ne),
            (16, SecurityType::Tight),
            (17, SecurityType::Ultra),
            (18, SecurityType::Tls),
            (19, SecurityType::VeNCrypt),
            (20, SecurityType::GtkVncSasl),
            (21, SecurityType::Md5Hash),
            (22, SecurityType::ColinDeanXvp),
        ] {
            assert_eq!(SecurityType::try_from(byte).unwrap(), expected);
            assert_eq!(u8::from(expected), byte, "the round trip has to hold");
        }
    }

    #[test]
    fn an_undocumented_security_type_is_refused_rather_than_reinterpreted() {
        // Upstream transmuted the byte into this enum after checking it against a list,
        // which is undefined behaviour the moment the list and the discriminants drift
        // apart. Anything unlisted has to be an error instead.
        for byte in [3u8, 4, 7, 15, 23, 200, 255] {
            match SecurityType::try_from(byte) {
                Err(VncError::InvalidSecurityType(n)) => assert_eq!(n, byte),
                other => panic!("{byte} gave {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_modern_server_offers_a_list() {
        // 3.7 and later send a count and then that many bytes.
        let bytes = [3u8, 1, 2, 19];
        let types = SecurityType::read(&mut &bytes[..], &VncVersion::RFB38)
            .await
            .unwrap();

        assert_eq!(
            types,
            vec![
                SecurityType::None,
                SecurityType::VncAuth,
                SecurityType::VeNCrypt
            ]
        );
    }

    #[tokio::test]
    async fn an_empty_list_carries_the_reason_the_server_refused() {
        // A count of zero means the connection failed, and a length-prefixed string
        // saying why follows. Reporting that verbatim is the only way the user learns
        // what a server objected to.
        let mut bytes = vec![0u8];
        bytes.extend_from_slice(&11u32.to_be_bytes());
        bytes.extend_from_slice(b"too many clients");

        let result = SecurityType::read(&mut &bytes[..], &VncVersion::RFB38).await;

        match result {
            Err(VncError::General(msg)) => assert!(msg.contains("too many"), "got {msg:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unknown_type_in_the_list_is_refused() {
        let bytes = [2u8, 1, 99];

        let result = SecurityType::read(&mut &bytes[..], &VncVersion::RFB38).await;

        assert!(
            matches!(result, Err(VncError::InvalidSecurityType(99))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_33_server_sends_one_word_and_no_list() {
        // In 3.3 the server decides alone and says so in four bytes. Reading a count
        // byte here instead would take the first byte of the word as a length.
        let bytes = 2u32.to_be_bytes();

        let types = SecurityType::read(&mut &bytes[..], &VncVersion::RFB33)
            .await
            .unwrap();

        assert_eq!(types, vec![SecurityType::VncAuth]);
    }

    #[tokio::test]
    async fn a_33_server_refusing_the_connection_carries_a_reason() {
        // Security type 0 in 3.3 means failure, followed by the same length-prefixed
        // string as the modern path.
        let mut bytes = 0u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&9u32.to_be_bytes());
        bytes.extend_from_slice(b"no access");

        let result = SecurityType::read(&mut &bytes[..], &VncVersion::RFB33).await;

        match result {
            Err(VncError::General(msg)) => assert!(msg.contains("no access"), "got {msg:?}"),
            other => panic!("{other:?}"),
        }
    }

    // ----------------------------------------------------------- the auth result

    #[test]
    fn anything_that_is_not_zero_is_a_failure() {
        // Upstream transmuted this word into a two-variant enum, so a server answering
        // 2 produced a value that was neither `Ok` nor `Failed` -- and then matched
        // against both arms without hitting either.
        assert!(matches!(AuthResult::from(0), AuthResult::Ok));
        for word in [1u32, 2, 3, 0xffff_ffff] {
            assert!(
                matches!(AuthResult::from(word), AuthResult::Failed),
                "{word} should be a failure"
            );
        }
    }

    // ------------------------------------------------------- the DES challenge

    /// The key VNC auth derives from a password: each byte's bits in reverse order.
    fn reversed(byte: u8) -> u8 {
        let mut out = 0;
        for j in 0..8 {
            out |= ((byte >> j) & 1) << (7 - j);
        }
        out
    }

    #[tokio::test]
    async fn the_password_becomes_a_bit_reversed_key() {
        // VNC auth reverses the bits of every byte before using the password as a DES
        // key. It is not a hash and not an accident of endianness: get it wrong and
        // every password is silently rejected by every server.
        let challenge = [7u8; 16];
        let helper = AuthHelper::read(&mut &challenge[..], "ab").await.unwrap();

        assert_eq!(helper.challenge, challenge);
        assert_eq!(helper.key[0], reversed(b'a'));
        assert_eq!(helper.key[1], reversed(b'b'));
        assert_eq!(&helper.key[2..], &[0; 6], "short passwords are zero padded");
    }

    #[tokio::test]
    async fn only_the_first_eight_characters_of_a_password_are_used() {
        // The key is eight bytes and the protocol offers no way around it, so a longer
        // password is truncated rather than folded in. Worth pinning: a user whose
        // password differs only past the eighth character would otherwise be baffled.
        let challenge = [0u8; 16];
        let short = AuthHelper::read(&mut &challenge[..], "12345678")
            .await
            .unwrap();
        let long = AuthHelper::read(&mut &challenge[..], "12345678ignored")
            .await
            .unwrap();

        assert_eq!(short.key, long.key);
    }

    #[tokio::test]
    async fn the_response_is_sixteen_bytes_and_depends_on_the_password() {
        // Two DES blocks, one per half of the challenge. The same challenge under a
        // different password has to give a different answer, which is the whole point.
        let challenge = [0x5au8; 16];

        let mut first = Vec::new();
        AuthHelper::read(&mut &challenge[..], "secret")
            .await
            .unwrap()
            .write(&mut first)
            .await
            .unwrap();

        let mut second = Vec::new();
        AuthHelper::read(&mut &challenge[..], "secres")
            .await
            .unwrap()
            .write(&mut second)
            .await
            .unwrap();

        assert_eq!(first.len(), 16, "the response is the challenge, encrypted");
        assert_ne!(first, second, "a different password gave the same response");
    }

    #[tokio::test]
    async fn the_result_word_decides_whether_the_password_was_accepted() {
        let challenge = [0u8; 16];

        for (word, accepted) in [(0u32, true), (1, false)] {
            let helper = AuthHelper::read(&mut &challenge[..], "pw").await.unwrap();
            let reply = word.to_be_bytes();
            let mut stream = tokio::io::join(&reply[..], Vec::new());
            let result = helper.finish(&mut stream).await.unwrap();

            assert_eq!(
                matches!(result, AuthResult::Ok),
                accepted,
                "the word {word} was read wrongly"
            );
        }
    }
}
