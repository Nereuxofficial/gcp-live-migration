use color_eyre::Result;
use std::net::IpAddr;

#[async_trait::async_trait]
pub trait Migration {
    async fn checkpoint(&mut self) -> Result<()>;
    async fn migrate(&mut self, ip_addr: IpAddr) -> Result<()>;
}
