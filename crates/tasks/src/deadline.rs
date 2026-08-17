//! Run budgets, measured on two clocks at once.
//!
//! Every budget in this server used to be one `tokio::time::sleep(budget)`,
//! which is `Instant`-based — and an `Instant` does not advance while the
//! machine is asleep. On the laptop this pipeline runs on that produced #929: a
//! build dispatched at 03:44, a host on battery from 04:22, a lid opened at
//! 12:34, and the deadline firing three and a half minutes later reporting
//! `build timed out after 3600s`. True in monotonic terms, and wrong in every
//! term a human uses: it held the serial build lane for nearly nine hours and
//! charged three specs a build attempt each for a closed lid, which CLAUDE.md's
//! own rule forbids — a `Timeout` is charged precisely because it "had the
//! entire budget", and this one had 38 minutes of it.
//!
//! A [`Deadline`] is therefore anchored on *both* clocks and expires on
//! whichever runs out first. Because both anchors are kept, the gap between
//! them at expiry *is* the time the host was not running: a suspend becomes a
//! measured fact rather than an inference, which is what lets the dispatchers
//! give it its own error, its own `exit_reason` and
//! [`FailureClass::Transport`](crate::protocol::FailureClass::Transport).
//!
//! # Why the monotonic reading is the floor
//!
//! [`Expiry::elapsed`] is `max(wall, awake)`, never the wall reading alone.
//! Wall-clock alone would hand the deadline to `settimeofday`: an NTP step
//! forwards could retire a run that had barely started, and a step backwards
//! could postpone one forever. Taking the larger of the two means a clock
//! adjustment can only ever degrade to the behaviour that shipped before this
//! module, while a suspend — the only thing that can legitimately make
//! wall-clock elapsed exceed monotonic elapsed — is still caught.
//!
//! # What a suspend costs the run
//!
//! Nothing is extended. A suspended run is killed at the wake, because no
//! agent's API connection survives an eight-hour suspend and handing the budget
//! back would only hold the serial lane longer for a run that is already dead.
//! `caffeinate -s` stays the operational answer; this makes a sleeping host
//! legible and free, not harmless.

use std::fmt;
use std::future::Future;
use std::time::{Duration, SystemTime};

use tokio::time::Instant;

/// How often a parked deadline wakes to re-read both clocks.
///
/// This is what makes the deadline fire on the *wake* rather than once the
/// remaining monotonic budget finally drains — without it a nine-hour suspend
/// is still followed by the 38 minutes of budget that were left. The cost is
/// nil: `cancel::bounded` already polls the cancellations table every 5s
/// underneath the same `select!`.
///
/// If this ever looks like it wants tuning, the thing to keep is the
/// *relationship*: it bounds how long after a wake a doomed run stays parked
/// holding the serial lane, so it must stay well under any budget anyone would
/// configure.
const WALL_CLOCK_TICK: Duration = Duration::from_secs(30);

/// How far the two clocks must disagree before the gap is called a suspend.
///
/// The direction of the uncertainty is the whole argument: misreading a real
/// timeout as a suspend costs one extra attempt, and misreading a suspend as a
/// timeout is #929.
const SUSPEND_FLOOR: Duration = Duration::from_secs(60);

/// A budget anchored on both clocks, expiring on whichever runs out first.
///
/// Built at the top of a dispatch and then carried, so a suspend *during VM
/// allocation* — which the `Instant` arithmetic this replaced could not see —
/// is caught like any other.
#[derive(Debug, Clone)]
pub struct Deadline {
    budget: Duration,
    /// Monotonic anchor. [`tokio::time::Instant`] and not `std::time::Instant`:
    /// under `tokio::time::pause()` a `sleep` auto-advances but a std `Instant`
    /// does not, so a poll loop written against std would spin forever in any
    /// test that pauses the clock. Outside `pause` they are the same reading.
    awake_from: Instant,
    /// Wall-clock anchor. Advances across a suspend, which is the entire point,
    /// and is also why it can never be the only anchor — see the module docs.
    wall_from: SystemTime,
}

impl Deadline {
    /// Start a budget now, on both clocks.
    pub fn starting_now(budget: Duration) -> Self {
        Self {
            budget,
            awake_from: Instant::now(),
            wall_from: SystemTime::now(),
        }
    }

    /// The budget this deadline was given.
    pub fn budget(&self) -> Duration {
        self.budget
    }

    /// Resolve when the budget has run out on either clock.
    ///
    /// Polls rather than sleeping the whole remainder, because the monotonic
    /// sleep a suspend interrupts would otherwise still have to drain before
    /// anyone learned the host had been away.
    pub async fn expired(&self) -> Expiry {
        loop {
            let reading = self.reading();
            let remaining = self.budget.saturating_sub(reading.elapsed);
            if remaining.is_zero() {
                return reading;
            }
            // `elapsed` is already the later of the two clocks, so this is the
            // smaller of the two remainders — the deadline fires on whichever
            // budget runs out first, and never early on either.
            tokio::time::sleep(remaining.min(WALL_CLOCK_TICK)).await;
        }
    }

    /// Both clocks, read now.
    fn reading(&self) -> Expiry {
        let awake = self.awake_from.elapsed();
        // The monotonic reading is the floor: a wall clock that has gone
        // backwards (or is unreadable) degrades to the behaviour that shipped
        // before this module rather than postponing the deadline forever.
        let elapsed = self.wall_from.elapsed().unwrap_or(awake).max(awake);
        Expiry {
            budget: self.budget,
            elapsed,
            awake,
        }
    }

    /// A deadline whose wall clock is already `suspended` further along than
    /// its monotonic one — a host that slept, without one having to sleep.
    ///
    /// This is the only way to drive the suspend path through the *real*
    /// [`crate::cancel::bounded`] without an injectable clock trait, and it is
    /// in-crate only on purpose: the classification it exercises lives in
    /// `scout`/`builder` unit tests, `crates/tasks/tests/` cannot see it, and
    /// making the anchors publicly overridable to buy an integration-level
    /// suspend test is the worse trade.
    #[cfg(test)]
    pub(crate) fn suspended_for(budget: Duration, suspended: Duration) -> Self {
        Self {
            budget,
            awake_from: Instant::now(),
            wall_from: SystemTime::now() - suspended,
        }
    }
}

/// What the two clocks said when a budget ran out.
///
/// The gap between [`Self::elapsed`] and [`Self::awake`] is the measurement
/// this whole module exists to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expiry {
    /// The budget that ran out.
    pub budget: Duration,
    /// Time since the deadline started, on whichever clock says more.
    pub elapsed: Duration,
    /// Time since the deadline started with the host actually running.
    pub awake: Duration,
}

impl Expiry {
    /// How long the host was not running during this budget.
    pub fn suspended(&self) -> Duration {
        self.elapsed.saturating_sub(self.awake)
    }

    /// Whether that gap is big enough to call a suspend rather than the two
    /// clocks being read microseconds apart.
    pub fn host_slept(&self) -> bool {
        self.suspended() >= SUSPEND_FLOOR
    }
}

/// The clause an `exit_reason` (or an event-log note) carries.
///
/// Deliberately never contains the words "timed out": two integration tests and
/// one unit test match that substring for a real deadline, and a suspend
/// satisfying them would put the whole distinction straight back.
impl fmt::Display for Expiry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host_slept() {
            write!(
                f,
                "the host was suspended for {} of a {} budget; only {} of it ran",
                human(self.suspended()),
                human(self.budget),
                human(self.awake),
            )
        } else {
            write!(f, "the {} budget ran out awake", human(self.budget))
        }
    }
}

/// Run `work` until it finishes or until `deadline` expires, for callers with
/// no cancellation to weave in.
///
/// `biased` with the work first, for the reason [`crate::cancel::bounded`] is:
/// an outcome already in hand is never discarded for a deadline that fired in
/// the same poll. Like `tokio::time::timeout`, this *drops* `work` when it does
/// not win.
pub async fn bounded<T>(deadline: &Deadline, work: impl Future<Output = T>) -> Result<T, Expiry> {
    tokio::pin!(work);
    tokio::select! {
        biased;
        outcome = &mut work => Ok(outcome),
        expiry = deadline.expired() => Err(expiry),
    }
}

/// A duration as a human reads one: `8h13m`, `1h`, `38m`, `45s`.
pub fn human(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        match (secs / 3600, (secs % 3600) / 60) {
            (hours, 0) => format!("{hours}h"),
            (hours, mins) => format!("{hours}h{mins}m"),
        }
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ordinary case, and the one every existing budget already had: work
    /// that finishes inside its budget comes back untouched.
    #[tokio::test]
    async fn work_that_finishes_inside_the_budget_is_handed_back() {
        let deadline = Deadline::starting_now(Duration::from_secs(30));
        assert_eq!(bounded(&deadline, async { 7 }).await, Ok(7));
    }

    /// A budget genuinely spent awake still expires, and still reads as one:
    /// nothing here may quietly rebrand a timeout.
    #[tokio::test]
    async fn a_budget_spent_awake_expires_and_is_not_a_suspend() {
        let deadline = Deadline::starting_now(Duration::from_millis(50));
        let expiry = bounded(&deadline, std::future::pending::<()>())
            .await
            .expect_err("the budget should have run out");
        assert!(!expiry.host_slept(), "{expiry:?}");
        assert!(expiry.suspended() < SUSPEND_FLOOR, "{expiry:?}");
    }

    /// The #929 shape: a one-hour budget, a host away for eight of them, and 38
    /// minutes of it actually run.
    ///
    /// The two anchors are read microseconds apart, so the recovered suspend is
    /// `8h ± a hair` — asserted with a tolerance rather than an equality.
    #[tokio::test]
    async fn a_host_that_slept_is_measured_rather_than_inferred() {
        let deadline =
            Deadline::suspended_for(Duration::from_secs(3600), Duration::from_secs(8 * 3600));
        let expiry = deadline.expired().await;

        assert!(expiry.host_slept(), "{expiry:?}");
        let slept = expiry.suspended();
        assert!(
            slept.abs_diff(Duration::from_secs(8 * 3600)) < Duration::from_secs(5),
            "{slept:?}"
        );
        // And barely any of the budget was actually spent running.
        assert!(expiry.awake < Duration::from_secs(5), "{expiry:?}");
    }

    /// The one string rule the dispatchers depend on: a suspend must never be
    /// mistakable for a deadline that was spent.
    #[test]
    fn the_suspend_clause_never_says_timed_out() {
        let expiry = Expiry {
            budget: Duration::from_secs(3600),
            elapsed: Duration::from_secs(8 * 3600 + 15 * 60),
            awake: Duration::from_secs(38 * 60),
        };
        let clause = expiry.to_string();
        assert_eq!(
            clause,
            "the host was suspended for 7h37m of a 1h budget; only 38m of it ran"
        );
        assert!(!clause.contains("timed out"), "{clause}");
    }

    /// A gap smaller than the floor is the two clocks being read at slightly
    /// different instants, not a laptop lid.
    #[test]
    fn a_gap_under_the_floor_is_not_a_suspend() {
        let expiry = Expiry {
            budget: Duration::from_secs(3600),
            elapsed: Duration::from_secs(3600) + Duration::from_millis(4),
            awake: Duration::from_secs(3600),
        };
        assert!(!expiry.host_slept(), "{expiry:?}");
        assert!(!expiry.to_string().contains("suspended"), "{expiry}");
    }

    /// A wall clock that has gone *backwards* — an NTP step — must not be able
    /// to postpone a deadline: the monotonic reading is the floor.
    #[tokio::test]
    async fn a_backwards_wall_clock_degrades_to_the_monotonic_budget() {
        let deadline = Deadline {
            budget: Duration::from_millis(50),
            awake_from: Instant::now(),
            wall_from: SystemTime::now() + Duration::from_secs(3600),
        };
        let expiry = deadline.expired().await;
        assert!(!expiry.host_slept(), "{expiry:?}");
        assert!(expiry.elapsed >= Duration::from_millis(50), "{expiry:?}");
    }

    /// An already-blown budget fires immediately rather than wrapping — the
    /// `saturating_sub` behaviour the reattach paths relied on.
    #[tokio::test]
    async fn a_spent_budget_expires_at_once() {
        let deadline = Deadline::starting_now(Duration::ZERO);
        let expiry = deadline.expired().await;
        assert_eq!(expiry.budget, Duration::ZERO);
    }

    #[test]
    fn durations_read_the_way_a_human_says_them() {
        assert_eq!(human(Duration::from_secs(45)), "45s");
        assert_eq!(human(Duration::from_secs(38 * 60)), "38m");
        assert_eq!(human(Duration::from_secs(8 * 3600 + 13 * 60)), "8h13m");
        assert_eq!(human(Duration::from_secs(3600)), "1h");
        assert_eq!(human(Duration::ZERO), "0s");
    }
}
