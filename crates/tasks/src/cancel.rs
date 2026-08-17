//! Interrupting a drain that is waiting on a VM.
//!
//! Both dispatchers spend almost all of a run parked on one `await`: the drain
//! loop, reading a vm-pool event stream until the supervisor reports a terminal
//! event. A run's deadline already interrupts that await, by racing it (see
//! [`crate::deadline`]). A cancel is the same shape of interruption arriving
//! from a different direction, so it travels the same way — [`bounded`] is that
//! race with a third arm.
//!
//! That is the whole reason cancelling is not just `deallocate`. Destroying the
//! VM out from under a parked drain does not wake it: the stream it is reading
//! will simply never produce another event, so the session stays `running`, the
//! serial build lane stays occupied, and nothing says the cancel took. The
//! request has to reach the drain, and the drain then tears the VM down through
//! the path it already uses at the deadline.
//!
//! # Why the request comes out of the store
//!
//! The process that takes the HTTP request is the process running the
//! dispatcher today — but the run may equally have been picked back up by
//! `resume_in_flight` after a restart, and the request may have been made
//! while nothing at all was listening. A durable row covers both, and the
//! store's event broadcast makes the common case immediate. The 5s poll
//! underneath it is not belt-and-braces: it is the backstop for a lagged
//! subscriber (the broadcast drops the oldest events for slow consumers) and
//! for a request that predates the subscription.

use std::future::Future;
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;
use tracing::warn;

use crate::deadline::{Deadline, Expiry};
use crate::events::EventPayload;
use crate::models::RunKind;
use crate::store::{CancelRequest, Store};

/// How often the observer re-reads the store when no event has woken it.
///
/// Short enough that a cancel made while nobody was watching still lands
/// promptly, long enough that a hundred parked scouts are not a load. It is a
/// backstop under a broadcast, not the mechanism.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How a bounded piece of work ended.
///
/// Three outcomes rather than `Result<T, Elapsed>` because a cancel is not a
/// failure of the work and must not be reported as one: the caller turns this
/// into a `cancelled` row with an actor and a rationale, not into a `failed`
/// one.
#[derive(Debug)]
pub enum Bounded<T> {
    /// The work finished on its own terms, within the budget and before any
    /// cancel was noticed.
    Completed(T),
    /// Somebody asked for this run to stop.
    Cancelled(CancelRequest),
    /// The budget ran out on one of its two clocks. The [`Expiry`] is what says
    /// *which*, and therefore whether the caller reports a timeout or a
    /// suspended host — see [`crate::deadline`].
    TimedOut(Expiry),
}

/// Run `work` until it finishes, until someone cancels this run, or until
/// `deadline` expires.
///
/// **`biased`, with the work first.** The three arms are polled in order, so an
/// outcome already in hand is never discarded for a cancel that happened to
/// arrive in the same poll — cancelling a run that finishes in the same breath
/// leaves the run's real outcome standing and says so in the ack. The stale
/// request row is harmless: run ids are never reused.
///
/// Like `tokio::time::timeout`, this *drops* `work` when it does not win, so
/// anything the caller needs to survive an interruption has to live outside the
/// future. Both dispatchers already keep their drain state outside it, because
/// the deadline path needed that first.
pub async fn bounded<T>(
    store: &Store,
    kind: RunKind,
    id: &str,
    deadline: &Deadline,
    work: impl Future<Output = T>,
) -> Bounded<T> {
    tokio::pin!(work);
    tokio::select! {
        biased;
        outcome = &mut work => Bounded::Completed(outcome),
        request = observe(store, kind, id) => Bounded::Cancelled(request),
        expiry = deadline.expired() => Bounded::TimedOut(expiry),
    }
}

/// Resolve when a cancel request exists for this run.
///
/// Subscribe *first*, then read: taking the snapshot first would leave a window
/// in which a request lands between the read and the subscription, and nothing
/// but the poll would ever notice it.
async fn observe(store: &Store, kind: RunKind, id: &str) -> CancelRequest {
    let mut events = store.subscribe_events();

    loop {
        match store.pending_cancel(kind, id).await {
            Ok(Some(request)) => return request,
            Ok(None) => {}
            // A cancel that cannot be read is not a cancel that can be
            // announced: keep the run going and try again on the next tick,
            // rather than tearing down a VM on a database hiccup.
            Err(e) => warn!(%kind, run_id = id, error = %e, "could not read the cancel request"),
        }

        // Either a matching event wakes us, or the poll does. Both then go
        // round to re-read the row, which is what makes the two paths agree on
        // one source of truth.
        tokio::select! {
            received = events.recv() => match received {
                Ok(event) => {
                    if !announces(&event.payload, kind, id) {
                        continue;
                    }
                }
                Err(RecvError::Lagged(missed)) => {
                    warn!(%kind, run_id = id, missed, "cancel watch lagged; falling back to the poll");
                }
                // The store outlives every dispatcher, so this is
                // unreachable in practice; the poll covers it regardless.
                Err(RecvError::Closed) => {
                    tokio::time::sleep(CANCEL_POLL_INTERVAL).await;
                }
            },
            _ = tokio::time::sleep(CANCEL_POLL_INTERVAL) => {}
        }
    }
}

/// Whether an event says a cancel was requested for *this* run.
fn announces(payload: &EventPayload, kind: RunKind, id: &str) -> bool {
    matches!(
        payload,
        EventPayload::RunCancelRequested { run_kind, run_id, .. }
            if *run_kind == kind && run_id == id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use crate::models::Actor;

    async fn store() -> Arc<Store> {
        Arc::new(Store::open_in_memory().await.unwrap())
    }

    async fn ask(store: &Store, kind: RunKind, id: &str) {
        store
            .request_cancel(kind, id, Actor::Human, Some("enough"), Some(1))
            .await
            .unwrap();
    }

    /// The ordinary case: nobody cancels anything and the work is handed back
    /// untouched.
    #[tokio::test]
    async fn work_that_finishes_first_is_what_comes_back() {
        let store = store().await;
        let outcome = bounded(
            &store,
            RunKind::Session,
            "sess_1",
            &Deadline::starting_now(Duration::from_secs(30)),
            async { 7 },
        )
        .await;
        assert!(matches!(outcome, Bounded::Completed(7)), "{outcome:?}");
    }

    /// A request made before anyone was watching — which is what a run picked
    /// back up by `resume_in_flight` after a restart looks like. There is no
    /// broadcast to catch, so this is the read-before-wait path.
    #[tokio::test]
    async fn a_request_already_on_record_interrupts_immediately() {
        let store = store().await;
        ask(&store, RunKind::Session, "sess_1").await;

        let outcome = bounded(
            &store,
            RunKind::Session,
            "sess_1",
            &Deadline::starting_now(Duration::from_secs(30)),
            std::future::pending::<()>(),
        )
        .await;
        match outcome {
            Bounded::Cancelled(request) => {
                assert_eq!(request.exit_reason(), "cancelled by human: enough");
            }
            other => panic!("{other:?}"),
        }
    }

    /// The live path: the request lands while the drain is already parked, and
    /// the broadcast is what wakes it — well inside the poll interval.
    #[tokio::test]
    async fn a_request_that_arrives_mid_run_wakes_the_drain() {
        let store = store().await;
        let writer = store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            ask(&writer, RunKind::Build, "build_1").await;
            writer
                .append_event(EventPayload::RunCancelRequested {
                    run_kind: RunKind::Build,
                    run_id: "build_1".into(),
                    actor: Actor::Human,
                    decision_seq: Some(1),
                })
                .await
                .unwrap();
        });

        let started = std::time::Instant::now();
        let outcome = bounded(
            &store,
            RunKind::Build,
            "build_1",
            &Deadline::starting_now(Duration::from_secs(30)),
            std::future::pending::<()>(),
        )
        .await;
        assert!(matches!(outcome, Bounded::Cancelled(_)), "{outcome:?}");
        assert!(
            started.elapsed() < CANCEL_POLL_INTERVAL,
            "the broadcast should beat the poll, not fall back to it"
        );
    }

    /// Another run's cancel is not this run's. The event carries both halves of
    /// the key precisely so a build and a session that share an id suffix
    /// cannot be confused for each other.
    #[tokio::test]
    async fn a_cancel_for_another_run_is_not_this_one() {
        let store = store().await;
        ask(&store, RunKind::Build, "build_1").await;
        store
            .append_event(EventPayload::RunCancelRequested {
                run_kind: RunKind::Build,
                run_id: "build_1".into(),
                actor: Actor::Human,
                decision_seq: None,
            })
            .await
            .unwrap();

        let outcome = bounded(
            &store,
            RunKind::Session,
            "build_1",
            &Deadline::starting_now(Duration::from_millis(150)),
            std::future::pending::<()>(),
        )
        .await;
        assert!(matches!(outcome, Bounded::TimedOut(_)), "{outcome:?}");
        assert!(!announces(
            &EventPayload::RunCancelRequested {
                run_kind: RunKind::Build,
                run_id: "build_1".into(),
                actor: Actor::Human,
                decision_seq: None,
            },
            RunKind::Session,
            "build_1"
        ));
    }

    /// The deadline still works, and still looks like a deadline: nothing about
    /// cancellation may quietly rebrand a timeout, which CLAUDE.md and the
    /// scout timeout tests pin from the other side.
    #[tokio::test]
    async fn the_budget_still_expires_on_its_own() {
        let store = store().await;
        let outcome = bounded(
            &store,
            RunKind::Session,
            "sess_1",
            &Deadline::starting_now(Duration::from_millis(100)),
            std::future::pending::<()>(),
        )
        .await;
        assert!(matches!(outcome, Bounded::TimedOut(_)), "{outcome:?}");
    }

    /// And a deadline the *host* blew through, rather than the run: the same
    /// arm fires, but the expiry it carries says the machine was asleep, which
    /// is what the dispatchers turn into a `Suspended` rather than a `Timeout`.
    #[tokio::test]
    async fn a_suspended_host_expires_through_the_same_arm_and_says_so() {
        let store = store().await;
        let outcome = bounded(
            &store,
            RunKind::Build,
            "build_1",
            &Deadline::suspended_for(Duration::from_secs(3600), Duration::from_secs(8 * 3600)),
            std::future::pending::<()>(),
        )
        .await;
        match outcome {
            Bounded::TimedOut(expiry) => {
                assert!(expiry.host_slept(), "{expiry:?}");
                assert!(!expiry.to_string().contains("timed out"), "{expiry}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// The `biased` ordering, from the losing side: a cancel already on record
    /// does not take an outcome that is already in hand. Cancelling a run that
    /// finishes in the same breath is fine, and the honest answer is that the
    /// run finished.
    #[tokio::test]
    async fn an_outcome_in_hand_beats_a_cancel_in_the_same_poll() {
        let store = store().await;
        ask(&store, RunKind::Session, "sess_1").await;

        let outcome = bounded(
            &store,
            RunKind::Session,
            "sess_1",
            &Deadline::starting_now(Duration::from_secs(30)),
            async { "concluded" },
        )
        .await;
        assert!(
            matches!(outcome, Bounded::Completed("concluded")),
            "{outcome:?}"
        );
    }
}
