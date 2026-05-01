use rdp_core::client::RdpSession;
use rdp_core::protocol::Transport;
use rdp_core::error::RdpError;
use async_trait::async_trait;
use std::error::Error;
use std::net::TcpStream;
use std::io::{Read, Write};

use crate::config::RdpConfig;

pub struct SimpleTransport {
    stream: TcpStream,
}

#[async_trait(?Send)]
impl Transport for SimpleTransport {
    async fn send(&mut self, data: &[u8]) -> Result<(), RdpError> {
        self.stream.write_all(data)
            .map_err(|e| RdpError::Io(format!("Failed to write: {}", e)))
    }

    async fn recv_exact(&mut self, n: usize) -> Result<Vec<u8>, RdpError> {
        let mut buffer = vec![0u8; n];
        self.stream.read_exact(&mut buffer)
            .map_err(|e| RdpError::Io(format!("Failed to read: {}", e)))?;
        Ok(buffer)
    }

    async fn close(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    async fn start_tls(&mut self) -> Result<Vec<u8>, RdpError> {
        Err(RdpError::Unsupported("TLS not supported in simple transport".to_string()))
    }
}

pub struct RdpConnection;

impl RdpConnection {
    pub async fn connect(config: &RdpConfig) -> Result<RdpSession<SimpleTransport>, Box<dyn Error>> {
        log::info!(
            "Connecting to {}:{} as {}",
            config.host,
            config.port,
            config.username
        );

        let stream = TcpStream::connect((config.host.as_str(), config.port))?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;

        let transport = SimpleTransport { stream };

        let session = RdpSession::login(
            transport,
            &config.domain,
            &config.username,
            &config.password,
            config.width,
            config.height,
            0x0409, // US English keyboard layout
        )
        .await?;

        log::info!("RDP session established");
        Ok(session)
    }
}
