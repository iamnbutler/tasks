//! Whether vm-pool has a slot, and whether dispatch should wait for one.
//!
//! `Scout::dispatch` moved a task to `Scouting` before it allocated, and the
//! allocate refusal returned before any session row existed — so
//! `finalize_failed`, the only thing that writes a task back to `Queued`, never
//! ran, and `run::next_dispatchable` (which looks only at `Queued`) could not
//! see the task again until the next boot's reconciliation. A momentarily full
//! pool cost a task a restart (#967).
//!
//! The unwind in [`crate::scout`] is one half of the fix and this is the other,
//! and neither works alone. `DISPATCH_TICK` is 500ms, so a bare requeue against
//! a pool that stays full retries twice a second forever: with #930's waiver in
//! force that is an unbounded stream of waiver `Note`s on the app's feed and in
//! the orchestrator's input, and without it, it is three dispatch attempts
//! burned inside two seconds on a task that did nothing wrong. So this is the
//! third dispatch hold, beside [`crate::github_health`] and
//! [`crate::updates`], at the same two gates — and it means the ordinary case
//! stops *making* the refused allocation rather than recovering from it.
//!
//! # The evidence is a `status` round trip, never a classified refusal
//!
//! A refusal reaches the host as a [`ClientError::Service`], and #930 gives it
//! a structural `kind` to read. **Do not wire this to that field.** Two
//! independent reasons:
//!
//! 1. It would close a circle. The natural clearing signal for a
//!    refusal-driven record is a *successful allocation*, and the hold is
//!    precisely what prevents one from being attempted.
//! 2. `available` is a plain field on `pool_status` and is gated by no
//!    protocol version at all, while `kind` needs a vm-pool restart to appear —
//!    vm-pool is a separate daemon that a server restart does not restart. So
//!    this hold works against the pool that is running right now, and #930's
//!    waiver does not. The window between #930 merging and the next pool
//!    restart is one where this hold is the only thing standing between a full
//!    pool and burned dispatch attempts.
//!
//! `status.available` is `max_vms - allocated`, which is the exact quantity
//! `Pool::allocate` checks. The two mechanisms answer different questions —
//! #930 decides what a refusal *costs*, this decides whether dispatch keeps
//! *happening* — and neither reads the other's signal.
//!
//! # The rules, and why each one is here
//!
//! - **An unreadable `status` touches nothing.** It is not evidence of a full
//!   pool and not evidence of a free one; the loops answer a dead socket by
//!   reconnecting, which is louder and more specific. This is
//!   [`crate::github_health`]'s first rule, for the same reason.
//! - **Nothing can deadlock on the hold.** What releases it is a VM handed
//!   back by work already in flight, which the gate does not touch. A
//!   permanently full pool holds forever — correctly, and now visibly, rather
//!   than burning attempts.
//! - **`available == 0 && total == 0`** — a `VM_POOL_MAX_VMS=0`, which binds
//!   the socket and answers `status` cheerfully while failing every allocate —
//!   reads as a permanent hold. That is the right answer, and it is why
//!   `total` is carried into the report: `0 of 0` and `0 of 6` are different
//!   problems with different fixes.

use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use vm_pool_client::{ClientError, PoolStatus};

/// How often a gate spends a `status` round trip.
///
/// [`PoolHealth::probe_due`] **claims** the slot, so the scout loop and the
/// build lane make one round trip between them per interval rather than two —
/// and, because the claim is what admits exactly one caller to the `observe`
/// that follows, it is also what makes exactly one of two racing loops write
/// the announcement.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// How long a record stays evidence without being refreshed.
///
/// Mostly there to keep `/status` honest: the two gates refresh the record as
/// they read it, so in the steady state nothing ever goes stale. It matters
/// when both dispatchers are gone — a paused server still probes, but a
/// shutdown does not — and `/status` must not report a hold nobody is
/// maintaining.
pub const STALE_AFTER: Duration = Duration::from_secs(60);

/// A run of observations in which the pool had no free slot, not yet ended by
/// one in which it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exhaustion {
    /// The first full observation in this run. **Not** moved by later ones: a
    /// human asking why nothing is dispatching wants "full since 03:12".
    pub since: DateTime<Utc>,
    /// The most recent full observation — what a live gate keeps moving, and
    /// therefore what keeps its own hold from expiring under it.
    pub last: DateTime<Utc>,
    /// How many full observations this run has seen. One is enough to hold;
    /// the count is for the human reading `/status`.
    pub observations: u32,
    /// `PoolStatus::total` as last seen. Carried because `0 of 0` is a
    /// `VM_POOL_MAX_VMS` that can never dispatch and `0 of 6` is work (or a
    /// leak) holding every slot — different problems, different fixes.
    pub total: usize,
}

impl Exhaustion {
    /// The sentence the event log gets when the hold goes on.
    ///
    /// It says what holding *costs*, because the reader's next question is
    /// whether the pipeline is losing work. It is not — and that is the whole
    /// point of holding rather than dispatching into a refusal.
    pub fn describe(&self) -> String {
        format!(
            "vm-pool has no free slot (0 of {} available since {}, {} observation(s)). \
             Scout and build dispatch is held until one frees: queued work stays \
             queued, nothing is charged an attempt, and the next probe that finds a \
             slot releases it",
            self.total,
            self.since.to_rfc3339(),
            self.observations
        )
    }
}

/// What one observation did to the record — the edge, so a caller can announce
/// it exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Nothing an announcement would be about: a free slot while free, a
    /// further full observation inside a hold already in force, or a `status`
    /// that could not be read at all.
    Unchanged,
    /// A hold just went on.
    Exhausted(Exhaustion),
    /// A hold that was in force just came off.
    Freed(Exhaustion),
}

/// Whether vm-pool has a slot, as last observed.
///
/// One record, shared by the two gates that write it and `/status` that reads
/// it — so they cannot disagree about whether a hold is in force.
#[derive(Debug, Default)]
pub struct PoolHealth {
    exhaustion: Mutex<Option<Exhaustion>>,
    /// When a probe was last claimed. `None` until the first claim, which is
    /// why the first call always probes.
    probed_at: Mutex<Option<DateTime<Utc>>>,
}

impl PoolHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the caller should spend a `status` round trip now — **claiming
    /// the slot if so**.
    ///
    /// The claim is the load-bearing part and it does two jobs at once. The
    /// scout loop and the build lane both tick every `DISPATCH_TICK`, so
    /// without it they would make two round trips per interval instead of one.
    /// And because exactly one caller is admitted to the [`Self::observe`]
    /// that follows, exactly one of two racing loops sees the `Transition`
    /// across an edge — which is what keeps a 500ms loop from writing a `Note`
    /// per tick, the event-log flood this whole change exists to prevent, one
    /// level up.
    pub fn probe_due(&self, now: DateTime<Utc>) -> bool {
        let mut probed = self.probed_at.lock().expect("pool health lock");
        let due = match *probed {
            None => true,
            Some(last) => now - last >= interval(PROBE_INTERVAL),
        };
        if due {
            *probed = Some(now);
        }
        due
    }

    /// Fold one `status` round trip in, and say what edge it crossed.
    ///
    /// - No free slot starts a hold or extends one.
    /// - A free slot clears one.
    /// - **An unreadable `status` touches nothing.** A dead socket is not
    ///   evidence of a full pool, and reading it as one would hold on the
    ///   strength of an observation nobody made.
    ///
    /// `now` is a parameter so this is testable without sleeping, and so a
    /// caller measures its record against one reading rather than two.
    pub fn observe(
        &self,
        status: &Result<PoolStatus, ClientError>,
        now: DateTime<Utc>,
    ) -> Transition {
        let mut guard = self.exhaustion.lock().expect("pool health lock");
        match status {
            Err(_) => Transition::Unchanged,
            Ok(status) if status.available > 0 => match guard.take() {
                // Only a hold that was actually in force is worth announcing a
                // release from; one that had already expired was released by
                // the staleness window.
                Some(exhaustion) if in_force(&exhaustion, now) => Transition::Freed(exhaustion),
                _ => Transition::Unchanged,
            },
            Ok(status) => match guard.as_mut() {
                Some(exhaustion) if in_force(exhaustion, now) => {
                    exhaustion.last = now;
                    exhaustion.observations += 1;
                    exhaustion.total = status.total;
                    Transition::Unchanged
                }
                // Nothing held, or what was held had already expired: a fresh
                // edge either way, and `since` starts here.
                _ => {
                    let exhaustion = Exhaustion {
                        since: now,
                        last: now,
                        observations: 1,
                        total: status.total,
                    };
                    *guard = Some(exhaustion.clone());
                    Transition::Exhausted(exhaustion)
                }
            },
        }
    }

    /// The hold in force right now, if any — the one predicate the two gates
    /// and `/status` all read, so they cannot disagree.
    pub fn hold(&self, now: DateTime<Utc>) -> Option<Exhaustion> {
        let guard = self.exhaustion.lock().expect("pool health lock");
        guard
            .as_ref()
            .filter(|exhaustion| in_force(exhaustion, now))
            .cloned()
    }
}

fn interval(d: Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or_else(|_| chrono::Duration::days(1))
}

/// Whether a record is recent enough to still be evidence.
///
/// The comparison is **signed**, exactly as `github_health`'s is. A clock that
/// stepped backwards makes the record look like it came from the future, which
/// is the one thing it certainly is not — an absolute age would read that as
/// "far too old" and release a hold a genuinely full pool set moments ago.
fn in_force(exhaustion: &Exhaustion, now: DateTime<Utc>) -> bool {
    now - exhaustion.last <= interval(STALE_AFTER)
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

    fn unreadable() -> Result<PoolStatus, ClientError> {
        Err(ClientError::Closed)
    }

    /// Nothing observed holds nothing — a server that has not yet talked to a
    /// pool must dispatch, or the gate is one only the gate keeps closed.
    #[test]
    fn nothing_observed_does_not_hold() {
        let health = PoolHealth::new();
        assert!(health.hold(at(0)).is_none());
    }

    /// The edges, announced once each however long the pool stays full.
    #[test]
    fn a_full_pool_holds_and_only_a_free_slot_releases_it() {
        let health = PoolHealth::new();

        let Transition::Exhausted(first) = health.observe(&status(0, 6), at(0)) else {
            panic!("the first full observation is the edge");
        };
        assert_eq!(first.since, at(0));
        assert_eq!(first.total, 6);
        assert!(health.hold(at(1)).is_some());

        // Still full: held, counted, and silent.
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
        assert_eq!(health.observe(&status(1, 6), at(21)), Transition::Unchanged);
    }

    /// The rule that stops a dead socket from becoming a permanent hold. It is
    /// neither evidence of a full pool nor of a free one, and the loops answer
    /// it by reconnecting.
    #[test]
    fn an_unreadable_status_neither_holds_nor_releases() {
        let health = PoolHealth::new();

        assert_eq!(health.observe(&unreadable(), at(0)), Transition::Unchanged);
        assert!(
            health.hold(at(0)).is_none(),
            "a dead socket is not a full pool"
        );

        health.observe(&status(0, 6), at(10));
        assert_eq!(health.observe(&unreadable(), at(20)), Transition::Unchanged);
        let held = health.hold(at(20)).expect("nor does it release a hold");
        assert_eq!(held.observations, 1, "and does not count as one either");
    }

    /// `VM_POOL_MAX_VMS=0` binds, answers `status` and fails every allocate.
    /// It reads as a permanent hold, and `total` is what tells the reader
    /// which problem they have.
    #[test]
    fn a_pool_of_zero_is_a_permanent_hold_that_names_itself() {
        let health = PoolHealth::new();
        let Transition::Exhausted(e) = health.observe(&status(0, 0), at(0)) else {
            panic!("no slot is no slot");
        };
        assert_eq!(e.total, 0);
        assert!(e.describe().contains("0 of 0"), "{}", e.describe());
    }

    /// The claim, which is what makes two loops cost one round trip — and
    /// what makes exactly one of them see the edge.
    #[test]
    fn a_probe_is_claimed_by_one_caller_per_interval() {
        let health = PoolHealth::new();
        let interval = PROBE_INTERVAL.as_secs() as i64;

        assert!(health.probe_due(at(0)), "the first probe is always due");
        assert!(!health.probe_due(at(0)), "the other loop, same tick");
        assert!(!health.probe_due(at(interval - 1)));
        assert!(health.probe_due(at(interval)));
        assert!(!health.probe_due(at(interval)));
    }

    /// Two loops racing across one edge write one announcement, not two —
    /// because only the caller that claimed the probe gets to `observe`. Two
    /// `Note`s per edge is the flood one level up from the one this change
    /// exists to prevent.
    #[test]
    fn two_loops_racing_across_an_edge_see_one_transition_each_way() {
        let health = PoolHealth::new();
        let interval = PROBE_INTERVAL.as_secs() as i64;

        // The full edge. Both loops tick; one claims.
        let mut edges = Vec::new();
        for _ in 0..2 {
            if health.probe_due(at(0)) {
                edges.push(health.observe(&status(0, 6), at(0)));
            }
        }
        assert_eq!(edges.len(), 1, "one round trip between the two loops");
        assert!(matches!(edges[0], Transition::Exhausted(_)));

        // The free edge, an interval later. Same again.
        let mut edges = Vec::new();
        for _ in 0..2 {
            if health.probe_due(at(interval)) {
                edges.push(health.observe(&status(2, 6), at(interval)));
            }
        }
        assert_eq!(edges.len(), 1);
        assert!(matches!(edges[0], Transition::Freed(_)));
    }

    /// The invariant is a *relationship* between the two constants, not either
    /// number: a record has to survive several missed probes before `/status`
    /// stops believing it, or a single slow tick would report the hold gone
    /// while both gates are still honouring it.
    #[test]
    fn the_staleness_window_outlasts_several_probes() {
        assert!(
            STALE_AFTER >= PROBE_INTERVAL * 4,
            "{STALE_AFTER:?} must outlast several {PROBE_INTERVAL:?} probes"
        );
    }

    /// A hold nobody refreshes expires — the backstop for both gates being
    /// gone, so `/status` cannot report a hold nothing is maintaining.
    #[test]
    fn a_hold_nobody_refreshes_expires() {
        let health = PoolHealth::new();
        let window = STALE_AFTER.as_secs() as i64;
        health.observe(&status(0, 6), at(0));
        assert!(health.hold(at(window)).is_some(), "not yet");
        assert!(health.hold(at(window + 1)).is_none());
    }

    /// A backwards clock step makes the record look like it came from the
    /// future, which is the one thing it is not.
    #[test]
    fn a_clock_that_stepped_backwards_still_holds() {
        let health = PoolHealth::new();
        health.observe(&status(0, 6), at(10_000));
        assert!(health.hold(at(0)).is_some());
    }

    /// The announcement answers the reader's real question — is the pipeline
    /// losing work? — rather than only stating the fact.
    #[test]
    fn the_announcement_says_what_holding_costs() {
        let health = PoolHealth::new();
        let Transition::Exhausted(e) = health.observe(&status(0, 6), at(0)) else {
            unreachable!()
        };
        let said = e.describe();
        assert!(said.contains("0 of 6"), "{said}");
        assert!(said.contains("queued work stays queued"), "{said}");
        assert!(said.contains("nothing is charged an attempt"), "{said}");
        assert!(said.contains("releases it"), "{said}");
    }
}
