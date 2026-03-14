//! Main run loop — wires all components together.
//!
//! This is intentionally thin — the logic lives in the library crates.

use crate::config::AppConfig;

/// Run the Tasks platform.
///
/// Constructs all components and starts the GitHub poll loop,
/// dispatch tick loop, and session management.
pub async fn run(config: AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Tasks platform starting...");
    eprintln!("  max_sessions: {}", config.max_sessions);
    eprintln!("  poll_interval: {:?}", config.poll_interval);
    eprintln!("  dispatch_interval: {:?}", config.dispatch_interval);
    eprintln!("  container_image: {}", config.container_image);

    // TODO: construct components and start loops (Task 12)

    Ok(())
}
