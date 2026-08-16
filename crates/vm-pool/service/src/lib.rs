//! vm-pool service library — VM pool manager with Unix socket API.
//!
//! Provides [`ServiceConfig`] and [`run_service`] for configurable deployment,
//! plus [`handle_command`] for direct testing of command handling.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info};
use vm_pool_manager::{
    EventLog, EventPayload, ImageRef, NoRuntime, Pool, PoolConfig, ServiceState, SnapshotStore,
    VmRuntime,
};
use vm_pool_protocol::{
    AppProtocol, NullProtocol, Request, Response, ServiceCommand, ServiceEvent,
};

/// Recover just the `id` field from a request line that failed to parse as a
/// full [`Request`], so the error response can still be correlated.
fn parse_request_id(line: &str) -> Option<u64> {
    #[derive(serde::Deserialize)]
    struct IdOnly {
        id: Option<u64>,
    }
    serde_json::from_str::<IdOnly>(line).ok().and_then(|v| v.id)
}

/// Configuration for the vm-pool service.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Path for the Unix socket.
    pub socket_path: PathBuf,
    /// Directory for snapshot storage.
    pub snapshot_dir: PathBuf,
    /// Pool configuration.
    pub pool: PoolConfig,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        let state_dir = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("vm-pool");

        Self {
            socket_path: PathBuf::from("/tmp/vm-pool.sock"),
            snapshot_dir: state_dir.join("snapshots"),
            pool: PoolConfig::default(),
        }
    }
}

/// Shared state for all connection handlers.
pub struct Service<R = NoRuntime, P: AppProtocol = NullProtocol>
where
    R: VmRuntime<P>,
{
    pub pool: Arc<Pool<R, P>>,
    pub events: Arc<EventLog<P>>,
    pub snapshots: SnapshotStore,
    pub config: ServiceConfig,
}

impl<P: AppProtocol> Service<NoRuntime, P> {
    /// Create a new service with the given configuration (no VM runtime backend).
    pub async fn new(config: ServiceConfig) -> anyhow::Result<Arc<Self>> {
        let events = EventLog::<P>::new();
        let pool = Pool::new(config.pool.clone(), events.clone());
        let snapshots = SnapshotStore::new(&config.snapshot_dir);
        snapshots.init().await?;

        Ok(Arc::new(Self {
            pool,
            events,
            snapshots,
            config,
        }))
    }
}

impl<R: VmRuntime<P>, P: AppProtocol> Service<R, P> {
    /// Create a new service with a specific runtime backend.
    pub async fn with_runtime(config: ServiceConfig, runtime: R) -> anyhow::Result<Arc<Self>> {
        let events = EventLog::<P>::new();
        let pool = Pool::with_runtime(config.pool.clone(), events.clone(), runtime);
        let snapshots = SnapshotStore::new(&config.snapshot_dir);
        snapshots.init().await?;

        Ok(Arc::new(Self {
            pool,
            events,
            snapshots,
            config,
        }))
    }

    /// Run the service, listening for connections on the Unix socket.
    /// This blocks until the listener encounters a fatal error.
    pub async fn run(self: &Arc<Self>) -> anyhow::Result<()> {
        let socket_path = &self.config.socket_path;

        // Clean up old socket
        let _ = std::fs::remove_file(socket_path);

        self.events
            .append(EventPayload::Service {
                state: ServiceState::Starting,
            })
            .await;

        let listener = UnixListener::bind(socket_path)?;
        info!("listening on {}", socket_path.display());

        self.events
            .append(EventPayload::Service {
                state: ServiceState::Ready,
            })
            .await;

        // Spawn health check loop
        let pool_for_health = self.pool.clone();
        let health_interval = self.config.pool.health_check_interval;
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(health_interval));
            loop {
                interval.tick().await;
                pool_for_health.health_check().await;
            }
        });

        // Accept connections
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let svc = Arc::clone(self);
                    tokio::spawn(async move { svc.handle_connection(stream).await });
                }
                Err(e) => {
                    error!("accept error: {}", e);
                }
            }
        }
    }

    async fn handle_connection(self: Arc<Self>, stream: UnixStream) {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let mut event_rx = self.events.subscribe();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<Response<P>>(64);

        // Forward VM application events to client. These are unsolicited —
        // they carry no request id, so clients route them to their event
        // stream rather than to a pending request.
        let response_tx_for_events = response_tx.clone();
        tokio::spawn(async move {
            while let Ok(event) = event_rx.recv().await {
                let seq = event.seq;
                if let EventPayload::VmApp {
                    vm_id,
                    event: app_event,
                } = event.payload
                {
                    // The same seq the event log assigned, so a client that
                    // attached can tell which live events its replay already
                    // covered.
                    let pushed = Response::push(ServiceEvent::VmApp {
                        vm_id,
                        event: app_event,
                        seq,
                    });
                    if response_tx_for_events.send(pushed).await.is_err() {
                        break;
                    }
                }
            }
        });

        // Response writer
        tokio::spawn(async move {
            while let Some(response) = response_rx.recv().await {
                if let Ok(json) = serde_json::to_string(&response) {
                    if writer.write_all(json.as_bytes()).await.is_err() {
                        break;
                    }
                    if writer.write_all(b"\n").await.is_err() {
                        break;
                    }
                    let _ = writer.flush().await;
                }
            }
        });

        // Command read loop
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let request: Request<P> = match serde_json::from_str(line.trim()) {
                        Ok(req) => req,
                        Err(e) => {
                            error!("invalid request: {}", e);
                            // Best effort: recover the id so the client can
                            // fail the right pending request instead of
                            // waiting forever.
                            let id = parse_request_id(line.trim());
                            let err = ServiceEvent::Error {
                                message: format!("invalid request: {e}"),
                            };
                            let response = match id {
                                Some(id) => Response::to_request(id, err),
                                None => Response::push(err),
                            };
                            let _ = response_tx.send(response).await;
                            continue;
                        }
                    };

                    let id = request.id;
                    let event = self.handle_command(request.command).await;
                    if response_tx
                        .send(Response::to_request(id, event))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    error!("read error: {}", e);
                    break;
                }
            }
        }
    }

    /// Handle a single command and return a response event.
    /// Public for direct testing without a socket.
    pub async fn handle_command(&self, command: ServiceCommand<P>) -> ServiceEvent<P> {
        match command {
            ServiceCommand::Status => {
                let status = self.pool.status().await;
                ServiceEvent::PoolStatus {
                    total: status.total,
                    available: status.available,
                    allocated: status.allocated,
                    protocol_version: vm_pool_protocol::PROTOCOL_VERSION,
                }
            }

            ServiceCommand::Allocate { image, config } => {
                let image_ref =
                    ImageRef::parse(&image).unwrap_or_else(|| ImageRef::new(&image, "latest"));

                match self.pool.allocate(image_ref, config).await {
                    Ok(vm_id) => ServiceEvent::VmAllocated { vm_id, image },
                    Err(e) => ServiceEvent::Error {
                        message: format!("allocate failed: {e}"),
                    },
                }
            }

            ServiceCommand::Deallocate { vm_id } => match self.pool.deallocate(&vm_id).await {
                Ok(()) => ServiceEvent::VmStopped { vm_id },
                Err(e) => ServiceEvent::Error {
                    message: format!("deallocate failed: {e}"),
                },
            },

            ServiceCommand::Send { vm_id, command } => {
                match self.pool.send_to_vm(&vm_id, command).await {
                    Ok(()) => {
                        // Command was forwarded; events will arrive via the event stream.
                        ServiceEvent::CommandSent { vm_id }
                    }
                    Err(e) => ServiceEvent::Error {
                        message: format!("send failed: {e}"),
                    },
                }
            }

            ServiceCommand::Snapshot { vm_id, name } => match self.pool.get(&vm_id).await {
                Some(_) => match self.snapshots.save(&vm_id, &name, "unknown").await {
                    Ok(_) => {
                        info!(%vm_id, name, "snapshot saved");
                        ServiceEvent::VmStopped { vm_id }
                    }
                    Err(e) => ServiceEvent::Error {
                        message: format!("snapshot failed: {e}"),
                    },
                },
                None => ServiceEvent::Error {
                    message: format!("VM not found: {vm_id}"),
                },
            },

            ServiceCommand::Restore { vm_id, snapshot } => {
                match self.snapshots.restore(&vm_id, &snapshot).await {
                    Ok(_path) => {
                        info!(%vm_id, snapshot, "restore initiated");
                        ServiceEvent::VmReady { vm_id }
                    }
                    Err(e) => ServiceEvent::Error {
                        message: format!("restore failed: {e}"),
                    },
                }
            }

            ServiceCommand::TailLogs { vm_id, lines } => {
                let log_lines = self.events.tail_vm_logs(&vm_id, lines).await;
                let lines = log_lines
                    .into_iter()
                    .map(|(stream, line, timestamp)| vm_pool_protocol::LogLine {
                        stream,
                        line,
                        timestamp,
                    })
                    .collect();
                ServiceEvent::LogTail { vm_id, lines }
            }

            ServiceCommand::SubscribeLogs { vm_id } => {
                info!(?vm_id, "subscribe to logs");
                ServiceEvent::LogsSubscribed { vm_id }
            }

            ServiceCommand::UnsubscribeLogs => {
                info!("unsubscribe from logs");
                ServiceEvent::LogsSubscribed { vm_id: None }
            }

            ServiceCommand::Attach {
                vm_id,
                since_seq,
                limit,
            } => {
                // Presence BEFORE the replay, and the order is load-bearing.
                // A VM that finishes between the two reads then reports
                // `present: true` plus its terminal event, and the caller
                // reads the outcome. Reversed, the same race reports a VM
                // that is gone with a replay taken before it ended — work
                // written off that had in fact concluded.
                let present = self.pool.get(&vm_id).await.is_some();
                let (events, dropped) = self
                    .events
                    .app_events_for_vm(&vm_id, since_seq, limit)
                    .await;
                info!(
                    %vm_id,
                    present,
                    replayed = events.len(),
                    dropped,
                    "client attached to a VM"
                );
                ServiceEvent::VmAttached {
                    vm_id,
                    present,
                    replay: events
                        .into_iter()
                        .map(|(seq, event)| vm_pool_protocol::ReplayedEvent { seq, event })
                        .collect(),
                    dropped,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_pool_protocol::{ShellCommand, ShellProtocol, VmConfig, VmId};

    async fn test_service() -> Arc<Service<NoRuntime, ShellProtocol>> {
        let dir = tempfile::tempdir().unwrap();
        let config = ServiceConfig {
            socket_path: dir.path().join("test.sock"),
            snapshot_dir: dir.path().join("snapshots"),
            pool: PoolConfig {
                max_vms: 3,
                health_check_interval: 30,
                vm_timeout: 7200,
            },
        };
        // Leak the tempdir so it lives for the test
        std::mem::forget(dir);
        Service::<NoRuntime, ShellProtocol>::new(config)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn handle_status() {
        let svc = test_service().await;
        let resp = svc.handle_command(ServiceCommand::Status).await;
        assert_eq!(
            resp,
            ServiceEvent::PoolStatus {
                total: 3,
                available: 3,
                allocated: 0,
                protocol_version: vm_pool_protocol::PROTOCOL_VERSION,
            }
        );
    }

    #[tokio::test]
    async fn handle_allocate_and_deallocate() {
        let svc = test_service().await;

        // Allocate
        let resp = svc
            .handle_command(ServiceCommand::Allocate {
                image: "agent:v1".into(),
                config: VmConfig::default(),
            })
            .await;

        let vm_id = match &resp {
            ServiceEvent::VmAllocated { vm_id, image } => {
                assert_eq!(image, "agent:v1");
                vm_id.clone()
            }
            other => panic!("expected VmAllocated, got {:?}", other),
        };

        // Verify status
        let resp = svc.handle_command(ServiceCommand::Status).await;
        assert_eq!(
            resp,
            ServiceEvent::PoolStatus {
                total: 3,
                available: 2,
                allocated: 1,
                protocol_version: vm_pool_protocol::PROTOCOL_VERSION,
            }
        );

        // Deallocate
        let resp = svc
            .handle_command(ServiceCommand::Deallocate {
                vm_id: vm_id.clone(),
            })
            .await;
        assert_eq!(resp, ServiceEvent::VmStopped { vm_id });

        // Back to full capacity
        let resp = svc.handle_command(ServiceCommand::Status).await;
        assert_eq!(
            resp,
            ServiceEvent::PoolStatus {
                total: 3,
                available: 3,
                allocated: 0,
                protocol_version: vm_pool_protocol::PROTOCOL_VERSION,
            }
        );
    }

    #[tokio::test]
    async fn handle_allocate_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let config = ServiceConfig {
            socket_path: dir.path().join("test.sock"),
            snapshot_dir: dir.path().join("snapshots"),
            pool: PoolConfig {
                max_vms: 1,
                health_check_interval: 30,
                vm_timeout: 7200,
            },
        };
        std::mem::forget(dir);
        let svc = Service::<NoRuntime, ShellProtocol>::new(config)
            .await
            .unwrap();

        svc.handle_command(ServiceCommand::Allocate {
            image: "agent:v1".into(),
            config: VmConfig::default(),
        })
        .await;

        let resp = svc
            .handle_command(ServiceCommand::Allocate {
                image: "agent:v1".into(),
                config: VmConfig::default(),
            })
            .await;

        match resp {
            ServiceEvent::Error { message } => {
                assert!(message.contains("exhausted"), "got: {message}");
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_deallocate_not_found() {
        let svc = test_service().await;
        let resp = svc
            .handle_command(ServiceCommand::Deallocate {
                vm_id: VmId::new("vm-nonexistent"),
            })
            .await;
        match resp {
            ServiceEvent::Error { message } => {
                assert!(message.contains("not found"), "got: {message}");
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_send_vm_not_found() {
        let svc = test_service().await;
        let resp = svc
            .handle_command(ServiceCommand::Send {
                vm_id: VmId::new("vm-nope"),
                command: ShellCommand::Execute {
                    command: "test".into(),
                },
            })
            .await;
        match resp {
            ServiceEvent::Error { message } => {
                assert!(message.contains("not found"), "got: {message}");
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_snapshot_vm_not_found() {
        let svc = test_service().await;
        let resp = svc
            .handle_command(ServiceCommand::Snapshot {
                vm_id: VmId::new("vm-nope"),
                name: "snap".into(),
            })
            .await;
        match resp {
            ServiceEvent::Error { message } => {
                assert!(message.contains("not found"), "got: {message}");
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_tail_logs_empty() {
        let svc = test_service().await;
        let resp = svc
            .handle_command(ServiceCommand::TailLogs {
                vm_id: VmId::new("vm-1"),
                lines: 10,
            })
            .await;
        match resp {
            ServiceEvent::LogTail { lines, .. } => {
                assert!(lines.is_empty());
            }
            other => panic!("expected LogTail, got {:?}", other),
        }
    }

    /// Attach answers the two questions a reattaching client has: is the VM
    /// still here, and what did I miss.
    #[tokio::test]
    async fn handle_attach_reports_presence_and_replays_events() {
        use vm_pool_manager::EventPayload;
        use vm_pool_protocol::{OutputStream, ShellEvent};

        let svc = test_service().await;
        let resp = svc
            .handle_command(ServiceCommand::Allocate {
                image: "agent:v1".into(),
                config: VmConfig::default(),
            })
            .await;
        let ServiceEvent::VmAllocated { vm_id, .. } = resp else {
            panic!("expected VmAllocated, got {resp:?}");
        };

        for i in 0..3 {
            svc.events
                .append(EventPayload::VmApp {
                    vm_id: vm_id.clone(),
                    event: ShellEvent::Output {
                        stream: OutputStream::Stdout,
                        data: format!("line {i}"),
                    },
                })
                .await;
        }

        let resp = svc
            .handle_command(ServiceCommand::Attach {
                vm_id: vm_id.clone(),
                since_seq: 0,
                limit: 2,
            })
            .await;
        match resp {
            ServiceEvent::VmAttached {
                vm_id: id,
                present,
                replay,
                dropped,
            } => {
                assert_eq!(id, vm_id);
                assert!(present, "the pool still holds it");
                assert_eq!(dropped, 1, "the limit cut the oldest");
                assert_eq!(replay.len(), 2);
                assert_eq!(
                    replay[1].event,
                    ShellEvent::Output {
                        stream: OutputStream::Stdout,
                        data: "line 2".into(),
                    }
                );
                assert!(replay[0].seq < replay[1].seq);
            }
            other => panic!("expected VmAttached, got {other:?}"),
        }

        // Deallocated: gone from the pool, but the log still has the events —
        // which is why `present: false` alone is not "lost".
        svc.handle_command(ServiceCommand::Deallocate {
            vm_id: vm_id.clone(),
        })
        .await;
        let resp = svc
            .handle_command(ServiceCommand::Attach {
                vm_id: vm_id.clone(),
                since_seq: 0,
                limit: 100,
            })
            .await;
        match resp {
            ServiceEvent::VmAttached {
                present, replay, ..
            } => {
                assert!(!present);
                assert_eq!(replay.len(), 3, "the log outlives the VM");
            }
            other => panic!("expected VmAttached, got {other:?}"),
        }
    }

    /// A VM nobody ever allocated: absent, with nothing to replay. Not an
    /// error — the caller decides what that means for its own work.
    #[tokio::test]
    async fn handle_attach_to_an_unknown_vm_is_empty_not_an_error() {
        let svc = test_service().await;
        let resp = svc
            .handle_command(ServiceCommand::Attach {
                vm_id: VmId::new("vm-never-existed"),
                since_seq: 0,
                limit: 10,
            })
            .await;
        match resp {
            ServiceEvent::VmAttached {
                present,
                replay,
                dropped,
                ..
            } => {
                assert!(!present);
                assert!(replay.is_empty());
                assert_eq!(dropped, 0);
            }
            other => panic!("expected VmAttached, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_subscribe_unsubscribe() {
        let svc = test_service().await;

        let resp = svc
            .handle_command(ServiceCommand::SubscribeLogs {
                vm_id: Some(VmId::new("vm-1")),
            })
            .await;
        assert_eq!(
            resp,
            ServiceEvent::LogsSubscribed {
                vm_id: Some(VmId::new("vm-1"))
            }
        );

        let resp = svc.handle_command(ServiceCommand::UnsubscribeLogs).await;
        assert_eq!(resp, ServiceEvent::LogsSubscribed { vm_id: None });
    }
}
