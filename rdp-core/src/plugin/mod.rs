use crate::error::RdpError;
use async_trait::async_trait;

#[async_trait(?Send)]
pub trait Plugin {
    fn name(&self) -> &str;
    async fn on_data(&mut self, data: &[u8]) -> Result<Option<Vec<u8>>, RdpError>;
}
