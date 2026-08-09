//! Supervisor library — infrastructure loop for PID 1 inside VMs.
//!
//! Provides [`run_supervisor`] which handles infrastructure commands
//! (Ping, Shutdown) and delegates application commands to a handler.

use std::future::Future;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{error, info};
use vm_pool_protocol::{AppProtocol, VmCommand, VmEvent};

/// Run the supervisor event loop.
///
/// Handles infrastructure commands (Ping → Pong, Shutdown → exit).
/// Forwards App commands to `handler`, which returns the events to emit.
pub async fn run_supervisor<P, H, Fut>(mut handler: H) -> Result<()>
where
    P: AppProtocol,
    H: FnMut(P::Command) -> Fut,
    Fut: Future<Output = Vec<P::Event>>,
{
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);

    emit_event::<P>(&mut stdout, VmEvent::Ready).await?;

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            info!("stdin closed, shutting down");
            break;
        }

        let command: VmCommand<P> = match serde_json::from_str(line.trim()) {
            Ok(cmd) => cmd,
            Err(e) => {
                error!("failed to parse command: {}", e);
                continue;
            }
        };

        match command {
            VmCommand::Ping => {
                emit_event::<P>(&mut stdout, VmEvent::Pong).await?;
            }
            VmCommand::Shutdown => {
                info!("shutdown requested");
                emit_event::<P>(&mut stdout, VmEvent::Shutdown).await?;
                break;
            }
            VmCommand::App { payload } => {
                let events = handler(payload).await;
                for event in events {
                    emit_event::<P>(&mut stdout, VmEvent::App { payload: event }).await?;
                }
            }
        }
    }

    info!("supervisor exiting");
    Ok(())
}

async fn emit_event<P: AppProtocol>(
    stdout: &mut tokio::io::Stdout,
    event: VmEvent<P>,
) -> Result<()> {
    let json = serde_json::to_string(&event)?;
    stdout.write_all(json.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}
