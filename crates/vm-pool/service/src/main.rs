//! vm-pool service binary entry point.

use anyhow::Result;
use vm_pool_manager::NoRuntime;
use vm_pool_protocol::ShellProtocol;
use vm_pool_service::{Service, ServiceConfig};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .init();

    let config = ServiceConfig::default();
    let service = Service::<NoRuntime, ShellProtocol>::new(config).await?;
    service.run().await
}
