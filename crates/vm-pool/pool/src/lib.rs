//! VM pool: allocation, limits, health monitoring, lifecycle.
//!
//! The pool manages VM instances through a [`VmRuntime`] trait that
//! abstracts the container backend. [`ContainerRuntime`] provides the
//! real implementation using apple/container.

pub mod events;
pub mod images;
pub mod ledger;
pub mod snapshot;
pub mod transport;

pub use events::{Event, EventLog, EventPayload, InfraEvent, ServiceState, VmState};
pub use images::{ImageError, ImageMetadata, ImageRef, ImageStore, ImageType};
pub use ledger::VmLedger;
pub use snapshot::{SnapshotError, SnapshotMetadata, SnapshotStore};
pub use transport::{TransportError, VmTransport, find_supervisor_binary};

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Weak};

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

/// How many VMs a pool holds at once when nothing says otherwise.
///
/// A *slot* is a VM **this pool allocated** — the entries in its own map, the
/// thing [`Pool::allocate`] counts against [`PoolConfig::max_vms`] and
/// [`vm_pool_protocol::PoolStatus::total`] reports. Nothing else consumes one.
/// In particular a `buildkit` VM does not: it is started by the container
/// runtime to service an image build, as an ordinary host process, and this
/// pool neither allocates it nor reconciles against what the runtime is
/// running. It costs host memory, not a slot.
pub const DEFAULT_MAX_VMS: usize = 6;

/// Configuration for the VM pool.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Ceiling on simultaneously allocated VMs; [`Pool::allocate`] fails with
    /// `pool exhausted` at it.
    ///
    /// Leave slack above what the workload steadily needs. Exhaustion is not a
    /// queue — an allocate that arrives at the ceiling is refused, and a caller
    /// that reads the refusal as "this work failed" charges it to the work.
    ///
    /// A VM whose owner died between allocate and deallocate holds its slot
    /// until *something* hands it back, and there are two such somethings.
    /// [`Pool`] frees the slot the moment the VM's event stream ends, which
    /// covers a VM that actually died; a VM that is still running with nobody
    /// left to talk to it holds its slot until its owner deallocates it or
    /// [`PoolConfig::vm_timeout`] ages it out. So a pool sized exactly to its
    /// steady state can still be refusing allocations for as long as one
    /// abandoned VM keeps running.
    pub max_vms: usize,
    pub health_check_interval: u64,
    pub vm_timeout: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_vms: DEFAULT_MAX_VMS,
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

/// A pool without a real VM backend: allocation is bookkeeping only.
///
/// It holds each VM's *event* sender for as long as the VM is allocated, so a
/// `NoRuntime` VM looks alive to the pool until something stops it. That is
/// not a detail — [`Pool`] now reads the end of a VM's event stream as "this
/// VM is gone" and hands the slot back, so a runtime that dropped its senders
/// on the way out of `start` would free every slot it had just filled.
/// [`NoRuntime::stop`] dropping the entry is what models a VM dying.
///
/// The *command* side is dead from the start, and deliberately so: any
/// `send_to_vm` against a `NoRuntime` VM fails, which is what makes it useful
/// for exercising allocation, eviction and health-check logic with no VM
/// backend.
#[derive(Debug, Default)]
pub struct NoRuntime {
    /// Type-erased `mpsc::Sender<VmEvent<P>>`, one per live VM. Erased because
    /// `NoRuntime` is not generic over the protocol while [`VmRuntime`] is.
    live: std::sync::Mutex<HashMap<VmId, Box<dyn std::any::Any + Send + Sync>>>,
}

impl NoRuntime {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Everything about a pool that a per-VM task may touch, held behind an `Arc`
/// so an event forwarder can outlive nothing and still hand a slot back.
///
/// Deliberately *not* the runtime. A forwarder gets a [`Weak`] to this — which
/// is enough to free a slot and record that it did, and not enough to start or
/// stop a VM — so the reclamation path cannot grow into a second, unsupervised
/// lifecycle manager beside [`Pool`]. `Weak`, so a task that outlives its pool
/// cannot resurrect it.
struct PoolState<P: AppProtocol = NullProtocol> {
    vms: RwLock<HashMap<VmId, VmEntry<P>>>,
    /// VMs this pool reclaimed on its own, awaiting the owner's `deallocate`.
    /// Bounded by "died without being deallocated, and not yet acknowledged":
    /// the entry is consumed by the first `deallocate` that asks for it.
    reclaimed: RwLock<HashSet<VmId>>,
    events: Arc<EventLog<P>>,
    ledger: VmLedger,
}

impl<P: AppProtocol> PoolState<P> {
    /// A VM's event stream ended, which is an exact statement that the VM is
    /// gone: the transport closed, or the runtime dropped its sender.
    ///
    /// Finding nothing in the map is the *common* case and means the teardown
    /// was deliberate — `deallocate` removes the entry before it stops the VM,
    /// and stopping it is what ends the stream. Only a VM still counted here
    /// died on its own.
    ///
    /// The ledger entry is deliberately **not** forgotten. A dead transport
    /// says the host side is gone; the container may well still be running,
    /// and it is the successor daemon's job to stop it. Over-stopping is
    /// idempotent, under-stopping is the leak.
    async fn reclaim(&self, vm_id: &VmId) {
        if self.vms.write().await.remove(vm_id).is_none() {
            return;
        }
        warn!(%vm_id, "VM died without being deallocated — reclaiming its slot");
        self.reclaimed.write().await.insert(vm_id.clone());
        self.events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Crashed,
            })
            .await;
    }
}

/// The VM pool manager, generic over runtime backend and application protocol.
pub struct Pool<R = NoRuntime, P: AppProtocol = NullProtocol> {
    config: PoolConfig,
    state: Arc<PoolState<P>>,
    runtime: R,
}

impl<P: AppProtocol> Pool<NoRuntime, P> {
    /// Create a pool without a runtime (commands to VMs will return VmNotReady).
    pub fn new(config: PoolConfig, events: Arc<EventLog<P>>) -> Arc<Self> {
        Self::with_runtime(config, events, NoRuntime::new())
    }
}

impl<R, P: AppProtocol> Pool<R, P> {
    /// A pool that does not remember its VMs across its own death. Equivalent
    /// to [`Pool::with_ledger`] with [`VmLedger::disabled`].
    pub fn with_runtime(config: PoolConfig, events: Arc<EventLog<P>>, runtime: R) -> Arc<Self> {
        Self::with_ledger(config, events, runtime, VmLedger::disabled())
    }

    /// A pool that writes what it starts to `ledger`, so its successor on the
    /// same socket can stop whatever it leaves behind. See [`VmLedger`].
    pub fn with_ledger(
        config: PoolConfig,
        events: Arc<EventLog<P>>,
        runtime: R,
        ledger: VmLedger,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            state: Arc::new(PoolState {
                vms: RwLock::new(HashMap::new()),
                reclaimed: RwLock::new(HashSet::new()),
                events,
                ledger,
            }),
            runtime,
        })
    }

    /// This pool's ledger of started VMs.
    pub fn ledger(&self) -> &VmLedger {
        &self.state.ledger
    }

    pub async fn status(&self) -> PoolStatus {
        let vms = self.state.vms.read().await;
        PoolStatus {
            total: self.config.max_vms,
            allocated: vms.len(),
            available: self.config.max_vms.saturating_sub(vms.len()),
        }
    }

    pub async fn get(&self, vm_id: &VmId) -> Option<VmState> {
        let vms = self.state.vms.read().await;
        vms.get(vm_id).map(|e| e.state)
    }

    pub async fn list(&self) -> Vec<(VmId, VmState)> {
        let vms = self.state.vms.read().await;
        vms.iter().map(|(id, e)| (id.clone(), e.state)).collect()
    }

    /// Send an application command to a VM.
    ///
    /// Infrastructure commands (Ping, Shutdown) are sent internally by the pool
    /// during lifecycle management — callers only send application messages.
    pub async fn send_to_vm(&self, vm_id: &VmId, command: P::Command) -> Result<(), PoolError> {
        let vms = self.state.vms.read().await;
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

impl<P: AppProtocol> VmRuntime<P> for NoRuntime {
    async fn start(
        &self,
        vm_id: &VmId,
        _image: &ImageRef,
        _config: &VmConfig,
    ) -> Result<VmHandle<P>, PoolError> {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<VmCommand<P>>(1);
        let (evt_tx, evt_rx) = mpsc::channel::<VmEvent<P>>(1);
        // `_cmd_rx` drops here: any send on cmd_tx returns Err. `evt_tx` is
        // kept until `stop`, so the VM's event stream stays open and the pool
        // goes on counting the slot — see the type's own docs.
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(vm_id.clone(), Box::new(evt_tx));
        Ok(VmHandle {
            command_tx: cmd_tx,
            event_rx: evt_rx,
        })
    }

    async fn stop(&self, vm_id: &VmId) -> Result<(), PoolError> {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(vm_id);
        Ok(())
    }
}

// Pool with a real runtime — VMs get transport channels.
impl<R: VmRuntime<P>, P: AppProtocol> Pool<R, P> {
    pub async fn allocate(&self, image: ImageRef, config: VmConfig) -> Result<VmId, PoolError> {
        let mut vms = self.state.vms.write().await;

        if vms.len() >= self.config.max_vms {
            return Err(PoolError::Exhausted {
                available: 0,
                requested: 1,
            });
        }

        let vm_id = generate_vm_id();
        info!(%vm_id, image = %image, "allocating VM");

        self.state.events.init_vm(&vm_id).await;
        self.state
            .events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Allocating,
            })
            .await;

        // Write-ahead: recorded before the VM exists, so the id is on disk for
        // the window in which this daemon can die between the spawn and the
        // write — which is precisely the crash the ledger exists for. A VM
        // that never starts is forgotten again below.
        self.state.ledger.record(&vm_id);

        let handle = match self.runtime.start(&vm_id, &image, &config).await {
            Ok(h) => h,
            Err(e) => {
                error!(%vm_id, error = %e, "failed to start VM");
                self.state.ledger.forget(&vm_id);
                self.state
                    .events
                    .append(EventPayload::VmLifecycle {
                        vm_id: vm_id.clone(),
                        state: VmState::Crashed,
                    })
                    .await;
                return Err(e);
            }
        };

        let entry = VmEntry {
            id: vm_id.clone(),
            image,
            config,
            state: VmState::Ready,
            started_at: now_ms(),
            command_tx: Some(handle.command_tx),
        };
        vms.insert(vm_id.clone(), entry);

        // *After* the insert, and the write lock held across both is what makes
        // that hold. A VM that dies instantly would otherwise find no entry to
        // reclaim, and then have a dead one inserted on top of it — a slot
        // leaked for the rest of the pool's life.
        tokio::spawn(forward_vm_events(
            vm_id.clone(),
            handle.event_rx,
            self.state.events.clone(),
            Arc::downgrade(&self.state),
        ));

        self.state
            .events
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
            let vms = self.state.vms.read().await;
            if vms.len() < self.config.max_vms {
                drop(vms);
                let vm_id = self.allocate(image, config).await?;
                return Ok((vm_id, None));
            }
        }

        // Pool is full — find the lowest-priority VM to evict
        let evict_id = {
            let vms = self.state.vms.read().await;
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

    /// Hand a VM back: stop it, free its slot, and forget it.
    ///
    /// Idempotent for a VM this pool already reclaimed on its own (its event
    /// stream ended without a `deallocate`), and an error for one it never
    /// had. Keeping those two apart is the point: `VmNotFound` means "this
    /// pool never started that VM", and a `deallocate` that answered `Ok` for
    /// any unknown id would let a client stop a container by guessing its
    /// name.
    ///
    /// A reclaimed VM still runs the whole teardown rather than returning
    /// early. A closed transport says the *host* side is gone — a supervisor
    /// that died inside a container that is still running looks exactly the
    /// same from here, and that is the leak this exists for.
    pub async fn deallocate(&self, vm_id: &VmId) -> Result<(), PoolError> {
        let entry = { self.state.vms.write().await.remove(vm_id) };
        if entry.is_none() && !self.state.reclaimed.write().await.remove(vm_id) {
            return Err(PoolError::VmNotFound(vm_id.clone()));
        }

        info!(%vm_id, "deallocating VM");

        self.state
            .events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Stopping,
            })
            .await;

        if let Err(e) = self.runtime.stop(vm_id).await {
            warn!(%vm_id, error = %e, "failed to stop VM via runtime");
        }
        self.state.ledger.forget(vm_id);

        drop(entry);

        self.state
            .events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Stopped,
            })
            .await;
        self.state.events.cleanup_vm(vm_id).await;
        Ok(())
    }

    /// Stop the VMs a previous daemon on this socket left running.
    ///
    /// Called once, before the service binds its socket, with whatever
    /// [`VmLedger::open`] carried over — so the pool never advertises capacity
    /// its predecessor's VMs are still consuming, and no client can allocate
    /// against a host that is about to have containers stopped underneath it.
    ///
    /// Safe because `bind_socket` admits one live daemon per socket path and
    /// the ledger is named for that path: everything in it at boot belongs to
    /// a daemon that is gone. These VMs were never in *this* pool's map, so
    /// this stops them through the runtime directly and then forgets each —
    /// one at a time, so a daemon that dies partway through hands the rest on.
    pub async fn reclaim_carried_over(&self, carried: Vec<VmId>) {
        if carried.is_empty() {
            return;
        }
        warn!(
            count = carried.len(),
            "stopping VMs a previous vm-pool left running"
        );
        for vm_id in carried {
            match self.runtime.stop(&vm_id).await {
                Ok(()) => info!(%vm_id, "stopped an orphaned VM"),
                // Forgotten anyway: the stop is best-effort and a ledger entry
                // that can never be discharged would be retried at every boot
                // from here to the end of time.
                Err(e) => warn!(%vm_id, error = %e, "could not stop an orphaned VM"),
            }
            self.state.ledger.forget(&vm_id);
        }
    }

    pub async fn health_check(&self) {
        let mut timed_out = Vec::new();
        {
            let vms = self.state.vms.read().await;
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

/// Pump one VM's events into the log, and hand its slot back when they stop.
///
/// The end of the stream is an exact statement that the VM is gone — the
/// transport closed, or the runtime dropped its sender — and it used to be
/// spent on a `debug!` line while the pool went on counting the VM as
/// allocated until `vm_timeout` aged it out, **two hours** later. That is the
/// slot leak, reclaimed here at the instant of death from a signal the pool
/// already had.
///
/// [`Weak`], because this task must not keep a pool alive, and must not be
/// able to bring one back.
async fn forward_vm_events<P: AppProtocol>(
    vm_id: VmId,
    mut event_rx: mpsc::Receiver<VmEvent<P>>,
    events: Arc<EventLog<P>>,
    state: Weak<PoolState<P>>,
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
    if let Some(state) = state.upgrade() {
        state.reclaim(&vm_id).await;
    }
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
    use std::path::Path;
    use std::time::Duration;
    use vm_pool_protocol::{ShellCommand, ShellEvent, ShellProtocol};
    use vm_pool_test_support::supervisor_binary;

    /// Wait for `predicate`, or fail. Every reclamation here happens on a
    /// spawned task, so the alternative is a sleep long enough to be slow and
    /// short enough to be flaky.
    async fn until(label: &str, mut predicate: impl AsyncFnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if predicate().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {label}");
    }

    /// A supervisor-backed pool with a real process behind every VM.
    fn supervisor_pool(max_vms: usize) -> Arc<Pool<SupervisorRuntime, ShellProtocol>> {
        Pool::with_runtime(
            PoolConfig {
                max_vms,
                health_check_interval: 300,
                // Deliberately the default two hours: if the timeout were
                // doing the reclaiming, these tests would pass for the wrong
                // reason and the bug would be untouched.
                vm_timeout: 7200,
            },
            EventLog::<ShellProtocol>::new(),
            SupervisorRuntime::new(supervisor_binary()),
        )
    }

    /// Kill the supervisor from inside itself. `sh -c` runs as the
    /// supervisor's child, so `$PPID` is the supervisor, and `kill -9` is a
    /// genuine VM death — a real process dying without a shutdown, on Linux,
    /// with no container runtime anywhere. It is the whole reason the
    /// reclamation path is testable here rather than only on a Mac.
    async fn kill_the_vm(pool: &Pool<SupervisorRuntime, ShellProtocol>, vm_id: &VmId) {
        pool.send_to_vm(
            vm_id,
            ShellCommand::Execute {
                command: "kill -9 $PPID".into(),
            },
        )
        .await
        .expect("the VM is alive to be killed");
    }

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

    // Reclamation: a VM that dies without being deallocated.

    /// The slot leak, as a test. The assertion that matters is the last one —
    /// not that a counter moved, but that the freed slot can actually carry
    /// work again. Before this, the pool went on counting a dead VM until
    /// `vm_timeout` aged it out two hours later.
    #[tokio::test]
    async fn a_vm_that_dies_gives_its_slot_back_to_the_next_allocation() {
        let pool = supervisor_pool(1);
        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert_eq!(pool.status().await.available, 0);
        assert!(
            pool.allocate(ImageRef::new("agent", "v1"), VmConfig::default())
                .await
                .is_err(),
            "the pool is full while the VM lives"
        );

        kill_the_vm(&pool, &vm_id).await;
        until("the dead VM's slot", async || {
            pool.status().await.available == 1
        })
        .await;
        assert_eq!(pool.get(&vm_id).await, None, "and it is gone from the map");

        let replacement = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .expect("the reclaimed slot carries work");
        pool.send_to_vm(
            &replacement,
            ShellCommand::Execute {
                command: "echo alive".into(),
            },
        )
        .await
        .expect("a real VM, not just a free counter");
        pool.deallocate(&replacement).await.unwrap();
    }

    /// The owner still calls `deallocate` — it has no idea the VM died — and
    /// that must succeed once. The teardown runs in full rather than returning
    /// early: a closed transport says the *host* side is gone, and a
    /// supervisor that died inside a container that is still running looks
    /// exactly the same from here.
    #[tokio::test]
    async fn deallocating_a_reclaimed_vm_succeeds_exactly_once() {
        let pool = supervisor_pool(2);
        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        kill_the_vm(&pool, &vm_id).await;
        until("the reclaim", async || pool.get(&vm_id).await.is_none()).await;

        pool.deallocate(&vm_id)
            .await
            .expect("the owner's deallocate is not an error");
        assert!(
            matches!(pool.deallocate(&vm_id).await, Err(PoolError::VmNotFound(_))),
            "the acknowledgement is consumed — this is not a pool that says Ok \
             to any id it is handed"
        );
    }

    /// `VmNotFound` keeps one meaning: this pool never had that VM. Widening
    /// it would let a client stop a container by guessing a name.
    #[tokio::test]
    async fn a_vm_this_pool_never_had_is_still_not_found() {
        let pool = supervisor_pool(1);
        assert!(matches!(
            pool.deallocate(&VmId::new("vm-someone-elses")).await,
            Err(PoolError::VmNotFound(_))
        ));
    }

    /// An ordinary deallocate ends the event stream too — it drops the command
    /// sender, which ends the bridge — so the reclamation path runs on *every*
    /// teardown. Finding nothing there is what "the teardown was deliberate"
    /// looks like, and it must not turn into a phantom reclaimed entry.
    #[tokio::test]
    async fn an_ordinary_teardown_leaves_nothing_to_reclaim() {
        let pool = supervisor_pool(1);
        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        pool.deallocate(&vm_id).await.unwrap();

        // Give the forwarder every chance to run late and misfile this as a
        // death.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            matches!(pool.deallocate(&vm_id).await, Err(PoolError::VmNotFound(_))),
            "a deliberate teardown does not leave an acknowledgement behind"
        );
        assert_eq!(pool.status().await.available, 1);
    }

    /// The ledger's half: what a pool started is on disk while it runs, and
    /// off it once the VM is handed back.
    #[tokio::test]
    async fn the_ledger_holds_a_vm_for_exactly_as_long_as_the_pool_owns_it() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, carried) = VmLedger::open(dir.path(), Path::new("/tmp/ledger-test.sock"));
        assert!(carried.is_empty());
        let pool = Pool::with_ledger(
            PoolConfig {
                max_vms: 2,
                health_check_interval: 300,
                vm_timeout: 7200,
            },
            EventLog::<ShellProtocol>::new(),
            SupervisorRuntime::new(supervisor_binary()),
            ledger,
        );

        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert_eq!(pool.ledger().ids(), vec![vm_id.clone()]);

        pool.deallocate(&vm_id).await.unwrap();
        assert!(pool.ledger().ids().is_empty(), "stopped, so forgotten");
    }

    /// A dead transport is not a dead container: the host side is gone, the
    /// container may well still be running, and stopping it is the successor's
    /// job. So reclaiming a slot deliberately does **not** forget the ledger
    /// entry — over-stopping is idempotent, under-stopping is the leak.
    #[tokio::test]
    async fn a_reclaimed_vm_keeps_its_ledger_entry_until_it_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, _) = VmLedger::open(dir.path(), Path::new("/tmp/reclaim-test.sock"));
        let pool = Pool::with_ledger(
            PoolConfig {
                max_vms: 2,
                health_check_interval: 300,
                vm_timeout: 7200,
            },
            EventLog::<ShellProtocol>::new(),
            SupervisorRuntime::new(supervisor_binary()),
            ledger,
        );

        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        kill_the_vm(&pool, &vm_id).await;
        until("the reclaim", async || pool.get(&vm_id).await.is_none()).await;

        assert_eq!(
            pool.ledger().ids(),
            vec![vm_id.clone()],
            "the slot came back; the container has not been stopped by anyone"
        );
        pool.deallocate(&vm_id).await.unwrap();
        assert!(pool.ledger().ids().is_empty());
    }

    /// A pool started from a ledger with entries stops each one and forgets
    /// it. There is no orphaned container to make on Linux — `container stop`
    /// is macOS-only and this path inherits it unchanged from `deallocate` —
    /// so what is under test is the bookkeeping: every carried id reaches the
    /// runtime's `stop`, and the ledger is empty afterwards.
    #[tokio::test]
    async fn a_second_pool_stops_what_the_first_left_behind() {
        /// A real [`NoRuntime`] with a note of what it was asked to stop. Not
        /// a stand-in for a VM — `NoRuntime` is the shipping bookkeeping-only
        /// backend — just the one observation this test needs, since a
        /// supervisor process cannot outlive the pool that spawned it.
        struct Recording {
            inner: NoRuntime,
            stopped: std::sync::Mutex<Vec<VmId>>,
        }

        impl VmRuntime<ShellProtocol> for Recording {
            async fn start(
                &self,
                vm_id: &VmId,
                image: &ImageRef,
                config: &VmConfig,
            ) -> Result<VmHandle<ShellProtocol>, PoolError> {
                self.inner.start(vm_id, image, config).await
            }

            async fn stop(&self, vm_id: &VmId) -> Result<(), PoolError> {
                self.stopped.lock().unwrap().push(vm_id.clone());
                VmRuntime::<ShellProtocol>::stop(&self.inner, vm_id).await
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let socket = Path::new("/tmp/carried-test.sock");

        // A daemon that started two VMs and died without stopping either.
        let (first, _) = VmLedger::open(dir.path(), socket);
        first.record(&VmId::new("vm-orphan-1"));
        first.record(&VmId::new("vm-orphan-2"));
        drop(first);

        let (ledger, carried) = VmLedger::open(dir.path(), socket);
        assert_eq!(carried.len(), 2, "the successor is told what is owed");
        let pool = Pool::with_ledger(
            PoolConfig::default(),
            EventLog::<ShellProtocol>::new(),
            Recording {
                inner: NoRuntime::new(),
                stopped: std::sync::Mutex::new(Vec::new()),
            },
            ledger,
        );
        pool.reclaim_carried_over(carried).await;

        let mut stopped = pool.runtime.stopped.lock().unwrap().clone();
        stopped.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(
            stopped,
            vec![VmId::new("vm-orphan-1"), VmId::new("vm-orphan-2")]
        );
        assert!(
            pool.ledger().ids().is_empty(),
            "discharged, so not owed again at the next boot"
        );
        assert_eq!(
            pool.status().await.allocated,
            0,
            "the orphans were never this pool's own VMs"
        );

        // And the next daemon is told nothing.
        let (_, carried) = VmLedger::open(dir.path(), socket);
        assert!(carried.is_empty());
    }
}
