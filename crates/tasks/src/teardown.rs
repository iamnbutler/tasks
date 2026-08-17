//! Bounded VM teardown, shared by both dispatchers.
//!
//! Handing a VM back is a request/response round-trip to vm-pool, and it was
//! unbounded on every path including failure. `build_5c65e18a` hit its 3600s
//! budget on schedule and then sat in `deallocate` for 84 minutes, holding the
//! serial build queue and writing nothing to the event log — a stall that is
//! invisible from the outside, which is what made the incident hard to read.
//!
//! So teardown gets its own small budget, and abandoning it is an event rather
//! than silence. Two things follow from that and should not be "fixed":
//!
//! - **Abandoning a `deallocate` leaks one entry in the vm-pool client's
//!   in-flight request table.** `request()` inserts its oneshot sender before
//!   awaiting, so cancelling at the await leaves the map entry until the
//!   response arrives or the connection closes. One entry per abandoned
//!   teardown, bounded by connection lifetime, versus an unbounded stall of
//!   the whole queue. Deliberate trade.
//! - **Freeing the VM is vm-pool's job either way.** Walking away leaves
//!   exactly the state the pool already handles when the server is killed
//!   mid-call: the VM's event stream ends when it dies, and the pool frees its
//!   slot at that moment — and a whole daemon's worth is stopped by the next
//!   daemon on its socket, off the ledger this one wrote. Nothing here needs
//!   to retry. (This used to claim the pool reaped VMs "the server stops
//!   tracking", which was never a thing the pool could see.)

use std::time::Duration;

use tracing::warn;
use vm_pool_client::ClientHandle;
use vm_pool_protocol::VmId;

use crate::events::EventPayload;
use crate::protocol::TasksProtocol;
use crate::store::Store;

/// How long a teardown may take before we stop waiting for it.
///
/// A constant, not an env var: the config surface is already large, and this
/// is an infrastructure sanity bound rather than an operating choice. If a
/// real pool is ever slow enough to need it tuned, that is a vm-pool bug worth
/// seeing. It is additive to the run budget, so it must stay small relative to
/// one — it is not a second budget to tune.
pub(crate) const DEALLOCATE_TIMEOUT: Duration = Duration::from_secs(120);

/// Hand a VM back, giving up after `timeout`. Returns whether the pool
/// acknowledged.
///
/// Never returns an error to the caller: a dispatch's outcome belongs to its
/// agent, not to how tidily the VM went away. In particular a teardown that
/// expires after a *successful* build must not make the build look failed,
/// which is why this is an event-log note and never an `exit_reason`.
///
/// `timeout` is a parameter rather than read from [`DEALLOCATE_TIMEOUT`] so a
/// test can drive the expiry path in milliseconds; both call sites pass the
/// constant.
pub(crate) async fn deallocate_bounded(
    client: &ClientHandle<TasksProtocol>,
    store: &Store,
    vm_id: &VmId,
    owner: &str,
    timeout: Duration,
) -> bool {
    match tokio::time::timeout(timeout, client.deallocate(vm_id)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            warn!(%vm_id, owner, error = %e, "failed to deallocate VM");
            false
        }
        Err(_elapsed) => {
            let secs = timeout.as_secs();
            warn!(%vm_id, owner, secs, "deallocate did not answer; abandoning it");
            // The log line above is for whoever is tailing the server; this is
            // for whoever is watching the stream afterwards, wondering where
            // the time went.
            let message = format!(
                "gave up waiting {secs}s for vm-pool to deallocate {vm_id} \
                 ({owner}); the pool frees its slot when that VM's event stream ends"
            );
            if let Err(e) = store
                .append_event(EventPayload::Note {
                    source: crate::run::DISPATCHER.into(),
                    message,
                })
                .await
            {
                warn!(%vm_id, error = %e, "could not record the abandoned teardown");
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use tokio::io::{AsyncBufReadExt, BufReader};
    use vm_pool_client::Client;

    /// A real client, a real socket, real framing — only the service's
    /// *silence* stands in. The listener accepts and reads every request line
    /// and answers none, which is what an 84-minute deallocate looked like
    /// from this side.
    #[tokio::test]
    async fn a_deallocate_that_never_answers_is_abandoned_and_said_so() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("silent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stream).lines();
                    while let Ok(Some(_line)) = lines.next_line().await {
                        // Read it, and say nothing.
                    }
                });
            }
        });

        let client = Client::<TasksProtocol>::connect(&socket).await.unwrap();
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let vm_id = VmId::new("vm-stuck".to_string());

        let started = std::time::Instant::now();
        let acked = deallocate_bounded(
            &client.handle(),
            &store,
            &vm_id,
            "build build_5c65e18a",
            Duration::from_millis(200),
        )
        .await;
        assert!(!acked, "nothing answered, so nothing was acknowledged");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the whole point is that it returns"
        );

        let notes: Vec<String> = store
            .all_events()
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                EventPayload::Note { message, .. } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(
            notes.len(),
            1,
            "a hung teardown is not allowed to be silent"
        );
        assert!(notes[0].contains("vm-stuck"), "{}", notes[0]);
        assert!(notes[0].contains("build_5c65e18a"), "{}", notes[0]);
    }
}
