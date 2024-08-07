use color_eyre::Result;
use std::net::IpAddr;
use std::time::Duration;

#[async_trait::async_trait]
pub trait Provider {
    async fn start_instance(&self, id: String) -> Result<IpAddr>;
    async fn wait_until_termination_signal(&self) -> Result<Duration>;
}
