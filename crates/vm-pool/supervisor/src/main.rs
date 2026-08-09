//! Supervisor binary — shell execution supervisor for VMs.
//!
//! Uses [`ShellProtocol`]: receives `Execute` commands, emits `Output` and
//! `CommandCompleted` events.

use anyhow::Result;
use tracing::info;
use vm_pool_protocol::{OutputStream, ShellCommand, ShellEvent, ShellProtocol};
use vm_pool_supervisor::run_supervisor;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .init();

    info!("supervisor starting (shell protocol)");

    run_supervisor::<ShellProtocol, _, _>(|cmd| async move {
        match cmd {
            ShellCommand::Execute { command } => execute_command(&command).await,
        }
    })
    .await
}

async fn execute_command(command: &str) -> Vec<ShellEvent> {
    info!("executing: {}", command);
    let mut events = Vec::new();

    match tokio::process::Command::new("sh")
        .args(["-c", command])
        .output()
        .await
    {
        Ok(output) => {
            let stdout_data = String::from_utf8_lossy(&output.stdout);
            if !stdout_data.is_empty() {
                events.push(ShellEvent::Output {
                    stream: OutputStream::Stdout,
                    data: stdout_data.into_owned(),
                });
            }

            let stderr_data = String::from_utf8_lossy(&output.stderr);
            if !stderr_data.is_empty() {
                events.push(ShellEvent::Output {
                    stream: OutputStream::Stderr,
                    data: stderr_data.into_owned(),
                });
            }

            let exit_code = output.status.code().unwrap_or(-1);
            events.push(ShellEvent::CommandCompleted { exit_code });
        }
        Err(e) => {
            events.push(ShellEvent::Output {
                stream: OutputStream::Stderr,
                data: format!("failed to execute command: {e}\n"),
            });
            events.push(ShellEvent::CommandCompleted { exit_code: -1 });
        }
    }

    events
}
