//! VM pool: allocation, limits, health monitoring, lifecycle.
//!
//! The pool manages VM instances through a [`VmRuntime`] trait that
//! abstracts the container backend. [`ContainerRuntime`] provides the
//! real implementation using apple/container.

pub mod events;
pub mod images;
pub mod snapshot;
pub mod transport;

pub use events::{Event, EventLog, EventPayload, InfraEvent, ServiceState, VmState};
pub use images::{ImageError, ImageMetadata, ImageRef, ImageStore, ImageType};
pub use snapshot::{SnapshotError, SnapshotMetadata, SnapshotStore};
pub use transport::{TransportError, VmTransport, find_supervisor_binary};

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, warn};
use vm_pool_protocol::{AppProtocol, NullProtocol, VmCommand, VmConfig, VmEvent, VmId};

#[derive(Debug, Error)]
pub enum PoolError {
    #[error("pool exhausted: {available} available, {requested} requested")]
    Exhausted { available: usize, requested: usize },
    #[error("VM not found: {0}")]
    VmNotFound(VmId),
    #[error("VM not ready: {0}")]
    VmNotReady(VmId),
    #[error("image error: {0}")]
    Image(#[from] ImageError),
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
    #[error("runtime error: {0}")]
    Runtime(String),
}

/// Handle to a running VM, providing command/event channels.
pub struct VmHandle<P: AppProtocol = NullProtocol> {
    pub command_tx: mpsc::Sender<VmCommand<P>>,
    pub event_rx: mpsc::Receiver<VmEvent<P>>,
}

/// Trait abstracting the VM container backend.
pub trait VmRuntime<P: AppProtocol = NullProtocol>: Send + Sync + 'static {
    fn start(
        &self,
        vm_id: &VmId,
        image: &ImageRef,
        config: &VmConfig,
    ) -> impl Future<Output = Result<VmHandle<P>, PoolError>> + Send;

    fn stop(&self, vm_id: &VmId) -> impl Future<Output = Result<(), PoolError>> + Send;
}

/// Real container runtime using apple/container CLI.
///
/// Starts VMs with `container run -i` and communicates via the
/// supervisor binary over JSON-line stdio.
pub struct ContainerRuntime {
    /// Transports keyed by VM ID, for stopping.
    transports: RwLock<HashMap<VmId, ()>>,
}

impl ContainerRuntime {
    pub fn new() -> Self {
        Self {
            transports: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for ContainerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: AppProtocol> VmRuntime<P> for ContainerRuntime {
    async fn start(
        &self,
        vm_id: &VmId,
        image: &ImageRef,
        config: &VmConfig,
    ) -> Result<VmHandle<P>, PoolError> {
        let image_tag = image.to_string();

        let cpus = config.cpus.unwrap_or(2).to_string();
        let memory = format!("{}M", config.memory_mb.unwrap_or(2048));

        let mut args: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "-i".into(),
            "--name".into(),
            vm_id.as_str().into(),
            "--cpus".into(),
            cpus,
            "--memory".into(),
            memory,
        ];

        for (key, value) in &config.env {
            args.push("-e".into());
            args.push(format!("{}={}", key, value));
        }

        args.push(image_tag);

        info!(%vm_id, ?args, "starting container");

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mut transport = VmTransport::<P>::spawn("container", &args_refs)
            .await
            .map_err(|e| PoolError::Runtime(format!("failed to spawn container: {e}")))?;

        // Wait for Ready event from supervisor
        let first_event =
            tokio::time::timeout(std::time::Duration::from_secs(60), transport.recv())
                .await
                .map_err(|_| PoolError::Runtime("timeout waiting for supervisor Ready".into()))?
                .ok_or_else(|| PoolError::Runtime("transport closed before Ready".into()))?;

        if !matches!(first_event, VmEvent::Ready) {
            return Err(PoolError::Runtime(format!(
                "expected Ready, got {:?}",
                first_event
            )));
        }

        // Set up command forwarding channels
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<VmCommand<P>>(64);
        let (evt_tx, evt_rx) = mpsc::channel::<VmEvent<P>>(64);

        // Bridge task: forward commands to transport, events from transport
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(command) => {
                                if let Err(e) = transport.send(&command).await {
                                    error!("failed to send command: {}", e);
                                    break;
                                }
                            }
                            None => {
                                // Command channel closed — shut down
                                let _ = transport.send(&VmCommand::<P>::Shutdown).await;
                                break;
                            }
                        }
                    }
                    event = transport.recv() => {
                        match event {
                            Some(evt) => {
                                if evt_tx.send(evt).await.is_err() {
                                    break;
                                }
                            }
                            None => {
                                // Transport closed
                                break;
                            }
                        }
                    }
                }
            }
            // Ensure cleanup
            let _ = transport.close().await;
        });

        self.transports.write().await.insert(vm_id.clone(), ());

        Ok(VmHandle {
            command_tx: cmd_tx,
            event_rx: evt_rx,
        })
    }

    async fn stop(&self, vm_id: &VmId) -> Result<(), PoolError> {
        self.transports.write().await.remove(vm_id);

        // Also stop via CLI in case the process is still running
        let output = tokio::process::Command::new("container")
            .args(["stop", vm_id.as_str()])
            .output()
            .await;

        match output {
            Ok(o) if o.status.success() => {
                info!(%vm_id, "container stopped");
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                debug!(%vm_id, %stderr, "container stop returned non-zero (may already be gone)");
            }
            Err(e) => {
                warn!(%vm_id, error = %e, "failed to run container stop");
            }
        }

        Ok(())
    }
}

/// A runtime that uses the supervisor binary directly (no container).
/// Useful for testing the full pool flow without needing container images.
pub struct SupervisorRuntime {
    supervisor_path: std::path::PathBuf,
}

impl SupervisorRuntime {
    pub fn new(supervisor_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            supervisor_path: supervisor_path.into(),
        }
    }
}

impl<P: AppProtocol> VmRuntime<P> for SupervisorRuntime {
    async fn start(
        &self,
        vm_id: &VmId,
        _image: &ImageRef,
        _config: &VmConfig,
    ) -> Result<VmHandle<P>, PoolError> {
        let path = self
            .supervisor_path
            .to_str()
            .ok_or_else(|| PoolError::Runtime("supervisor path is not valid UTF-8".into()))?;

        let mut transport = VmTransport::<P>::spawn(path, &[])
            .await
            .map_err(|e| PoolError::Runtime(format!("failed to spawn supervisor: {e}")))?;

        // Wait for Ready
        let first_event = transport
            .recv()
            .await
            .ok_or_else(|| PoolError::Runtime("transport closed before Ready".into()))?;

        if !matches!(first_event, VmEvent::Ready) {
            return Err(PoolError::Runtime(format!(
                "expected Ready, got {:?}",
                first_event
            )));
        }

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<VmCommand<P>>(64);
        let (evt_tx, evt_rx) = mpsc::channel::<VmEvent<P>>(64);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(command) => {
                                if let Err(e) = transport.send(&command).await {
                                    error!("failed to send command: {}", e);
                                    break;
                                }
                            }
                            None => {
                                let _ = transport.send(&VmCommand::<P>::Shutdown).await;
                                break;
                            }
                        }
                    }
                    event = transport.recv() => {
                        match event {
                            Some(evt) => {
                                if evt_tx.send(evt).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
            let _ = transport.close().await;
        });

        info!(%vm_id, path = %self.supervisor_path.display(), "supervisor started");

        Ok(VmHandle {
            command_tx: cmd_tx,
            event_rx: evt_rx,
        })
    }

    async fn stop(&self, vm_id: &VmId) -> Result<(), PoolError> {
        debug!(%vm_id, "supervisor stop (channel drop handles shutdown)");
        Ok(())
    }
}

/// Configuration for the VM pool.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub max_vms: usize,
    pub health_check_interval: u64,
    pub vm_timeout: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_vms: 6,
            health_check_interval: 30,
            vm_timeout: 7200,
        }
    }
}

/// State of a VM in the pool.
struct VmEntry<P: AppProtocol = NullProtocol> {
    #[allow(dead_code)]
    id: VmId,
    #[allow(dead_code)]
    image: ImageRef,
    config: VmConfig,
    state: VmState,
    started_at: u64,
    command_tx: Option<mpsc::Sender<VmCommand<P>>>,
}

/// Marker type for pools without a runtime.
pub struct NoRuntime;

/// The VM pool manager, generic over runtime backend and application protocol.
pub struct Pool<R = NoRuntime, P: AppProtocol = NullProtocol> {
    config: PoolConfig,
    vms: RwLock<HashMap<VmId, VmEntry<P>>>,
    events: Arc<EventLog<P>>,
    runtime: R,
}

impl<P: AppProtocol> Pool<NoRuntime, P> {
    /// Create a pool without a runtime (commands to VMs will return VmNotReady).
    pub fn new(config: PoolConfig, events: Arc<EventLog<P>>) -> Arc<Self> {
        Arc::new(Self {
            config,
            vms: RwLock::new(HashMap::new()),
            events,
            runtime: NoRuntime,
        })
    }
}

impl<R, P: AppProtocol> Pool<R, P> {
    pub fn with_runtime(config: PoolConfig, events: Arc<EventLog<P>>, runtime: R) -> Arc<Self> {
        Arc::new(Self {
            config,
            vms: RwLock::new(HashMap::new()),
            events,
            runtime,
        })
    }

    pub async fn status(&self) -> PoolStatus {
        let vms = self.vms.read().await;
        PoolStatus {
            total: self.config.max_vms,
            allocated: vms.len(),
            available: self.config.max_vms.saturating_sub(vms.len()),
        }
    }

    pub async fn get(&self, vm_id: &VmId) -> Option<VmState> {
        let vms = self.vms.read().await;
        vms.get(vm_id).map(|e| e.state)
    }

    pub async fn list(&self) -> Vec<(VmId, VmState)> {
        let vms = self.vms.read().await;
        vms.iter().map(|(id, e)| (id.clone(), e.state)).collect()
    }

    /// Send an application command to a VM.
    ///
    /// Infrastructure commands (Ping, Shutdown) are sent internally by the pool
    /// during lifecycle management — callers only send application messages.
    pub async fn send_to_vm(&self, vm_id: &VmId, command: P::Command) -> Result<(), PoolError> {
        let vms = self.vms.read().await;
        let entry = vms
            .get(vm_id)
            .ok_or_else(|| PoolError::VmNotFound(vm_id.clone()))?;

        match &entry.command_tx {
            Some(tx) => tx
                .send(VmCommand::App { payload: command })
                .await
                .map_err(|_| PoolError::Runtime(format!("VM {} command channel closed", vm_id))),
            None => Err(PoolError::VmNotReady(vm_id.clone())),
        }
    }
}

// NoRuntime implements VmRuntime as a pool-bookkeeping-only stub: allocations
// succeed and are tracked in memory, but the returned channels are immediately
// disconnected, so any subsequent `send_to_vm` fails. Useful for exercising
// pool allocation/eviction/health-check logic without a real VM backend.
impl<P: AppProtocol> VmRuntime<P> for NoRuntime {
    async fn start(
        &self,
        _vm_id: &VmId,
        _image: &ImageRef,
        _config: &VmConfig,
    ) -> Result<VmHandle<P>, PoolError> {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<VmCommand<P>>(1);
        let (_evt_tx, evt_rx) = mpsc::channel::<VmEvent<P>>(1);
        // _cmd_rx and _evt_tx drop here: any send on cmd_tx returns Err,
        // any recv on evt_rx returns None.
        Ok(VmHandle {
            command_tx: cmd_tx,
            event_rx: evt_rx,
        })
    }

    async fn stop(&self, _vm_id: &VmId) -> Result<(), PoolError> {
        Ok(())
    }
}

// Pool with a real runtime — VMs get transport channels.
impl<R: VmRuntime<P>, P: AppProtocol> Pool<R, P> {
    pub async fn allocate(&self, image: ImageRef, config: VmConfig) -> Result<VmId, PoolError> {
        let mut vms = self.vms.write().await;

        if vms.len() >= self.config.max_vms {
            return Err(PoolError::Exhausted {
                available: 0,
                requested: 1,
            });
        }

        let vm_id = generate_vm_id();
        info!(%vm_id, image = %image, "allocating VM");

        self.events.init_vm(&vm_id).await;
        self.events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Allocating,
            })
            .await;

        let handle = match self.runtime.start(&vm_id, &image, &config).await {
            Ok(h) => h,
            Err(e) => {
                error!(%vm_id, error = %e, "failed to start VM");
                self.events
                    .append(EventPayload::VmLifecycle {
                        vm_id: vm_id.clone(),
                        state: VmState::Crashed,
                    })
                    .await;
                return Err(e);
            }
        };

        let events = self.events.clone();
        let fwd_vm_id = vm_id.clone();
        tokio::spawn(forward_vm_events(fwd_vm_id, handle.event_rx, events));

        let entry = VmEntry {
            id: vm_id.clone(),
            image,
            config,
            state: VmState::Ready,
            started_at: now_ms(),
            command_tx: Some(handle.command_tx),
        };
        vms.insert(vm_id.clone(), entry);

        self.events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Ready,
            })
            .await;

        Ok(vm_id)
    }

    /// Allocate a VM, evicting the lowest-priority VM if the pool is full.
    ///
    /// Returns `(new_vm_id, Option<evicted_vm_id>)`. Fails if the pool is full
    /// and no VM has a strictly lower priority than the requested config.
    pub async fn allocate_or_evict(
        &self,
        image: ImageRef,
        config: VmConfig,
    ) -> Result<(VmId, Option<VmId>), PoolError> {
        // Try normal allocation first
        {
            let vms = self.vms.read().await;
            if vms.len() < self.config.max_vms {
                drop(vms);
                let vm_id = self.allocate(image, config).await?;
                return Ok((vm_id, None));
            }
        }

        // Pool is full — find the lowest-priority VM to evict
        let evict_id = {
            let vms = self.vms.read().await;
            let candidate = vms
                .iter()
                .filter(|(_, entry)| entry.config.priority < config.priority)
                .min_by_key(|(_, entry)| (entry.config.priority, entry.started_at));

            match candidate {
                Some((id, entry)) => {
                    info!(
                        evicting = %id,
                        evict_priority = %entry.config.priority,
                        new_priority = %config.priority,
                        "evicting lower-priority VM to make room"
                    );
                    id.clone()
                }
                None => {
                    return Err(PoolError::Exhausted {
                        available: 0,
                        requested: 1,
                    });
                }
            }
        };

        // Evict the chosen VM
        self.deallocate(&evict_id).await?;

        // Now allocate
        let vm_id = self.allocate(image, config).await?;
        Ok((vm_id, Some(evict_id)))
    }

    pub async fn deallocate(&self, vm_id: &VmId) -> Result<(), PoolError> {
        let entry = {
            let mut vms = self.vms.write().await;
            vms.remove(vm_id)
                .ok_or_else(|| PoolError::VmNotFound(vm_id.clone()))?
        };

        info!(%vm_id, "deallocating VM");

        self.events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Stopping,
            })
            .await;

        if let Err(e) = self.runtime.stop(vm_id).await {
            warn!(%vm_id, error = %e, "failed to stop VM via runtime");
        }

        drop(entry);

        self.events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Stopped,
            })
            .await;
        self.events.cleanup_vm(vm_id).await;
        Ok(())
    }

    pub async fn health_check(&self) {
        let mut timed_out = Vec::new();
        {
            let vms = self.vms.read().await;
            for (vm_id, entry) in vms.iter() {
                let age_ms = now_ms().saturating_sub(entry.started_at);
                let age_s = age_ms / 1000;
                if age_ms >= self.config.vm_timeout * 1000 {
                    warn!(%vm_id, age_s, timeout = self.config.vm_timeout, "VM exceeded timeout");
                    timed_out.push(vm_id.clone());
                }
                debug!(%vm_id, state = ?entry.state, age_s, "health check");
            }
        }
        for vm_id in timed_out {
            if let Err(e) = self.deallocate(&vm_id).await {
                warn!(%vm_id, error = %e, "failed to deallocate timed-out VM");
            }
        }
    }
}

async fn forward_vm_events<P: AppProtocol>(
    vm_id: VmId,
    mut event_rx: mpsc::Receiver<VmEvent<P>>,
    events: Arc<EventLog<P>>,
) {
    while let Some(event) = event_rx.recv().await {
        match event {
            VmEvent::Ready => {
                events
                    .append(EventPayload::VmInfra {
                        vm_id: vm_id.clone(),
                        event: InfraEvent::Ready,
                    })
                    .await;
            }
            VmEvent::Pong => {
                events
                    .append(EventPayload::VmInfra {
                        vm_id: vm_id.clone(),
                        event: InfraEvent::Pong,
                    })
                    .await;
            }
            VmEvent::Shutdown => {
                events
                    .append(EventPayload::VmInfra {
                        vm_id: vm_id.clone(),
                        event: InfraEvent::Shutdown,
                    })
                    .await;
            }
            VmEvent::App { payload } => {
                events
                    .append(EventPayload::VmApp {
                        vm_id: vm_id.clone(),
                        event: payload,
                    })
                    .await;
            }
        }
    }
    debug!(%vm_id, "VM event forwarder stopped");
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStatus {
    pub total: usize,
    pub allocated: usize,
    pub available: usize,
}

fn generate_vm_id() -> VmId {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    VmId::new(format!("vm-{:x}-{:x}", ts, count))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_pool_protocol::{ShellCommand, ShellEvent, ShellProtocol};
    use vm_pool_test_support::supervisor_binary;

    fn test_pool(max_vms: usize) -> Arc<Pool<NoRuntime, ShellProtocol>> {
        let events = EventLog::<ShellProtocol>::new();
        Pool::new(
            PoolConfig {
                max_vms,
                health_check_interval: 30,
                vm_timeout: 7200,
            },
            events,
        )
    }

    #[tokio::test]
    async fn allocate_and_status() {
        let pool = test_pool(3);
        assert_eq!(pool.status().await.available, 3);

        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert!(!vm_id.as_str().is_empty());
        assert_eq!(pool.status().await.allocated, 1);
    }

    #[tokio::test]
    async fn allocate_until_exhausted() {
        let pool = test_pool(2);
        pool.allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        pool.allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        let result = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await;
        assert!(matches!(result, Err(PoolError::Exhausted { .. })));
    }

    #[tokio::test]
    async fn deallocate_frees_slot() {
        let pool = test_pool(1);
        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert_eq!(pool.status().await.available, 0);
        pool.deallocate(&vm_id).await.unwrap();
        assert_eq!(pool.status().await.available, 1);
    }

    #[tokio::test]
    async fn deallocate_not_found() {
        let pool = test_pool(3);
        assert!(matches!(
            pool.deallocate(&VmId::new("vm-nope")).await,
            Err(PoolError::VmNotFound(_))
        ));
    }

    #[tokio::test]
    async fn get_vm_state() {
        let pool = test_pool(3);
        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert_eq!(pool.get(&vm_id).await, Some(VmState::Ready));
        assert_eq!(pool.get(&VmId::new("vm-missing")).await, None);
    }

    #[tokio::test]
    async fn list_vms() {
        let pool = test_pool(3);
        let vm1 = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        let vm2 = pool
            .allocate(ImageRef::new("auto", "v1"), VmConfig::default())
            .await
            .unwrap();
        let list = pool.list().await;
        assert_eq!(list.len(), 2);
        let ids: Vec<&VmId> = list.iter().map(|(id, _)| id).collect();
        assert!(ids.contains(&&vm1));
        assert!(ids.contains(&&vm2));
    }

    #[tokio::test]
    async fn health_check_removes_timed_out() {
        let events = EventLog::<ShellProtocol>::new();
        let pool: Arc<Pool<NoRuntime, ShellProtocol>> = Pool::new(
            PoolConfig {
                max_vms: 3,
                health_check_interval: 1,
                vm_timeout: 0,
            },
            events,
        );
        pool.allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert_eq!(pool.status().await.allocated, 1);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        pool.health_check().await;
        assert_eq!(pool.status().await.allocated, 0);
    }

    #[tokio::test]
    async fn unique_vm_ids() {
        let pool = test_pool(10);
        let mut ids = Vec::new();
        for _ in 0..10 {
            ids.push(
                pool.allocate(ImageRef::new("agent", "v1"), VmConfig::default())
                    .await
                    .unwrap(),
            );
        }
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), 10);
    }

    #[tokio::test]
    async fn send_to_vm_no_runtime_channel_closed() {
        // With NoRuntime, allocate succeeds but the returned command_tx is
        // connected to an immediately-dropped receiver, so sends fail.
        let pool = test_pool(3);
        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        let err = pool
            .send_to_vm(
                &vm_id,
                ShellCommand::Execute {
                    command: "test".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, PoolError::Runtime(_)),
            "expected Runtime, got {err:?}"
        );
    }

    #[tokio::test]
    async fn send_to_vm_not_found() {
        let pool = test_pool(3);
        assert!(matches!(
            pool.send_to_vm(
                &VmId::new("vm-nope"),
                ShellCommand::Execute {
                    command: "test".into(),
                },
            )
            .await,
            Err(PoolError::VmNotFound(_))
        ));
    }

    // Integration tests using real supervisor process

    #[tokio::test]
    async fn supervisor_runtime_allocate_and_send() {
        let binary = supervisor_binary();
        let events = EventLog::<ShellProtocol>::new();
        let runtime = SupervisorRuntime::new(&binary);
        let pool: Arc<Pool<SupervisorRuntime, ShellProtocol>> = Pool::with_runtime(
            PoolConfig {
                max_vms: 3,
                health_check_interval: 300,
                vm_timeout: 7200,
            },
            events,
            runtime,
        );

        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();

        pool.send_to_vm(
            &vm_id,
            ShellCommand::Execute {
                command: "echo hi".into(),
            },
        )
        .await
        .unwrap();

        // Give the event forwarder a moment
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        pool.deallocate(&vm_id).await.unwrap();
        assert_eq!(pool.status().await.allocated, 0);
    }

    #[tokio::test]
    async fn supervisor_runtime_events_forwarded_to_log() {
        let binary = supervisor_binary();
        let events = EventLog::<ShellProtocol>::new();
        let pool: Arc<Pool<SupervisorRuntime, ShellProtocol>> = Pool::with_runtime(
            PoolConfig {
                max_vms: 3,
                health_check_interval: 300,
                vm_timeout: 7200,
            },
            events.clone(),
            SupervisorRuntime::new(&binary),
        );

        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();

        // Execute a command — the supervisor will emit Output + CommandCompleted
        pool.send_to_vm(
            &vm_id,
            ShellCommand::Execute {
                command: "echo pool-test".into(),
            },
        )
        .await
        .unwrap();

        // Wait for events to propagate
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let vm_events = events.for_vm(&vm_id).await;
        let app_events: Vec<_> = vm_events
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::VmApp { event, .. } => Some(event.clone()),
                _ => None,
            })
            .collect();

        let has_output = app_events.iter().any(|e| {
            matches!(
                e,
                ShellEvent::Output { data, .. } if data.contains("pool-test")
            )
        });
        assert!(
            has_output,
            "expected ShellEvent::Output with pool-test, got: {app_events:?}"
        );

        pool.deallocate(&vm_id).await.unwrap();
    }
}
