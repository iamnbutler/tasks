//! Shared command and event type definitions for vm-pool.
//!
//! This crate defines the protocol used for communication between:
//! - Host (service) ↔ VM (supervisor) over stdio
//! - Tasks (client) ↔ vm-pool (service) over Unix socket

use std::fmt;
use std::fmt::Debug;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Background/batch work. First to be evicted.
    Low = 0,
    /// Normal interactive work.
    Normal = 1,
    /// Urgent work. Can evict Low and Normal.
    High = 2,
    /// Critical work. Can evict anything below.
    Critical = 3,
}

impl Default for Priority {
    fn default() -> Self {
        Priority::Normal
    }
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    PoolStatus {
        total: usize,
        available: usize,
        allocated: usize,
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
    Error { message: String },
    /// Acknowledgment that an application command was forwarded to a VM.
    CommandSent { vm_id: VmId },
    /// Application event forwarded from a VM.
    VmApp { vm_id: VmId, event: P::Event },
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
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: ServiceEvent<ShellProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn service_event_error_roundtrip() {
        let event: ServiceEvent = ServiceEvent::Error {
            message: "pool exhausted".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"error\""));
        let parsed: ServiceEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn service_event_pool_status_roundtrip() {
        let event: ServiceEvent = ServiceEvent::PoolStatus {
            total: 6,
            available: 4,
            allocated: 2,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: ServiceEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
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
