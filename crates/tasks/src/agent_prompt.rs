//! The one thing a Scout and a Builder both have to be told about the harness
//! they are running under.
//!
//! # What this is for
//!
//! #962: a Scout finished its implementation, hit a cold 750-crate build, put
//! it behind three `until [ -f /tmp/test.log ]; do sleep 20; done` waiters,
//! said it would pick the result up when the tests reported, and ended its turn
//! 490 seconds into a 3600 second budget. Under `claude --print` the turn
//! ending *is* the run ending: the children were killed a moment later, the
//! supervisor collected a `SPEC.md` that was never written, and the task was
//! charged a dispatch attempt for a verdict nothing had reached.
//!
//! Backgrounding was not a bad instinct. It was the only move available. The
//! Scout and Builder VMs set **no bash timeout at all**, so the agent ran under
//! Claude Code's 120s default with a 600s ceiling, and a cold workspace build
//! fits in neither. Telling an agent to run it inline under that ceiling would
//! have been an instruction it could not follow — which is why this section
//! ships in the same change as the per-command budget in
//! [`crate::run::agent_vm_config`], and why shipping it alone would have looked
//! like a fix while being inert.
//!
//! # One module, one fact, two agents
//!
//! The section is generated from the same [`command_budget`] applied to the
//! same budget the VM is actually given, so the prompt can never promise a
//! command length the harness will kill. That is the standing rule for anything
//! a prompt claims about its environment.
//!
//! # It sits last
//!
//! The slot immediately before `## Your job` belongs to the directions section
//! (CLAUDE.md pins that adjacency, and the prompt regression tests now pin it
//! in code). Last is where the failure happens: an agent parks on a background
//! command at the *end* of a long run, not the start of one.

use std::time::Duration;

use tasks_protocol::AgentRole;

use crate::deadline::command_budget;

/// The `## How this run works` section, for an agent of `role` with a run
/// budget of `budget`.
///
/// Three claims, and each is checkable against something this server does:
/// how long the whole run has (the dispatcher's own deadline), how long one
/// command may run (the `BASH_DEFAULT_TIMEOUT_MS` this server sets on the VM),
/// and what happens to a backgrounded child (the `claude --print` process
/// model).
///
/// The verification sentence differs by role on purpose. A Scout's output is
/// reviewed before anything is built from it, so the narrowest command that
/// exercises the change is the right one. A Builder's branch ships, and the
/// supervisor runs the project's whole suite against it before a pull request
/// exists — so the reason to widen comes up far more often. A single shared
/// sentence would have to be wrong for one of them.
pub fn harness_section(role: AgentRole, budget: Duration) -> String {
    let run_secs = budget.as_secs();
    let command_secs = command_budget(budget).as_secs();
    let verification = match role {
        AgentRole::Scout => {
            "Verify proportionately to what you touched: the narrowest command \
             that actually exercises your change is the right one, and a whole-\
             workspace build to check a one-file edit is how a run spends its \
             budget on nothing. Your spec is reviewed before anything is built \
             from it, so what a reviewer needs from you is that the claims in it \
             were checked, not that everything was rebuilt."
        }
        AgentRole::Builder => {
            "Verify proportionately to what you touched, but lean wider than a \
             Scout would: your branch ships. After you stop, the supervisor runs \
             this project's own test suite against the committed tree, and a red \
             run costs you your one repair round and then the whole build. \
             Getting there first is entirely in your interest."
        }
    };
    format!(
        "## How this run works\n\n\
         Read this before you plan how to verify anything.\n\n\
         **This is a single turn, and it is the whole run.** When your turn \
         ends, this run ends: the VM is torn down, and whatever is on disk at \
         that moment is everything anyone will ever see of it. There is no \
         later turn in which you pick something up.\n\n\
         **A backgrounded command dies with your turn.** Not when it finishes — \
         when your turn ends. `cmd &`, `nohup`, a `sleep`-and-poll loop, \
         anything you leave running while you go and do something else: the \
         moment you stop, it is killed, and any file it was going to write is \
         never written. Waiting for a result you have backgrounded by ending \
         your turn does not work and cannot be made to work. Run it in the \
         foreground and wait for it.\n\n\
         **You have the time to do that.** One command may run for {command_secs}s \
         here — not the 120s you may be used to — and the whole run has \
         {run_secs}s. A cold build of a large workspace fits inside that \
         comfortably. It is affordable rather than fast, and it is the intended \
         way to spend the budget.\n\n\
         {verification}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The number in the prompt is the number the harness enforces, for both
    /// agents. Anything else is a promise the VM will break.
    #[test]
    fn the_section_quotes_the_command_budget_the_harness_will_enforce() {
        let budget = Duration::from_secs(3600);
        for role in [AgentRole::Scout, AgentRole::Builder] {
            let section = harness_section(role, budget);
            assert!(section.contains("1800s"), "{section}");
            assert!(section.contains("3600s"), "{section}");
        }
    }

    /// The whole point of the section, in both prompts.
    #[test]
    fn the_section_says_a_backgrounded_command_dies_with_the_turn() {
        for role in [AgentRole::Scout, AgentRole::Builder] {
            let section = harness_section(role, Duration::from_secs(3600));
            assert!(
                section.contains("A backgrounded command dies with your turn"),
                "{section}"
            );
            assert!(section.contains("## How this run works"), "{section}");
        }
    }

    /// Verification breadth differs by role, because the two outputs are read
    /// by different things at different times.
    #[test]
    fn a_builder_is_told_to_verify_wider_than_a_scout() {
        let scout = harness_section(AgentRole::Scout, Duration::from_secs(3600));
        let builder = harness_section(AgentRole::Builder, Duration::from_secs(3600));
        assert!(scout.contains("reviewed before anything is built from it"));
        assert!(builder.contains("your branch ships"));
        assert_ne!(scout, builder);
    }

    /// A short budget still promises a runnable command: the floor under
    /// `command_budget` never exceeds the run itself.
    #[test]
    fn a_short_run_still_quotes_a_command_budget_inside_it() {
        let section = harness_section(AgentRole::Scout, Duration::from_secs(90));
        assert!(section.contains("60s"), "{section}");
        assert!(section.contains("90s"), "{section}");
    }
}
