use super::{
    auth::{AuthHelper, AuthResult, SecurityType},
    connection::VncClient,
};
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tracing::{info, trace};

use crate::rfb::{PixelFormat, VncEncoding, VncError, VncVersion};

pub enum VncState<S, F>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    F: Future<Output = Result<String, VncError>> + Send + Sync + 'static,
{
    Handshake(VncConnector<S, F>),
    Authenticate(VncConnector<S, F>),
    Connected(VncClient),
}

impl<S, F> VncState<S, F>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    F: Future<Output = Result<String, VncError>> + Send + Sync + 'static,
{
    pub fn try_start(
        self,
    ) -> Pin<Box<dyn Future<Output = Result<Self, VncError>> + Send + Sync + 'static>> {
        Box::pin(async move {
            match self {
                VncState::Handshake(mut connector) => {
                    // Read the rfbversion informed by the server
                    let rfbversion = VncVersion::read(&mut connector.stream).await?;
                    trace!(
                        "Our version {:?}, server version {:?}",
                        connector.rfb_version, rfbversion
                    );
                    let rfbversion = if connector.rfb_version < rfbversion {
                        connector.rfb_version
                    } else {
                        rfbversion
                    };

                    // Record the negotiated rfbversion
                    connector.rfb_version = rfbversion;
                    trace!("Negotiated rfb version: {:?}", rfbversion);
                    rfbversion.write(&mut connector.stream).await?;
                    Ok(VncState::Authenticate(connector).try_start().await?)
                }
                VncState::Authenticate(mut connector) => {
                    let security_types =
                        SecurityType::read(&mut connector.stream, &connector.rfb_version).await?;

                    assert!(!security_types.is_empty());

                    if security_types.contains(&SecurityType::None) {
                        match connector.rfb_version {
                            VncVersion::RFB33 => {
                                // If the security-type is 1, for no authentication, the server does not
                                // send the SecurityResult message but proceeds directly to the
                                // initialization messages (Section 7.3).
                                info!("No auth needed in vnc3.3");
                            }
                            VncVersion::RFB37 => {
                                // After the security handshake, if the security-type is 1, for no
                                // authentication, the server does not send the SecurityResult message
                                // but proceeds directly to the initialization messages (Section 7.3).
                                info!("No auth needed in vnc3.7");
                                SecurityType::write(&SecurityType::None, &mut connector.stream)
                                    .await?;
                            }
                            VncVersion::RFB38 => {
                                info!("No auth needed in vnc3.8");
                                SecurityType::write(&SecurityType::None, &mut connector.stream)
                                    .await?;
                                let mut ok = [0; 4];
                                connector.stream.read_exact(&mut ok).await?;
                            }
                        }
                    } else {
                        // choose a auth method
                        if security_types.contains(&SecurityType::VncAuth) {
                            if connector.rfb_version != VncVersion::RFB33 {
                                // In the security handshake (Section 7.1.2), rather than a two-way
                                // negotiation, the server decides the security type and sends a single
                                // word:

                                //            +--------------+--------------+---------------+
                                //            | No. of bytes | Type [Value] | Description   |
                                //            +--------------+--------------+---------------+
                                //            | 4            | U32          | security-type |
                                //            +--------------+--------------+---------------+

                                // The security-type may only take the value 0, 1, or 2.  A value of 0
                                // means that the connection has failed and is followed by a string
                                // giving the reason, as described in Section 7.1.2.
                                SecurityType::write(&SecurityType::VncAuth, &mut connector.stream)
                                    .await?;
                            }
                        } else {
                            let msg = "Security type apart from Vnc Auth has not been implemented";
                            return Err(VncError::General(msg.to_owned()));
                        }

                        // get password
                        if connector.auth_methond.is_none() {
                            return Err(VncError::NoPassword);
                        }

                        let credential = (connector.auth_methond.take().unwrap()).await?;

                        // auth
                        let auth = AuthHelper::read(&mut connector.stream, &credential).await?;
                        auth.write(&mut connector.stream).await?;
                        let result = auth.finish(&mut connector.stream).await?;
                        if let AuthResult::Failed = result {
                            if let VncVersion::RFB37 = connector.rfb_version {
                                // In VNC Authentication (Section 7.2.2), if the authentication fails,
                                // the server sends the SecurityResult message, but does not send an
                                // error message before closing the connection.
                                return Err(VncError::WrongPassword);
                            }
                            let _ = connector.stream.read_u32().await?;
                            let mut err_msg = String::new();
                            connector.stream.read_to_string(&mut err_msg).await?;
                            return Err(VncError::General(err_msg));
                        }
                    }
                    info!("auth done, client connected");

                    Ok(VncState::Connected(
                        VncClient::new(
                            connector.stream,
                            connector.allow_shared,
                            connector.pixel_format,
                            connector.encodings,
                            connector.quality,
                            connector.compression,
                        )
                        .await?,
                    ))
                }
                _ => unreachable!(),
            }
        })
    }

    pub fn finish(self) -> Result<VncClient, VncError> {
        if let VncState::Connected(client) = self {
            Ok(client)
        } else {
            Err(VncError::ConnectError)
        }
    }
}

/// Connection Builder to setup a vnc client
pub struct VncConnector<S, F>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: Future<Output = Result<String, VncError>> + Send + Sync + 'static,
{
    stream: S,
    auth_methond: Option<F>,
    rfb_version: VncVersion,
    allow_shared: bool,
    pixel_format: Option<PixelFormat>,
    encodings: Vec<VncEncoding>,
    quality: Option<u8>,
    compression: Option<u8>,
}

impl<S, F> VncConnector<S, F>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    F: Future<Output = Result<String, VncError>> + Send + Sync + 'static,
{
    /// To new a vnc client configuration with stream `S`
    ///
    /// `S` should implement async I/O methods
    ///
    /// ```no_run
    /// use vnc::{PixelFormat, VncConnector, VncError};
    /// use tokio::{self, net::TcpStream};
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), VncError> {
    ///     let tcp = TcpStream::connect("127.0.0.1:5900").await?;
    ///     let vnc = VncConnector::new(tcp)
    ///         .set_auth_method(async move { Ok("password".to_string()) })
    ///         .add_encoding(vnc::VncEncoding::Tight)
    ///         .add_encoding(vnc::VncEncoding::Zrle)
    ///         .add_encoding(vnc::VncEncoding::CopyRect)
    ///         .add_encoding(vnc::VncEncoding::Raw)
    ///         .allow_shared(true)
    ///         .set_pixel_format(PixelFormat::bgra())
    ///         .build()?
    ///         .try_start()
    ///         .await?
    ///         .finish()?;
    ///     Ok(())
    /// }
    /// ```
    ///
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            auth_methond: None,
            allow_shared: true,
            rfb_version: VncVersion::RFB38,
            pixel_format: None,
            encodings: Vec::new(),
            quality: None,
            compression: None,
        }
    }

    /// An async callback which is used to query credentials if the vnc server has set
    ///
    /// ```no_compile
    /// connector = connector.set_auth_method(async move { Ok("password".to_string()) })
    /// ```
    ///
    /// if you're building a wasm app,
    /// the async callback also allows you to combine it to a promise
    ///
    /// ```no_compile
    /// #[wasm_bindgen]
    /// extern "C" {
    ///     fn get_password() -> js_sys::Promise;
    /// }
    ///
    /// connector = connector
    ///        .set_auth_method(async move {
    ///            let auth = JsFuture::from(get_password()).await.unwrap();
    ///            Ok(auth.as_string().unwrap())
    ///     });
    /// ```
    ///
    /// While in the js code
    ///
    ///
    /// ```javascript
    /// var password = '';
    /// function get_password() {
    ///     return new Promise((reslove, reject) => {
    ///        document.getElementById("submit_password").addEventListener("click", () => {
    ///             password = window.document.getElementById("input_password").value
    ///             reslove(password)
    ///         })
    ///     });
    /// }
    /// ```
    ///
    /// The future won't be polled if the sever doesn't apply any password protections to the session
    ///
    pub fn set_auth_method(mut self, auth_callback: F) -> Self {
        self.auth_methond = Some(auth_callback);
        self
    }

    /// The max vnc version that we supported
    ///
    /// Version should be one of the [VncVersion]
    ///
    #[allow(dead_code)] // protocol surface: the version is negotiated downward
    pub fn set_version(mut self, version: VncVersion) -> Self {
        self.rfb_version = version;
        self
    }

    /// Set the rgb order which you will use to resolve the image data
    ///
    /// In most of the case, use `PixelFormat::bgra()` on little endian PCs
    ///
    /// And use `PixelFormat::rgba()` on wasm apps (with canvas)
    ///
    /// Also, customized format is allowed
    ///
    /// Will use the default format informed by the vnc server if not set
    ///
    /// In this condition, the client will get a [crate::rfb::VncEvent::SetPixelFormat] event notified
    ///
    pub fn set_pixel_format(mut self, pf: PixelFormat) -> Self {
        self.pixel_format = Some(pf);
        self
    }

    /// Shared-flag is non-zero (true) if the server should try to share the
    ///
    /// desktop by leaving other clients connected, and zero (false) if it
    ///
    /// should give exclusive access to this client by disconnecting all
    ///
    /// other clients.
    ///
    pub fn allow_shared(mut self, allow_shared: bool) -> Self {
        self.allow_shared = allow_shared;
        self
    }

    /// Client encodings that we want to use
    ///
    /// One of [VncEncoding]
    ///
    /// [VncEncoding::Raw] must be sent as the RFC required
    ///
    /// The order to add encodings is the order to inform the server
    ///
    pub fn add_encoding(mut self, encoding: VncEncoding) -> Self {
        self.encodings.push(encoding);
        self
    }

    /// Add several encodings at once, which lets a caller decide conditionally
    /// without breaking the builder chain.
    pub fn add_encodings(mut self, encodings: &[VncEncoding]) -> Self {
        self.encodings.extend_from_slice(encodings);
        self
    }

    /// Ask for a JPEG quality level, 0 (worst) to 9 (best).
    ///
    /// Leaving this unset is meaningful: the spec says Tight does not use JPEG at all
    /// unless a quality level is given, so silence is how a client asks for a lossless
    /// picture.
    pub fn set_quality(mut self, level: Option<u8>) -> Self {
        self.quality = level;
        self
    }

    /// Ask for a compression level, 0 (least) to 9 (most). Lossless either way: this
    /// trades the server's CPU against bandwidth.
    pub fn set_compression(mut self, level: Option<u8>) -> Self {
        self.compression = level;
        self
    }

    /// Complete the client configuration
    ///
    pub fn build(self) -> Result<VncState<S, F>, VncError> {
        if self.encodings.is_empty() {
            return Err(VncError::NoEncoding);
        }
        Ok(VncState::Handshake(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rfb::VncEncoding;
    use std::collections::VecDeque;
    use std::future::Ready;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use tokio::io::{AsyncWrite, ReadBuf};

    /// A server that says only what the test scripts, and remembers what the client
    /// said back.
    ///
    /// Running out of script is end-of-stream rather than pending, so a client that
    /// wants more than the test provided fails instead of hanging. That is what lets
    /// these tests stop at the end of the handshake: whatever the client does next
    /// -- `ClientInit`, then waiting for `ServerInit` -- errors, and by then the bytes
    /// worth asserting on have already been written.
    #[derive(Clone, Default)]
    struct Script {
        inbound: Arc<Mutex<VecDeque<u8>>>,
        outbound: Arc<Mutex<Vec<u8>>>,
    }

    impl Script {
        fn new(inbound: Vec<u8>) -> Self {
            Self {
                inbound: Arc::new(Mutex::new(inbound.into())),
                outbound: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn written(&self) -> Vec<u8> {
            self.outbound.lock().unwrap().clone()
        }

        /// What the client never got round to reading.
        fn unread(&self) -> Vec<u8> {
            self.inbound.lock().unwrap().iter().copied().collect()
        }
    }

    impl AsyncRead for Script {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let mut inbound = self.inbound.lock().unwrap();
            while buf.remaining() > 0 {
                match inbound.pop_front() {
                    Some(byte) => buf.put_slice(&[byte]),
                    None => break,
                }
            }
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for Script {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.outbound.lock().unwrap().extend_from_slice(data);
            Poll::Ready(Ok(data.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A password that is already available, so the connector's generic future has a
    /// concrete type these tests can name.
    type Password = Ready<Result<String, VncError>>;

    fn client(
        script: &Script,
        ours: VncVersion,
        password: Option<&str>,
    ) -> VncState<Script, Password> {
        let mut connector = VncConnector::new(script.clone())
            .add_encoding(VncEncoding::Raw)
            .set_version(ours);
        if let Some(password) = password {
            connector = connector.set_auth_method(std::future::ready(Ok(password.to_string())));
        }
        connector.build().unwrap()
    }

    /// The version string a server announces itself with.
    fn version(v: VncVersion) -> Vec<u8> {
        <VncVersion as Into<&[u8; 12]>>::into(v).to_vec()
    }

    /// The shared-desktop flag, the first thing written after the handshake finishes.
    const CLIENT_INIT: u8 = 1;

    // ------------------------------------------------------ version negotiation

    #[tokio::test]
    async fn the_lower_of_the_two_versions_is_agreed_and_echoed() {
        // Whichever side is older decides, and the agreed version is written back so the
        // server knows which handshake follows. Sending our own version regardless would
        // put a 3.3 server into a handshake it does not implement.
        for (server, ours, expected) in [
            (VncVersion::RFB38, VncVersion::RFB38, VncVersion::RFB38),
            (VncVersion::RFB33, VncVersion::RFB38, VncVersion::RFB33),
            (VncVersion::RFB37, VncVersion::RFB38, VncVersion::RFB37),
            (VncVersion::RFB38, VncVersion::RFB33, VncVersion::RFB33),
            (VncVersion::RFB38, VncVersion::RFB37, VncVersion::RFB37),
        ] {
            let script = Script::new(version(server));
            // The script stops after the version, so the security handshake fails --
            // by which point the version has been written.
            let _ = client(&script, ours, None).try_start().await;

            assert_eq!(
                script.written(),
                version(expected),
                "server {server:?} with ours {ours:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_unrecognised_version_is_treated_as_33() {
        // RFC 6143 7.1.1: other version numbers are reported by some servers, and are
        // to be read as 3.3 because they do not implement the later handshakes. Assuming
        // the newest instead would hang waiting for a security list that never comes.
        for announced in [&b"RFB 004.001\n"[..], b"RFB 999.999\n", b"NOT A VERSION"] {
            let script = Script::new(announced.to_vec());
            let _ = client(&script, VncVersion::RFB38, None).try_start().await;

            assert_eq!(
                script.written(),
                version(VncVersion::RFB33),
                "{:?} should be read as 3.3",
                String::from_utf8_lossy(announced)
            );
        }
    }

    // ------------------------------------------------- the security handshake

    #[tokio::test]
    async fn no_auth_on_33_sends_nothing_and_goes_straight_to_client_init() {
        // 3.3 has no two-way negotiation at all: the server sends one word and the
        // client answers nothing. Echoing the type here would be read as the shared flag
        // and put every later message one byte out.
        let mut inbound = version(VncVersion::RFB33);
        inbound.extend_from_slice(&u32::from(SecurityType::None as u8).to_be_bytes());
        let script = Script::new(inbound);

        let _ = client(&script, VncVersion::RFB38, None).try_start().await;

        let mut expected = version(VncVersion::RFB33);
        expected.push(CLIENT_INIT);
        assert_eq!(script.written(), expected);
    }

    #[tokio::test]
    async fn no_auth_on_37_echoes_the_type_but_reads_no_result() {
        // 3.7 added the client's choice but not the SecurityResult that follows it, so
        // waiting for one would hang against a server that is behaving correctly.
        let mut inbound = version(VncVersion::RFB37);
        inbound.extend_from_slice(&[1, SecurityType::None as u8]);
        let script = Script::new(inbound);

        let _ = client(&script, VncVersion::RFB38, None).try_start().await;

        let mut expected = version(VncVersion::RFB37);
        expected.push(SecurityType::None as u8);
        expected.push(CLIENT_INIT);
        assert_eq!(script.written(), expected);
    }

    #[tokio::test]
    async fn no_auth_on_38_echoes_the_type_and_consumes_the_security_result() {
        // 3.8 sends a SecurityResult even when no authentication happened. Leaving those
        // four bytes on the stream would make them the start of ServerInit.
        let mut inbound = version(VncVersion::RFB38);
        inbound.extend_from_slice(&[1, SecurityType::None as u8]);
        inbound.extend_from_slice(&0u32.to_be_bytes());
        let script = Script::new(inbound);

        let _ = client(&script, VncVersion::RFB38, None).try_start().await;

        let mut expected = version(VncVersion::RFB38);
        expected.push(SecurityType::None as u8);
        expected.push(CLIENT_INIT);
        assert_eq!(script.written(), expected);
        assert!(
            script.unread().is_empty(),
            "the SecurityResult was left on the stream"
        );
    }

    #[tokio::test]
    async fn vnc_auth_on_33_is_not_echoed_either() {
        // The same asymmetry as no-auth, on the path that actually matters: a 3.3 server
        // has already decided, so answering would desynchronise the stream. Stopping at
        // the missing password keeps the assertion to the bytes under test.
        let mut inbound = version(VncVersion::RFB33);
        inbound.extend_from_slice(&u32::from(SecurityType::VncAuth as u8).to_be_bytes());
        let script = Script::new(inbound);

        let result = client(&script, VncVersion::RFB38, None)
            .try_start()
            .await
            .map(|_| ());

        assert!(matches!(result, Err(VncError::NoPassword)), "{result:?}");
        assert_eq!(
            script.written(),
            version(VncVersion::RFB33),
            "3.3 must not answer with a security type"
        );
    }

    #[tokio::test]
    async fn vnc_auth_on_38_is_echoed_before_the_password_is_needed() {
        let mut inbound = version(VncVersion::RFB38);
        inbound.extend_from_slice(&[1, SecurityType::VncAuth as u8]);
        let script = Script::new(inbound);

        let result = client(&script, VncVersion::RFB38, None)
            .try_start()
            .await
            .map(|_| ());

        assert!(matches!(result, Err(VncError::NoPassword)), "{result:?}");
        let mut expected = version(VncVersion::RFB38);
        expected.push(SecurityType::VncAuth as u8);
        assert_eq!(script.written(), expected);
    }

    #[tokio::test]
    async fn a_server_offering_nothing_we_implement_says_so() {
        // VeNCrypt-only servers exist and this client does not do TLS. The message has to
        // name the problem, because "connection failed" sends the user looking at the
        // network instead of at the server's configuration.
        let mut inbound = version(VncVersion::RFB38);
        inbound.extend_from_slice(&[2, SecurityType::VeNCrypt as u8, SecurityType::Tls as u8]);
        let script = Script::new(inbound);

        let result = client(&script, VncVersion::RFB38, Some("pw"))
            .try_start()
            .await
            .map(|_| ());

        match result {
            Err(VncError::General(msg)) => {
                assert!(
                    msg.contains("Vnc Auth"),
                    "should name what is missing: {msg}"
                )
            }
            other => panic!("{other:?}"),
        }
    }

    // -------------------------------------------------------- the DES exchange

    #[tokio::test]
    async fn the_challenge_response_goes_out_before_the_result_is_read() {
        // The full VNC auth exchange: type, then sixteen bytes of encrypted challenge,
        // then the result. Sixteen is two DES blocks, one per half of the challenge.
        let mut inbound = version(VncVersion::RFB38);
        inbound.extend_from_slice(&[1, SecurityType::VncAuth as u8]);
        inbound.extend_from_slice(&[0x5a; 16]); // challenge
        inbound.extend_from_slice(&0u32.to_be_bytes()); // accepted
        let script = Script::new(inbound);

        let _ = client(&script, VncVersion::RFB38, Some("secret"))
            .try_start()
            .await;

        let written = script.written();
        assert_eq!(
            written.len(),
            12 + 1 + 16 + 1,
            "expected version, type, response and client init: {written:?}"
        );
        let response = &written[13..29];
        assert!(
            response.iter().any(|&b| b != 0),
            "the response should be the encrypted challenge, not zeros"
        );
    }

    #[tokio::test]
    async fn a_rejected_password_on_38_reports_the_servers_reason() {
        // 3.8 follows a failure with a length-prefixed explanation, and that string is
        // the only thing that distinguishes a wrong password from a locked account.
        let mut inbound = version(VncVersion::RFB38);
        inbound.extend_from_slice(&[1, SecurityType::VncAuth as u8]);
        inbound.extend_from_slice(&[0; 16]);
        inbound.extend_from_slice(&1u32.to_be_bytes()); // failed
        inbound.extend_from_slice(&18u32.to_be_bytes());
        inbound.extend_from_slice(b"too many attempts");
        let script = Script::new(inbound);

        let result = client(&script, VncVersion::RFB38, Some("wrong"))
            .try_start()
            .await
            .map(|_| ());

        match result {
            Err(VncError::General(msg)) => assert!(msg.contains("too many"), "got {msg:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rejected_password_on_37_does_not_wait_for_a_reason() {
        // 3.7 closes the connection after the failure without sending an explanation, so
        // reading one would block until the server hung up rather than reporting the
        // wrong password.
        let mut inbound = version(VncVersion::RFB37);
        inbound.extend_from_slice(&[1, SecurityType::VncAuth as u8]);
        inbound.extend_from_slice(&[0; 16]);
        inbound.extend_from_slice(&1u32.to_be_bytes()); // failed
        // Bytes a 3.8 client would have read as a reason. They must be left alone.
        inbound.extend_from_slice(b"NOT A REASON");
        let script = Script::new(inbound);

        let result = client(&script, VncVersion::RFB37, Some("wrong"))
            .try_start()
            .await
            .map(|_| ());

        assert!(matches!(result, Err(VncError::WrongPassword)), "{result:?}");
        assert_eq!(
            script.unread(),
            b"NOT A REASON".to_vec(),
            "3.7 should not read an explanation that was never sent"
        );
    }

    // ------------------------------------------------------------------ builder

    #[test]
    fn a_client_with_no_encodings_is_refused() {
        // Raw is mandatory, so an empty list is a programming error rather than
        // something to negotiate.
        let script = Script::default();
        let result = VncConnector::<Script, Password>::new(script).build();

        assert!(matches!(result, Err(VncError::NoEncoding)), "unexpected");
    }
}
