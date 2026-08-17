//! vm-pool service library — VM pool manager with Unix socket API.
//!
//! Provides [`ServiceConfig`] and [`run_service`] for configurable deployment,
//! plus [`handle_command`] for direct testing of command handling.

use std::path::PathBuf;
use std::sync::Arc;

use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info, warn};
use vm_pool_manager::{
    DEFAULT_MAX_VMS, EventLog, EventPayload, ImageRef, NoRuntime, Pool, PoolConfig, ServiceState,
    SnapshotStore, VmRuntime,
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

impl ServiceConfig {
    /// [`Default`], with [`PoolConfig::max_vms`] taken from [`MAX_VMS_ENV`].
    ///
    /// Deliberately *not* what `default()` does. `default()` is what tests and
    /// embedders build a config with, and one that reads the ambient
    /// environment lets whoever's shell happens to be running the suite decide
    /// what it asserts.
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            pool: PoolConfig {
                max_vms: max_vms_from_env()?,
                ..PoolConfig::default()
            },
            ..Self::default()
        })
    }
}

/// The environment variable that sizes the pool.
pub const MAX_VMS_ENV: &str = "VM_POOL_MAX_VMS";

/// Why a configuration value could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "{MAX_VMS_ENV} must be a positive integer (the number of VMs this pool may hold \
         at once); got {value:?}"
    )]
    MaxVms { value: String },
}

/// Resolve [`PoolConfig::max_vms`] from [`MAX_VMS_ENV`].
///
/// Public and separate from [`ServiceConfig::from_env`] on purpose: the service
/// has two entry points — the stock `vm-pool` binary, and any embedder that
/// hand-builds a `ServiceConfig` because it needs a runtime or an app protocol
/// this crate's `main` cannot name — and a knob only one of them honours is
/// worse than no knob at all, because it is documented and ignored.
///
/// Unset, empty or whitespace reads as "not configured" and yields
/// [`DEFAULT_MAX_VMS`]. Anything else that is not a positive integer is an
/// error rather than a fallback: `0` binds the socket and answers `status`
/// cheerfully while failing *every* allocate, and a typo silently running a
/// capacity nobody chose is the failure this knob exists to end.
pub fn max_vms_from_env() -> Result<usize, ConfigError> {
    max_vms_from(std::env::var(MAX_VMS_ENV).ok())
}

/// The pure half of [`max_vms_from_env`], so tests never touch the process
/// environment (`set_var` is `unsafe` in edition 2024 and races every other
/// thread in the test binary).
fn max_vms_from(value: Option<String>) -> Result<usize, ConfigError> {
    let Some(raw) = value else {
        return Ok(DEFAULT_MAX_VMS);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_MAX_VMS);
    }
    match trimmed.parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(ConfigError::MaxVms { value: raw }),
    }
}

/// How long [`bind_socket`]'s probe waits for a connect to be answered before
/// it treats the path as occupied. The answer comes out of the kernel's listen
/// backlog, not from the daemon's accept loop, so this is generous for a live
/// socket and only ever bites on something pathological — which is exactly the
/// case that must not be taken over.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Why a socket path could not be bound.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// Something answered a connect on the path. It has an owner, and this
    /// process is not it.
    #[error(
        "another vm-pool is already listening on {0} — refusing to start a second one. \
         Stop the running daemon first (its VMs are still its own), or point this one \
         at a different VM_POOL_SOCKET"
    )]
    AlreadyRunning(PathBuf),
    /// Something is at the path that is not a socket. The old code would have
    /// deleted it.
    #[error("{0} exists and is not a socket — refusing to remove it")]
    NotASocket(PathBuf),
    #[error("could not remove the stale socket at {path}")]
    Unlink {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not bind {path}")]
    Bind {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Whether something is listening on `path` right now.
///
/// **Every unreadable answer counts as occupied.** Only `ECONNREFUSED` (a
/// socket file whose daemon is gone) and `ENOENT` (nothing there at all) are
/// "free"; a third-kind io error, or a connect that does not return inside
/// [`PROBE_TIMEOUT`], reads as occupied and says why. The asymmetry is the
/// whole design: a wrong refusal costs one error message and one restart, a
/// wrong takeover costs the running daemon every VM it holds and every run
/// those VMs are carrying.
async fn probe(path: &Path) -> bool {
    match tokio::time::timeout(PROBE_TIMEOUT, UnixStream::connect(path)).await {
        Ok(Ok(_)) => true,
        Ok(Err(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            false
        }
        Ok(Err(e)) => {
            warn!(
                "could not tell whether {} has an owner ({e}); treating it as occupied",
                path.display()
            );
            true
        }
        Err(_) => {
            warn!(
                "connect to {} did not answer within {PROBE_TIMEOUT:?}; treating it as occupied",
                path.display()
            );
            true
        }
    }
}

/// Bind the service's Unix socket, refusing to displace a live daemon.
///
/// The decision order is `symlink_metadata` → is-it-a-socket → probe → unlink
/// → bind. `symlink_metadata` rather than `metadata`, so a dangling symlink at
/// the path is not followed into a stat error and mistaken for something else.
///
/// This exists because the previous code unconditionally `remove_file`d the
/// path and then bound it, which silently displaced a running pool: the first
/// daemon went on listening on an unlinked inode — healthy, `pgrep`-able,
/// resolvable by `lsof` (which reads by path, and the path had been recreated
/// underneath it) and unreachable forever — while the server reconnected to
/// the path, found the *new* pool, and handed it the queued work. Neither std
/// nor tokio unlinks a listener's path on drop, which is what makes the stale
/// case real as well as testable: bind, drop, and the file is still there.
///
/// `pub` on purpose — it is the unit under test, and any future vm-pool entry
/// point should call it rather than re-deriving the rule.
pub async fn bind_socket(path: &Path) -> Result<UnixListener, BindError> {
    // `symlink_metadata`: a dangling symlink is a thing that exists, and
    // following it would report NotFound for a path that is not free.
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_socket() {
            return Err(BindError::NotASocket(path.to_path_buf()));
        }
        if probe(path).await {
            return Err(BindError::AlreadyRunning(path.to_path_buf()));
        }
        warn!(
            "removing stale socket {} (nothing is listening on it)",
            path.display()
        );
        std::fs::remove_file(path).map_err(|source| BindError::Unlink {
            path: path.to_path_buf(),
            source,
        })?;
    }

    UnixListener::bind(path).map_err(|source| BindError::Bind {
        path: path.to_path_buf(),
        source,
    })
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

        // The `Starting` append stays *ahead* of the bind, so a refusal is
        // still preceded by the state it was refused in.
        self.events
            .append(EventPayload::Service {
                state: ServiceState::Starting,
            })
            .await;

        let listener = bind_socket(socket_path).await?;
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

    #[test]
    fn an_unset_or_blank_pool_size_is_the_default() {
        assert_eq!(max_vms_from(None).unwrap(), DEFAULT_MAX_VMS);
        assert_eq!(max_vms_from(Some(String::new())).unwrap(), DEFAULT_MAX_VMS);
        assert_eq!(max_vms_from(Some("  ".into())).unwrap(), DEFAULT_MAX_VMS);
    }

    #[test]
    fn a_positive_integer_sizes_the_pool() {
        for (raw, expected) in [("1", 1), ("6", 6), ("12", 12), (" 9 ", 9)] {
            assert_eq!(max_vms_from(Some(raw.into())).unwrap(), expected, "{raw:?}");
        }
    }

    #[test]
    fn anything_that_is_not_a_positive_integer_refuses_to_start() {
        // `0` is the one that matters: it binds, answers `status`, and then
        // fails every allocate — silently reproducing the exhaustion this knob
        // exists to make configurable.
        for raw in ["0", "-1", "six", "3.5", "1_000", "6 vms"] {
            assert!(
                max_vms_from(Some(raw.into())).is_err(),
                "{raw:?} should be refused, not clamped or defaulted"
            );
        }
    }

    #[test]
    fn the_refusal_names_the_variable_and_the_value() {
        let message = max_vms_from(Some("six".into())).unwrap_err().to_string();
        assert!(message.contains(MAX_VMS_ENV), "{message}");
        assert!(message.contains("six"), "{message}");
        assert!(message.contains("positive integer"), "{message}");
    }

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

    /// The incident, as a test: a second daemon must not take the path from a
    /// live one. And the assertion that matters is the second half — the
    /// incumbent is still *reachable through the path* afterwards, which is
    /// precisely what the old unlink-then-bind destroyed while leaving the
    /// first process alive and looking healthy.
    #[tokio::test]
    async fn a_live_socket_is_refused_and_its_owner_stays_reachable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let incumbent = bind_socket(&path).await.expect("a free path binds");

        let err = bind_socket(&path)
            .await
            .expect_err("a live socket has an owner");
        assert!(matches!(err, BindError::AlreadyRunning(ref p) if *p == path));
        let message = err.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(
            message.contains("Stop the running daemon first"),
            "the reader does not yet know a first daemon exists: {message}"
        );

        // Still the incumbent's, and still answering on the path. No
        // `accept()` here on purpose: the kernel answers out of the listen
        // backlog, which is the same thing `probe` relies on.
        UnixStream::connect(&path)
            .await
            .expect("the first listener still owns the path");
        drop(incumbent);
    }

    /// The recovery that used to need a human with `rm`. Neither std nor tokio
    /// unlinks a listener's path on drop, so the file is asserted to survive
    /// the drop first — without that half the test could pass vacuously on a
    /// path that was simply free.
    #[tokio::test]
    async fn a_stale_socket_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        drop(bind_socket(&path).await.unwrap());
        assert!(path.exists(), "the socket file outlives its listener");

        let listener = bind_socket(&path).await.expect("a dead socket is stale");
        UnixStream::connect(&path)
            .await
            .expect("the reclaimed path answers");
        drop(listener);
    }

    /// The unconditional `remove_file` would have deleted a regular file a
    /// human put at the path. Its contents are checked afterwards, because
    /// "the call failed" and "the call failed without eating the file" are
    /// different claims.
    #[tokio::test]
    async fn a_regular_file_at_the_path_is_refused_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, "someone's notes").unwrap();

        let err = bind_socket(&path).await.expect_err("that is not a socket");
        assert!(matches!(err, BindError::NotASocket(ref p) if *p == path));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "someone's notes");
    }

    /// The refusal reaches `run()` rather than being swallowed inside it —
    /// the entry point both `vm-pool-service`'s `main` and `tasks vm-pool`
    /// go through.
    #[tokio::test]
    async fn run_surfaces_a_refused_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taken.sock");
        let _incumbent = bind_socket(&path).await.unwrap();

        let config = ServiceConfig {
            socket_path: path.clone(),
            snapshot_dir: dir.path().join("snapshots"),
            pool: PoolConfig::default(),
        };
        let svc = Service::<NoRuntime, ShellProtocol>::new(config)
            .await
            .unwrap();
        let err = svc.run().await.expect_err("the socket is occupied");
        assert!(
            err.to_string().contains("refusing to start a second one"),
            "{err}"
        );
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
