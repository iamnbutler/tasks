//! Agent-output capture, shared by the Scout and Builder dispatchers.
//!
//! One implementation with two owners: a dispatcher spawns a writer keyed to a
//! [`TranscriptOwner`], pushes every output line into the returned
//! [`TranscriptSink`], and [`flush`]es before it finalizes the run's row — so a
//! client refetching on the completion event finds a whole transcript rather
//! than a truncated one.
//!
//! The caps are per *run*, not per session: the same 32 KiB line cut and the
//! same 8 MiB budget bound a build exactly as they bound a scout.

use std::sync::Arc;

use tracing::warn;

use crate::models::{TranscriptOwner, TranscriptStream};
use crate::protocol::LogStream;
use crate::store::Store;

/// Longest single line we persist. One tool result can be enormous; past this
/// the line is cut on a char boundary and marked.
pub const MAX_TRANSCRIPT_LINE_BYTES: usize = 32 * 1024;
/// Total transcript bytes persisted for one run. Past this we write one notice
/// and stop recording — the agent itself keeps running untouched.
pub const MAX_TRANSCRIPT_BYTES_PER_RUN: usize = 8 * 1024 * 1024;
/// Depth of the hand-off queue between the drain loop and the writer task.
const TRANSCRIPT_QUEUE_CAPACITY: usize = 1024;
/// Lines coalesced into one transaction.
const TRANSCRIPT_BATCH: usize = 64;

/// The wire enum meets the domain enum here, at the dispatcher boundary, so
/// neither `models` nor `store` has to know about the protocol crate. A free
/// function rather than `From`: both enums live in other crates (tasks-api,
/// tasks-protocol), so the orphan rule forbids the impl here — and neither of
/// those crates may know about the other.
pub fn transcript_stream(s: LogStream) -> TranscriptStream {
    match s {
        LogStream::Stdout => TranscriptStream::Stdout,
        LogStream::Stderr => TranscriptStream::Stderr,
    }
}

/// Non-blocking handle the drain loop pushes agent output into.
///
/// `push` must never await the store: the drain loop is also what waits for
/// the run's terminal event, and it reads a vm-pool broadcast that drops the
/// oldest events for slow consumers. Making SQLite latency into lost agent
/// events would be a bad trade, so this is a `try_send` onto a bounded queue
/// and a separate task does the writing.
pub struct TranscriptSink {
    tx: tokio::sync::mpsc::Sender<(TranscriptStream, String)>,
    /// Bytes accepted so far, against `MAX_TRANSCRIPT_BYTES_PER_RUN`.
    bytes: usize,
    /// Set once the byte budget is spent, so the notice is written once.
    capped: bool,
    /// Lines lost to queue pressure since the last marker.
    dropped: u64,
    /// Total dropped, for the summary line on the way out.
    pub dropped_total: u64,
}

impl TranscriptSink {
    pub fn push(&mut self, stream: TranscriptStream, line: String) {
        if self.capped {
            return;
        }
        // Scrub *before* truncating, and here rather than per-caller: since
        // #825 this sink is the one write path for both owners, so a single
        // call covers scout sessions and builds at once — which is exactly
        // what #825 said the fix for #759 had to do.
        //
        // Before, not after, because a 32 KiB cut landing inside
        // `x-access-token:<token>@` strands a token prefix with no `@` behind
        // it, which no later pass can recognise as a credential.
        let line = truncate_line(crate::redact::redact_owned(line));
        if self.bytes + line.len() > MAX_TRANSCRIPT_BYTES_PER_RUN {
            self.capped = true;
            // Best-effort: if even this doesn't fit the queue, the log still
            // records the cap when the sink is dropped.
            let _ = self.tx.try_send((
                TranscriptStream::Stderr,
                format!(
                    "[tasks] transcript truncated: this run passed {} bytes; \
                     nothing further will be recorded (the agent is unaffected)",
                    MAX_TRANSCRIPT_BYTES_PER_RUN
                ),
            ));
            return;
        }

        // A dropped line leaves no hole a reader could detect, because seq is
        // assigned at persist time. So say so explicitly as soon as there's room.
        if self.dropped > 0
            && self
                .tx
                .try_send((
                    TranscriptStream::Stderr,
                    format!("[tasks] {} transcript line(s) dropped here", self.dropped),
                ))
                .is_ok()
        {
            self.dropped = 0;
        }

        let len = line.len();
        match self.tx.try_send((stream, line)) {
            Ok(()) => self.bytes += len,
            Err(_) => {
                self.dropped += 1;
                self.dropped_total += 1;
            }
        }
    }
}

/// Cut an over-long line on a char boundary and say how much went missing.
///
/// The `[tasks: truncated ` prefix is a cross-language contract: a cut
/// stream-json record is no longer valid JSON, and clients match this prefix
/// to label the line "truncated record" rather than dumping a wall of escaped
/// JSON. The wording after the prefix can change; the prefix can't.
pub fn truncate_line(line: String) -> String {
    if line.len() <= MAX_TRANSCRIPT_LINE_BYTES {
        return line;
    }
    let mut cut = MAX_TRANSCRIPT_LINE_BYTES;
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    let dropped = line.len() - cut;
    let mut out = line;
    out.truncate(cut);
    out.push_str(&format!("…[tasks: truncated {dropped} bytes]"));
    out
}

/// Spawn the task that drains the sink's queue into the store, coalescing up
/// to [`TRANSCRIPT_BATCH`] lines per transaction. Finishes when the sink is
/// dropped and the queue is empty.
pub fn spawn_transcript_writer(
    store: Arc<Store>,
    owner: TranscriptOwner,
) -> (TranscriptSink, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(TRANSCRIPT_QUEUE_CAPACITY);
    let handle = tokio::spawn(async move {
        let mut batch = Vec::with_capacity(TRANSCRIPT_BATCH);
        while rx.recv_many(&mut batch, TRANSCRIPT_BATCH).await > 0 {
            if let Err(e) = store.append_transcript_lines(&owner, &batch).await {
                warn!(owner = %owner, error = %e, "persisting transcript lines failed");
            }
            batch.clear();
        }
    });
    (
        TranscriptSink {
            tx,
            bytes: 0,
            capped: false,
            dropped: 0,
            dropped_total: 0,
        },
        handle,
    )
}

/// Close the queue and let the writer finish.
///
/// Both dispatchers call this *before* the run's row is finalized and its
/// completion event appended, so a client refetching on that event finds the
/// whole transcript. On the timeout path the drain future is already
/// cancelled, and whatever is queued here is all that survives — which is
/// exactly why the flush cannot wait until after the error escapes.
pub async fn flush(sink: TranscriptSink, writer: tokio::task::JoinHandle<()>, owner: &str) {
    if sink.dropped_total > 0 {
        warn!(
            owner,
            dropped = sink.dropped_total,
            "transcript lines dropped under queue pressure"
        );
    }
    drop(sink);
    if let Err(e) = writer.await {
        warn!(owner, error = %e, "transcript writer task failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_long_lines_are_cut_on_a_char_boundary_and_marked() {
        // Multi-byte chars straddling the cut must not panic or corrupt.
        let line = "é".repeat(MAX_TRANSCRIPT_LINE_BYTES);
        let out = truncate_line(line);
        assert!(out.contains("[tasks: truncated"));
        assert!(out.len() < MAX_TRANSCRIPT_LINE_BYTES + 64);

        let short = "left alone".to_string();
        assert_eq!(truncate_line(short.clone()), short);
    }

    #[test]
    fn a_cut_stream_json_record_stops_being_json_but_stays_marked() {
        // Why clients need the marker: one `Read` of a moderately large file
        // is enough to blow the per-line cap, and what's left is no longer a
        // parseable record — only the marker says why.
        let record = serde_json::json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "content": "y".repeat(MAX_TRANSCRIPT_LINE_BYTES),
                }],
            },
        })
        .to_string();
        assert!(serde_json::from_str::<serde_json::Value>(&record).is_ok());

        let cut = truncate_line(record);
        assert!(
            serde_json::from_str::<serde_json::Value>(&cut).is_err(),
            "a cut record must stop parsing — that's what puts it in the client's raw path"
        );
        assert!(cut.contains("[tasks: truncated "));
    }

    #[test]
    fn the_run_byte_cap_counts_bytes_not_lines() {
        // Room for every push, so nothing is lost to queue pressure and the
        // cap is the only thing that can stop recording.
        let (tx, mut rx) = tokio::sync::mpsc::channel(TRANSCRIPT_QUEUE_CAPACITY);
        let mut sink = TranscriptSink {
            tx,
            bytes: 0,
            capped: false,
            dropped: 0,
            dropped_total: 0,
        };

        let line = "x".repeat(MAX_TRANSCRIPT_LINE_BYTES);
        let fits = MAX_TRANSCRIPT_BYTES_PER_RUN / MAX_TRANSCRIPT_LINE_BYTES;
        for _ in 0..fits {
            sink.push(TranscriptStream::Stdout, line.clone());
        }
        assert!(!sink.capped, "exactly the budget must not trip the cap");

        // One byte over the budget trips it and queues the notice.
        sink.push(TranscriptStream::Stdout, "x".into());
        assert!(sink.capped);
        sink.push(TranscriptStream::Stdout, "ignored after the cap".into());

        let mut recorded = 0;
        let mut last = String::new();
        while let Ok((_, l)) = rx.try_recv() {
            recorded += 1;
            last = l;
        }
        assert_eq!(recorded, fits + 1, "capped pushes must not be recorded");
        assert!(last.contains("transcript truncated"));
    }
}
