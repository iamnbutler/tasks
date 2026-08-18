//! Shared command and event type definitions for vm-pool.
//!
//! This crate defines the protocol used for communication between:
//! - Host (service) ↔ VM (supervisor) over stdio
//! - Tasks (client) ↔ vm-pool (service) over Unix socket

use std::fmt;
use std::fmt::Debug;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub mod redact;

use redact::is_secret_name;

/// What this build speaks, reported on [`ServiceEvent::PoolStatus`].
///
/// | revision | added |
/// | --- | --- |
/// | 0 ([`PRE_VERSIONING`]) | everything through `unsubscribe_logs` |
/// | 1 | [`ServiceCommand::Attach`], [`ServiceEvent::VmAttached`], `seq` on [`ServiceEvent::VmApp`], and this field |
/// | 2 | `kind` on [`ServiceEvent::Error`] ([`ServiceErrorKind`]) |
///
/// vm-pool is a long-lived daemon upgraded separately from its clients, so a
/// new client routinely talks to a service running an older binary. Serde
/// rescues an added *field* — an absent one decodes as its default. It cannot
/// rescue an added *command*: an old service rejects the whole line at decode
/// time (`unknown variant attach`), and the client sees that as an ordinary
/// service error, indistinguishable from the command failing on its merits.
/// So a caller that needs a command introduced after some revision has to ask
/// first, and this is what it asks about.
///
/// # Adding to the protocol
///
/// Bump `PROTOCOL_VERSION`, and give the addition its own
/// `<THING>_PROTOCOL_VERSION` constant beside [`ATTACH_PROTOCOL_VERSION`], so
/// callers gate on the capability they actually need rather than on a bare
/// number they have to keep in their heads.
pub const PROTOCOL_VERSION: u32 = 2;

/// What a service that predates version reporting says *by omitting the
/// field*.
///
/// It is an answer, not a missing value: such a peer speaks everything through
/// `unsubscribe_logs` and nothing after it.
pub const PRE_VERSIONING: u32 = 0;

/// The revision that introduced [`ServiceCommand::Attach`].
///
/// Gate on this — not on [`PROTOCOL_VERSION`] — when all you need is to
/// reattach; see [`ServiceEvent::PoolStatus`].
pub const ATTACH_PROTOCOL_VERSION: u32 = 1;

/// The revision that introduced [`ServiceErrorKind`] on
/// [`ServiceEvent::Error`].
///
/// It exists because this file's own rule requires one per addition, and it
/// gates **nothing**. The two situations look alike and are not: an added
/// *command* is rejected at decode time by the peers it exists to identify, so
/// [`ServiceCommand::Attach`] has to be asked about first; an added *field* is
/// covered by `#[serde(default)]`, so an absent `kind` decodes fine and every
/// reader already handles [`ServiceErrorKind::Unspecified`]. Its only consumer
/// is a report — "the pool you are talking to predates this, so a refusal
/// cannot be told apart from a misconfiguration". Do **not** turn it into a
/// dispatch gate: an old vm-pool runs scouts and builds perfectly well, and
/// refusing to dispatch because its *error* vocabulary is old would turn a
/// rare miscount into an outage.
pub const ERROR_KIND_PROTOCOL_VERSION: u32 = 2;

// A gate above what this build speaks is a permanent, silent "unsupported"
// against every peer including itself. Catch it at compile time — a `#[test]`
// would trip clippy's `assertions_on_constants` and is the weaker guarantee.
const _: () = assert!(PROTOCOL_VERSION >= ATTACH_PROTOCOL_VERSION);
const _: () = assert!(ATTACH_PROTOCOL_VERSION > PRE_VERSIONING);
const _: () = assert!(PROTOCOL_VERSION >= ERROR_KIND_PROTOCOL_VERSION);
const _: () = assert!(ERROR_KIND_PROTOCOL_VERSION > PRE_VERSIONING);

/// Strongly-typed VM identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VmId(String);

impl VmId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VmId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for VmId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for VmId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Commands sent from host to supervisor (inside VM).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(bound(
    serialize = "P::Command: Serialize",
    deserialize = "P::Command: DeserializeOwned",
))]
pub enum VmCommand<P: AppProtocol = NullProtocol> {
    /// Health check ping.
    Ping,
    /// Graceful shutdown.
    Shutdown,
    /// Application-defined command (forwarded to child processes inside VM).
    App { payload: P::Command },
}

/// Events emitted by supervisor to host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(bound(
    serialize = "P::Event: Serialize",
    deserialize = "P::Event: DeserializeOwned",
))]
pub enum VmEvent<P: AppProtocol = NullProtocol> {
    /// Supervisor is ready.
    Ready,
    /// Pong response to ping.
    Pong,
    /// Supervisor is shutting down.
    Shutdown,
    /// Application-defined event (emitted by child processes inside VM).
    App { payload: P::Event },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Defines application-specific command and event types that flow through VMs.
///
/// vm-pool handles infrastructure messages (ping, shutdown, health, allocation).
/// The application defines everything else via this trait.
///
/// Implementors must be `Clone + Debug` themselves so generic derives on
/// `VmCommand<P>` / `VmEvent<P>` / `Event<P>` can add the bounds they need.
/// Zero-sized markers with `#[derive(Debug, Clone, Copy)]` satisfy this.
pub trait AppProtocol: Send + Sync + Clone + Debug + 'static {
    /// Commands the application sends to processes inside VMs.
    type Command: Serialize + DeserializeOwned + Send + Sync + Clone + Debug + PartialEq + 'static;

    /// Events that processes inside VMs emit back to the application.
    type Event: Serialize + DeserializeOwned + Send + Sync + Clone + Debug + PartialEq + 'static;
}

/// No application messages. Used when only infrastructure operations are needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NullProtocol;

impl AppProtocol for NullProtocol {
    type Command = NullCommand;
    type Event = NullEvent;
}

/// Uninhabited command type — can never be constructed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NullCommand {}

/// Uninhabited event type — can never be constructed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NullEvent {}

/// Built-in protocol for shell command execution.
/// Equivalent to the original hardcoded Execute/Output/CommandCompleted behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellProtocol;

impl AppProtocol for ShellProtocol {
    type Command = ShellCommand;
    type Event = ShellEvent;
}

/// Shell command — execute a shell command inside the VM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShellCommand {
    /// Execute a shell command via `sh -c`.
    Execute { command: String },
}

/// Shell event — output from a shell command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShellEvent {
    /// Command output (stdout/stderr).
    Output { stream: OutputStream, data: String },
    /// Command completed with exit code.
    CommandCompleted { exit_code: i32 },
}

/// Stream type for log output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
    Supervisor,
}

/// A single log line with metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    pub stream: LogStream,
    pub line: String,
    pub timestamp: u64,
}

/// Priority level for VM allocation. Higher priority VMs can evict
/// lower priority ones when the pool is full.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Background/batch work. First to be evicted.
    Low = 0,
    /// Normal interactive work.
    #[default]
    Normal = 1,
    /// Urgent work. Can evict Low and Normal.
    High = 2,
    /// Critical work. Can evict anything below.
    Critical = 3,
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Priority::Low => f.write_str("low"),
            Priority::Normal => f.write_str("normal"),
            Priority::High => f.write_str("high"),
            Priority::Critical => f.write_str("critical"),
        }
    }
}

/// Configuration for a VM.
///
/// `Debug` is hand-written rather than derived, and that is the load-bearing
/// part: `env` is *where a credential enters the system* — for an agent VM it
/// carries the API key — so a derived `Debug` on this type is the leak of #923
/// one `tracing` field away, in a type every caller holds. `Serialize` is
/// untouched: redaction is a property of formatting, never of the data, and
/// the VM still has to be started with the real value.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VmConfig {
    /// CPU cores (default: 2).
    #[serde(default)]
    pub cpus: Option<u32>,
    /// Memory in MB (default: 2048).
    #[serde(default)]
    pub memory_mb: Option<u32>,
    /// Priority level for pool eviction.
    #[serde(default)]
    pub priority: Priority,
    /// Environment variables to set.
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

impl fmt::Debug for VmConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VmConfig")
            .field("cpus", &self.cpus)
            .field("memory_mb", &self.memory_mb)
            .field("priority", &self.priority)
            .field("env", &MaskedEnv(&self.env))
            .finish()
    }
}

/// An environment rendered exactly as `Vec<(String, String)>` renders itself,
/// except that a secret-named value is masked. The *name* is always kept:
/// "did the key get through at all" is what such a line is read for.
struct MaskedEnv<'a>(&'a [(String, String)]);

impl fmt::Debug for MaskedEnv<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(name, value)| {
                let value = if is_secret_name(name) {
                    redact::REDACTED
                } else {
                    value.as_str()
                };
                (name.as_str(), value)
            }))
            .finish()
    }
}

/// Commands sent from Tasks to vm-pool service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(bound(
    serialize = "P::Command: Serialize",
    deserialize = "P::Command: DeserializeOwned",
))]
pub enum ServiceCommand<P: AppProtocol = NullProtocol> {
    /// Allocate a new VM from the pool.
    Allocate { image: String, config: VmConfig },
    /// Deallocate a VM back to the pool.
    Deallocate { vm_id: VmId },
    /// Send an application command to a VM.
    Send { vm_id: VmId, command: P::Command },
    /// Save VM state to a snapshot.
    Snapshot { vm_id: VmId, name: String },
    /// Restore VM from a snapshot.
    Restore { vm_id: VmId, snapshot: String },
    /// Get pool status.
    Status,
    /// Get last N log lines from a VM.
    TailLogs { vm_id: VmId, lines: usize },
    /// Subscribe to real-time logs from a VM (or all VMs if None).
    SubscribeLogs { vm_id: Option<VmId> },
    /// Unsubscribe from log streaming.
    UnsubscribeLogs,
    /// Pick up a VM a previous client was following.
    ///
    /// A client that goes away is invisible from inside the VM: the workload
    /// keeps running and keeps emitting events, and the service keeps logging
    /// them. This asks for that log back — the application events recorded for
    /// `vm_id` with sequence number `since_seq` or higher, newest `limit` of
    /// them — plus whether the pool still holds the VM.
    ///
    /// `limit` is the caller's, deliberately: the reply is one line on a
    /// line-oriented socket, and a long-running agent emits thousands of
    /// events. The newest are kept, because a terminal event is by
    /// construction the last one emitted.
    Attach {
        vm_id: VmId,
        since_seq: u64,
        limit: usize,
    },
}

/// One application event as it was recorded in the event log, carrying the
/// sequence number it was appended under.
///
/// `seq` is what lets a client splice a replay against live traffic without
/// double-delivering: every live event whose `seq` is covered by the replay
/// has already been handed over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "P::Event: Serialize",
    deserialize = "P::Event: DeserializeOwned",
))]
pub struct ReplayedEvent<P: AppProtocol = NullProtocol> {
    pub seq: u64,
    pub event: P::Event,
}

/// Which of vm-pool's *own* conditions produced a [`ServiceEvent::Error`].
///
/// The message beside it is prose written for a human; this is the field a
/// caller decides on. `pool exhausted` and `no such image` differ only as
/// English, and a client that told them apart by grepping would change meaning
/// the next time somebody improved a sentence — so the fact rides its own
/// field.
///
/// vm-pool learns nothing about its callers from this. It states which of its
/// conditions was hit; what to do about a "no" — retry, wait, give up, charge
/// somebody for it — stays entirely the caller's, exactly as this crate's
/// dependency rule requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceErrorKind {
    /// This peer did not say — it predates the field, or the condition has no
    /// better name here. The default, and deliberately the *unhelpful* one:
    /// a caller must not read silence as any specific condition.
    #[default]
    Unspecified,
    /// The pool has no room: every slot is allocated. A property of the
    /// moment rather than of the request.
    Capacity,
    /// No such VM in this pool.
    NoSuchVm,
    /// The VM exists but is not in a state that can take the command.
    NotReady,
    /// The image could not be resolved or parsed.
    Image,
    /// The container runtime failed — spawning, stopping, or talking to a VM.
    Runtime,
    /// vm-pool's own stdio link to a VM failed. **Not** a statement about the
    /// caller's connection to vm-pool, and not the same question as any
    /// "transport" a caller may have of its own.
    Transport,
    /// The request could not be understood at all.
    BadRequest,
    /// Something else this build can name but has no category for.
    Other,
}

impl ServiceErrorKind {
    /// The wire form, and what a log line prints.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Capacity => "capacity",
            Self::NoSuchVm => "no_such_vm",
            Self::NotReady => "not_ready",
            Self::Image => "image",
            Self::Runtime => "runtime",
            Self::Transport => "transport",
            Self::BadRequest => "bad_request",
            Self::Other => "other",
        }
    }

    /// The wire form, read forgivingly: anything unrecognised is
    /// [`ServiceErrorKind::Unspecified`]. See the [`Deserialize`] impl.
    fn from_wire(raw: &str) -> Self {
        match raw {
            "capacity" => Self::Capacity,
            "no_such_vm" => Self::NoSuchVm,
            "not_ready" => Self::NotReady,
            "image" => Self::Image,
            "runtime" => Self::Runtime,
            "transport" => Self::Transport,
            "bad_request" => Self::BadRequest,
            "other" => Self::Other,
            _ => Self::Unspecified,
        }
    }
}

impl fmt::Display for ServiceErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Skew runs both ways, and only one way is covered by `#[serde(default)]`.
///
/// An *older* service omitting the field is the routine case, and the
/// attribute handles it. This impl is hand-written for the other direction: a
/// *newer* service naming a kind this build has never heard of must not make
/// the error response undecodable, because a response that fails to decode is
/// never delivered — and the request it answers then waits for its connection
/// to die. **A refusal turned into a hang is strictly worse than a refusal
/// whose reason is unknown.** Unknown decays to `Unspecified`, which every
/// reader already handles.
///
/// Reading the value as a [`serde_json::Value`] first is what makes a kind
/// sent as a number or a null decay the same way a misspelt string does;
/// `#[serde(other)]` cannot express any of this on a plain unit-variant enum.
/// The same shape as `tasks_protocol::FailureClass`, for the same reason.
impl<'de> Deserialize<'de> for ServiceErrorKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(deserializer)?;
        Ok(raw
            .as_str()
            .map(ServiceErrorKind::from_wire)
            .unwrap_or_default())
    }
}

/// Events emitted by vm-pool service to Tasks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(bound(
    serialize = "P::Event: Serialize",
    deserialize = "P::Event: DeserializeOwned",
))]
pub enum ServiceEvent<P: AppProtocol = NullProtocol> {
    /// VM was allocated.
    VmAllocated { vm_id: VmId, image: String },
    /// VM started and supervisor is ready.
    VmReady { vm_id: VmId },
    /// VM stopped (graceful).
    VmStopped { vm_id: VmId },
    /// VM crashed or was killed.
    VmCrashed { vm_id: VmId, error: String },
    /// Pool status response.
    ///
    /// `protocol_version` is what the *service* speaks — see
    /// [`PROTOCOL_VERSION`]. It rides `status` because `status` has been in the
    /// protocol since its first revision, so a peer of any age answers it; a
    /// dedicated handshake command would be rejected at decode time by exactly
    /// the peers it exists to identify. `#[serde(default)]` makes an absent
    /// field decode as [`PRE_VERSIONING`], which is the correct reading of
    /// silence rather than a missing value — the same technique as `seq` on
    /// [`ServiceEvent::VmApp`].
    PoolStatus {
        total: usize,
        available: usize,
        allocated: usize,
        #[serde(default)]
        protocol_version: u32,
    },
    /// Log line from a VM (streamed).
    VmLog {
        vm_id: VmId,
        stream: LogStream,
        line: String,
    },
    /// Response to TailLogs command.
    LogTail { vm_id: VmId, lines: Vec<LogLine> },
    /// Acknowledgment of log subscription.
    LogsSubscribed { vm_id: Option<VmId> },
    /// An error occurred processing a command.
    ///
    /// `message` is prose for a human. `kind` is the field a caller *decides*
    /// on — see [`ServiceErrorKind`], and never grep the message. `#[serde(default)]`
    /// so a service predating [`ERROR_KIND_PROTOCOL_VERSION`] still decodes;
    /// its silence reads as [`ServiceErrorKind::Unspecified`], which is an
    /// answer ("this peer cannot say") rather than a missing value.
    Error {
        message: String,
        #[serde(default)]
        kind: ServiceErrorKind,
    },
    /// Acknowledgment that an application command was forwarded to a VM.
    CommandSent { vm_id: VmId },
    /// Application event forwarded from a VM.
    ///
    /// `seq` is the event log's sequence number, which a reattaching client
    /// compares against its replay watermark. `#[serde(default)]` so a peer
    /// that predates [`ServiceCommand::Attach`] still decodes — it just reads
    /// every event as seq 0, which is exactly what it did before.
    VmApp {
        vm_id: VmId,
        event: P::Event,
        #[serde(default)]
        seq: u64,
    },
    /// Response to [`ServiceCommand::Attach`].
    ///
    /// `present` is whether the pool still holds the VM. It is read *before*
    /// the replay, so a VM that finishes in between reports `present: true`
    /// alongside its terminal event — the safe way round, since the caller
    /// then reads the outcome instead of writing the work off.
    ///
    /// `present: false` is not the same as "lost": if the pool reaped the VM
    /// after the workload finished, the terminal event is still in `replay`.
    ///
    /// `dropped` counts events the `limit` cut off the front of.
    VmAttached {
        vm_id: VmId,
        present: bool,
        replay: Vec<ReplayedEvent<P>>,
        dropped: u64,
    },
}

/// Wire envelope for a command sent from a client to the service over the
/// Unix socket.
///
/// `id` is assigned by the client and is unique **per connection**. The
/// service echoes it on the direct response to this command, which lets a
/// client correlate responses even while asynchronous events are interleaved
/// on the same connection.
///
/// This envelope applies to the host↔service socket only. The service↔VM
/// stdio protocol ([`VmCommand`] / [`VmEvent`]) is unframed as before.
///
/// Wire form: `{"id":7,"command":{"type":"status"}}`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "P::Command: Serialize",
    deserialize = "P::Command: DeserializeOwned",
))]
pub struct Request<P: AppProtocol = NullProtocol> {
    /// Client-assigned, per-connection request id.
    pub id: u64,
    /// The command to execute.
    pub command: ServiceCommand<P>,
}

impl<P: AppProtocol> Request<P> {
    pub fn new(id: u64, command: ServiceCommand<P>) -> Self {
        Self { id, command }
    }
}

/// Wire envelope for anything the service writes back to a client.
///
/// `id` is `Some(request_id)` when this is the direct response to a command,
/// and `None` for asynchronously pushed events (VM application events,
/// future unsolicited notifications). Clients route `Some` to the pending
/// request that owns the id and `None` to their event stream.
///
/// Wire form (response): `{"id":7,"event":{"type":"pool_status",...}}`
/// Wire form (async event): `{"event":{"type":"vm_app",...}}`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "P::Event: Serialize",
    deserialize = "P::Event: DeserializeOwned",
))]
pub struct Response<P: AppProtocol = NullProtocol> {
    /// The request this responds to, or `None` for a pushed event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// The event payload.
    pub event: ServiceEvent<P>,
}

impl<P: AppProtocol> Response<P> {
    /// A direct response to request `id`.
    pub fn to_request(id: u64, event: ServiceEvent<P>) -> Self {
        Self {
            id: Some(id),
            event,
        }
    }

    /// An asynchronously pushed event, not tied to any request.
    pub fn push(event: ServiceEvent<P>) -> Self {
        Self { id: None, event }
    }

    /// Whether this is a pushed event rather than a command response.
    pub fn is_push(&self) -> bool {
        self.id.is_none()
    }
}

/// Encode a value as a JSON line (no embedded newlines, terminated by \n).
pub fn encode_json_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut json = serde_json::to_string(value)?;
    json.push('\n');
    Ok(json)
}

/// Decode a JSON line.
pub fn decode_json_line<'a, T: Deserialize<'a>>(line: &'a str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_id_display() {
        let id = VmId::new("vm-abc123");
        assert_eq!(id.to_string(), "vm-abc123");
        assert_eq!(id.as_str(), "vm-abc123");
    }

    #[test]
    fn vm_id_serde_transparent() {
        let id = VmId::new("vm-abc123");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"vm-abc123\"");
        let parsed: VmId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn vm_id_equality_and_hash() {
        use std::collections::HashSet;
        let a = VmId::new("vm-1");
        let b = VmId::from("vm-1".to_string());
        let c: VmId = "vm-1".into();
        assert_eq!(a, b);
        assert_eq!(b, c);
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }

    #[test]
    fn vm_command_shutdown_roundtrip() {
        let cmd: VmCommand = VmCommand::Shutdown;
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, "{\"type\":\"shutdown\"}");
        let parsed: VmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn vm_command_ping_roundtrip() {
        let cmd: VmCommand = VmCommand::Ping;
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, "{\"type\":\"ping\"}");
        let parsed: VmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn vm_event_ready_roundtrip() {
        let event: VmEvent = VmEvent::Ready;
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, "{\"type\":\"ready\"}");
        let parsed: VmEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn service_command_allocate_roundtrip() {
        let cmd: ServiceCommand = ServiceCommand::Allocate {
            image: "agent:v1.0.0".into(),
            config: VmConfig {
                cpus: Some(2),
                memory_mb: Some(4096),
                priority: Priority::High,
                env: vec![("KEY".into(), "VALUE".into())],
            },
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: ServiceCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn service_command_status_roundtrip() {
        let cmd: ServiceCommand = ServiceCommand::Status;
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, "{\"type\":\"status\"}");
        let parsed: ServiceCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn service_command_send_shell_roundtrip() {
        let cmd: ServiceCommand<ShellProtocol> = ServiceCommand::Send {
            vm_id: VmId::new("vm-abc"),
            command: ShellCommand::Execute {
                command: "echo hi".into(),
            },
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: ServiceCommand<ShellProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn service_event_vm_app_roundtrip() {
        let event: ServiceEvent<ShellProtocol> = ServiceEvent::VmApp {
            vm_id: VmId::new("vm-abc"),
            event: ShellEvent::Output {
                stream: OutputStream::Stdout,
                data: "hello\n".into(),
            },
            seq: 17,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: ServiceEvent<ShellProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    /// A peer that predates attach sends `vm_app` without a `seq`. It must
    /// still decode — the field defaults, and such a peer has no replay to
    /// splice against anyway.
    #[test]
    fn service_event_vm_app_decodes_without_a_seq() {
        let json = r#"{"type":"vm_app","vm_id":"vm-abc",
            "event":{"type":"command_completed","exit_code":0}}"#;
        let parsed: ServiceEvent<ShellProtocol> = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            ServiceEvent::VmApp {
                vm_id: VmId::new("vm-abc"),
                event: ShellEvent::CommandCompleted { exit_code: 0 },
                seq: 0,
            }
        );
    }

    #[test]
    fn service_command_attach_roundtrip() {
        let cmd: ServiceCommand<ShellProtocol> = ServiceCommand::Attach {
            vm_id: VmId::new("vm-abc"),
            since_seq: 12,
            limit: 256,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: ServiceCommand<ShellProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn service_event_vm_attached_roundtrip() {
        let event: ServiceEvent<ShellProtocol> = ServiceEvent::VmAttached {
            vm_id: VmId::new("vm-abc"),
            present: true,
            replay: vec![
                ReplayedEvent {
                    seq: 4,
                    event: ShellEvent::Output {
                        stream: OutputStream::Stdout,
                        data: "working\n".into(),
                    },
                },
                ReplayedEvent {
                    seq: 9,
                    event: ShellEvent::CommandCompleted { exit_code: 0 },
                },
            ],
            dropped: 3,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: ServiceEvent<ShellProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn service_event_error_roundtrip() {
        let event: ServiceEvent = ServiceEvent::Error {
            message: "pool exhausted".into(),
            kind: ServiceErrorKind::Capacity,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"error\""));
        assert!(json.contains("\"kind\":\"capacity\""), "got: {json}");
        let parsed: ServiceEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    /// Every kind goes out and comes back as itself. Without this, the
    /// forgiving `Deserialize` below could decay *everything* and the
    /// decay test would still pass.
    #[test]
    fn every_error_kind_survives_its_own_wire_form() {
        for kind in [
            ServiceErrorKind::Unspecified,
            ServiceErrorKind::Capacity,
            ServiceErrorKind::NoSuchVm,
            ServiceErrorKind::NotReady,
            ServiceErrorKind::Image,
            ServiceErrorKind::Runtime,
            ServiceErrorKind::Transport,
            ServiceErrorKind::BadRequest,
            ServiceErrorKind::Other,
        ] {
            let event: ServiceEvent = ServiceEvent::Error {
                message: "something".into(),
                kind,
            };
            let json = serde_json::to_string(&event).unwrap();
            assert!(json.contains(kind.as_str()), "{kind} not on the wire: {json}");
            let parsed: ServiceEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, event, "{kind}");
        }
    }

    /// The routine skew: a service predating [`ERROR_KIND_PROTOCOL_VERSION`]
    /// omits the field entirely. Silence is an answer — "this peer cannot
    /// say" — and never a decode failure.
    #[test]
    fn an_error_from_a_service_that_cannot_say_reads_as_unspecified() {
        let json = r#"{"type":"error","message":"allocate failed: pool exhausted"}"#;
        let parsed: ServiceEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            ServiceEvent::Error {
                message: "allocate failed: pool exhausted".into(),
                kind: ServiceErrorKind::Unspecified,
            }
        );
    }

    /// The other direction, and the one the hand-written impl is for. A
    /// *newer* service naming a kind this build has never heard of must still
    /// deliver the refusal: an undecodable error response is never delivered
    /// at all, and the request it answers then hangs until the connection
    /// dies. A known kind is asserted in the same test, or "everything
    /// decays" would pass this.
    #[test]
    fn an_error_kind_this_build_cannot_read_decays_instead_of_failing() {
        for wire in [
            r#""quantum_flux""#,
            "7",
            "null",
            "true",
            r#"{"nested":"object"}"#,
        ] {
            let json = format!(r#"{{"type":"error","message":"nope","kind":{wire}}}"#);
            let parsed: ServiceEvent =
                serde_json::from_str(&json).unwrap_or_else(|e| panic!("{wire} failed to decode: {e}"));
            assert_eq!(
                parsed,
                ServiceEvent::Error {
                    message: "nope".into(),
                    kind: ServiceErrorKind::Unspecified,
                },
                "{wire}"
            );
        }

        // The negative half: a kind this build *does* know is not decayed.
        let json = r#"{"type":"error","message":"nope","kind":"capacity"}"#;
        let parsed: ServiceEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            ServiceEvent::Error {
                message: "nope".into(),
                kind: ServiceErrorKind::Capacity,
            }
        );
    }

    #[test]
    fn service_event_pool_status_roundtrip() {
        let event: ServiceEvent = ServiceEvent::PoolStatus {
            total: 6,
            available: 4,
            allocated: 2,
            protocol_version: PROTOCOL_VERSION,
        };
        let json = serde_json::to_string(&event).unwrap();
        // Against the constant, not a literal: what matters is that the
        // field is on the wire, and pinning the number only breaks the next
        // bump.
        assert!(
            json.contains(&format!("\"protocol_version\":{PROTOCOL_VERSION}")),
            "got: {json}"
        );
        let parsed: ServiceEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    /// The load-bearing compatibility case: a service that predates version
    /// reporting sends `pool_status` without the field, and that silence has
    /// to read as an answer — [`PRE_VERSIONING`] — rather than as a decode
    /// failure. Everything the gate does rests on getting an answer here.
    #[test]
    fn service_event_pool_status_decodes_without_a_version() {
        let json = r#"{"type":"pool_status","total":6,"available":4,"allocated":2}"#;
        let parsed: ServiceEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            ServiceEvent::PoolStatus {
                total: 6,
                available: 4,
                allocated: 2,
                protocol_version: PRE_VERSIONING,
            }
        );
    }

    #[test]
    fn service_event_log_tail_roundtrip() {
        let event: ServiceEvent = ServiceEvent::LogTail {
            vm_id: VmId::new("vm-1"),
            lines: vec![
                LogLine {
                    stream: LogStream::Stdout,
                    line: "output line".into(),
                    timestamp: 1234567890,
                },
                LogLine {
                    stream: LogStream::Stderr,
                    line: "error line".into(),
                    timestamp: 1234567891,
                },
            ],
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: ServiceEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn vm_config_defaults() {
        let config = VmConfig::default();
        assert_eq!(config.cpus, None);
        assert_eq!(config.memory_mb, None);
        assert!(config.env.is_empty());
    }

    #[test]
    fn vm_config_missing_fields_deserialize() {
        let json = "{}";
        let config: VmConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config, VmConfig::default());
    }

    #[test]
    fn encode_decode_json_line() {
        let cmd: VmCommand = VmCommand::Ping;
        let line = encode_json_line(&cmd).unwrap();
        assert!(line.ends_with('\n'));
        assert!(!line[..line.len() - 1].contains('\n'));
        let parsed: VmCommand = decode_json_line(&line).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn log_stream_variants() {
        let streams = [LogStream::Stdout, LogStream::Stderr, LogStream::Supervisor];
        for stream in streams {
            let json = serde_json::to_string(&stream).unwrap();
            let parsed: LogStream = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, stream);
        }
    }

    #[test]
    fn output_stream_variants() {
        let streams = [OutputStream::Stdout, OutputStream::Stderr];
        for stream in streams {
            let json = serde_json::to_string(&stream).unwrap();
            let parsed: OutputStream = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, stream);
        }
    }

    #[test]
    fn service_command_subscribe_logs_with_vm_id() {
        let cmd: ServiceCommand = ServiceCommand::SubscribeLogs {
            vm_id: Some(VmId::new("vm-1")),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: ServiceCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn service_command_subscribe_logs_all() {
        let cmd: ServiceCommand = ServiceCommand::SubscribeLogs { vm_id: None };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: ServiceCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn shell_command_execute_roundtrip() {
        let cmd = ShellCommand::Execute {
            command: "ls -la".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"execute\""));
        let parsed: ShellCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn shell_event_output_roundtrip() {
        let event = ShellEvent::Output {
            stream: OutputStream::Stdout,
            data: "hello\n".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: ShellEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn shell_event_command_completed_roundtrip() {
        let event = ShellEvent::CommandCompleted { exit_code: 42 };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: ShellEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn vm_command_app_shell_roundtrip() {
        let cmd: VmCommand<ShellProtocol> = VmCommand::App {
            payload: ShellCommand::Execute {
                command: "ls".into(),
            },
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"app\""));
        let parsed: VmCommand<ShellProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn vm_event_app_shell_roundtrip() {
        let event: VmEvent<ShellProtocol> = VmEvent::App {
            payload: ShellEvent::Output {
                stream: OutputStream::Stdout,
                data: "hello\n".into(),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: VmEvent<ShellProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn request_envelope_roundtrip() {
        let req: Request = Request::new(7, ServiceCommand::Status);
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, "{\"id\":7,\"command\":{\"type\":\"status\"}}");
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, req);
    }

    #[test]
    fn request_envelope_with_app_command() {
        let req: Request<ShellProtocol> = Request::new(
            42,
            ServiceCommand::Send {
                vm_id: VmId::new("vm-abc"),
                command: ShellCommand::Execute {
                    command: "echo hi".into(),
                },
            },
        );
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request<ShellProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, req);
    }

    #[test]
    fn response_to_request_carries_id() {
        let resp: Response = Response::to_request(
            7,
            ServiceEvent::PoolStatus {
                total: 3,
                available: 2,
                allocated: 1,
                protocol_version: PROTOCOL_VERSION,
            },
        );
        assert!(!resp.is_push());
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.starts_with("{\"id\":7,"), "got: {json}");
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn response_push_omits_id() {
        let resp: Response<ShellProtocol> = Response::push(ServiceEvent::VmApp {
            vm_id: VmId::new("vm-abc"),
            event: ShellEvent::Output {
                stream: OutputStream::Stdout,
                data: "hello\n".into(),
            },
            seq: 1,
        });
        assert!(resp.is_push());
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("\"id\""), "got: {json}");
        let parsed: Response<ShellProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
        assert_eq!(parsed.id, None);
    }

    #[test]
    fn response_missing_id_field_deserializes_as_push() {
        let json = "{\"event\":{\"type\":\"vm_ready\",\"vm_id\":\"vm-1\"}}";
        let parsed: Response = serde_json::from_str(json).unwrap();
        assert!(parsed.is_push());
        assert_eq!(
            parsed.event,
            ServiceEvent::VmReady {
                vm_id: VmId::new("vm-1")
            }
        );
    }

    #[test]
    fn envelope_json_lines_are_single_line() {
        let req: Request = Request::new(1, ServiceCommand::Status);
        let line = encode_json_line(&req).unwrap();
        assert!(line.ends_with('\n'));
        assert!(!line[..line.len() - 1].contains('\n'));
        let parsed: Request = decode_json_line(&line).unwrap();
        assert_eq!(parsed, req);
    }

    #[test]
    fn vm_command_infra_with_null_protocol() {
        let cmd: VmCommand<NullProtocol> = VmCommand::Ping;
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, "{\"type\":\"ping\"}");
        let parsed: VmCommand<NullProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn vm_event_infra_with_null_protocol() {
        let event: VmEvent<NullProtocol> = VmEvent::Ready;
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, "{\"type\":\"ready\"}");
        let parsed: VmEvent<NullProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }
}
