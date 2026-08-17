//! vm-pool service binary entry point.

use anyhow::Result;
use tracing::info;
use vm_pool_manager::NoRuntime;
use vm_pool_protocol::ShellProtocol;
use vm_pool_service::{MAX_VMS_ENV, Service, ServiceConfig};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .init();

    // Resolved before anything binds, so an unusable value costs an error
    // message rather than a daemon that is up and the wrong size.
    let config = ServiceConfig::from_env()?;
    info!(
        max_vms = config.pool.max_vms,
        var = MAX_VMS_ENV,
        "pool capacity"
    );
    let service = Service::<NoRuntime, ShellProtocol>::new(config).await?;
    service.run().await
}
