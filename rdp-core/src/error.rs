use thiserror::Error;

#[derive(Debug, Error)]
pub enum RdpError {
    #[error("IO error: {0}")]
    Io(String),
    #[error("Protocol error: {0}")]
    Protocol(String),
    #[error("Authentication error: {0}")]
    Auth(String),
    #[error("Crypto error: {0}")]
    Crypto(String),
    #[error("Connection closed")]
    Closed,
    #[error("Unsupported: {0}")]
    Unsupported(String),
}

impl From<std::io::Error> for RdpError {
    fn from(e: std::io::Error) -> Self {
        RdpError::Io(e.to_string())
    }
}
