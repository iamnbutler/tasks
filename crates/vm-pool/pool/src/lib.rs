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

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
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
    /// that reads the refusal as "this work failed" charges it to the work. A
    /// leaked VM (one whose owner died between allocate and deallocate) holds
    /// its slot until that VM's own event stream ends — which for an agent VM
    /// whose owner died early can be most of an hour — so a pool sized exactly
    /// to the steady state spends that window refusing everything.
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

/// A pool without a container backend: allocations succeed and are tracked in
/// memory, but the returned command channel is disconnected, so any subsequent
/// `send_to_vm` fails. Useful for exercising allocation/eviction/health-check
/// logic without a real VM backend.
///
/// It **holds** each allocation's event sender. That is the whole job of the
/// map: with the sender dropped at the end of `start` (as it once was), the
/// per-VM forwarder returns immediately and slot reclamation frees the slot
/// that was just filled. `NoRuntime::stop` dropping the entry is how a
/// runtime-less VM dies. The command receiver is still dropped on purpose —
/// `send_to_vm_no_runtime_channel_closed` depends on the *command* side being
/// dead, not the event side.
#[derive(Default)]
pub struct NoRuntime {
    /// `Box<dyn Any>` because this type is not generic over the protocol and
    /// nothing ever reads a sender back — keeping it alive is all it is for.
    senders: RwLock<HashMap<VmId, Box<dyn Any + Send + Sync>>>,
}

impl NoRuntime {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Everything a per-VM event forwarder may touch.
///
/// Split out of [`Pool`] so a forwarder can hold a [`Weak`] to it: enough to
/// free a slot, not enough to start or stop a VM, and it cannot resurrect a
/// dropped pool. The runtime and the config stay on `Pool` for exactly that
/// reason.
struct PoolState<P: AppProtocol = NullProtocol> {
    vms: RwLock<HashMap<VmId, VmEntry<P>>>,
    /// VMs this pool reclaimed itself, waiting for their owner's `deallocate`
    /// to consume the acknowledgement. See [`Pool::deallocate`].
    reclaimed: RwLock<HashSet<VmId>>,
    events: Arc<EventLog<P>>,
    ledger: VmLedger,
}

impl<P: AppProtocol> PoolState<P> {
    fn new(events: Arc<EventLog<P>>) -> Self {
        Self {
            vms: RwLock::new(HashMap::new()),
            reclaimed: RwLock::new(HashSet::new()),
            events,
            ledger: VmLedger::disabled(),
        }
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
        Arc::new(Self {
            config,
            state: Arc::new(PoolState::new(events)),
            runtime: NoRuntime::new(),
        })
    }
}

impl<R, P: AppProtocol> Pool<R, P> {
    pub fn with_runtime(config: PoolConfig, events: Arc<EventLog<P>>, runtime: R) -> Arc<Self> {
        Arc::new(Self {
            config,
            state: Arc::new(PoolState::new(events)),
            runtime,
        })
    }

    /// Read the ledger for `socket_path` and return the VM ids the previous
    /// daemon on that socket left running.
    ///
    /// **Inert.** It reads a file and seeds this pool's own ledger; it starts
    /// nothing and stops nothing. Acting on the result is
    /// [`Pool::reclaim_carried_over`], which is a separate call on purpose:
    /// reading a file is safe anywhere, and stopping VMs is only safe once
    /// this process has *won the socket*. That this one sits on the impl block
    /// **without** the [`VmRuntime`] bound puts the split in the types rather
    /// than only in the prose.
    pub async fn adopt_ledger(&self, state_dir: &Path, socket_path: &Path) -> Vec<VmId> {
        self.state.ledger.enable(state_dir, socket_path).await
    }

    /// The VM ids this pool's ledger currently asserts. Diagnostics — and what
    /// lets a test assert that merely constructing a service adopted nothing.
    pub async fn ledger_outstanding(&self) -> Vec<VmId> {
        self.state.ledger.outstanding().await
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
        // kept, so the VM's event stream stays open until `stop` — otherwise
        // the forwarder would reclaim the slot the instant it was filled.
        self.senders
            .write()
            .await
            .insert(vm_id.clone(), Box::new(evt_tx));
        Ok(VmHandle {
            command_tx: cmd_tx,
            event_rx: evt_rx,
        })
    }

    async fn stop(&self, vm_id: &VmId) -> Result<(), PoolError> {
        // Dropping the sender is how a runtime-less VM dies.
        self.senders.write().await.remove(vm_id);
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

        // Write-ahead, not write-behind: recording after the start loses
        // exactly the VM whose daemon died between the spawn and the write,
        // which is the crash window the ledger exists for.
        self.state.ledger.record(&vm_id).await;

        let handle = match self.runtime.start(&vm_id, &image, &config).await {
            Ok(h) => h,
            Err(e) => {
                error!(%vm_id, error = %e, "failed to start VM");
                self.state.ledger.forget(&vm_id).await;
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

        // Spawned *after* the insert, and the still-held write lock is what
        // makes that hold: a VM that dies instantly would otherwise find no
        // entry to reclaim and then have a dead one inserted on top of it — a
        // slot leaked for the pool's whole lifetime. The lock is also still
        // held across the `Ready` append below, so a `Crashed` cannot land in
        // the log ahead of its own `Ready`. Do not add a `drop(vms)` here.
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

    /// Ask the runtime to stop the VMs a previous daemon on this socket left
    /// running, as returned by [`Pool::adopt_ledger`].
    ///
    /// **Only ever call this after winning the socket.** Before the bind, the
    /// ids might belong to a *live* peer, and stopping them would kill that
    /// pool's in-flight work.
    ///
    /// Carried VMs are never entered into the pool's map and consume no slot —
    /// they are the predecessor's, and the only thing wanted from them is that
    /// they stop.
    ///
    /// **Single-shot against [`ContainerRuntime`]**, whose `stop` returns
    /// `Ok(())` whether or not the container died. So an id here is forgotten
    /// after *one* attempt: the honest sentence is "the successor asked the
    /// runtime to stop it", not "it is stopped". An id whose `stop` reports
    /// `Err` is kept in the ledger for the next boot — the retry mechanism for
    /// a runtime that can report failure, which this one cannot. What *is*
    /// recoverable everywhere is an interrupted reclaim: each forget persists
    /// the remainder, so a daemon that dies partway through this loop hands
    /// what it did not reach to the next one.
    pub async fn reclaim_carried_over(&self, carried: Vec<VmId>) {
        if carried.is_empty() {
            return;
        }
        warn!(
            count = carried.len(),
            "the last pool on this socket left VMs running; asking the runtime to stop them"
        );
        for vm_id in carried {
            match self.runtime.stop(&vm_id).await {
                Ok(()) => {
                    info!(%vm_id, "asked the runtime to stop a VM left by the previous pool");
                    self.state.ledger.forget(&vm_id).await;
                }
                Err(e) => {
                    warn!(
                        %vm_id,
                        error = %e,
                        "could not stop a VM left by the previous pool; keeping it in the \
                         ledger for the next boot"
                    );
                }
            }
        }
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

    /// Hand a VM back.
    ///
    /// Succeeds for a VM this pool already reclaimed itself (its event stream
    /// ended, so the slot was freed at the moment of death) by *consuming* the
    /// acknowledgement — so a second `deallocate` is `VmNotFound` again, and
    /// `VmNotFound` keeps its one honest meaning: this pool never had that VM.
    /// Do not "simplify" that into returning `Ok` for anything unknown; it
    /// would let any client stop any container by name.
    ///
    /// A reclaimed VM still runs the **full** teardown rather than returning
    /// early: a supervisor that died inside a container that is still running
    /// looks identical from here, and that is the leak this is all for.
    pub async fn deallocate(&self, vm_id: &VmId) -> Result<(), PoolError> {
        let entry = {
            let mut vms = self.state.vms.write().await;
            match vms.remove(vm_id) {
                Some(entry) => Some(entry),
                // Not in the map: either this pool reclaimed it when it died,
                // or it was never ours.
                None => {
                    if !self.state.reclaimed.write().await.remove(vm_id) {
                        return Err(PoolError::VmNotFound(vm_id.clone()));
                    }
                    None
                }
            }
        };

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

        drop(entry);

        // The only place a VM leaves the ledger through this pool's own work.
        self.state.ledger.forget(vm_id).await;

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

/// Forward one VM's events into the log, and free its slot when the stream
/// ends.
///
/// The end of the stream is the moment of death, and it is the only signal
/// this process gets for a VM that died on its own — it used to be spent on a
/// `debug!` line while the pool went on counting the VM until `vm_timeout`,
/// two hours away.
///
/// It holds a [`Weak`] to the pool's state: enough to free a slot, not enough
/// to start or stop a VM, and it cannot resurrect a dropped pool. It
/// deliberately does **not** `cleanup_vm` (a client reattaching to learn how
/// its run ended reads that replay) and does **not** `ledger.forget` (a dead
/// transport is not a dead container).
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
    reclaim_slot(&vm_id, &state).await;
}

/// Free the slot of a VM whose event stream ended.
///
/// Finding nothing in the map is the **common case** and means the teardown
/// was deliberate: an ordinary `deallocate` removes the entry first and then
/// drops the command sender, which is what ends the stream. Only a VM still in
/// the map at this point died on its own.
async fn reclaim_slot<P: AppProtocol>(vm_id: &VmId, state: &Weak<PoolState<P>>) {
    let Some(state) = state.upgrade() else {
        return;
    };
    let died = state.vms.write().await.remove(vm_id).is_some();
    if !died {
        return;
    }

    warn!(%vm_id, "VM died on its own; freeing its slot");
    // The owner still gets to call `deallocate` and have it succeed — and to
    // run the full teardown, since a supervisor that died inside a container
    // that is still running looks identical from here.
    state.reclaimed.write().await.insert(vm_id.clone());
    state
        .events
        .append(EventPayload::VmLifecycle {
            vm_id: vm_id.clone(),
            state: VmState::Crashed,
        })
        .await;
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

    /// Poll for a condition rather than sleeping a fixed time: reclamation
    /// lands on the forwarder's own task, so there is no instant it is
    /// guaranteed to have happened by.
    macro_rules! eventually {
        ($what:expr, $cond:expr) => {{
            let mut ok = false;
            for _ in 0..400 {
                if $cond {
                    ok = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            assert!(ok, "timed out waiting for {}", $what);
        }};
    }

    /// A [`VmRuntime`] that records what it was asked to stop, and can be told
    /// to report failure for a given id.
    ///
    /// A stub at a seam the shipping code already has one for ([`NoRuntime`]),
    /// not a mock of a process boundary — so it is within vm-pool's no-mocks
    /// rule. It exists because "the runtime was asked to stop nothing" cannot
    /// be observed through `SupervisorRuntime` (whose `stop` is a no-op) or
    /// `NoRuntime`.
    ///
    /// It is duplicated in the service crate's tests rather than shared
    /// through `vm-pool-test-support`, because that crate is a dev-dependency
    /// *of* this one and a shared home would need a dev-dependency cycle.
    /// Twenty lines of test code is the cheaper price.
    ///
    /// A cloneable handle around shared state, so the test keeps one while the
    /// pool owns another (`Arc<RecordingRuntime>` would trip the orphan rule
    /// in the service crate, and the two copies are deliberately identical).
    #[derive(Clone, Default)]
    pub(crate) struct RecordingRuntime {
        inner: Arc<RecordingInner>,
    }

    #[derive(Default)]
    pub(crate) struct RecordingInner {
        stopped: std::sync::Mutex<Vec<VmId>>,
        failing: std::sync::Mutex<HashSet<VmId>>,
        senders: RwLock<HashMap<VmId, mpsc::Sender<VmEvent<ShellProtocol>>>>,
    }

    impl RecordingRuntime {
        fn stopped(&self) -> Vec<VmId> {
            self.inner.stopped.lock().unwrap().clone()
        }

        fn fail_stop_for(&self, vm_id: &VmId) {
            self.inner.failing.lock().unwrap().insert(vm_id.clone());
        }

        /// End a VM's event stream without any deallocate — a VM dying on its
        /// own, which is what the slot leak was.
        async fn kill(&self, vm_id: &VmId) {
            self.inner.senders.write().await.remove(vm_id);
        }
    }

    impl VmRuntime<ShellProtocol> for RecordingRuntime {
        async fn start(
            &self,
            vm_id: &VmId,
            _image: &ImageRef,
            _config: &VmConfig,
        ) -> Result<VmHandle<ShellProtocol>, PoolError> {
            let (cmd_tx, _cmd_rx) = mpsc::channel(1);
            let (evt_tx, evt_rx) = mpsc::channel(1);
            self.inner
                .senders
                .write()
                .await
                .insert(vm_id.clone(), evt_tx);
            Ok(VmHandle {
                command_tx: cmd_tx,
                event_rx: evt_rx,
            })
        }

        async fn stop(&self, vm_id: &VmId) -> Result<(), PoolError> {
            self.inner.stopped.lock().unwrap().push(vm_id.clone());
            self.inner.senders.write().await.remove(vm_id);
            if self.inner.failing.lock().unwrap().contains(vm_id) {
                return Err(PoolError::Runtime(format!("stop failed for {vm_id}")));
            }
            Ok(())
        }
    }

    fn recording_pool(
        max_vms: usize,
    ) -> (Arc<Pool<RecordingRuntime, ShellProtocol>>, RecordingRuntime) {
        let runtime = RecordingRuntime::default();
        let pool = Pool::with_runtime(
            PoolConfig {
                max_vms,
                health_check_interval: 300,
                // 7200 on purpose: if the health check's timeout were doing
                // the work, these tests would pass for the wrong reason.
                vm_timeout: 7200,
            },
            EventLog::<ShellProtocol>::new(),
            runtime.clone(),
        );
        (pool, runtime)
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

    // --- Slot reclamation: a VM that dies while the pool still counts it ---

    /// The leak, as a test, against a real process death: the supervisor runs
    /// commands as `sh -c`, so `$PPID` inside that shell is the supervisor
    /// itself. No apple/container anywhere, and no mocks.
    ///
    /// The assertion that matters is that the slot is **reallocatable** — not
    /// that a counter moved. Before this, the pool went on counting the dead
    /// VM until `vm_timeout`, two hours away.
    #[tokio::test]
    async fn a_vm_that_dies_frees_its_slot_for_the_next_allocation() {
        let pool: Arc<Pool<SupervisorRuntime, ShellProtocol>> = Pool::with_runtime(
            PoolConfig {
                max_vms: 1,
                health_check_interval: 300,
                vm_timeout: 7200,
            },
            EventLog::<ShellProtocol>::new(),
            SupervisorRuntime::new(supervisor_binary()),
        );

        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert_eq!(pool.status().await.available, 0);
        assert!(
            pool.allocate(ImageRef::new("agent", "v1"), VmConfig::default())
                .await
                .is_err(),
            "the pool holds one slot and it is taken"
        );

        pool.send_to_vm(
            &vm_id,
            ShellCommand::Execute {
                command: "kill -9 $PPID".into(),
            },
        )
        .await
        .unwrap();

        eventually!("the dead VM's slot", pool.status().await.available == 1);
        pool.allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .expect("the freed slot is usable, not just uncounted");
    }

    /// Its owner still gets to hand it back, and the teardown is the **full**
    /// one — a supervisor that died inside a container that is still running
    /// looks identical from here, and that is the leak this is all for.
    #[tokio::test]
    async fn a_dead_vms_owner_can_still_deallocate_it_and_the_runtime_is_asked() {
        let (pool, runtime) = recording_pool(2);
        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();

        // The VM dies on its own: its event stream ends without a deallocate.
        runtime.kill(&vm_id).await;
        eventually!("the reclaimed slot", pool.status().await.available == 2);
        assert!(
            runtime.stopped().is_empty(),
            "reclaiming a slot is not a teardown"
        );

        pool.deallocate(&vm_id)
            .await
            .expect("the owner's deallocate still succeeds");
        assert_eq!(
            runtime.stopped(),
            vec![vm_id.clone()],
            "the container may well still be running"
        );
    }

    /// The acknowledgement is consumed, so `VmNotFound` keeps its one honest
    /// meaning: this pool never had that VM. Simplifying this into "return
    /// `Ok` for anything unknown" would let any client stop any container by
    /// name.
    #[tokio::test]
    async fn the_acknowledgement_is_consumed_by_the_first_deallocate() {
        let (pool, runtime) = recording_pool(2);
        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        runtime.kill(&vm_id).await;
        eventually!("the reclaimed slot", pool.status().await.available == 2);

        pool.deallocate(&vm_id).await.unwrap();
        assert!(matches!(
            pool.deallocate(&vm_id).await,
            Err(PoolError::VmNotFound(_))
        ));
        assert!(matches!(
            pool.deallocate(&VmId::new("vm-never-allocated")).await,
            Err(PoolError::VmNotFound(_))
        ));
    }

    /// An ordinary `deallocate` also ends the stream — it drops the command
    /// sender — so the reclamation path runs on *every* teardown. Finding
    /// nothing is the common case and means the teardown was deliberate, and
    /// it must not leave an acknowledgement behind for a second deallocate to
    /// consume.
    #[tokio::test]
    async fn an_ordinary_deallocate_leaves_no_acknowledgement_behind() {
        let (pool, _runtime) = recording_pool(2);
        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        pool.deallocate(&vm_id).await.unwrap();

        // Give the forwarder every chance to run before asking again.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(matches!(
            pool.deallocate(&vm_id).await,
            Err(PoolError::VmNotFound(_))
        ));
    }

    /// Reclaiming a slot must not `cleanup_vm`: a client reattaching to learn
    /// how its run ended reads that replay, and a VM that died on its own is
    /// exactly the case where it needs to.
    #[tokio::test]
    async fn a_reclaimed_vms_events_are_still_there_to_replay() {
        use vm_pool_protocol::{OutputStream, ShellEvent};

        let (pool, runtime) = recording_pool(2);
        let events = EventLog::<ShellProtocol>::new();
        // The pool made its own log; drive this one through the pool's.
        drop(events);

        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        pool.state
            .events
            .append(EventPayload::VmApp {
                vm_id: vm_id.clone(),
                event: ShellEvent::Output {
                    stream: OutputStream::Stdout,
                    data: "the last thing it said".into(),
                },
            })
            .await;

        runtime.kill(&vm_id).await;
        eventually!("the reclaimed slot", pool.status().await.available == 2);

        let (replay, _dropped) = pool.state.events.app_events_for_vm(&vm_id, 0, 100).await;
        assert_eq!(replay.len(), 1, "the replay outlives the VM");
    }

    // --- The ledger: what this pool started, across its own death ---

    #[tokio::test]
    async fn allocating_records_to_the_ledger_and_deallocating_forgets() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vm-pool.sock");
        let (pool, _runtime) = recording_pool(2);
        assert!(pool.adopt_ledger(dir.path(), &socket).await.is_empty());

        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert_eq!(pool.ledger_outstanding().await, vec![vm_id.clone()]);

        pool.deallocate(&vm_id).await.unwrap();
        assert!(pool.ledger_outstanding().await.is_empty());
    }

    /// A dead transport is not a dead container, so reclaiming a slot leaves
    /// the ledger entry alone. Its owner's `deallocate` is what clears it —
    /// and until then it is exactly what a successor needs.
    #[tokio::test]
    async fn a_vm_that_died_on_its_own_stays_in_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vm-pool.sock");
        let (pool, runtime) = recording_pool(2);
        pool.adopt_ledger(dir.path(), &socket).await;

        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        runtime.kill(&vm_id).await;
        eventually!("the reclaimed slot", pool.status().await.available == 2);

        assert_eq!(pool.ledger_outstanding().await, vec![vm_id]);
    }

    /// A start that fails must not leave the write-ahead record behind: no VM
    /// was ever started, so there is nothing for a successor to stop.
    #[tokio::test]
    async fn a_failed_start_forgets_its_write_ahead_record() {
        struct RefusingRuntime;
        impl VmRuntime<ShellProtocol> for RefusingRuntime {
            async fn start(
                &self,
                _vm_id: &VmId,
                _image: &ImageRef,
                _config: &VmConfig,
            ) -> Result<VmHandle<ShellProtocol>, PoolError> {
                Err(PoolError::Runtime("no".into()))
            }
            async fn stop(&self, _vm_id: &VmId) -> Result<(), PoolError> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vm-pool.sock");
        let pool: Arc<Pool<RefusingRuntime, ShellProtocol>> = Pool::with_runtime(
            PoolConfig::default(),
            EventLog::<ShellProtocol>::new(),
            RefusingRuntime,
        );
        pool.adopt_ledger(dir.path(), &socket).await;

        assert!(
            pool.allocate(ImageRef::new("agent", "v1"), VmConfig::default())
                .await
                .is_err()
        );
        assert!(pool.ledger_outstanding().await.is_empty());
    }

    /// Carried VMs are the predecessor's. They are stopped, they consume no
    /// slot, and — against a runtime whose `stop` reports success — they are
    /// forgotten after that *one* attempt.
    #[tokio::test]
    async fn carried_vms_are_stopped_consume_no_slot_and_are_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vm-pool.sock");

        let (first, _) = recording_pool(2);
        first.adopt_ledger(dir.path(), &socket).await;
        let a = first
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        let b = first
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        drop(first); // the daemon goes away; the VMs do not

        let (second, runtime) = recording_pool(2);
        let carried = second.adopt_ledger(dir.path(), &socket).await;
        assert_eq!(carried.len(), 2);
        assert!(
            runtime.stopped().is_empty(),
            "adopting a ledger reads a file and nothing else"
        );

        second.reclaim_carried_over(carried).await;
        let mut stopped = runtime.stopped();
        stopped.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        let mut expected = vec![a, b];
        expected.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        assert_eq!(stopped, expected);
        assert_eq!(
            second.status().await.available,
            2,
            "a carried VM is not this pool's to count"
        );
        assert!(second.ledger_outstanding().await.is_empty());
    }

    /// The retry mechanism, for a runtime that can report failure.
    /// [`ContainerRuntime`] is **not** such a runtime — its `stop` returns
    /// `Ok(())` whether or not the container died — so nothing here claims
    /// this fires in production. The branch is ready if `stop` ever gets a
    /// verdict.
    #[tokio::test]
    async fn an_id_whose_stop_reports_failure_stays_in_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vm-pool.sock");

        let (first, _) = recording_pool(2);
        first.adopt_ledger(dir.path(), &socket).await;
        let a = first
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        let b = first
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        drop(first);

        let (second, runtime) = recording_pool(2);
        runtime.fail_stop_for(&a);
        let carried = second.adopt_ledger(dir.path(), &socket).await;
        second.reclaim_carried_over(carried).await;
        assert_eq!(second.ledger_outstanding().await, vec![a.clone()]);

        // And the next boot carries exactly the one that did not stop.
        let (third, _) = recording_pool(2);
        assert_eq!(third.adopt_ledger(dir.path(), &socket).await, vec![a]);
        assert_ne!(b, VmId::new(""));
    }

    /// Required item (1), end to end: a daemon that dies partway through the
    /// reclaim loop hands whatever it did not reach to the next one. This is
    /// only true because `enable` seeds the in-memory set with the carried
    /// ids — otherwise the first `forget` rewrites the file from an empty set
    /// and erases the rest at that moment.
    #[tokio::test]
    async fn a_reclaim_that_dies_after_one_stop_leaves_the_rest_for_the_next_boot() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vm-pool.sock");

        let (first, _) = recording_pool(2);
        first.adopt_ledger(dir.path(), &socket).await;
        let a = first
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        let b = first
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        drop(first);

        // The second daemon reclaims exactly one and then dies.
        let (second, runtime) = recording_pool(2);
        let carried = second.adopt_ledger(dir.path(), &socket).await;
        second.reclaim_carried_over(vec![carried[0].clone()]).await;
        let reached = runtime.stopped();
        assert_eq!(reached.len(), 1);
        drop(second);

        let (third, _) = recording_pool(2);
        let still_carried = third.adopt_ledger(dir.path(), &socket).await;
        let survivor = if reached[0] == a { b } else { a };
        assert_eq!(
            still_carried,
            vec![survivor],
            "the one the dead daemon never reached is still the next one's to stop"
        );
    }

    /// The other order — the successor allocates before it reclaims — which is
    /// the shape the bug actually took.
    #[tokio::test]
    async fn the_successors_first_allocation_does_not_erase_the_carried_ids() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vm-pool.sock");

        let (first, _) = recording_pool(3);
        first.adopt_ledger(dir.path(), &socket).await;
        let old = first
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        drop(first);

        let (second, _) = recording_pool(3);
        let carried = second.adopt_ledger(dir.path(), &socket).await;
        assert_eq!(carried, vec![old.clone()]);
        let fresh = second
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();

        let (third, _) = recording_pool(3);
        let mut seen = third.adopt_ledger(dir.path(), &socket).await;
        seen.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        let mut expected = vec![old, fresh];
        expected.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        assert_eq!(seen, expected);
    }

    /// A pool that never enabled a ledger writes nothing anywhere — which is
    /// every embedder that does not run a socket, including the rest of this
    /// suite.
    #[tokio::test]
    async fn a_pool_without_a_ledger_records_nothing() {
        let (pool, _runtime) = recording_pool(2);
        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert!(pool.ledger_outstanding().await.is_empty());
        pool.deallocate(&vm_id).await.unwrap();
    }
}
