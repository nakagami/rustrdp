use async_trait::async_trait;
use rustrdp_core::client::RdpSession;
use rustrdp_core::error::RdpError;
use rustrdp_core::protocol::Transport;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::ClientConfig;
use rustls::{DigitallySignedStruct, SignatureScheme};
use socket2::{Socket, TcpKeepalive};
use std::error::Error;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

use crate::config::RdpConfig;

/// No-op TLS certificate verifier — accepts any server certificate.
/// RDP servers typically use self-signed certificates.
#[derive(Debug)]
struct NoCertVerifier;

impl ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

pub struct SimpleTransport {
    stream: Option<Stream>,
    host: String,
    // Persistent read buffer — survives future cancellation so partial reads are not lost.
    read_buf: Vec<u8>,
}

#[async_trait(?Send)]
impl Transport for SimpleTransport {
    async fn send(&mut self, data: &[u8]) -> Result<(), RdpError> {
        match self.stream.as_mut() {
            Some(Stream::Plain(s)) => s.write_all(data).await.map_err(RdpError::from),
            Some(Stream::Tls(s)) => s.write_all(data).await.map_err(RdpError::from),
            None => Err(RdpError::Closed),
        }
    }

    async fn recv_exact(&mut self, n: usize) -> Result<Vec<u8>, RdpError> {
        // Use a persistent read buffer so that if this future is cancelled mid-read (e.g. by
        // tokio::time::timeout), already-consumed bytes are not lost.  tokio::AsyncReadExt::read()
        // is cancel-safe (reads 0 or more bytes atomically), while read_exact() is not.
        let mut tmp = [0u8; 65536];
        while self.read_buf.len() < n {
            let want = (n - self.read_buf.len()).min(tmp.len());
            let got = match self.stream.as_mut() {
                Some(Stream::Plain(s)) => s.read(&mut tmp[..want]).await.map_err(RdpError::from)?,
                Some(Stream::Tls(s)) => s.read(&mut tmp[..want]).await.map_err(RdpError::from)?,
                None => return Err(RdpError::Closed),
            };
            if got == 0 {
                return Err(RdpError::Closed);
            }
            self.read_buf.extend_from_slice(&tmp[..got]);
        }
        let result = self.read_buf[..n].to_vec();
        self.read_buf.drain(..n);
        Ok(result)
    }

    async fn close(&mut self) {
        match self.stream.take() {
            Some(Stream::Plain(mut s)) => {
                let _ = s.shutdown().await;
            }
            Some(Stream::Tls(mut s)) => {
                let _ = s.shutdown().await;
            }
            None => {}
        }
    }

    async fn start_tls(&mut self) -> Result<Vec<u8>, RdpError> {
        let plain = match self.stream.take() {
            Some(Stream::Plain(s)) => s,
            _ => {
                return Err(RdpError::Unsupported(
                    "start_tls called in invalid state".to_string(),
                ))
            }
        };

        let config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| RdpError::Io(e.to_string()))?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
                .with_no_client_auth();

        let server_name: ServerName<'static> = match self.host.parse::<IpAddr>() {
            Ok(ip) => ServerName::IpAddress(ip.into()),
            Err(_) => ServerName::try_from(self.host.clone())
                .map_err(|_| RdpError::Io(format!("Invalid hostname: {}", self.host)))?,
        };

        let connector = TlsConnector::from(Arc::new(config));
        let tls_stream = connector
            .connect(server_name, plain)
            .await
            .map_err(|e| RdpError::Io(format!("TLS handshake failed: {}", e)))?;

        // Extract server certificate RSA public key for NLA/CredSSP
        let cert_der: Option<Vec<u8>> = tls_stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|certs| certs.first())
            .map(|cert| cert.as_ref().to_vec());

        let spki = cert_der
            .and_then(|der| extract_rsa_pubkey(&der))
            .unwrap_or_default();

        log::debug!("TLS handshake complete, RSA pubkey len={}", spki.len());

        self.stream = Some(Stream::Tls(Box::new(tls_stream)));
        Ok(spki)
    }
}

/// Extract RSAPublicKey (PKCS#1) bytes from a DER-encoded X.509 certificate.
fn extract_rsa_pubkey(cert_der: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 0;
    der_expect_tag(cert_der, &mut pos, 0x30)?;
    der_skip_length(cert_der, &mut pos)?;
    der_expect_tag(cert_der, &mut pos, 0x30)?;
    let tbs_len = der_skip_length(cert_der, &mut pos)?;
    let tbs_end = pos + tbs_len;

    if pos < cert_der.len() && cert_der[pos] == 0xA0 {
        der_expect_tag(cert_der, &mut pos, 0xA0)?;
        let l = der_skip_length(cert_der, &mut pos)?;
        pos += l;
    }

    for _ in 0..5 {
        if pos >= tbs_end {
            return None;
        }
        pos += 1;
        let l = der_skip_length(cert_der, &mut pos)?;
        pos += l;
    }

    if pos >= tbs_end || cert_der[pos] != 0x30 {
        return None;
    }
    pos += 1;
    der_skip_length(cert_der, &mut pos)?;
    der_expect_tag(cert_der, &mut pos, 0x30)?;
    let alg_len = der_skip_length(cert_der, &mut pos)?;
    pos += alg_len;
    der_expect_tag(cert_der, &mut pos, 0x03)?;
    let bs_len = der_skip_length(cert_der, &mut pos)?;
    if bs_len < 1 || pos >= cert_der.len() {
        return None;
    }
    pos += 1; // skip unused-bits byte
    let rsa_len = bs_len - 1;
    if pos + rsa_len > cert_der.len() {
        return None;
    }
    Some(cert_der[pos..pos + rsa_len].to_vec())
}

fn der_expect_tag(data: &[u8], pos: &mut usize, expected: u8) -> Option<()> {
    if *pos >= data.len() || data[*pos] != expected {
        return None;
    }
    *pos += 1;
    Some(())
}

fn der_skip_length(data: &[u8], pos: &mut usize) -> Option<usize> {
    if *pos >= data.len() {
        return None;
    }
    let b = data[*pos];
    *pos += 1;
    if b & 0x80 == 0 {
        Some(b as usize)
    } else {
        let n = (b & 0x7F) as usize;
        if *pos + n > data.len() {
            return None;
        }
        let mut len = 0usize;
        for _ in 0..n {
            len = (len << 8) | data[*pos] as usize;
            *pos += 1;
        }
        Some(len)
    }
}

pub struct RdpConnection;

impl RdpConnection {
    pub async fn connect(
        config: &RdpConfig,
    ) -> Result<RdpSession<SimpleTransport>, Box<dyn Error>> {
        log::info!(
            "Connecting to {}:{} as {}",
            config.host,
            config.port,
            config.username
        );

        // Connect and tune socket options before the RDP handshake.
        let stream = TcpStream::connect((config.host.as_str(), config.port)).await?;
        stream.set_nodelay(true)?;

        // Use socket2 to set the OS-level TCP receive buffer to 4 MB.
        // RDP H.264 I-frames can be several hundred KB; a large buffer lets
        // the kernel absorb a burst without shrinking the receive window and
        // throttling the server, reducing inter-frame gaps during screen
        // animations.  This mirrors grdpsdl2's tc.SetReadBuffer(4 * 1024 * 1024).
        //
        // Also enable TCP keepalive (probe after 300 s idle) so that silent
        // NAT/firewall drops are detected within minutes rather than hanging
        // forever, matching grdpsdl2's net.Dialer{KeepAlive: 300*time.Second}.
        let stream = {
            let std_stream = stream.into_std()?;
            let sock = Socket::from(std_stream);
            let _ = sock.set_recv_buffer_size(4 * 1024 * 1024);
            let ka = TcpKeepalive::new().with_time(Duration::from_secs(300));
            let _ = sock.set_tcp_keepalive(&ka);
            // Restore non-blocking mode expected by tokio.
            sock.set_nonblocking(true)?;
            let std_stream: std::net::TcpStream = sock.into();
            TcpStream::from_std(std_stream)?
        };

        let transport = SimpleTransport {
            stream: Some(Stream::Plain(stream)),
            host: config.host.clone(),
            read_buf: Vec::new(),
        };

        let session = RdpSession::login(
            transport,
            &config.domain,
            &config.username,
            &config.password,
            config.width,
            config.height,
            0x0409,
            Some(&crate::h264::H264Decoder::new_boxed),
        )
        .await?;

        log::info!("RDP session established");
        Ok(session)
    }
}
