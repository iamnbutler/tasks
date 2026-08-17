//! Client library for communicating with the vm-pool service.
//!
//! Provides a high-level async API over the Unix socket protocol.
//! The client is generic over an [`AppProtocol`](vm_pool_protocol::AppProtocol)
//! — specify the protocol that matches the service the client is talking to.
//!
//! # Correlation
//!
//! One connection carries both command responses and asynchronously pushed
//! events (VM application events). Every command is written as a
//! [`Request`](vm_pool_protocol::Request) with a per-connection id; the
//! service echoes that id on the direct
//! [`Response`](vm_pool_protocol::Response). A background reader task routes
//! each line by id: `Some(id)` resolves the matching in-flight request,
//! `None` goes to the event stream. Requests can therefore be issued
//! concurrently, and a VM event arriving mid-request is never mistaken for a
//! response.
//!
//! # Example
//!
//! ```no_run
//! # async fn example() -> Result<(), vm_pool_client::ClientError> {
//! use vm_pool_client::Client;
//! use vm_pool_protocol::{NullProtocol, VmConfig};
//!
//! let mut client: Client<NullProtocol> =
//!     Client::connect("/tmp/vm-pool.sock").await?;
//!
//! let status = client.status().await?;
//! println!("available: {}", status.available);
//!
//! let vm_id = client.allocate("agent:v1.0.0", VmConfig::default()).await?;
//! println!("allocated: {}", vm_id);
//!
//! client.deallocate(&vm_id).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Concurrency
//!
//! [`Client::handle`] hands out a cheap, cloneable [`ClientHandle`] whose
//! request methods take `&self`. Any number of handles can issue commands
//! over the same connection at the same time:
//!
//! ```no_run
//! # async fn example() -> Result<(), vm_pool_client::ClientError> {
//! # use vm_pool_client::Client;
//! # use vm_pool_protocol::{NullProtocol, VmConfig};
//! let client: Client<NullProtocol> = Client::connect("/tmp/vm-pool.sock").await?;
//! let a = client.handle();
//! let b = client.handle();
//! let (status, vm) = tokio::join!(a.status(), b.allocate("agent:v1", VmConfig::default()));
//! # let _ = (status?, vm?);
//! # Ok(())
//! # }
//! ```
//!
//! Events can be observed from as many places as needed via
//! [`Client::subscribe_events`] / [`ClientHandle::subscribe_events`]; each
//! subscriber receives every event pushed after it subscribed.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, warn};
use vm_pool_protocol::{
    AppProtocol, LogLine, NullProtocol, ReplayedEvent, Request, Response, ServiceCommand,
    ServiceEvent, VmConfig, VmId,
};

/// How many pushed events a subscriber may fall behind before the oldest are
/// dropped. See [`EventStream`] for the drop semantics.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("connection failed: {0}")]
    Connect(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("connection closed")]
    Closed,
    #[error("service error: {0}")]
    Service(String),
    #[error("unexpected response: {0}")]
    UnexpectedResponse(String),
}

/// Pool status information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStatus {
    pub total: usize,
    pub available: usize,
    pub allocated: usize,
    /// The protocol revision the *service* speaks — see
    /// [`vm_pool_protocol::PROTOCOL_VERSION`]. A service that predates version
    /// reporting omits the field and is read as
    /// [`PRE_VERSIONING`](vm_pool_protocol::PRE_VERSIONING).
    pub protocol_version: u32,
}

impl PoolStatus {
    /// Whether the service speaks at least this protocol revision.
    ///
    /// Gate on the constant for the command you need, not on a bare number:
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), vm_pool_client::ClientError> {
    /// use vm_pool_client::Client;
    /// use vm_pool_protocol::{ATTACH_PROTOCOL_VERSION, NullProtocol};
    ///
    /// let client: Client<NullProtocol> = Client::connect("/tmp/vm-pool.sock").await?;
    /// if client.handle().status().await?.speaks(ATTACH_PROTOCOL_VERSION) {
    ///     // `attach` will be understood; without this it is rejected at
    ///     // decode time and arrives as an ordinary `ClientError::Service`.
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn speaks(&self, version: u32) -> bool {
        self.protocol_version >= version
    }
}

/// What [`ClientHandle::attach`] found: whether the pool still holds the VM,
/// and the application events it recorded while nobody was listening.
///
/// `present: false` does not mean the work is lost. If the pool reaped the VM
/// after the workload finished, the terminal event is still in `replay` — it
/// is only `!present` *and* nothing terminal in the replay that means the
/// work is gone, and what counts as terminal is the application's business,
/// not this crate's.
#[derive(Debug, Clone, PartialEq)]
pub struct Attachment<P: AppProtocol = NullProtocol> {
    /// Whether the pool still holds the VM.
    pub present: bool,
    /// Recorded application events, oldest first.
    pub replay: Vec<ReplayedEvent<P>>,
    /// How many older events the requested limit cut off the front of.
    pub dropped: u64,
}

impl<P: AppProtocol> Attachment<P> {
    /// The highest sequence number in the replay, if any. Live events at or
    /// below it have already been delivered here, so a caller splicing the
    /// two together skips them.
    pub fn last_seq(&self) -> Option<u64> {
        self.replay.last().map(|e| e.seq)
    }
}

/// Recover from a poisoned mutex: the guarded data is a plain map/option and
/// stays consistent even if a holder panicked.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Shared connection state: the write side, the in-flight request table, and
/// the event fan-out.
struct Connection<P: AppProtocol> {
    /// Serialized request lines, drained by the writer task.
    cmd_tx: mpsc::Sender<String>,
    /// Next per-connection request id.
    next_id: AtomicU64,
    /// In-flight requests awaiting a correlated response.
    pending: Mutex<HashMap<u64, oneshot::Sender<ServiceEvent<P>>>>,
    /// Fan-out for pushed (uncorrelated) events. `None` once the connection
    /// is closed.
    event_tx: Mutex<Option<broadcast::Sender<ServiceEvent<P>>>>,
    /// Set once the reader task has stopped; no further responses can arrive.
    closed: std::sync::atomic::AtomicBool,
}

impl<P: AppProtocol> Connection<P> {
    /// Route one decoded line: correlated responses go to their waiting
    /// request, uncorrelated events go to the event fan-out.
    fn dispatch(&self, response: Response<P>) {
        match response.id {
            Some(id) => {
                let waiter = lock(&self.pending).remove(&id);
                match waiter {
                    Some(tx) => {
                        // Receiver may have been dropped (cancelled request).
                        let _ = tx.send(response.event);
                    }
                    None => {
                        warn!("response for unknown request id {id}, dropping");
                    }
                }
            }
            None => {
                if let Some(tx) = lock(&self.event_tx).as_ref() {
                    // No subscribers is normal; the event is simply dropped.
                    let _ = tx.send(response.event);
                }
            }
        }
    }

    /// Tear down: fail every in-flight request and close all event streams.
    ///
    /// `closed` is set first so a request racing with teardown either has its
    /// entry cleared here or observes the flag and gives up itself.
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        lock(&self.pending).clear();
        lock(&self.event_tx).take();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn subscribe(&self) -> EventStream<P> {
        match lock(&self.event_tx).as_ref() {
            Some(tx) => EventStream { rx: tx.subscribe() },
            None => {
                // Connection already closed: hand back a stream that is
                // immediately at end-of-stream.
                let (tx, rx) = broadcast::channel(1);
                drop(tx);
                EventStream { rx }
            }
        }
    }
}

/// A stream of asynchronously pushed events from the service (currently VM
/// application events).
///
/// Each stream is independent and starts at the moment it was created.
///
/// Slow consumers are not allowed to stall the connection: a stream that
/// falls more than [`EVENT_CHANNEL_CAPACITY`] events behind loses the oldest
/// events. [`EventStream::recv`] skips over such gaps and logs a warning; use
/// [`EventStream::into_inner`] if you need to observe the lag count yourself.
pub struct EventStream<P: AppProtocol = NullProtocol> {
    rx: broadcast::Receiver<ServiceEvent<P>>,
}

impl<P: AppProtocol> EventStream<P> {
    /// Receive the next event, or `None` once the connection is closed and
    /// all buffered events have been delivered.
    pub async fn recv(&mut self) -> Option<ServiceEvent<P>> {
        loop {
            match self.rx.recv().await {
                Ok(event) => return Some(event),
                Err(broadcast::error::RecvError::Closed) => return None,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("event stream lagged; {n} events dropped");
                }
            }
        }
    }

    /// The underlying broadcast receiver, for callers that want to handle
    /// lag explicitly.
    pub fn into_inner(self) -> broadcast::Receiver<ServiceEvent<P>> {
        self.rx
    }
}

/// A cheap, cloneable handle to a connection. Request methods take `&self`,
/// so handles can be shared across tasks and used concurrently.
pub struct ClientHandle<P: AppProtocol = NullProtocol> {
    conn: Arc<Connection<P>>,
}

impl<P: AppProtocol> Clone for ClientHandle<P> {
    fn clone(&self) -> Self {
        Self {
            conn: Arc::clone(&self.conn),
        }
    }
}

impl<P: AppProtocol> ClientHandle<P> {
    /// Subscribe to asynchronously pushed events. Independent of any other
    /// subscriber, and of [`Client::next_event`].
    pub fn subscribe_events(&self) -> EventStream<P> {
        self.conn.subscribe()
    }

    /// Send a command and wait for the response correlated with it.
    pub async fn request(
        &self,
        command: ServiceCommand<P>,
    ) -> Result<ServiceEvent<P>, ClientError> {
        let id = self.conn.next_id.fetch_add(1, Ordering::Relaxed);
        let json = serde_json::to_string(&Request::new(id, command))?;

        let (tx, rx) = oneshot::channel();
        lock(&self.conn.pending).insert(id, tx);

        // The reader may have torn down between the check and the insert;
        // `close` sets the flag before clearing `pending`, so one of the two
        // always sees the other.
        if self.conn.is_closed() {
            lock(&self.conn.pending).remove(&id);
            return Err(ClientError::Closed);
        }

        debug!("sending: {}", json);
        if self.conn.cmd_tx.send(json).await.is_err() {
            lock(&self.conn.pending).remove(&id);
            return Err(ClientError::Closed);
        }

        // The reader task drops the sender (via `close`) if the connection
        // dies, which surfaces here as `Closed`.
        rx.await.map_err(|_| ClientError::Closed)
    }

    /// Convert a ServiceEvent::Error into a ClientError, or return the event.
    fn check_error(event: ServiceEvent<P>) -> Result<ServiceEvent<P>, ClientError> {
        match event {
            ServiceEvent::Error { message } => Err(ClientError::Service(message)),
            other => Ok(other),
        }
    }

    /// Get pool status.
    pub async fn status(&self) -> Result<PoolStatus, ClientError> {
        let resp = self.request(ServiceCommand::Status).await?;
        match Self::check_error(resp)? {
            ServiceEvent::PoolStatus {
                total,
                available,
                allocated,
                protocol_version,
            } => Ok(PoolStatus {
                total,
                available,
                allocated,
                protocol_version,
            }),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Allocate a new VM. Returns the VM ID.
    pub async fn allocate(&self, image: &str, config: VmConfig) -> Result<VmId, ClientError> {
        let resp = self
            .request(ServiceCommand::Allocate {
                image: image.to_string(),
                config,
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::VmAllocated { vm_id, .. } => Ok(vm_id),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Deallocate a VM.
    pub async fn deallocate(&self, vm_id: &VmId) -> Result<(), ClientError> {
        let resp = self
            .request(ServiceCommand::Deallocate {
                vm_id: vm_id.clone(),
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::VmStopped { .. } => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Send an application command to a VM.
    ///
    /// Returns when the service acks forwarding (CommandSent). The VM's
    /// application response events arrive asynchronously on the event stream.
    pub async fn send_to_vm(&self, vm_id: &VmId, command: P::Command) -> Result<(), ClientError> {
        let resp = self
            .request(ServiceCommand::Send {
                vm_id: vm_id.clone(),
                command,
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::CommandSent { .. } => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Save a snapshot of a VM.
    pub async fn snapshot(&self, vm_id: &VmId, name: &str) -> Result<(), ClientError> {
        let resp = self
            .request(ServiceCommand::Snapshot {
                vm_id: vm_id.clone(),
                name: name.to_string(),
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::VmStopped { .. } => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Restore a VM from a snapshot.
    pub async fn restore(&self, vm_id: &VmId, snapshot: &str) -> Result<(), ClientError> {
        let resp = self
            .request(ServiceCommand::Restore {
                vm_id: vm_id.clone(),
                snapshot: snapshot.to_string(),
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::VmReady { .. } => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Tail log lines from a VM.
    pub async fn tail_logs(&self, vm_id: &VmId, lines: usize) -> Result<Vec<LogLine>, ClientError> {
        let resp = self
            .request(ServiceCommand::TailLogs {
                vm_id: vm_id.clone(),
                lines,
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::LogTail { lines, .. } => Ok(lines),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Subscribe to logs from a specific VM (or all VMs if None).
    pub async fn subscribe_logs(&self, vm_id: Option<&VmId>) -> Result<(), ClientError> {
        let resp = self
            .request(ServiceCommand::SubscribeLogs {
                vm_id: vm_id.cloned(),
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::LogsSubscribed { .. } => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Unsubscribe from log streaming.
    pub async fn unsubscribe_logs(&self) -> Result<(), ClientError> {
        let resp = self.request(ServiceCommand::UnsubscribeLogs).await?;
        match Self::check_error(resp)? {
            ServiceEvent::LogsSubscribed { .. } => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Pick up a VM this client was not following: ask whether the pool still
    /// holds it and replay up to `limit` of its application events from
    /// `since_seq` on.
    ///
    /// **Subscribe before calling this.** Events landing between the service
    /// taking the replay snapshot and this client subscribing are otherwise
    /// lost with no trace: [`ClientHandle::subscribe_events`] only delivers
    /// what is pushed after it, and the replay only covers what was recorded
    /// before it. Subscribing first makes the two overlap, and the overlap is
    /// what [`Attachment::last_seq`] lets the caller discard.
    ///
    /// **Ask [`status`](Self::status) first and gate on
    /// `speaks(`[`ATTACH_PROTOCOL_VERSION`](vm_pool_protocol::ATTACH_PROTOCOL_VERSION)`)**
    /// if a rejection would be expensive. vm-pool is upgraded separately from
    /// its clients, and a service that predates this command rejects the line
    /// at decode time — which arrives here as [`ClientError::Service`],
    /// indistinguishable (without matching serde's message text, which is not
    /// stable) from a genuine failure to attach to a VM that does exist. Only
    /// the caller knows which of those two it can afford to be wrong about.
    pub async fn attach(
        &self,
        vm_id: &VmId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Attachment<P>, ClientError> {
        let resp = self
            .request(ServiceCommand::Attach {
                vm_id: vm_id.clone(),
                since_seq,
                limit,
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::VmAttached {
                present,
                replay,
                dropped,
                ..
            } => Ok(Attachment {
                present,
                replay,
                dropped,
            }),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }
}

/// Client for communicating with the vm-pool service.
///
/// Owns the connection: dropping it (and every [`ClientHandle`] taken from
/// it) closes the socket. It also owns one event stream, drained by
/// [`Client::next_event`].
pub struct Client<P: AppProtocol = NullProtocol> {
    handle: ClientHandle<P>,
    events: EventStream<P>,
}

impl<P: AppProtocol> Client<P> {
    /// Connect to the vm-pool service at the given Unix socket path.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path.as_ref()).await?;
        let (reader, writer) = stream.into_split();

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(64);
        let (event_tx, event_rx) = broadcast::channel::<ServiceEvent<P>>(EVENT_CHANNEL_CAPACITY);

        let conn = Arc::new(Connection {
            cmd_tx,
            next_id: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            event_tx: Mutex::new(Some(event_tx)),
            closed: std::sync::atomic::AtomicBool::new(false),
        });

        // Writer task. Exits when the last handle is dropped (cmd_tx gone),
        // which closes the write half and lets the service see EOF.
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(line) = cmd_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if writer.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = writer.flush().await;
            }
        });

        // Reader task. Holds only a Weak reference so it never keeps the
        // connection alive on its own.
        let weak: Weak<Connection<P>> = Arc::downgrade(&conn);
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let Some(conn) = weak.upgrade() else { break };
                        match serde_json::from_str::<Response<P>>(line.trim()) {
                            Ok(response) => conn.dispatch(response),
                            Err(e) => warn!("dropping undecodable line: {e}"),
                        }
                    }
                    Err(_) => break,
                }
            }
            if let Some(conn) = weak.upgrade() {
                conn.close();
            }
        });

        let events = EventStream { rx: event_rx };

        Ok(Self {
            handle: ClientHandle { conn },
            events,
        })
    }

    /// A cloneable handle whose request methods take `&self`.
    pub fn handle(&self) -> ClientHandle<P> {
        self.handle.clone()
    }

    /// Subscribe to asynchronously pushed events. Independent of
    /// [`Client::next_event`] and of any other subscriber.
    pub fn subscribe_events(&self) -> EventStream<P> {
        self.handle.subscribe_events()
    }

    /// Send a command and wait for the response correlated with it.
    pub async fn request(
        &mut self,
        command: ServiceCommand<P>,
    ) -> Result<ServiceEvent<P>, ClientError> {
        self.handle.request(command).await
    }

    /// Get pool status.
    pub async fn status(&mut self) -> Result<PoolStatus, ClientError> {
        self.handle.status().await
    }

    /// Allocate a new VM. Returns the VM ID.
    pub async fn allocate(&mut self, image: &str, config: VmConfig) -> Result<VmId, ClientError> {
        self.handle.allocate(image, config).await
    }

    /// Deallocate a VM.
    pub async fn deallocate(&mut self, vm_id: &VmId) -> Result<(), ClientError> {
        self.handle.deallocate(vm_id).await
    }

    /// Send an application command to a VM.
    ///
    /// Returns when the service acks forwarding (CommandSent). The VM's
    /// application response events arrive asynchronously via
    /// [`Client::next_event`].
    pub async fn send_to_vm(
        &mut self,
        vm_id: &VmId,
        command: P::Command,
    ) -> Result<(), ClientError> {
        self.handle.send_to_vm(vm_id, command).await
    }

    /// Save a snapshot of a VM.
    pub async fn snapshot(&mut self, vm_id: &VmId, name: &str) -> Result<(), ClientError> {
        self.handle.snapshot(vm_id, name).await
    }

    /// Restore a VM from a snapshot.
    pub async fn restore(&mut self, vm_id: &VmId, snapshot: &str) -> Result<(), ClientError> {
        self.handle.restore(vm_id, snapshot).await
    }

    /// Tail log lines from a VM.
    pub async fn tail_logs(
        &mut self,
        vm_id: &VmId,
        lines: usize,
    ) -> Result<Vec<LogLine>, ClientError> {
        self.handle.tail_logs(vm_id, lines).await
    }

    /// Subscribe to logs from a specific VM (or all VMs if None).
    pub async fn subscribe_logs(&mut self, vm_id: Option<&VmId>) -> Result<(), ClientError> {
        self.handle.subscribe_logs(vm_id).await
    }

    /// Unsubscribe from log streaming.
    pub async fn unsubscribe_logs(&mut self) -> Result<(), ClientError> {
        self.handle.unsubscribe_logs().await
    }

    /// Pick up a VM this client was not following. See
    /// [`ClientHandle::attach`] — including the ordering requirement.
    pub async fn attach(
        &mut self,
        vm_id: &VmId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Attachment<P>, ClientError> {
        self.handle.attach(vm_id, since_seq, limit).await
    }

    /// Receive the next asynchronously pushed event (VM application events).
    /// Command responses are never delivered here — they are returned by the
    /// request that asked for them.
    ///
    /// Returns None once the connection is closed and buffered events are
    /// drained.
    pub async fn next_event(&mut self) -> Option<ServiceEvent<P>> {
        self.events.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vm_pool_manager::{EventPayload, NoRuntime, PoolConfig};
    use vm_pool_protocol::{OutputStream, ShellEvent, ShellProtocol};
    use vm_pool_service::{Service, ServiceConfig};

    type TestClient = Client<ShellProtocol>;
    type TestService = Service<NoRuntime, ShellProtocol>;

    async fn start_service(
        max_vms: usize,
    ) -> (Arc<TestService>, std::path::PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");

        let config = ServiceConfig {
            socket_path: socket_path.clone(),
            snapshot_dir: dir.path().join("snapshots"),
            state_dir: Some(dir.path().join("state")),
            pool: PoolConfig {
                max_vms,
                health_check_interval: 300,
                vm_timeout: 7200,
            },
        };

        let service = TestService::new(config).await.unwrap();
        let svc = service.clone();
        tokio::spawn(async move { svc.run().await });

        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        (service, socket_path, dir)
    }

    /// Connect, then round-trip one command.
    ///
    /// `connect` returns as soon as the socket is accepted at the OS level,
    /// which can be before the service has spawned the per-connection task
    /// that subscribes to the event bus. A completed round-trip proves that
    /// task is running, so events pushed afterwards are guaranteed to be
    /// forwarded to this connection.
    async fn connect_client(socket_path: &std::path::Path) -> TestClient {
        let mut client = Client::<ShellProtocol>::connect(socket_path).await.unwrap();
        client.status().await.expect("warm-up request");
        client
    }

    /// Start a service on a temp socket and return a connected client.
    async fn test_client() -> (TestClient, Arc<TestService>, tempfile::TempDir) {
        let (service, socket_path, dir) = start_service(3).await;
        let client = connect_client(&socket_path).await;
        (client, service, dir)
    }

    /// Push a VM application event through the service's event bus, exactly
    /// as a real VM's supervisor output would.
    async fn push_app_event(svc: &Arc<TestService>, vm_id: &VmId, data: String) {
        svc.events
            .append(EventPayload::VmApp {
                vm_id: vm_id.clone(),
                event: ShellEvent::Output {
                    stream: OutputStream::Stdout,
                    data,
                },
            })
            .await;
    }

    #[tokio::test]
    async fn client_status() {
        let (mut client, _svc, _dir) = test_client().await;

        let status = client.status().await.unwrap();
        assert_eq!(status.total, 3);
        assert_eq!(status.available, 3);
        assert_eq!(status.allocated, 0);
    }

    /// A current service says so, and `speaks` agrees.
    #[tokio::test]
    async fn client_status_reports_the_protocol_version() {
        let (mut client, _svc, _dir) = test_client().await;

        let status = client.status().await.unwrap();
        assert_eq!(
            status.protocol_version,
            vm_pool_protocol::PROTOCOL_VERSION,
            "the service reports what it speaks"
        );
        assert!(status.speaks(vm_pool_protocol::ATTACH_PROTOCOL_VERSION));
        assert!(status.speaks(vm_pool_protocol::PRE_VERSIONING));
        assert!(!status.speaks(vm_pool_protocol::PROTOCOL_VERSION + 1));
    }

    /// The case the gate exists for, against the only honest peer left: raw
    /// bytes in the shape a service that predates version reporting emitted.
    /// That binary is not in this tree any more, so its wire form is the test.
    #[tokio::test]
    async fn a_pre_versioning_service_reports_pre_versioning() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("old.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

        // An old service: it answers `status` in the old shape, with no
        // `protocol_version` field at all.
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                let id: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                let reply = format!(
                    r#"{{"id":{},"event":{{"type":"pool_status","total":3,"available":3,"allocated":0}}}}"#,
                    id["id"]
                );
                writer.write_all(reply.as_bytes()).await.unwrap();
                writer.write_all(b"\n").await.unwrap();
                writer.flush().await.unwrap();
                line.clear();
            }
        });

        let mut client = Client::<ShellProtocol>::connect(&socket_path)
            .await
            .unwrap();
        let status = client.status().await.unwrap();
        assert_eq!(status.protocol_version, vm_pool_protocol::PRE_VERSIONING);
        assert!(
            !status.speaks(vm_pool_protocol::ATTACH_PROTOCOL_VERSION),
            "silence about the version is an answer: no attach"
        );
    }

    #[tokio::test]
    async fn client_allocate_and_deallocate() {
        let (mut client, _svc, _dir) = test_client().await;

        let vm_id = client
            .allocate("agent:v1", VmConfig::default())
            .await
            .unwrap();

        let status = client.status().await.unwrap();
        assert_eq!(status.allocated, 1);

        client.deallocate(&vm_id).await.unwrap();

        let status = client.status().await.unwrap();
        assert_eq!(status.allocated, 0);
    }

    #[tokio::test]
    async fn client_allocate_error() {
        let (_svc, socket_path, _dir) = start_service(0).await;

        let mut client = Client::<ShellProtocol>::connect(&socket_path)
            .await
            .unwrap();
        let result = client.allocate("agent:v1", VmConfig::default()).await;
        assert!(matches!(result, Err(ClientError::Service(_))));
    }

    #[tokio::test]
    async fn client_tail_logs() {
        let (mut client, _svc, _dir) = test_client().await;

        let vm_id = VmId::new("vm-nonexistent");
        let logs = client.tail_logs(&vm_id, 10).await.unwrap();
        assert!(logs.is_empty());
    }

    #[tokio::test]
    async fn client_subscribe_unsubscribe() {
        let (mut client, _svc, _dir) = test_client().await;

        client.subscribe_logs(None).await.unwrap();
        client.unsubscribe_logs().await.unwrap();
    }

    #[tokio::test]
    async fn client_full_lifecycle() {
        let (mut client, _svc, _dir) = test_client().await;

        // Check initial status
        let status = client.status().await.unwrap();
        assert_eq!(status.available, 3);

        // Allocate two VMs
        let vm1 = client
            .allocate("agent:v1", VmConfig::default())
            .await
            .unwrap();
        let vm2 = client
            .allocate("automation:v1", VmConfig::default())
            .await
            .unwrap();

        let status = client.status().await.unwrap();
        assert_eq!(status.allocated, 2);
        assert_eq!(status.available, 1);

        // Deallocate one
        client.deallocate(&vm1).await.unwrap();

        let status = client.status().await.unwrap();
        assert_eq!(status.allocated, 1);
        assert_eq!(status.available, 2);

        // Deallocate the other
        client.deallocate(&vm2).await.unwrap();

        let status = client.status().await.unwrap();
        assert_eq!(status.allocated, 0);
    }

    /// The wire really does carry correlation ids: a raw socket peer sees the
    /// id it sent echoed on the response, and pushed events carry no id.
    #[tokio::test]
    async fn wire_format_echoes_request_id() {
        let (svc, socket_path, _dir) = start_service(3).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        writer
            .write_all(b"{\"id\":99,\"command\":{\"type\":\"status\"}}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let response: Response<ShellProtocol> = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(response.id, Some(99));
        assert!(matches!(response.event, ServiceEvent::PoolStatus { .. }));

        // A pushed VM event has no id.
        push_app_event(&svc, &VmId::new("vm-1"), "streamed".into()).await;

        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let response: Response<ShellProtocol> = serde_json::from_str(line.trim()).unwrap();
        assert!(response.is_push(), "pushed event must not carry an id");
        assert!(!line.contains("\"id\""), "got: {line}");
    }

    /// A malformed request still gets a correlated error back, so the caller
    /// fails rather than hanging.
    #[tokio::test]
    async fn malformed_request_error_is_correlated() {
        let (_svc, socket_path, _dir) = start_service(3).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        writer
            .write_all(b"{\"id\":5,\"command\":{\"type\":\"no_such_command\"}}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let response: Response<ShellProtocol> = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(response.id, Some(5));
        assert!(matches!(response.event, ServiceEvent::Error { .. }));
    }

    /// Regression: a VM streaming application events must never be mistaken
    /// for a command response, and must never be swallowed by a request.
    ///
    /// Before correlation, `request()` returned "the next line on the socket",
    /// so an event landing between a command and its response was consumed as
    /// the response — the request failed and the event was lost.
    #[tokio::test]
    async fn events_never_consumed_as_responses() {
        let (svc, socket_path, _dir) = start_service(3).await;
        let client = connect_client(&socket_path).await;

        let vm_id = VmId::new("vm-streaming");
        const EVENT_COUNT: usize = 200;
        const REQUEST_COUNT: usize = 200;

        // Drain events concurrently, recording their payloads.
        let mut events = client.subscribe_events();
        let drain = tokio::spawn(async move {
            let mut seen: Vec<usize> = Vec::new();
            while let Some(event) = events.recv().await {
                match event {
                    ServiceEvent::VmApp { vm_id, event, .. } => {
                        assert_eq!(vm_id, VmId::new("vm-streaming"));
                        let ShellEvent::Output { data, .. } = event else {
                            panic!("unexpected app event: {event:?}");
                        };
                        let n: usize = data.parse().expect("event payload");
                        seen.push(n);
                        if n == EVENT_COUNT - 1 {
                            break;
                        }
                    }
                    other => panic!("command response leaked onto event stream: {other:?}"),
                }
            }
            seen
        });

        // Stream app events while commands are in flight.
        let svc_for_events = svc.clone();
        let vm_for_events = vm_id.clone();
        let stream = tokio::spawn(async move {
            for i in 0..EVENT_COUNT {
                push_app_event(&svc_for_events, &vm_for_events, i.to_string()).await;
                tokio::task::yield_now().await;
            }
        });

        // Every response must be the response to the command just issued.
        let handle = client.handle();
        for _ in 0..REQUEST_COUNT {
            let status = handle.status().await.expect("status must not fail");
            assert_eq!(status.total, 3);
            assert_eq!(status.allocated, 0);

            let logs = handle
                .tail_logs(&VmId::new("vm-absent"), 5)
                .await
                .expect("tail_logs must not fail");
            assert!(logs.is_empty());
        }

        stream.await.unwrap();
        let seen = drain.await.unwrap();

        assert_eq!(
            seen,
            (0..EVENT_COUNT).collect::<Vec<_>>(),
            "every streamed event must reach the event stream, in order"
        );
    }

    /// Two handles issuing interleaved commands on one connection: each gets
    /// its own response, never the other's.
    #[tokio::test]
    async fn concurrent_handles_route_correctly() {
        let (_svc, socket_path, _dir) = start_service(64).await;
        let client = Client::<ShellProtocol>::connect(&socket_path)
            .await
            .unwrap();

        let a = client.handle();
        let b = client.handle();

        // `a` only ever asks for status; `b` only ever tails logs for a VM it
        // names after the iteration. Any mis-routing shows up as a wrong
        // variant or a wrong vm_id.
        let a_task = tokio::spawn(async move {
            for _ in 0..100 {
                let status = a.status().await.unwrap();
                assert_eq!(status.total, 64);
            }
        });

        let b_task = tokio::spawn(async move {
            for i in 0..100 {
                let vm = VmId::new(format!("vm-{i}"));
                let resp = b
                    .request(ServiceCommand::TailLogs {
                        vm_id: vm.clone(),
                        lines: 3,
                    })
                    .await
                    .unwrap();
                match resp {
                    ServiceEvent::LogTail { vm_id, .. } => assert_eq!(vm_id, vm),
                    other => panic!("expected LogTail for {vm}, got {other:?}"),
                }
            }
        });

        a_task.await.unwrap();
        b_task.await.unwrap();
    }

    /// Many handles allocating at once over one connection: every allocation
    /// returns a distinct VM id and the pool agrees on the total.
    #[tokio::test]
    async fn concurrent_allocations_are_distinct() {
        let (_svc, socket_path, _dir) = start_service(16).await;
        let mut client = Client::<ShellProtocol>::connect(&socket_path)
            .await
            .unwrap();

        let mut tasks = Vec::new();
        for _ in 0..16 {
            let handle = client.handle();
            tasks.push(tokio::spawn(async move {
                handle.allocate("agent:v1", VmConfig::default()).await
            }));
        }

        let mut ids = std::collections::HashSet::new();
        for task in tasks {
            let vm_id = task.await.unwrap().unwrap();
            assert!(
                ids.insert(vm_id),
                "duplicate vm id from concurrent allocate"
            );
        }

        let status = client.status().await.unwrap();
        assert_eq!(status.allocated, 16);
    }

    /// Independent subscribers each see every event.
    #[tokio::test]
    async fn subscribers_are_independent() {
        let (svc, socket_path, _dir) = start_service(3).await;
        let mut client = connect_client(&socket_path).await;

        let mut first = client.subscribe_events();
        let mut second = client.handle().subscribe_events();

        let vm_id = VmId::new("vm-1");
        push_app_event(&svc, &vm_id, "one".into()).await;

        for stream in [&mut first, &mut second] {
            match stream.recv().await.unwrap() {
                ServiceEvent::VmApp { event, .. } => {
                    assert_eq!(
                        event,
                        ShellEvent::Output {
                            stream: OutputStream::Stdout,
                            data: "one".into(),
                        }
                    );
                }
                other => panic!("expected VmApp, got {other:?}"),
            }
        }

        // The client's own stream sees it too.
        match client.next_event().await.unwrap() {
            ServiceEvent::VmApp { .. } => {}
            other => panic!("expected VmApp, got {other:?}"),
        }
    }

    /// A client that never saw a VM's events can still pick them up, and the
    /// splice is exact: the replay carries seq numbers, and the live events
    /// it already covered are identifiable rather than guessed at.
    #[tokio::test]
    async fn attach_replays_what_a_client_missed_and_marks_the_overlap() {
        let (svc, socket_path, _dir) = start_service(3).await;
        let vm_id = VmId::new("vm-was-running");

        // Traffic while nobody is connected — the restart window.
        for i in 0..3 {
            push_app_event(&svc, &vm_id, format!("before {i}")).await;
        }

        let client = connect_client(&socket_path).await;
        // Subscribe BEFORE attaching: anything landing between the snapshot
        // and the subscription must be covered by one of the two.
        let mut events = client.subscribe_events();
        let attachment = client.handle().attach(&vm_id, 0, 100).await.unwrap();

        assert!(!attachment.present, "no runtime, so no live VM entry");
        assert_eq!(attachment.dropped, 0);
        assert_eq!(attachment.replay.len(), 3);
        let last_seq = attachment.last_seq().expect("replay is non-empty");
        match &attachment.replay[0].event {
            ShellEvent::Output { data, .. } => assert_eq!(data, "before 0"),
            other => panic!("unexpected replayed event: {other:?}"),
        }

        // Live traffic from here on carries seq values past the watermark.
        push_app_event(&svc, &vm_id, "after".into()).await;
        loop {
            match events.recv().await.expect("stream is open") {
                ServiceEvent::VmApp { seq, event, .. } => {
                    if seq <= last_seq {
                        // Already in the replay; a real consumer drops these.
                        continue;
                    }
                    match event {
                        ShellEvent::Output { data, .. } => assert_eq!(data, "after"),
                        other => panic!("unexpected event: {other:?}"),
                    }
                    assert!(seq > last_seq);
                    break;
                }
                other => panic!("expected VmApp, got {other:?}"),
            }
        }

        // A bounded replay keeps the newest and counts the rest.
        let bounded = client.handle().attach(&vm_id, 0, 2).await.unwrap();
        assert_eq!(bounded.dropped, 2);
        assert_eq!(bounded.replay.len(), 2);
    }

    /// When the service goes away, in-flight and future requests fail with
    /// `Closed` and event streams end rather than hanging.
    #[tokio::test]
    async fn connection_close_ends_streams_and_requests() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

        let mut client = Client::<ShellProtocol>::connect(&socket_path)
            .await
            .unwrap();

        // Accept and immediately drop the connection.
        let (stream, _) = listener.accept().await.unwrap();
        drop(stream);

        assert!(client.next_event().await.is_none());
        assert!(matches!(client.status().await, Err(ClientError::Closed)));
    }
}
