//! Tasks platform — main entry point.
//!
//! Constructs all components and runs the platform.
//! This binary is intentionally thin — logic lives in the library crates.

mod config;
mod run_loop;

use config::AppConfig;

#[tokio::main]
async fn main() {
    let config = match AppConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = run_loop::run(config).await {
        eprintln!("Fatal error: {e}");
        std::process::exit(1);
    }
}
