//! Host-side communication with VMs over stdio.
//!
//! [`VmTransport`] spawns a child process and communicates via JSON-line
//! protocol over stdin/stdout.

use std::process::Stdio;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use tracing::{debug, error};
use vm_pool_protocol::{AppProtocol, NullProtocol, VmCommand, VmEvent};

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("transport closed")]
    Closed,
    #[error("send failed: receiver dropped")]
    SendFailed,
}

/// Handle for communicating with a VM process over stdio.
pub struct VmTransport<P: AppProtocol = NullProtocol> {
    stdin: ChildStdin,
    events_rx: mpsc::Receiver<VmEvent<P>>,
    child: Child,
}

impl<P: AppProtocol> VmTransport<P> {
    /// Spawn a new process and set up JSON-line communication.
    pub async fn spawn(command: &str, args: &[&str]) -> Result<Self, TransportError> {
        let mut child = tokio::process::Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "failed to get stdin"))?;

        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "failed to get stdout")
        })?;

        let (events_tx, events_rx) = mpsc::channel(64);
        tokio::spawn(read_events::<P>(stdout, events_tx));

        Ok(Self {
            stdin,
            events_rx,
            child,
        })
    }

    /// Send a command to the VM.
    pub async fn send(&mut self, command: &VmCommand<P>) -> Result<(), TransportError> {
        let json = serde_json::to_string(command)?;
        debug!("sending command: {}", json);
        self.stdin.write_all(json.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Receive the next event from the VM.
    pub async fn recv(&mut self) -> Option<VmEvent<P>> {
        self.events_rx.recv().await
    }

    /// Try to receive an event without blocking.
    pub fn try_recv(&mut self) -> Option<VmEvent<P>> {
        self.events_rx.try_recv().ok()
    }

    /// Close the transport by dropping stdin and waiting for the child.
    pub async fn close(mut self) -> Result<std::process::ExitStatus, TransportError> {
        drop(self.stdin);
        let status = self.child.wait().await?;
        Ok(status)
    }

    /// Kill the child process.
    pub async fn kill(&mut self) -> Result<(), TransportError> {
        self.child.kill().await?;
        Ok(())
    }

    /// Check if the child process has exited.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, TransportError> {
        Ok(self.child.try_wait()?)
    }
}

/// Read events from a child's stdout, parsing JSON lines.
async fn read_events<P: AppProtocol>(stdout: ChildStdout, tx: mpsc::Sender<VmEvent<P>>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                debug!("stdout closed");
                break;
            }
            Ok(_) => match serde_json::from_str::<VmEvent<P>>(line.trim()) {
                Ok(event) => {
                    debug!("received event: {:?}", event);
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    error!("failed to parse event: {}", e);
                }
            },
            Err(e) => {
                error!("read error: {}", e);
                break;
            }
        }
    }
}

/// Build the supervisor binary and return its path.
/// Used by tests and by the container runtime for copying into images.
pub fn find_supervisor_binary() -> Option<std::path::PathBuf> {
    // Check common locations
    let candidates = [
        std::env::current_dir()
            .ok()?
            .join("target/debug/supervisor"),
        std::env::current_dir()
            .ok()?
            .join("target/release/supervisor"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_pool_protocol::{OutputStream, ShellCommand, ShellEvent, ShellProtocol};

    /// Build the supervisor and return its path.
    async fn build_supervisor() -> std::path::PathBuf {
        let output = tokio::process::Command::new("cargo")
            .args(["build", "-p", "vm-pool-supervisor", "--message-format=json"])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "supervisor build failed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find_map(|msg| {
                if msg.get("reason")?.as_str()? == "compiler-artifact"
                    && msg.get("target")?.get("name")?.as_str()? == "supervisor"
                {
                    Some(std::path::PathBuf::from(msg.get("executable")?.as_str()?))
                } else {
                    None
                }
            })
            .expect("could not find supervisor binary path in cargo output")
    }

    #[tokio::test]
    async fn supervisor_ping_pong() {
        let binary = build_supervisor().await;
        let mut transport = VmTransport::<ShellProtocol>::spawn(binary.to_str().unwrap(), &[])
            .await
            .unwrap();

        assert_eq!(transport.recv().await.unwrap(), VmEvent::Ready);

        transport.send(&VmCommand::Ping).await.unwrap();
        assert_eq!(transport.recv().await.unwrap(), VmEvent::Pong);

        transport.send(&VmCommand::Shutdown).await.unwrap();
        assert_eq!(transport.recv().await.unwrap(), VmEvent::Shutdown);

        let status = transport.close().await.unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn supervisor_execute_with_output() {
        let binary = build_supervisor().await;
        let mut transport = VmTransport::<ShellProtocol>::spawn(binary.to_str().unwrap(), &[])
            .await
            .unwrap();

        assert_eq!(transport.recv().await.unwrap(), VmEvent::Ready);

        transport
            .send(&VmCommand::App {
                payload: ShellCommand::Execute {
                    command: "echo hello world".into(),
                },
            })
            .await
            .unwrap();

        let event = transport.recv().await.unwrap();
        match &event {
            VmEvent::App {
                payload: ShellEvent::Output { stream, data },
            } => {
                assert_eq!(*stream, OutputStream::Stdout);
                assert_eq!(data.trim(), "hello world");
            }
            other => panic!("expected App(Output), got {:?}", other),
        }

        match transport.recv().await.unwrap() {
            VmEvent::App {
                payload: ShellEvent::CommandCompleted { exit_code },
            } => assert_eq!(exit_code, 0),
            other => panic!("expected App(CommandCompleted), got {:?}", other),
        }

        transport.send(&VmCommand::Shutdown).await.unwrap();
        assert_eq!(transport.recv().await.unwrap(), VmEvent::Shutdown);
        assert!(transport.close().await.unwrap().success());
    }

    #[tokio::test]
    async fn supervisor_execute_nonzero_exit() {
        let binary = build_supervisor().await;
        let mut transport = VmTransport::<ShellProtocol>::spawn(binary.to_str().unwrap(), &[])
            .await
            .unwrap();

        assert_eq!(transport.recv().await.unwrap(), VmEvent::Ready);

        transport
            .send(&VmCommand::App {
                payload: ShellCommand::Execute {
                    command: "exit 42".into(),
                },
            })
            .await
            .unwrap();

        match transport.recv().await.unwrap() {
            VmEvent::App {
                payload: ShellEvent::CommandCompleted { exit_code },
            } => assert_eq!(exit_code, 42),
            other => panic!("expected App(CommandCompleted), got {:?}", other),
        }

        transport.send(&VmCommand::Shutdown).await.unwrap();
        assert_eq!(transport.recv().await.unwrap(), VmEvent::Shutdown);
        assert!(transport.close().await.unwrap().success());
    }
}
