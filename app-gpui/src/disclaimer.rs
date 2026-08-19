//! What running Tasks does to your machine and your GitHub account, in one
//! place (#984).
//!
//! Three surfaces say this — the About window, the Server window's pipeline
//! control, and the Play button's tooltip — and they are read at three
//! different moments: before you point this at anything, while you are
//! looking at a stopped pipeline, and in the half-second before you start it.
//! Three inline strings would be three claims that drift apart, so the words
//! live here and the surfaces read them.
//!
//! The canonical text is the README's `## Read this first`. These constants
//! are the same claims, shorter; `crates/tasks/tests/disclaimer.rs` reads
//! *this file* and the README and fails if either stops naming an act the
//! other names. That test is in the server's tree rather than here because
//! `app-gpui` is not a workspace member — `make test` never runs the app's
//! own tests, and a guard nothing runs is not a guard.
//!
//! One distinction is worth not flattening, because it is the one a reader
//! most needs and the easiest to lose in an edit: **the agents are confined
//! and the server is not.** A Scout's or Builder's lease reaches Anthropic
//! and reads the single repository it was dispatched for
//! (`Scopes::AGENT`, `crates/tasks/src/broker.rs`); it cannot push. The push,
//! the merge and the close are the server's own acts, under its own
//! credential. "Agents can push" is wrong, and wrong in the direction that
//! makes a disclaimer ignorable.

/// One sentence, for the top of the About window.
pub const HEADLINE: &str = "Tasks runs coding agents against your repositories unattended.";

/// The four acts, for a reader who has not seen the README.
pub const SUMMARY: &str = "Scouts and Builders are Claude Code agents started with \
     --dangerously-skip-permissions: inside their VM they run whatever they decide to run, and \
     nothing asks you first. They are confined — a run's credentials reach Anthropic and read \
     the one repository it was dispatched for, and agents cannot push. The server is what \
     writes: on an agent's say-so it pushes branches, opens pull requests, merges them, \
     comments on issues and closes them. The local API has no authentication, so anything else \
     on this machine can drive the pipeline too, and the orchestrator is not in a VM at all — \
     it runs beside the server with whatever ORCHESTRATOR_CMD allows.";

/// Under the Server window's Play / Pause / Stop row. Always shown, including
/// while the pipeline is playing: a caution that disappears once the risk is
/// taken only ever warns the people not taking it.
pub const PIPELINE_CAUTION: &str = "Play lets the server push branches, open and merge pull \
     requests, and close issues on an agent's say-so, without asking.";

/// The Play button's tooltip. It used to read "work moves on its own", which
/// is true and says nothing about whose repositories it moves in.
pub const PLAY_TOOLTIP: &str =
    "Play — work moves on its own: the server pushes, merges and closes without asking";

/// Where the full version lives.
pub const README_POINTER: &str = "README.md, under \"Read this first\", says this in full.";

#[cfg(test)]
mod tests {
    use super::*;

    /// The surfaces render these directly, so an empty one is a blank line in
    /// the About window rather than a compile error.
    #[test]
    fn every_surface_has_words() {
        for text in [
            HEADLINE,
            SUMMARY,
            PIPELINE_CAUTION,
            PLAY_TOOLTIP,
            README_POINTER,
        ] {
            assert!(!text.trim().is_empty());
        }
    }

    /// The correction that the issue's own bullet list got wrong: agents are
    /// read-only and repo-bound, the server is what writes. Losing either
    /// half leaves a true-sounding sentence that misdescribes the system —
    /// "agents can push" overstates it, and dropping the server's acts
    /// understates it.
    #[test]
    fn the_summary_keeps_agents_confined_and_the_server_writing() {
        let summary = SUMMARY.to_lowercase();
        assert!(summary.contains("cannot push"));
        assert!(summary.contains("one repository"));
        assert!(summary.contains("pushes branches"));
        assert!(summary.contains("opens pull requests"));
    }

    /// The tooltip is read in the half-second before the pipeline starts, so
    /// it has to name an act rather than a mood.
    #[test]
    fn the_play_tooltip_names_an_act() {
        let tooltip = PLAY_TOOLTIP.to_lowercase();
        assert!(tooltip.contains("pushes"));
        assert!(tooltip.contains("merges"));
        assert!(tooltip.contains("closes"));
    }
}
