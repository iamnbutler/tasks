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
//! # The two thresholds, and why they are not one
//!
//! An expiry is asked two questions, and they have different answers:
//!
//! 1. *Availability* — should this run be abandoned at the wake, or handed the
//!    monotonic budget it still has? [`WAKE_KILL_FLOOR`] answers that, and it
//!    gates the wall-clock arm of the expiry itself.
//! 2. *Accountability* — did the run get enough of its budget to be judged on
//!    what it produced? [`WAIVED_BUDGET_SHARE`] answers that, via
//!    [`Expiry::starved_by_suspend`], and it is what picks `Suspended` over
//!    `Timeout` at the dispatchers.
//!
//! One constant answering both is #944. Because the deadline fires on
//! `max(wall, awake)`, a run whose host napped *at all* can never reach its
//! budget awake — so a single 61-second nap anywhere inside an hour waived the
//! strike for a run that spent 59 of its 60 minutes working. "The host slept"
//! is simply not the question a strike hangs on; "how much of the budget went
//! unspent" is.
//!
//! # Why the monotonic reading is the floor
//!
//! [`Expiry::elapsed`] is `max(wall, awake)`, never the wall reading alone.
//! Wall-clock alone would hand the deadline to `settimeofday`, and the two
//! directions are *not* symmetric. A step **backwards** is fully neutralised:
//! `max()` discards it, and the deadline degrades to exactly the monotonic
//! behaviour that shipped before this module. A step **forwards** is not —
//! `max()` takes it, so it adds elapsed time the run never had, and a large
//! enough one retires a run early while reporting a suspend that never
//! happened.
//!
//! Nothing here can tell that apart from a real suspend, because the
//! measurement *is* the disagreement between the two clocks: a lid and an NTP
//! step forwards leave the same trace. That is accepted rather than solved —
//! solving it would mean a third source of truth about time, on a laptop, to
//! catch a case that is rarer than the one this module exists for. What has
//! changed since #944 is that the bill is bounded: a forward step smaller than
//! [`WAKE_KILL_FLOOR`] leaves the wall arm disarmed and costs the run nothing
//! at all, and one larger than it costs the run its remaining budget but only
//! waives the strike if the step is also a quarter of that budget or more.
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

/// How long the host must have been away before a run that still has monotonic
/// budget left is abandoned at the wake.
///
/// This is an *availability* threshold and it is measured in minutes, because
/// the pipeline is already built to survive a short absence: the supervisor
/// inside the VM re-invokes an agent whose API connection dropped mid-response
/// (`{SCOUT,BUILDER}_MAX_RESUMES`, on a 2s/15s/30s backoff), which is exactly
/// what a brief nap causes. Killing a run for that throws away work the
/// pipeline would have recovered by itself. Past a few minutes the resume
/// ladder is spent, and what is left is a dead run holding the serial lane.
///
/// Two properties are worth stating outright. The gap is *cumulative* since the
/// deadline started, so three four-minute naps trip it even though no single
/// one does.
///
/// And what this floor bounds is a run's **awake execution past the point wall
/// elapsed reached the budget** — never wall-clock elapsed itself. The sentence
/// that stood here until #955 claimed the latter: *because the wall arm is
/// disarmed below it, a run can outlive its wall-clock budget by less than this
/// floor, never more*. Its reason was sound about the regime it names — while
/// the arm is disarmed the whole suspend is under this floor, so the wall-clock
/// overshoot is under it too — and the claim is simply generalised past that
/// regime. What the clause silently excludes is the case that breaks it: a
/// single nap at or past this floor *arms* the arm, and that nap is itself the
/// overshoot.
///
/// The two halves separately, then. **Wall-clock elapsed has no bound, and
/// costs nothing.** [`Expiry::remaining`] answers `None` as soon as `awake`
/// reaches the budget, whatever the suspend is, so a lid closed for three hours
/// during a disarmed run's last tick fires three hours past the wall-clock
/// budget — and nothing caps that, because nothing caps a suspend. It is free:
/// the run was not running for any of it, and the serial lane is released at
/// the wake either way. **Awake execution is bounded, by the monotonic arm, and
/// strictly under this floor.** Write `s` for the suspend accumulated when wall
/// elapsed first reaches the budget, at which point `awake = budget − s` by
/// definition. Neither branch of [`Expiry::remaining`] ever answers with more
/// than the monotonic remainder (the armed branch returns `budget − elapsed`,
/// and `elapsed >= awake`), and [`Deadline::expired`] sleeps
/// `remaining.min(WALL_CLOCK_TICK)` — so `awake` never passes `budget`, leaving
/// at most `s` of awake execution to be spent past that point. Disarmed there,
/// `s` is under this floor by definition. Armed there, the arm's own
/// `wall_left` is already zero, so the next poll fires — at most one
/// [`WALL_CLOCK_TICK`] of awake later, and a tick is *less* than this floor
/// rather than something to add to it.
///
/// So the tick is not a term in this bound at all. The question it does answer
/// is its own: how long after a wake a doomed run stays parked holding the
/// serial lane.
const WAKE_KILL_FLOOR: Duration = Duration::from_secs(5 * 60);

/// The share of the budget that must have gone unspent awake before the strike
/// is waived.
///
/// This is an *accountability* threshold, and unlike [`WAKE_KILL_FLOOR`] it is
/// a fraction rather than a duration. Every budget it is read against is
/// configurable — `SCOUT_TIMEOUT_SECS`, `BUILDER_TIMEOUT_SECS`,
/// `ORCHESTRATOR_TIMEOUT_SECS` (900s today) — and the reattach paths derive
/// budgets shorter still. Against a 600s budget, "ten minutes unspent" is
/// satisfiable only by a run that was never awake at all, so a flat floor would
/// make the waiver quietly unreachable exactly where runs are shortest.
///
/// The direction of the uncertainty is unchanged from #929: misreading a real
/// timeout as a suspend costs one extra attempt, and misreading a suspend as a
/// timeout charged three specs each for a closed lid.
const WAIVED_BUDGET_SHARE: f64 = 0.25;

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

    /// Resolve when the budget has run out on either armed clock.
    ///
    /// Polls rather than sleeping the whole remainder, because the monotonic
    /// sleep a suspend interrupts would otherwise still have to drain before
    /// anyone learned the host had been away. The [`WALL_CLOCK_TICK`] cap is
    /// also what *arms* the wall arm mid-sleep: a nap that starts during one is
    /// seen a tick later, never a budget later.
    ///
    /// The firing decision itself is [`Expiry::remaining`], so the gate and the
    /// sleep interval are one expression and cannot drift apart.
    pub async fn expired(&self) -> Expiry {
        loop {
            let reading = self.reading();
            let Some(remaining) = reading.remaining() else {
                return reading;
            };
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
    ///
    /// A `suspended` under [`WAKE_KILL_FLOOR`] leaves the wall arm disarmed, so
    /// the deadline still takes the full `budget` in real monotonic time before
    /// it fires. That is what makes the short-nap case expressible as a test at
    /// all — and why the test that needs it passes a millisecond budget.
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

    /// How much of the budget the run never got, because the host was not
    /// running to give it.
    pub fn unspent(&self) -> Duration {
        self.budget.saturating_sub(self.awake)
    }

    /// Whether enough of the budget went unspent for the strike to be waived.
    ///
    /// Measured on [`Self::unspent`] and deliberately **never** on
    /// [`Self::suspended`]. A twenty-minute lid arriving in the last stretch of
    /// an hour leaves a run that had fifty minutes awake and produced nothing,
    /// and CLAUDE.md's rule charges that: a strike is for a verdict, and fifty
    /// minutes of an hour is one. The same twenty minutes arriving early leaves
    /// a run that had forty, and that is not.
    ///
    /// One measurement suffices, and no second "was that really a suspend"
    /// test belongs here: an expiry only ever happens with `elapsed >= budget`,
    /// so `unspent = budget − awake <= elapsed − awake = suspended`. Budget lost
    /// can never exceed the suspend that lost it, which subsumes the job the old
    /// 60-second floor was doing — two clocks read microseconds apart leave
    /// nothing unspent.
    pub fn starved_by_suspend(&self) -> bool {
        self.unspent() >= self.budget.mul_f64(WAIVED_BUDGET_SHARE)
    }

    /// Whether this run was abandoned at a wake with budget still on the
    /// monotonic clock — the middle state the split creates, and the only one
    /// that is both a suspend and a charge.
    fn wake_killed(&self) -> bool {
        !self.unspent().is_zero() && self.suspended() >= WAKE_KILL_FLOOR
    }

    /// How much longer this budget has to run, or `None` if it has expired.
    ///
    /// The monotonic arm is always armed. The wall arm is armed only once the
    /// host has been away for [`WAKE_KILL_FLOOR`]; below that a run drains its
    /// monotonic budget exactly as it did before this module existed, which is
    /// what keeps a nap the in-VM resume ladder would have absorbed from
    /// costing a run its remaining time.
    ///
    /// Neither arm ever answers with more than the monotonic remainder — the
    /// armed branch returns `budget − elapsed`, and `elapsed >= awake` — so
    /// with [`Deadline::expired`] sleeping `remaining.min(WALL_CLOCK_TICK)`,
    /// `awake` can never pass `budget`. That is the invariant the bound in
    /// [`WAKE_KILL_FLOOR`]'s docs rests on, and it is the reason the bound is
    /// on awake execution rather than on the wall clock.
    ///
    /// The monotonic arm answering first — `awake_left` is computed and can
    /// return `None` *before* the floor is consulted — is deliberate, and it is
    /// why the wall-clock overshoot has no bound at all. Reordering the two
    /// checks would buy none: past the wall-clock budget the armed branch's own
    /// `wall_left` is zero and answers `None` identically. The cause is that
    /// the sleep is monotonic and a suspend has no cap, so there is no bound to
    /// be found by rearranging this function.
    fn remaining(&self) -> Option<Duration> {
        let awake_left = self.budget.saturating_sub(self.awake);
        if awake_left.is_zero() {
            return None;
        }
        if self.suspended() >= WAKE_KILL_FLOOR {
            // `elapsed >= awake`, so this is the smaller of the two remainders
            // whenever it is armed — the deadline fires on whichever budget
            // runs out first, and never early on either.
            let wall_left = self.budget.saturating_sub(self.elapsed);
            return (!wall_left.is_zero()).then_some(wall_left);
        }
        Some(awake_left)
    }
}

/// The clause an `exit_reason` (or an event-log note) carries.
///
/// Three sentences, because splitting the thresholds created a third state: a
/// long nap arriving late kills the run at the wake and still charges it, for a
/// budget it had almost all of.
///
/// No sentence may contain the words "timed out": two integration tests and one
/// unit test match that substring for a real deadline, and a suspend satisfying
/// them would put the whole distinction straight back.
impl fmt::Display for Expiry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.starved_by_suspend() {
            write!(
                f,
                "the host was suspended for {} of a {} budget; only {} of it ran",
                human(self.suspended()),
                human(self.budget),
                human(self.awake),
            )
        } else if self.wake_killed() {
            write!(
                f,
                "the host was suspended for {} of a {} budget, but {} of it ran",
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
        assert!(!expiry.starved_by_suspend(), "{expiry:?}");
        assert!(expiry.suspended() < WAKE_KILL_FLOOR, "{expiry:?}");
    }

    /// #944, and the one test that discriminates: a host that napped for 61
    /// seconds and then burned every millisecond of its budget awake is a
    /// timeout, not an excuse. Under the old single 60-second floor this
    /// panics — the nap alone was the whole predicate.
    ///
    /// It is expressible because a suspend under [`WAKE_KILL_FLOOR`] leaves the
    /// wall arm disarmed, so the deadline still takes the full (millisecond)
    /// budget in real monotonic time.
    #[tokio::test]
    async fn a_short_nap_does_not_excuse_a_budget_that_was_spent_awake() {
        let deadline = Deadline::suspended_for(Duration::from_millis(50), Duration::from_secs(61));
        let expiry = deadline.expired().await;

        // It burned the budget awake…
        assert!(expiry.awake >= Duration::from_millis(50), "{expiry:?}");
        assert!(expiry.unspent().is_zero(), "{expiry:?}");
        // …the nap is still measured…
        assert!(expiry.suspended() >= Duration::from_secs(60), "{expiry:?}");
        // …it is simply not an excuse.
        assert!(!expiry.starved_by_suspend(), "{expiry:?}");
    }

    /// The middle state: a twenty-minute lid arriving in the last stretch of an
    /// hour. The run is abandoned at the wake — that is what
    /// [`WAKE_KILL_FLOOR`] is for — and still charged, because fifty minutes of
    /// an hour is a verdict.
    #[test]
    fn a_late_suspend_kills_the_run_and_still_charges_it() {
        let expiry = Expiry {
            budget: Duration::from_secs(3600),
            elapsed: Duration::from_secs(70 * 60),
            awake: Duration::from_secs(50 * 60),
        };
        assert!(expiry.wake_killed(), "{expiry:?}");
        assert!(!expiry.starved_by_suspend(), "{expiry:?}");
        assert_eq!(
            expiry.to_string(),
            "the host was suspended for 20m of a 1h budget, but 50m of it ran"
        );
        assert!(!expiry.to_string().contains("timed out"), "{expiry}");
    }

    /// The *same* twenty minutes, arriving early, and the whole argument for
    /// measuring lost budget rather than lid time: this run got forty minutes
    /// of its hour, so it is starved and the strike is waived.
    #[test]
    fn the_same_suspend_arriving_early_is_waived() {
        let expiry = Expiry {
            budget: Duration::from_secs(3600),
            elapsed: Duration::from_secs(60 * 60),
            awake: Duration::from_secs(40 * 60),
        };
        assert_eq!(expiry.suspended(), Duration::from_secs(20 * 60));
        assert!(expiry.starved_by_suspend(), "{expiry:?}");
        assert_eq!(
            expiry.to_string(),
            "the host was suspended for 20m of a 1h budget; only 40m of it ran"
        );
    }

    /// The wall arm is armed by the *suspend*, not by the wall clock: two
    /// readings at the same wall-clock instant, one four minutes of nap and one
    /// five, and only the second one is over.
    #[test]
    fn the_wall_arm_arms_at_the_wake_kill_floor() {
        let budget = Duration::from_secs(3600);
        let four = Expiry {
            budget,
            elapsed: budget,
            awake: Duration::from_secs(56 * 60),
        };
        let five = Expiry {
            budget,
            elapsed: budget,
            awake: Duration::from_secs(55 * 60),
        };
        assert_eq!(four.remaining(), Some(Duration::from_secs(4 * 60)));
        assert_eq!(five.remaining(), None);
    }

    /// The half of [`WAKE_KILL_FLOOR`]'s claim that is true, and the one the
    /// sentence #955 replaced was reaching for: awake execution past the point
    /// wall elapsed reached the budget is bounded, and bounded *strictly under*
    /// the floor — not by the floor plus a [`WALL_CLOCK_TICK`], which
    /// double-counts.
    ///
    /// A reading a second under the floor of nap, a tick of budget left, and
    /// wall elapsed already past the budget: the arm is still disarmed, so the
    /// run is still handed its last tick, and that tick is bounded by the
    /// suspend that bought it.
    #[test]
    fn awake_execution_past_the_wall_budget_stays_under_the_floor() {
        let budget = Duration::from_secs(3600);
        let awake = budget - WALL_CLOCK_TICK;
        let nap = WAKE_KILL_FLOOR - Duration::from_secs(1);
        let disarmed = Expiry {
            budget,
            elapsed: awake + nap,
            awake,
        };
        assert!(disarmed.elapsed > budget, "{disarmed:?}");
        let left = disarmed
            .remaining()
            .expect("still disarmed, so still armed only on awake");
        assert_eq!(left, WALL_CLOCK_TICK);
        assert!(left <= disarmed.suspended(), "{disarmed:?}");
        assert!(disarmed.suspended() < WAKE_KILL_FLOOR, "{disarmed:?}");

        // One more second of nap arms the wall arm, and arming only ever
        // shortens the window: `wall_left` is already zero, so it is over.
        let armed = Expiry {
            elapsed: disarmed.elapsed + Duration::from_secs(1),
            ..disarmed
        };
        assert_eq!(armed.suspended(), WAKE_KILL_FLOOR);
        assert_eq!(armed.remaining(), None);

        // And armed *before* the wall budget runs out, the arm still answers
        // with no more than the monotonic remainder — the invariant the whole
        // bound rests on.
        let early = Expiry {
            budget,
            elapsed: Duration::from_secs(50 * 60),
            awake: Duration::from_secs(50 * 60) - Duration::from_secs(400),
        };
        assert!(early.suspended() >= WAKE_KILL_FLOOR, "{early:?}");
        let wall_left = early.remaining().expect("the wall budget has not run out");
        let awake_left = early.budget - early.awake;
        assert!(wall_left <= awake_left, "{wall_left:?} vs {awake_left:?}");
    }

    /// The other half, and the one the old sentence got backwards: the
    /// *wall-clock* overshoot at the firing poll has no bound whatsoever,
    /// because nothing caps a suspend.
    ///
    /// A one-hour budget spent entirely awake, with the lid closed for three
    /// hours during the last tick of it. The deadline is over — and it reads as
    /// a plain timeout, which is the point: the overshoot is free, and #944 is
    /// working. `unspent()` is zero because the run had every second of its
    /// budget, so neither suspend sentence applies even though `suspended()` is
    /// three hours. That pairing looks like a bug and is not, so it is pinned
    /// rather than left to be discovered and "fixed".
    #[test]
    fn the_wall_clock_overshoot_at_the_firing_poll_is_unbounded() {
        let expiry = Expiry {
            budget: Duration::from_secs(3600),
            elapsed: Duration::from_secs(4 * 3600),
            awake: Duration::from_secs(3600),
        };
        assert_eq!(expiry.remaining(), None);
        assert_eq!(expiry.suspended(), Duration::from_secs(3 * 3600));
        assert!(expiry.unspent().is_zero(), "{expiry:?}");
        assert!(!expiry.starved_by_suspend(), "{expiry:?}");
        assert!(!expiry.wake_killed(), "{expiry:?}");
        assert_eq!(expiry.to_string(), "the 1h budget ran out awake");
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

        assert!(expiry.starved_by_suspend(), "{expiry:?}");
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

    /// The two clocks read at slightly different instants leave nothing
    /// unspent, which is why the waiver needs no second "was that really a
    /// suspend" test of its own.
    #[test]
    fn a_gap_under_the_floor_is_not_a_suspend() {
        let expiry = Expiry {
            budget: Duration::from_secs(3600),
            elapsed: Duration::from_secs(3600) + Duration::from_millis(4),
            awake: Duration::from_secs(3600),
        };
        assert!(expiry.unspent().is_zero(), "{expiry:?}");
        assert!(!expiry.starved_by_suspend(), "{expiry:?}");
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
        assert!(!expiry.starved_by_suspend(), "{expiry:?}");
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
