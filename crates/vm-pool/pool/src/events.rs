//! Append-only event log with pub/sub and log streaming.
//!
//! Provides:
//! - Append-only event persistence
//! - Pub/sub for real-time event streaming
//! - Per-VM circular log buffers for tailing

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};
use tracing::debug;
use vm_pool_protocol::{AppProtocol, LogStream, NullProtocol, VmId};

/// A timestamped, sequenced event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(
    serialize = "P::Event: Serialize",
    deserialize = "P::Event: serde::de::DeserializeOwned",
))]
pub struct Event<P: AppProtocol = NullProtocol> {
    /// Monotonic sequence number.
    pub seq: u64,
    /// Unix timestamp (ms).
    pub timestamp: u64,
    /// Source VM ID (if from a VM).
    pub vm_id: Option<VmId>,
    /// Event payload.
    pub payload: EventPayload<P>,
}

/// Infrastructure events that vm-pool handles internally (non-application).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InfraEvent {
    Ready,
    Pong,
    Shutdown,
}

/// Event payload types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(bound(
    serialize = "P::Event: Serialize",
    deserialize = "P::Event: serde::de::DeserializeOwned",
))]
pub enum EventPayload<P: AppProtocol = NullProtocol> {
    /// VM lifecycle event.
    VmLifecycle { vm_id: VmId, state: VmState },
    /// Log line from a VM.
    VmLog {
        vm_id: VmId,
        stream: LogStream,
        line: String,
    },
    /// VM infrastructure event (Ready, Pong, Shutdown).
    VmInfra { vm_id: VmId, event: InfraEvent },
    /// Application event from a VM.
    VmApp { vm_id: VmId, event: P::Event },
    /// Pool state change.
    PoolState { total: usize, available: usize },
    /// Service lifecycle.
    Service { state: ServiceState },
}

impl<P: AppProtocol> EventPayload<P> {
    /// Extract the VM ID from this payload, if it has one.
    pub fn vm_id(&self) -> Option<&VmId> {
        match self {
            EventPayload::VmLifecycle { vm_id, .. }
            | EventPayload::VmLog { vm_id, .. }
            | EventPayload::VmInfra { vm_id, .. }
            | EventPayload::VmApp { vm_id, .. } => Some(vm_id),
            EventPayload::PoolState { .. } | EventPayload::Service { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    Allocating,
    Starting,
    Ready,
    Stopping,
    Stopped,
    Crashed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Starting,
    Ready,
    ShuttingDown,
}

/// Circular buffer for VM logs (for tail functionality).
struct VmLogBuffer {
    lines: Vec<(LogStream, String, u64)>,
    capacity: usize,
    head: usize,
    len: usize,
}

impl VmLogBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            lines: Vec::with_capacity(capacity),
            capacity,
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, stream: LogStream, line: String, timestamp: u64) {
        if self.len < self.capacity {
            self.lines.push((stream, line, timestamp));
            self.len += 1;
        } else {
            self.lines[self.head] = (stream, line, timestamp);
            self.head = (self.head + 1) % self.capacity;
        }
    }

    fn tail(&self, n: usize) -> Vec<(LogStream, String, u64)> {
        let n = n.min(self.len);
        if n == 0 {
            return Vec::new();
        }
        if self.len < self.capacity {
            // Buffer hasn't wrapped yet
            self.lines[self.len - n..].to_vec()
        } else {
            // Buffer has wrapped; head points to the oldest entry
            let start = (self.head + self.capacity - n) % self.capacity;
            let mut result = Vec::with_capacity(n);
            for i in 0..n {
                let idx = (start + i) % self.capacity;
                result.push(self.lines[idx].clone());
            }
            result
        }
    }
}

/// Event log with append-only storage and pub/sub.
pub struct EventLog<P: AppProtocol = NullProtocol> {
    events: RwLock<Vec<Event<P>>>,
    next_seq: RwLock<u64>,
    broadcast: broadcast::Sender<Event<P>>,
    vm_logs: RwLock<HashMap<VmId, VmLogBuffer>>,
}

impl<P: AppProtocol> EventLog<P> {
    pub fn new() -> Arc<Self> {
        let (broadcast, _) = broadcast::channel(1024);
        Arc::new(Self {
            events: RwLock::new(Vec::new()),
            next_seq: RwLock::new(0),
            broadcast,
            vm_logs: RwLock::new(HashMap::new()),
        })
    }

    /// Append an event to the log.
    pub async fn append(&self, payload: EventPayload<P>) -> Event<P> {
        let mut seq = self.next_seq.write().await;
        let timestamp = now_ms();
        let vm_id = payload.vm_id().cloned();

        let event = Event {
            seq: *seq,
            timestamp,
            vm_id,
            payload: payload.clone(),
        };
        *seq += 1;
        drop(seq);

        self.events.write().await.push(event.clone());

        if let EventPayload::VmLog {
            vm_id,
            stream,
            line,
        } = &payload
        {
            let mut vm_logs = self.vm_logs.write().await;
            vm_logs
                .entry(vm_id.clone())
                .or_insert_with(|| VmLogBuffer::new(1000))
                .push(*stream, line.clone(), timestamp);
        }

        let _ = self.broadcast.send(event.clone());
        debug!("appended event seq={}", event.seq);

        event
    }

    /// Subscribe to real-time events.
    pub fn subscribe(&self) -> broadcast::Receiver<Event<P>> {
        self.broadcast.subscribe()
    }

    /// Get events since a sequence number.
    pub async fn since(&self, seq: u64) -> Vec<Event<P>> {
        let events = self.events.read().await;
        events.iter().filter(|e| e.seq >= seq).cloned().collect()
    }

    /// Get events for a specific VM.
    pub async fn for_vm(&self, vm_id: &VmId) -> Vec<Event<P>> {
        let events = self.events.read().await;
        events
            .iter()
            .filter(|e| e.vm_id.as_ref() == Some(vm_id))
            .cloned()
            .collect()
    }

    /// Tail the last N log lines for a VM.
    pub async fn tail_vm_logs(&self, vm_id: &VmId, n: usize) -> Vec<(LogStream, String, u64)> {
        let vm_logs = self.vm_logs.read().await;
        vm_logs.get(vm_id).map(|b| b.tail(n)).unwrap_or_default()
    }

    /// Initialize a log buffer for a new VM.
    pub async fn init_vm(&self, vm_id: &VmId) {
        let mut vm_logs = self.vm_logs.write().await;
        vm_logs.insert(vm_id.clone(), VmLogBuffer::new(1000));
    }

    /// Clean up log buffer when VM is deallocated.
    pub async fn cleanup_vm(&self, vm_id: &VmId) {
        let mut vm_logs = self.vm_logs.write().await;
        vm_logs.remove(vm_id);
    }

    /// Total number of events in the log.
    pub async fn len(&self) -> usize {
        self.events.read().await.len()
    }

    /// Whether the event log is empty.
    pub async fn is_empty(&self) -> bool {
        self.events.read().await.is_empty()
    }
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

    #[test]
    fn vm_log_buffer_push_and_tail_under_capacity() {
        let mut buf = VmLogBuffer::new(5);
        buf.push(LogStream::Stdout, "line1".into(), 1);
        buf.push(LogStream::Stderr, "line2".into(), 2);
        buf.push(LogStream::Stdout, "line3".into(), 3);

        let tail = buf.tail(2);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].1, "line2");
        assert_eq!(tail[1].1, "line3");
    }

    #[test]
    fn vm_log_buffer_tail_more_than_available() {
        let mut buf = VmLogBuffer::new(5);
        buf.push(LogStream::Stdout, "line1".into(), 1);
        buf.push(LogStream::Stdout, "line2".into(), 2);

        let tail = buf.tail(10);
        assert_eq!(tail.len(), 2);
    }

    #[test]
    fn vm_log_buffer_wraps_around() {
        let mut buf = VmLogBuffer::new(3);
        buf.push(LogStream::Stdout, "a".into(), 1);
        buf.push(LogStream::Stdout, "b".into(), 2);
        buf.push(LogStream::Stdout, "c".into(), 3);
        // Buffer is now full: [a, b, c], head=0
        buf.push(LogStream::Stdout, "d".into(), 4);
        // Buffer: [d, b, c], head=1 — oldest is b

        let tail = buf.tail(3);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].1, "b");
        assert_eq!(tail[1].1, "c");
        assert_eq!(tail[2].1, "d");
    }

    #[test]
    fn vm_log_buffer_wraps_multiple_times() {
        let mut buf = VmLogBuffer::new(2);
        for i in 0..10 {
            buf.push(LogStream::Stdout, format!("line{}", i), i as u64);
        }
        let tail = buf.tail(2);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].1, "line8");
        assert_eq!(tail[1].1, "line9");
    }

    #[test]
    fn vm_log_buffer_empty() {
        let buf = VmLogBuffer::new(5);
        assert!(buf.tail(5).is_empty());
        assert!(buf.tail(0).is_empty());
    }

    #[tokio::test]
    async fn event_log_append_and_query() {
        let log = EventLog::<NullProtocol>::new();
        let vm_id = VmId::new("vm-1");

        let e1 = log
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Allocating,
            })
            .await;
        assert_eq!(e1.seq, 0);
        assert_eq!(e1.vm_id, Some(vm_id.clone()));

        let e2 = log
            .append(EventPayload::Service {
                state: ServiceState::Ready,
            })
            .await;
        assert_eq!(e2.seq, 1);
        assert_eq!(e2.vm_id, None);

        assert_eq!(log.len().await, 2);
    }

    #[tokio::test]
    async fn event_log_since() {
        let log = EventLog::<NullProtocol>::new();
        let vm_id = VmId::new("vm-1");

        for _ in 0..5 {
            log.append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Ready,
            })
            .await;
        }

        let since_3 = log.since(3).await;
        assert_eq!(since_3.len(), 2);
        assert_eq!(since_3[0].seq, 3);
        assert_eq!(since_3[1].seq, 4);
    }

    #[tokio::test]
    async fn event_log_for_vm() {
        let log = EventLog::<NullProtocol>::new();
        let vm1 = VmId::new("vm-1");
        let vm2 = VmId::new("vm-2");

        log.append(EventPayload::VmLifecycle {
            vm_id: vm1.clone(),
            state: VmState::Ready,
        })
        .await;
        log.append(EventPayload::VmLifecycle {
            vm_id: vm2.clone(),
            state: VmState::Ready,
        })
        .await;
        log.append(EventPayload::VmLifecycle {
            vm_id: vm1.clone(),
            state: VmState::Stopped,
        })
        .await;

        let vm1_events = log.for_vm(&vm1).await;
        assert_eq!(vm1_events.len(), 2);

        let vm2_events = log.for_vm(&vm2).await;
        assert_eq!(vm2_events.len(), 1);
    }

    #[tokio::test]
    async fn event_log_vm_log_tailing() {
        let log = EventLog::<NullProtocol>::new();
        let vm_id = VmId::new("vm-1");
        log.init_vm(&vm_id).await;

        for i in 0..5 {
            log.append(EventPayload::VmLog {
                vm_id: vm_id.clone(),
                stream: LogStream::Stdout,
                line: format!("line {}", i),
            })
            .await;
        }

        let tail = log.tail_vm_logs(&vm_id, 3).await;
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].1, "line 2");
        assert_eq!(tail[1].1, "line 3");
        assert_eq!(tail[2].1, "line 4");
    }

    #[tokio::test]
    async fn event_log_cleanup_vm() {
        let log = EventLog::<NullProtocol>::new();
        let vm_id = VmId::new("vm-1");
        log.init_vm(&vm_id).await;

        log.append(EventPayload::VmLog {
            vm_id: vm_id.clone(),
            stream: LogStream::Stdout,
            line: "hello".into(),
        })
        .await;

        let tail = log.tail_vm_logs(&vm_id, 10).await;
        assert_eq!(tail.len(), 1);

        log.cleanup_vm(&vm_id).await;
        let tail = log.tail_vm_logs(&vm_id, 10).await;
        assert!(tail.is_empty());
    }

    #[tokio::test]
    async fn event_log_subscribe() {
        let log = EventLog::<NullProtocol>::new();
        let mut rx = log.subscribe();

        let vm_id = VmId::new("vm-1");
        log.append(EventPayload::VmLifecycle {
            vm_id: vm_id.clone(),
            state: VmState::Ready,
        })
        .await;

        let event = rx.recv().await.unwrap();
        assert_eq!(event.seq, 0);
        assert_eq!(event.vm_id, Some(vm_id));
    }

    #[test]
    fn event_payload_vm_id_extraction() {
        let vm_id = VmId::new("vm-1");

        let payload: EventPayload = EventPayload::VmLifecycle {
            vm_id: vm_id.clone(),
            state: VmState::Ready,
        };
        assert_eq!(payload.vm_id(), Some(&vm_id));

        let payload: EventPayload = EventPayload::VmInfra {
            vm_id: vm_id.clone(),
            event: InfraEvent::Ready,
        };
        assert_eq!(payload.vm_id(), Some(&vm_id));

        let payload: EventPayload = EventPayload::Service {
            state: ServiceState::Ready,
        };
        assert_eq!(payload.vm_id(), None);

        let payload: EventPayload = EventPayload::PoolState {
            total: 6,
            available: 4,
        };
        assert_eq!(payload.vm_id(), None);
    }
}
