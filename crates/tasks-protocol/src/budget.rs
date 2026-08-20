//! One command's share of a run's budget, and how much of a run is left.
//!
//! Both halves are here rather than in the server because the same two numbers
//! are needed on both sides of the VM boundary and there must be exactly one
//! definition of each. The server computes [`command_budget`] to set the
//! agent's per-command ceiling and to quote it in the prompt; the supervisors
//! compute the same function against the run budget they were handed, to
//! decide whether a further invocation of the agent could be acted on at all
//! (see [`crate::agent_run::decide_continuation`]). `crates/tasks`'s
//! `deadline` module re-exports [`command_budget`] and [`MIN_COMMAND_BUDGET`],
//! so a server-side caller still reads them from where the run budgets live.
//!
//! This module spawns nothing and reads no environment, which is the split
//! [`crate::verify`] and [`crate::agent_run`] already set.

use std::time::{Duration, Instant};

/// Floor under [`command_budget`], so a very short turn still allows a command
/// long enough to be worth running.
pub const MIN_COMMAND_BUDGET: Duration = Duration::from_secs(60);

/// How long one command may run inside a run of `turn`.
///
/// Half, and the half is the statable guarantee: whatever a command spent, at
/// least that much run is left to report it in. The failure this comes from
/// was a 600s orchestrator turn against Claude Code's own 600s per-command
/// ceiling, where a single command could consume the entire turn and leave
/// nothing to report with — observed as an agent "killed before writing
/// output".
///
/// Derived rather than configured. A second knob is a second thing to get
/// wrong, and the invariant that matters is a relationship between the two
/// numbers, not either number alone. The floor never exceeds the turn itself.
pub fn command_budget(turn: Duration) -> Duration {
    (turn / 2).max(MIN_COMMAND_BUDGET.min(turn))
}

/// What is left of a run's budget, as the supervisor inside the VM sees it.
///
/// The host states the remainder once, at dispatch, and the VM measures
/// forward from there. `None` is a host that said nothing (one too old to
/// carry the field), and it stays `None` all the way through rather than being
/// rounded to a guess — every consumer of this treats "we do not know" as its
/// own answer, because guessing an hour would be a claim about time the host
/// will not honour.
#[derive(Debug, Clone, Copy)]
pub struct RunBudget {
    started: Instant,
    total: Option<Duration>,
}

impl RunBudget {
    /// Anchor at this instant, against the seconds the host said were left.
    pub fn starting_now(total_secs: Option<u64>) -> Self {
        Self::anchored(Instant::now(), total_secs)
    }

    /// Anchor at an instant the caller already took.
    ///
    /// A supervisor stamps `Instant::now()` before it clones, and the run
    /// budget the host stated covers that clone — so the anchor is the one the
    /// caller already has, never a second one taken later, which would hand the
    /// run back time it had already spent.
    pub fn anchored(started: Instant, total_secs: Option<u64>) -> Self {
        Self {
            started,
            total: total_secs.map(Duration::from_secs),
        }
    }

    /// What the host said was left when the run started.
    pub fn total(&self) -> Option<Duration> {
        self.total
    }

    /// Seconds still unspent, or `None` if the host stated no budget.
    ///
    /// Saturating: a run past its budget reads zero rather than wrapping, and
    /// zero is exactly the answer every caller wants there.
    pub fn remaining(&self) -> Option<Duration> {
        self.total
            .map(|total| total.saturating_sub(self.started.elapsed()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The relationship, not the number: a command may never outlast the run
    /// that has to report on it.
    #[test]
    fn a_command_budget_never_exceeds_its_turn() {
        for secs in [1u64, 30, 60, 61, 120, 600, 900, 3600, 86_400] {
            let turn = Duration::from_secs(secs);
            assert!(
                command_budget(turn) <= turn,
                "a {secs}s turn allowed a longer command"
            );
        }
    }

    #[test]
    fn a_command_gets_half_the_turn_with_a_floor_under_it() {
        assert_eq!(
            command_budget(Duration::from_secs(900)),
            Duration::from_secs(450)
        );
        assert_eq!(
            command_budget(Duration::from_secs(90)),
            MIN_COMMAND_BUDGET,
            "the floor lifts a short turn's command budget"
        );
        assert_eq!(
            command_budget(Duration::from_secs(3600)),
            Duration::from_secs(1800),
            "a scout or builder run"
        );
    }

    /// A host that said nothing must not be turned into a number here.
    #[test]
    fn an_unstated_budget_stays_unstated() {
        let budget = RunBudget::starting_now(None);
        assert_eq!(budget.total(), None);
        assert_eq!(budget.remaining(), None);
    }

    #[test]
    fn a_stated_budget_counts_down_and_stops_at_zero() {
        let budget = RunBudget::starting_now(Some(3600));
        let remaining = budget.remaining().expect("a stated budget");
        assert!(remaining <= Duration::from_secs(3600));
        assert!(remaining > Duration::from_secs(3590), "nothing has elapsed");

        // Saturating rather than wrapping: a run past its budget reads zero.
        let spent = RunBudget::anchored(Instant::now() - Duration::from_secs(120), Some(60));
        assert_eq!(spent.remaining(), Some(Duration::ZERO));
    }
}
