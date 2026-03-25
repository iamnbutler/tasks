//! Tasks Desktop — GPUI + gpuikit native client.

pub mod api;
pub(crate) mod sse;
pub mod state;

pub use api::{ApiClient, ApiError};
pub use sse::{SseClient, SseClientEvent, SseConnectionState, SseFilters};
pub use state::{AppState, AppStateEvent, ConnectionStatus, create_app_state};

use std::sync::OnceLock;

/// Background tokio runtime for reqwest.
///
/// GPUI runs on smol, but reqwest needs a tokio reactor. This runtime
/// stays alive for the process lifetime. Call `install_tokio()` at startup
/// to enter its context on the main thread; smol worker threads inherit it.
static TOKIO_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

pub(crate) fn tokio_runtime() -> &'static tokio::runtime::Runtime {
    TOKIO_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to create tokio runtime")
    })
}

/// Enter the tokio runtime context. Call once at startup before any HTTP calls.
/// Returns a guard that must be held for the lifetime of the application.
pub fn install_tokio() -> tokio::runtime::EnterGuard<'static> {
    tokio_runtime().enter()
}
