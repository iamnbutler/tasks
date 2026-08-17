//! Picking up work that outlived the process that started it.
//!
//! A scout or a build runs inside a VM, under its own supervisor, managed by
//! vm-pool — a separate daemon. None of that dies when `tasks serve` does. The
//! only thing a restart actually loses is the *stream*: the connection the
//! dispatcher was reading events on. So the recovery is to open a new one and
//! ask vm-pool for what was said in between.
//!
//! # The splice
//!
//! The ordering below is the whole of this module's difficulty, and getting it
//! wrong fails silently — no error, just events that vanish or arrive twice:
//!
//! 1. **Subscribe**, then
//! 2. **attach**, then
//! 3. deliver the **replay**, then
//! 4. deliver **live** events, skipping any the replay already covered.
//!
//! Subscribing first is what makes the two sources overlap. The client's
//! broadcast only delivers what is pushed after the subscription, and the
//! replay only covers what was recorded before the snapshot; reverse the order
//! and everything landing in between falls in the gap with nothing to record
//! that it did. Overlap is recoverable — every event carries the event log's
//! `seq`, so anything at or below the replay's high-water mark is dropped on
//! the way past. A gap is not.
//!
//! # What the replay is not
//!
//! It is not a durable log the consumer can treat as new information. A
//! replayed event may well have been persisted already by the process that
//! died, and there is no watermark saying which. Consumers must therefore
//! treat [`Origin::Replayed`] as "state I need, output I already have" — see
//! `scout::follow`, which rebuilds its in-memory state from the replay but
//! writes one marker line instead of re-persisting the transcript tail.
//!
//! # Whether the peer can be attached to at all
//!
//! vm-pool is a separate daemon with its own lifetime, so a freshly built
//! server routinely talks to a service running an older binary — one that
//! predates [`ServiceCommand::Attach`](vm_pool_protocol::ServiceCommand) and
//! rejects the line at decode time. That rejection is a fact about the
//! *deployment*, not about the run: the scout or build on the other side is
//! alive and would have been recoverable by a newer daemon. Treating it as a
//! failure of the run is the worst available answer, because the run is then
//! killed by the very code path that exists to save it.
//!
//! So it is asked once per boot, about the service, by [`attach_support`] —
//! before any row is claimed. Too old, unanswerable, and unreachable all land
//! in the same place: claim nothing, and let `reconcile_startup` write the
//! rows off exactly as a server without reattachment did.

use std::fmt;

use tracing::info;
use vm_pool_client::{ClientError, ClientHandle, EventStream, PoolStatus};
use vm_pool_protocol::{ATTACH_PROTOCOL_VERSION, ServiceEvent, VmId};

use crate::protocol::{TaskEvent, TasksProtocol};

/// How many past events one attach asks for.
///
/// Bounded, and bounded *by the caller*: the reply is a single line on a
/// line-oriented socket, and a twenty-minute agent emits thousands of
/// `Progress` records. An unbounded replay would be one enormous JSON line.
///
/// The window keeps the newest events, which is the correct end to keep — the
/// terminal event is by construction the last one emitted. The cost is the
/// other end: on a long run the window no longer reaches back to `Started`,
/// which is why the branch it carries is persisted the moment it arrives
/// rather than at finalize.
pub const REPLAY_LIMIT: usize = 256;

/// Whether the vm-pool on the other end of this connection understands
/// [`attach`] at all.
///
/// The two "no" variants are kept apart for the log line, not for the
/// decision — [`is_supported`](Self::is_supported) is false for both, and a
/// pool that will not say what it speaks is not one to send an unrecognised
/// command to.
#[derive(Debug)]
pub enum AttachSupport {
    /// The service speaks this revision, which is new enough.
    Yes(u32),
    /// The service speaks this revision, which predates `attach`.
    TooOld(u32),
    /// The service would not say. `status` is the oldest command there is, so
    /// if that does not come back, nothing better will.
    Unknown(ClientError),
}

impl AttachSupport {
    /// Whether in-flight work may be claimed and reattached to.
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Yes(_))
    }
}

impl fmt::Display for AttachSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Yes(version) => write!(f, "vm-pool speaks protocol v{version}"),
            Self::TooOld(version) => write!(
                f,
                "vm-pool speaks protocol v{version}, and attach needs \
                 v{ATTACH_PROTOCOL_VERSION} — restart vm-pool"
            ),
            Self::Unknown(e) => write!(
                f,
                "vm-pool would not report its protocol version ({e}) — restart vm-pool"
            ),
        }
    }
}

/// Ask the service, once, whether it understands [`attach`].
///
/// One `status` round trip. `status` is used rather than a dedicated handshake
/// precisely because it has been in the protocol since its first revision: a
/// new command would be rejected by exactly the peers this needs an answer
/// from. See [`vm_pool_protocol::PROTOCOL_VERSION`].
pub async fn attach_support(client: &ClientHandle<TasksProtocol>) -> AttachSupport {
    match client.status().await {
        Ok(status) => support_of(&status),
        Err(e) => AttachSupport::Unknown(e),
    }
}

/// The classification half of [`attach_support`], over a [`PoolStatus`] the
/// caller already has.
///
/// Split out so a caller with more than one question about the pool can answer
/// them all from one `status` reply rather than making a round trip each.
pub fn support_of(status: &PoolStatus) -> AttachSupport {
    if status.speaks(ATTACH_PROTOCOL_VERSION) {
        AttachSupport::Yes(status.protocol_version)
    } else {
        AttachSupport::TooOld(status.protocol_version)
    }
}

/// What a reattachment recovered: whether vm-pool still holds the VM, the
/// events recorded while nobody was reading, and how many older ones the
/// window cut off.
#[derive(Debug, Clone)]
pub struct Resume {
    /// Whether the pool still holds the VM.
    ///
    /// **`false` is not the same as "lost."** If the pool reaped the VM after
    /// the run finished, the terminal event is still in `replay` and the work
    /// is entirely recoverable. Only `!present` *and* no terminal event in the
    /// replay is a real orphan — and what counts as terminal is the caller's
    /// judgement, since it is scout/build vocabulary.
    pub present: bool,
    /// Recorded application events for this VM, oldest first.
    pub replay: Vec<TaskEvent>,
    /// Events the [`REPLAY_LIMIT`] window cut off the front of.
    pub dropped: u64,
    /// Highest event-log seq the replay covers.
    last_seq: Option<u64>,
}

impl Resume {
    /// Whether a live event with this seq was already delivered by the replay.
    pub fn covers(&self, seq: u64) -> bool {
        covered(self.last_seq, seq)
    }
}

/// The splice rule, in one place: a replay covers every seq up to and
/// including its own last one. Inclusive, because that last event *is* one the
/// caller has already been handed — an off-by-one here delivers it twice.
fn covered(watermark: Option<u64>, seq: u64) -> bool {
    watermark.is_some_and(|last| seq <= last)
}

/// Subscribe and attach, in that order.
///
/// The subscription is returned alongside the snapshot precisely so a caller
/// cannot take them in the other order — see the module docs for why that
/// loses events with no trace.
pub async fn attach(
    client: &ClientHandle<TasksProtocol>,
    vm_id: &VmId,
) -> Result<(EventStream<TasksProtocol>, Resume), ClientError> {
    let events = client.subscribe_events();
    let attachment = client.attach(vm_id, 0, REPLAY_LIMIT).await?;
    let resume = Resume {
        present: attachment.present,
        dropped: attachment.dropped,
        last_seq: attachment.last_seq(),
        replay: attachment
            .replay
            .into_iter()
            .map(|replayed| replayed.event)
            .collect(),
    };
    info!(
        %vm_id,
        present = resume.present,
        replayed = resume.replay.len(),
        dropped = resume.dropped,
        "attached to a VM left running by a previous process"
    );
    Ok((events, resume))
}

/// Where an event reached the consumer from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Replayed from vm-pool's log. Everything it says is true, but it may
    /// already have been acted on by the process that died — so it is safe to
    /// rebuild *state* from and unsafe to append *output* from.
    Replayed,
    /// Arrived live on this connection, and is new by construction.
    Live,
}

/// One VM's application events: the replay first, then the live stream with
/// the overlap removed.
///
/// The same type serves a fresh dispatch ([`AppEvents::live`], empty replay)
/// and a reattachment ([`AppEvents::resumed`]), which is what lets the drain
/// loops be written once.
pub struct AppEvents<'a> {
    stream: &'a mut EventStream<TasksProtocol>,
    vm_id: VmId,
    replay: std::vec::IntoIter<TaskEvent>,
    /// Live events at or below this seq were already delivered by the replay.
    covered_through: Option<u64>,
}

impl<'a> AppEvents<'a> {
    /// A dispatch this process started: nothing to replay, every live event
    /// is new.
    pub fn live(stream: &'a mut EventStream<TasksProtocol>, vm_id: VmId) -> Self {
        Self {
            stream,
            vm_id,
            replay: Vec::new().into_iter(),
            covered_through: None,
        }
    }

    /// A run picked up from a previous process. Takes `resume` by value: a
    /// replayed `Completed` carries a whole git bundle, and cloning the vector
    /// to keep a second copy would be megabytes for nothing.
    pub fn resumed(
        stream: &'a mut EventStream<TasksProtocol>,
        vm_id: VmId,
        resume: Resume,
    ) -> Self {
        Self {
            stream,
            vm_id,
            covered_through: resume.last_seq,
            replay: resume.replay.into_iter(),
        }
    }

    /// The next event for this VM, or `None` once the stream is closed.
    ///
    /// Events for other VMs (concurrent scouts share one connection) and
    /// pool-level chatter are skipped here, so callers only ever see their
    /// own traffic.
    pub async fn next(&mut self) -> Option<(Origin, TaskEvent)> {
        if let Some(event) = self.replay.next() {
            return Some((Origin::Replayed, event));
        }
        loop {
            let event = self.stream.recv().await?;
            let ServiceEvent::VmApp { vm_id, event, seq } = event else {
                continue;
            };
            if vm_id != self.vm_id {
                continue;
            }
            // Already handed over as part of the replay. Live events are
            // ordered by seq, so this only ever discards the overlap the
            // subscribe-then-attach ordering deliberately created.
            if covered(self.covered_through, seq) {
                continue;
            }
            return Some((Origin::Live, event));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{LogStream, ScoutEvent};

    fn progress(line: &str) -> TaskEvent {
        TaskEvent::Scout(ScoutEvent::Progress {
            stream: LogStream::Stdout,
            line: line.into(),
        })
    }

    fn resume(replay: Vec<TaskEvent>, last_seq: Option<u64>) -> Resume {
        Resume {
            present: true,
            replay,
            dropped: 0,
            last_seq,
        }
    }

    /// The watermark is inclusive: the replay's own last event must not be
    /// delivered a second time when it also arrives live.
    #[test]
    fn the_replay_watermark_covers_its_own_last_event() {
        let r = resume(vec![progress("a"), progress("b")], Some(7));
        assert!(r.covers(0));
        assert!(r.covers(7));
        assert!(!r.covers(8));
    }

    /// An empty replay covers nothing — every live event is new. This is the
    /// case where the VM was allocated but had not said anything yet, and
    /// getting it wrong would swallow the whole run.
    #[test]
    fn an_empty_replay_covers_nothing() {
        let r = resume(Vec::new(), None);
        assert!(!r.covers(0));
        assert!(!r.covers(u64::MAX));
    }
}
