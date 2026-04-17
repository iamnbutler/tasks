//! tasks server entry point.
//!
//! The server is intentionally minimal at this step — opens the store and exits.
//! Later steps wire up the GitHub poller, vm-pool client, orchestrator, and HTTP API.

use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing::info;

use tasks::store::Store;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
        )
        .init();

    let data_dir = data_dir()?;
    tokio::fs::create_dir_all(&data_dir)
        .await
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    let db_path = data_dir.join("tasks.db");

    info!(db = %db_path.display(), "opening store");
    let store = Store::open(&db_path).await?;
    let mode = store.get_mode().await?;
    info!(?mode, "store ready");

    Ok(())
}

fn data_dir() -> Result<PathBuf> {
    if let Ok(s) = std::env::var("TASKS_DATA_DIR") {
        return Ok(PathBuf::from(s));
    }
    let home = dirs_home()?;
    Ok(home.join(".local/state/tasks-v2"))
}

fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME environment variable not set")
}
