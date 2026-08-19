//! Whether vm-pool has a slot to give, and whether dispatch should wait for
//! one.
//!
//! A Scout that cannot be allocated a VM used to fail *and strand its task*:
//! `Scout::dispatch` moves the task to `Scouting` before it allocates, and the
//! refusal returns before any session row exists, so nothing put the task back
//! to `Queued` and `run::next_dispatchable` — which looks only at `Queued` —
//! could not see it again until the next boot's reconciliation (#967). The
//! unwind in `Scout::dispatch` fixes the stranding; this module is the other
//! half, and neither works alone. A bare requeue against a pool that stays full
//! is a retry at `DISPATCH_TICK` — twice a second, forever, each one a waiver
//! `Note` on the event feed the app and the orchestrator read.
//!
//! So this is the third dispatch hold, beside [`crate::github_health`] and
//! [`crate::updates`], at the same two gates. In memory, like both of them,
//! and for the same reason: it is a fact about another process with a
//! timestamp on it.
//!
//! Two decisions carry the design.
//!
//! **The record is written only from a `status` round trip, never from
//! classifying a refusal.** A refusal reaches the host as a message, and this
//! codebase does not decide on reason text; `PoolStatus::available` is
//! `max_vms - allocated`, which is the exact quantity `Pool::allocate` checks.
//! It also breaks a circle: the natural clearing signal for a
//! refusal-driven record would be a *successful allocation*, which is the one
//! thing a hold prevents. (This is deliberately **not** wired to #930's
//! `ServiceErrorKind` for that reason — the two answer different questions,
//! and one of them needs a vm-pool restart to become true while `available`
//! has been on `pool_status` since long before either.)
//!
//! **[`PoolHealth::probe_due`] claims the slot**, so the scout loop and the
//! build lane make one round trip between them rather than two — and, under
//! that same claim, exactly one of two racing callers gets the [`Transition`]
//! that writes the `Note`. Announcing off the *hold* instead would be a `Note`
//! per loop per tick, which is the event-log flood this whole change exists to
//! prevent, one level up.
//!
//! Nothing can deadlock on it: what releases the hold is a VM handed back by
//! work already in flight, which the gate does not touch. A permanently full
//! pool holds forever — correctly, and now visibly, rather than burning
//! attempts.

use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use vm_pool_client::{ClientError, PoolStatus};

use tasks_api::http::PoolHold;

/// How often the record is refreshed. One local unix round trip, claimed by
/// whichever gate asks first.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// How long a record stays evidence without a refresh.
///
/// Mostly there to keep `/status` honest: the two gates refresh the record as
/// they read it, so in the steady state nothing ever reaches this. It is the
/// backstop for a dispatcher that stopped asking.
pub const STALE_AFTER: Duration = Duration::from_secs(60);

/// The invariant is a *relationship* between the two, not two knobs: the
/// window has to outlast several probes or a live gate would expire its own
/// record between reads.
const _: () = assert!(STALE_AFTER.as_secs() >= 4 * PROBE_INTERVAL.as_secs());

/// A run of observations in which the pool had no free slot, not yet ended by
/// one where it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exhaustion {
    /// The first full observation in this run. **Not** moved by later ones: a
    /// human asking why nothing is dispatching wants "full since 03:12".
    pub since: DateTime<Utc>,
    /// The most recent observation, which is what keeps the record from going
    /// stale under a live gate.
    pub last: DateTime<Utc>,
    /// How many observations this run has made. One is enough to hold; the
    /// count is for the human reading `/status`.
    pub observations: u32,
    /// How many slots the pool holds in total. Carried because `0 of 0` is a
    /// `VM_POOL_MAX_VMS` that can never dispatch anything, while `0 of 6` is
    /// work — or a leak — holding every slot, and the two want different
    /// answers from whoever reads them.
    pub total: usize,
}

impl Exhaustion {
    /// The sentence the event log and `/status` get.
    ///
    /// `0 of N` rather than "full", for the reason `total` is carried at all.
    pub fn describe(&self) -> String {
        format!(
            "vm-pool has no free slot (0 of {} since {}, {} observation(s)). Scout and \
             build dispatch waits for one: queued work stays queued, nothing is charged \
             an attempt, and the next VM handed back releases it",
            self.total,
            self.since.to_rfc3339(),
            self.observations
        )
    }

    /// The wire shape `/status` reports.
    pub fn to_hold(&self) -> PoolHold {
        PoolHold {
            since: self.since,
            last_seen: self.last,
            observations: self.observations,
            total: self.total,
        }
    }
}

/// What one observation did to the record — the edge, so a caller can announce
/// it exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Nothing an announcement would be about: a free slot while free, a
    /// further full observation inside a hold already in force, or a `status`
    /// that could not be read.
    Unchanged,
    /// A hold just went on.
    Exhausted(Exhaustion),
    /// A hold that was in force just came off.
    Freed(Exhaustion),
}

/// Whether vm-pool has room, as last observed.
///
/// One record, shared by both dispatchers and `/status`, so the three cannot
/// disagree about whether a hold is in force.
#[derive(Debug, Default)]
pub struct PoolHealth {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    exhaustion: Option<Exhaustion>,
    /// When a probe was last *claimed* — not when it answered. Claiming is
    /// what makes two gates share one round trip.
    probed_at: Option<DateTime<Utc>>,
}

impl PoolHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim the next probe, if one is due.
    ///
    /// `true` at most once per [`PROBE_INTERVAL`] across every caller: the
    /// claim is taken under the lock, so of two gates asking in the same tick
    /// one probes and the other reads what it wrote.
    pub fn probe_due(&self, now: DateTime<Utc>) -> bool {
        let mut inner = self.inner.lock().expect("pool health lock");
        let due = match inner.probed_at {
            // Signed, like `in_force` below: a clock that stepped backwards
            // makes the last probe look like the future, and the answer to
            // that is to probe, never to wait out a window that will not end.
            Some(last) => now - last >= interval(),
            None => true,
        };
        if due {
            inner.probed_at = Some(now);
        }
        due
    }

    /// Fold one `status` round trip in, and say what edge it crossed.
    ///
    /// - `available == 0` starts a hold or extends one.
    /// - Any free slot clears it.
    /// - **An unreadable `status` touches nothing.** It is not evidence of a
    ///   full pool and not evidence of a free one, and the loops answer a dead
    ///   socket by reconnecting, which is louder and more specific. Same rule
    ///   as [`crate::github_health`]'s first, and it matters here for the same
    ///   reason: absence of evidence must never hold.
    pub fn observe(
        &self,
        status: &Result<PoolStatus, ClientError>,
        now: DateTime<Utc>,
    ) -> Transition {
        let Ok(status) = status else {
            return Transition::Unchanged;
        };
        let mut inner = self.inner.lock().expect("pool health lock");
        if status.available > 0 {
            return match inner.exhaustion.take() {
                Some(run) if in_force(&run, now) => Transition::Freed(run),
                _ => Transition::Unchanged,
            };
        }
        match inner.exhaustion.as_mut() {
            Some(run) if in_force(run, now) => {
                run.last = now;
                run.observations += 1;
                run.total = status.total;
                Transition::Unchanged
            }
            // Nothing held, or what was held had already expired: a fresh edge
            // either way, and `since` starts here.
            _ => {
                let run = Exhaustion {
                    since: now,
                    last: now,
                    observations: 1,
                    total: status.total,
                };
                inner.exhaustion = Some(run.clone());
                Transition::Exhausted(run)
            }
        }
    }

    /// The hold in force right now, if any — the one predicate both gates and
    /// `/status` read.
    pub fn hold(&self, now: DateTime<Utc>) -> Option<Exhaustion> {
        let inner = self.inner.lock().expect("pool health lock");
        inner
            .exhaustion
            .as_ref()
            .filter(|run| in_force(run, now))
            .cloned()
    }
}

fn interval() -> chrono::Duration {
    chrono::Duration::from_std(PROBE_INTERVAL).expect("probe interval fits")
}

fn stale_after() -> chrono::Duration {
    chrono::Duration::from_std(STALE_AFTER).expect("staleness window fits")
}

/// Whether a record is recent enough to still be evidence.
///
/// **Signed**, exactly as [`crate::github_health`]'s is: a clock that stepped
/// backwards makes the record look like it came from the future, which is the
/// one thing it certainly is not, and an absolute age would read the step as
/// "far too old" and release a hold set moments ago.
fn in_force(run: &Exhaustion, now: DateTime<Utc>) -> bool {
    now - run.last <= stale_after()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn status(available: usize, total: usize) -> Result<PoolStatus, ClientError> {
        Ok(PoolStatus {
            total,
            available,
            allocated: total - available,
            protocol_version: vm_pool_protocol::PROTOCOL_VERSION,
        })
    }

    /// Nothing observed never holds — the rule that keeps a hold from being a
    /// wedge. A server that has not reached its pool yet must dispatch, not
    /// wait for evidence it has no way to gather.
    #[test]
    fn nothing_observed_does_not_hold() {
        let health = PoolHealth::new();
        assert!(health.hold(at(0)).is_none());
    }

    /// The edges, announced once each: the first full observation holds, later
    /// ones are silent, and the first free slot releases it.
    #[test]
    fn a_full_pool_holds_once_and_a_free_slot_releases_it_once() {
        let health = PoolHealth::new();

        let Transition::Exhausted(run) = health.observe(&status(0, 6), at(0)) else {
            panic!("the first full observation is the edge");
        };
        assert_eq!(run.since, at(0));
        assert_eq!(run.total, 6);
        assert!(health.hold(at(1)).is_some());

        for (i, t) in [5, 10, 15].into_iter().enumerate() {
            assert_eq!(health.observe(&status(0, 6), at(t)), Transition::Unchanged);
            let held = health.hold(at(t)).expect("still held");
            assert_eq!(held.since, at(0), "`since` is when it filled up");
            assert_eq!(held.last, at(t), "`last` is what keeps it alive");
            assert_eq!(held.observations as usize, i + 2);
        }

        let Transition::Freed(ended) = health.observe(&status(1, 6), at(20)) else {
            panic!("the release is an edge too");
        };
        assert_eq!(ended.observations, 4);
        assert!(health.hold(at(20)).is_none());
        // And a second free observation says nothing.
        assert_eq!(health.observe(&status(2, 6), at(21)), Transition::Unchanged);
    }

    /// An unreadable `status` is not evidence of anything. It neither starts a
    /// hold nor releases one — the loops answer a dead socket by reconnecting.
    #[test]
    fn an_unreadable_status_neither_holds_nor_releases() {
        let health = PoolHealth::new();
        let dead: Result<PoolStatus, ClientError> = Err(ClientError::Closed);

        assert_eq!(health.observe(&dead, at(0)), Transition::Unchanged);
        assert!(health.hold(at(0)).is_none());

        health.observe(&status(0, 6), at(10));
        assert_eq!(health.observe(&dead, at(15)), Transition::Unchanged);
        assert!(
            health.hold(at(15)).is_some(),
            "a dead socket does not release"
        );
    }

    /// `VM_POOL_MAX_VMS=0` binds, answers `status` cheerfully and fails every
    /// allocate. It reads as a permanent hold, which is the right answer — and
    /// it is why `total` is carried into the report rather than the word
    /// "full".
    #[test]
    fn a_pool_of_zero_is_a_permanent_hold_that_says_so() {
        let health = PoolHealth::new();
        let Transition::Exhausted(run) = health.observe(&status(0, 0), at(0)) else {
            panic!("a pool of zero has no free slot");
        };
        assert_eq!(run.total, 0);
        assert!(run.describe().contains("0 of 0"), "{}", run.describe());
    }

    /// The claim is what makes two gates share one round trip — and what makes
    /// exactly one of two racing callers get the transition that writes the
    /// `Note`.
    #[test]
    fn one_probe_is_claimed_per_interval_however_many_gates_ask() {
        let health = PoolHealth::new();
        assert!(health.probe_due(at(0)), "nothing probed yet");
        assert!(
            !health.probe_due(at(0)),
            "the other gate reads what it wrote"
        );
        assert!(!health.probe_due(at(4)));
        assert!(health.probe_due(at(5)), "due again after the interval");
    }

    /// A record nobody refreshes expires, so `/status` cannot report a hold
    /// from a dispatcher that stopped asking. Signed comparison: a clock that
    /// stepped backwards holds rather than releasing.
    #[test]
    fn an_unrefreshed_record_expires_and_a_backwards_clock_does_not_release_it() {
        let health = PoolHealth::new();
        health.observe(&status(0, 6), at(1_000));
        assert!(
            health
                .hold(at(1_000 + STALE_AFTER.as_secs() as i64))
                .is_some()
        );
        assert!(
            health
                .hold(at(1_001 + STALE_AFTER.as_secs() as i64))
                .is_none()
        );
        assert!(health.hold(at(0)).is_some(), "a backwards clock holds");
    }
}
