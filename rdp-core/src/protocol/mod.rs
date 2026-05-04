pub mod drdynvc;
pub mod lic;
pub mod nla;
pub mod pdu;
pub mod rdpgfx;
pub mod rdpsnd;
pub mod sec;
pub mod t125;
pub mod tpkt;
pub mod x224;
pub mod zgfx;

use crate::error::RdpError;
use async_trait::async_trait;

#[async_trait(?Send)]
pub trait Transport {
    async fn send(&mut self, data: &[u8]) -> Result<(), RdpError>;
    async fn recv_exact(&mut self, n: usize) -> Result<Vec<u8>, RdpError>;
    async fn close(&mut self);
    async fn start_tls(&mut self) -> Result<Vec<u8>, RdpError>;
}
