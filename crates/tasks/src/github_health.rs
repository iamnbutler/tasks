//! Whether GitHub is answering, and whether dispatch should wait for it.
//!
//! A Scout clones and a Builder clones, so work dispatched during a GitHub
//! outage dies at its first step — a *pre-agent setup failure*, which the
//! strike rule deliberately keeps charged (a clone against a base branch that
//! is gone fails identically forever, and waiving it would retry forever with
//! nothing to stop it). So a three-spec batch spent three build attempts of
//! three for something no spec did (#939). That rule is untouched here; what
//! changes is that the poller already knew and never told anyone.
//!
//! This is the record it now writes: in memory, exactly like the vm-pool
//! precondition it mirrors, and deliberately **not** a table — it is a
//! GitHub-owned fact with a timestamp on it, which this codebase does not
//! persist.
//!
//! Three rules keep a hold from becoming a silent stall, and each is the answer
//! to a way this could go permanently wrong:
//!
//! 1. **Absence of evidence never holds.** A server with no `GITHUB_TOKEN`
//!    makes no observations at all, and a hold it could never clear would stop
//!    it dispatching a scout forever. Default open.
//! 2. **Only a fresh success clears one.** A 404 on one pull request is not
//!    GitHub coming back, so [`GitHubHealth::observe`] touches nothing for an
//!    error that is not [`GhError::is_unavailable`].
//! 3. **A hold nobody is refreshing expires.** If the poll loop dies, rule 2
//!    has no writer left. [`GitHubHealth::stale_after`] is the backstop, and it
//!    is generous on purpose — during an outage the poller's own requests are
//!    the slow kind, so a tight window would expire a hold *during* the outage
//!    it was set for.
//!
//! Holding is safe here in a way it usually is not: `POST /builds` still
//! records the request, queued work stays queued, no attempt is charged, and
//! the batch takes the lane on the tick after GitHub answers.

use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::github::GhError;

/// Multiple of `TASKS_POLL_INTERVAL` after which an unrefreshed hold expires.
const STALE_POLLS: u32 = 10;

/// Floor under [`GitHubHealth::stale_after`], so a very short poll interval
/// (tests, an impatient operator) cannot produce a window that expires a hold
/// mid-outage.
const STALE_FLOOR: Duration = Duration::from_secs(10 * 60);

/// A run of failed GitHub calls that has not been ended by a success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outage {
    /// The first failure in this run. **Not** reset by later failures: a human
    /// asking about a stalled pipeline wants "down since 03:12".
    pub since: DateTime<Utc>,
    /// The most recent failure — which is what a live poller keeps moving, and
    /// therefore what keeps its own hold from expiring under it.
    pub last: DateTime<Utc>,
    /// How many failed calls this run has seen. One is enough to hold; the
    /// count is for the human reading `/status`.
    pub failures: u32,
    /// The most recent failure, rendered. Prose for a reader — nothing decides
    /// on it.
    pub error: String,
}

impl Outage {
    /// The sentence the event log gets when a hold goes on.
    ///
    /// It has to say what holding *costs*, because the reader's next question
    /// is whether the pipeline is losing work. It is not: this is the one
    /// member of the "infrastructure billed to the work" family that is
    /// preventable rather than merely classifiable after the fact.
    pub fn describe(&self) -> String {
        format!(
            "GitHub is not answering (since {}, {} failed call(s); latest: {}). \
             Scout and build dispatch is held until it does: queued work stays \
             queued, nothing is charged an attempt, and the next poll that \
             succeeds releases it",
            self.since.to_rfc3339(),
            self.failures,
            self.error
        )
    }
}

/// What one observation did to the record — the edge, so a caller can announce
/// it exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Nothing an announcement would be about: a success while healthy, a
    /// further failure inside a hold already in force, or an error that is
    /// neither evidence of an outage nor a release from one.
    Unchanged,
    /// A hold just went on.
    Lost(Outage),
    /// A hold that was in force just came off.
    Recovered(Outage),
}

/// Whether GitHub is answering, as last observed.
///
/// One record, shared by the poller that writes it and the two dispatchers plus
/// `/status` that read it. The staleness window is **bound at construction**
/// rather than at each read, deliberately: the scout loop, the build lane and
/// `/status` must not be able to disagree about whether a hold is in force, and
/// only one of the three has the poll interval to hand.
#[derive(Debug)]
pub struct GitHubHealth {
    outage: Mutex<Option<Outage>>,
    stale_after: chrono::Duration,
}

impl Default for GitHubHealth {
    /// The 10-minute floor — what a default `TASKS_POLL_INTERVAL` computes.
    fn default() -> Self {
        Self::new(Duration::from_secs(60))
    }
}

impl GitHubHealth {
    pub fn new(poll_interval: Duration) -> Self {
        let stale_after = Self::stale_after(poll_interval);
        Self {
            outage: Mutex::new(None),
            stale_after: chrono::Duration::from_std(stale_after)
                .unwrap_or_else(|_| chrono::Duration::days(1)),
        }
    }

    /// How long a hold survives without being refreshed.
    ///
    /// Ten polls, floored at ten minutes. Generous on purpose: during an
    /// outage the poller's own requests are the *slow* kind — connect timeouts,
    /// not refusals — so a window tight enough to feel responsive would expire
    /// the hold during the outage it was set for, which is exactly backwards.
    /// The only thing it has to catch is a poll loop that died.
    pub fn stale_after(poll_interval: Duration) -> Duration {
        poll_interval.saturating_mul(STALE_POLLS).max(STALE_FLOOR)
    }

    /// Fold one GitHub call's outcome in, and say what edge it crossed.
    ///
    /// The three rules, in code:
    ///
    /// - `Ok` clears, whatever it was a call to. A success is the only thing
    ///   that releases a hold.
    /// - An [unavailable][GhError::is_unavailable] error starts a hold or
    ///   extends one.
    /// - **Anything else touches nothing.** A 404 on one pull request must not
    ///   clear a hold a real outage set on the call before it, and must not
    ///   start one that would never clear.
    ///
    /// `now` is a parameter so the whole thing is testable without sleeping,
    /// and so a caller measures its record against one reading rather than two.
    pub fn observe<T>(&self, result: &Result<T, GhError>, now: DateTime<Utc>) -> Transition {
        let mut guard = self.outage.lock().expect("github health lock");
        match result {
            Ok(_) => match guard.take() {
                // Only a hold that was actually in force is worth announcing a
                // release from; one that had already expired was released by
                // the staleness window and said so then.
                Some(outage) if in_force(&outage, now, self.stale_after) => {
                    Transition::Recovered(outage)
                }
                _ => Transition::Unchanged,
            },
            Err(e) if e.is_unavailable() => {
                let error = e.to_string();
                match guard.as_mut() {
                    Some(outage) if in_force(outage, now, self.stale_after) => {
                        outage.last = now;
                        outage.failures += 1;
                        outage.error = error;
                        Transition::Unchanged
                    }
                    // Nothing held, or what was held had already expired: this
                    // is a fresh edge either way, and `since` starts here.
                    _ => {
                        let outage = Outage {
                            since: now,
                            last: now,
                            failures: 1,
                            error,
                        };
                        *guard = Some(outage.clone());
                        Transition::Lost(outage)
                    }
                }
            }
            Err(_) => Transition::Unchanged,
        }
    }

    /// The hold in force right now, if any — the one predicate the dispatchers
    /// and `/status` both read, so they cannot disagree.
    pub fn hold(&self, now: DateTime<Utc>) -> Option<Outage> {
        let guard = self.outage.lock().expect("github health lock");
        guard
            .as_ref()
            .filter(|outage| in_force(outage, now, self.stale_after))
            .cloned()
    }

    /// The record as it stands, expired or not. Reporting and diagnostics
    /// only — never a gate, or the staleness window would mean nothing.
    pub fn last_outage(&self) -> Option<Outage> {
        self.outage.lock().expect("github health lock").clone()
    }
}

/// Whether an outage record is recent enough to still be evidence.
///
/// The comparison is **signed**. A clock that stepped backwards makes the
/// record look like it came from the future, and that is the one thing it
/// certainly is not — an absolute age would read the step as "far too old" and
/// silently release a hold that a real outage set moments ago. Signed, a
/// negative age is simply not greater than the window, so it holds.
fn in_force(outage: &Outage, now: DateTime<Utc>, stale_after: chrono::Duration) -> bool {
    now - outage.last <= stale_after
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn unavailable() -> Result<(), GhError> {
        Err(GhError::Rest {
            what: "list issues".into(),
            status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
            message: "Service Unavailable".into(),
        })
    }

    fn not_found() -> Result<(), GhError> {
        Err(GhError::Rest {
            what: "pull request 7".into(),
            status: reqwest::StatusCode::NOT_FOUND,
            message: "Not Found".into(),
        })
    }

    /// Rule 1. A tokenless server observes nothing at all, and a hold it could
    /// never clear would stop it dispatching a scout forever.
    #[test]
    fn nothing_observed_does_not_hold() {
        let health = GitHubHealth::default();
        assert!(health.hold(at(0)).is_none());
        assert!(health.last_outage().is_none());
    }

    /// Rule 2, and the announce-once edges: one failure is enough to hold, the
    /// hold is announced once however long the outage runs, and only a success
    /// releases it — announced once too.
    #[test]
    fn one_failure_holds_and_only_a_success_releases_it() {
        let health = GitHubHealth::default();

        let Transition::Lost(outage) = health.observe(&unavailable(), at(0)) else {
            panic!("the first failure is the edge");
        };
        assert_eq!(outage.since, at(0));
        assert_eq!(outage.failures, 1);
        assert!(health.hold(at(1)).is_some());

        // Still failing: held, counted, and silent.
        for (i, t) in [30, 60, 90].into_iter().enumerate() {
            assert_eq!(health.observe(&unavailable(), at(t)), Transition::Unchanged);
            let held = health.hold(at(t)).expect("still held");
            assert_eq!(held.since, at(0), "`since` is when it went down");
            assert_eq!(held.last, at(t), "`last` is what keeps it alive");
            assert_eq!(held.failures as usize, i + 2);
        }

        let Transition::Recovered(ended) = health.observe(&Ok(()), at(120)) else {
            panic!("the release is an edge too");
        };
        assert_eq!(ended.failures, 4);
        assert!(health.hold(at(120)).is_none());
        assert!(health.last_outage().is_none());
        // And a second success says nothing.
        assert_eq!(health.observe(&Ok(()), at(121)), Transition::Unchanged);
    }

    /// The case that is neither evidence nor release. A 404 on one pull request
    /// must not clear a hold the call before it set, and must not start one
    /// that nothing could ever clear.
    #[test]
    fn an_answer_we_did_not_like_neither_holds_nor_releases() {
        let health = GitHubHealth::default();

        assert_eq!(health.observe(&not_found(), at(0)), Transition::Unchanged);
        assert!(health.hold(at(0)).is_none(), "a 404 is GitHub answering");

        health.observe(&unavailable(), at(10));
        assert_eq!(health.observe(&not_found(), at(20)), Transition::Unchanged);
        let held = health.hold(at(20)).expect("a 404 does not release a hold");
        assert_eq!(held.failures, 1, "and does not count as one either");
    }

    /// Rule 3. If the poll loop dies there is no writer left, and the hold has
    /// to come off by itself or the pipeline stalls permanently and silently.
    #[test]
    fn a_hold_nobody_refreshes_expires() {
        let health = GitHubHealth::new(Duration::from_secs(60));
        let window = GitHubHealth::stale_after(Duration::from_secs(60));
        assert_eq!(window, STALE_FLOOR, "ten 60s polls floors at ten minutes");

        health.observe(&unavailable(), at(0));
        assert!(
            health.hold(at(window.as_secs() as i64)).is_some(),
            "not yet"
        );
        assert!(health.hold(at(window.as_secs() as i64 + 1)).is_none());
        // Expired for the gate, still there for a reader asking what happened.
        assert!(health.last_outage().is_some());
    }

    /// The other half of the window, and the reason it is generous: a poller
    /// that is still running keeps its own hold alive indefinitely, however
    /// long the outage lasts.
    #[test]
    fn a_live_poller_outlives_its_own_staleness_window() {
        let health = GitHubHealth::new(Duration::from_secs(60));
        let window = GitHubHealth::stale_after(Duration::from_secs(60)).as_secs() as i64;

        for tick in 0..(3 * window / 60) {
            health.observe(&unavailable(), at(tick * 60));
        }
        let last = (3 * window / 60 - 1) * 60;
        let held = health
            .hold(at(last))
            .expect("still holding after 30 minutes");
        assert_eq!(held.since, at(0));
        assert!(held.failures > 10);
    }

    /// A backwards clock step makes the record look like it came from the
    /// future, which is the one thing it is not. An absolute age would read
    /// that as "far too old" and release a hold a real outage just set.
    #[test]
    fn a_clock_that_stepped_backwards_still_holds() {
        let health = GitHubHealth::default();
        health.observe(&unavailable(), at(10_000));
        assert!(health.hold(at(0)).is_some(), "an hour 'before' the record");
    }

    /// A hold that already expired is not extended by the next failure — it is
    /// a fresh outage, announced as one. Otherwise a stalled-then-revived
    /// poller would hold again with nothing on the feed to say so.
    #[test]
    fn a_failure_after_the_window_starts_a_new_outage() {
        let health = GitHubHealth::new(Duration::from_secs(60));
        let window = GitHubHealth::stale_after(Duration::from_secs(60)).as_secs() as i64;

        health.observe(&unavailable(), at(0));
        let Transition::Lost(fresh) = health.observe(&unavailable(), at(window + 60)) else {
            panic!("an expired hold is not in force, so this is an edge");
        };
        assert_eq!(fresh.since, at(window + 60));
        assert_eq!(fresh.failures, 1);
    }

    /// The window is a relationship to the poll interval, not a constant — a
    /// slow poller must not have its hold expire between two of its own polls.
    #[test]
    fn the_window_scales_with_the_poll_interval() {
        assert_eq!(
            GitHubHealth::stale_after(Duration::from_secs(1)),
            STALE_FLOOR,
            "the floor covers an impatient interval"
        );
        assert_eq!(
            GitHubHealth::stale_after(Duration::from_secs(600)),
            Duration::from_secs(6000)
        );
    }

    /// The sentence that reaches the event log has to answer the reader's next
    /// question — is the pipeline losing work? — rather than only stating the
    /// fact.
    #[test]
    fn the_announcement_says_what_holding_costs() {
        let health = GitHubHealth::default();
        let Transition::Lost(outage) = health.observe(&unavailable(), at(0)) else {
            unreachable!()
        };
        let said = outage.describe();
        assert!(said.contains("queued work stays queued"), "{said}");
        assert!(said.contains("nothing is charged an attempt"), "{said}");
        assert!(said.contains("releases it"), "{said}");
        assert!(said.contains("Service Unavailable"), "{said}");
    }
}
