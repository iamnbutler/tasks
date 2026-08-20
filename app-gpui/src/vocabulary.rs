//! What the machine's words mean.
//!
//! This app renders the pipeline's own vocabulary — `awaiting_merge`,
//! `ready_to_build`, `pending_review` — and until now it rendered it with no
//! definition anywhere. A reader who did not already know what "Awaiting
//! Merge" meant had nowhere in the app to find out.
//!
//! Two rules give this module its shape, and each is a way the fix goes
//! quietly wrong:
//!
//! - **The label stays the machine's word, title-cased.** Renaming
//!   `awaiting_merge` to something friendlier would put a second vocabulary
//!   between this app and `tasks status`, the event feed and `/status` —
//!   worse than an unexplained term, because a reader who looked the friendly
//!   name up would find nothing. What changed is that the word now carries its
//!   definition. The one exception is [`gh_state`], which rendered its wire
//!   string *raw* (a lowercase `open` beside a title-cased `In Review`) and is
//!   now "Issue Open" / "Issue Closed", because a bare "Open" does not say
//!   what is open.
//! - **Exhaustive matches, no `_` arm.** A state added to `tasks-api` becomes
//!   a compile error here rather than falling through to a title-cased wire
//!   string, which is the failure this module exists to end and is precisely
//!   the kind that comes back invisibly. Same idiom as `empty_state`'s total
//!   walk.
//!
//! The gloss reaches the reader through [`crate::components::status_badge`],
//! which takes it **by value** — the badge that showed `Awaiting Merge` and
//! explained it nowhere is now one nobody can write. The precedent is
//! `Check::fail` taking its fix by value in `tasks doctor`: a convention is
//! what the next call site quietly skips.

use tasks_client::api::models::{BuildStatus, GhState, SpecQueueStatus, TaskState};

use crate::components::title_case;

/// One word of the pipeline's vocabulary, and what it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    /// The machine's own word, title-cased — what the badge renders.
    pub label: String,
    /// One sentence a reader who does not know the word can act on.
    pub gloss: &'static str,
}

impl Term {
    fn new(wire: &str, gloss: &'static str) -> Self {
        Self {
            label: title_case(wire),
            gloss,
        }
    }
}

/// Where a task sits in the pipeline.
pub fn task_state(state: TaskState) -> Term {
    let gloss = match state {
        TaskState::Backlog => {
            "Ingested from GitHub and untouched — nobody has picked this up, and nothing will \
             dispatch until somebody does."
        }
        TaskState::Queued => {
            "Picked up and waiting its turn for a Scout, in the order the rail's tree shows."
        }
        TaskState::Scouting => {
            "A Scout is exploring the work right now, in its own VM, to produce a spec."
        }
        TaskState::InReview => {
            "A Scout produced a spec and it is waiting for a human verdict — approve it, or send \
             it back with feedback."
        }
        TaskState::ReadyToBuild => {
            "Its spec is approved and parked until a Builder run consumes it; builds are strictly \
             serial, so it waits for the lane."
        }
        TaskState::Building => {
            "A Builder is implementing this task's spec right now, possibly batched with others \
             on one branch."
        }
        TaskState::AwaitingMerge => {
            "The Builder opened a pull request and nobody has resolved it yet. A PR is a claim, \
             not a delivery — this is still live work."
        }
        TaskState::Done => {
            "Shipped, which here means exactly one thing: the GitHub issue is closed upstream."
        }
        TaskState::Rejected => {
            "Either somebody decided against this, or three Scouts failed to produce a spec. \
             Reopening the issue makes the second kind eligible again."
        }
    };
    Term::new(state.as_str(), gloss)
}

/// Where a spec sits in the review queue.
pub fn spec_queue_status(status: SpecQueueStatus) -> Term {
    let gloss = match status {
        SpecQueueStatus::PendingReview => {
            "Waiting for a verdict. Nothing builds from this until somebody approves it."
        }
        SpecQueueStatus::Approved => {
            "Cleared for implementation, and it will be picked up when the serial build lane \
             frees."
        }
        SpecQueueStatus::NeedsRevision => {
            "Sent back with feedback; the task returns to the queue for another Scout, which \
             reads that feedback."
        }
        SpecQueueStatus::Blocked => {
            "Three build attempts were spent without a pull request, so nothing will dispatch \
             from it again."
        }
        SpecQueueStatus::Rejected => {
            "Ruled out by a reviewer — this particular spec will not be implemented."
        }
        SpecQueueStatus::Built => {
            "Consumed by a Builder run that succeeded. Terminal, and assigned by the system \
             rather than by a reviewer."
        }
    };
    Term::new(status.as_str(), gloss)
}

/// Where one Builder run sits.
pub fn build_status(status: BuildStatus) -> Term {
    let gloss = match status {
        BuildStatus::Queued => {
            "Requested, waiting for the serial build loop to claim it — one build runs at a time."
        }
        BuildStatus::Running => "A Builder VM is implementing this batch right now.",
        BuildStatus::Succeeded => {
            "The branch was pushed and a pull request opened. What happens next is the merge."
        }
        BuildStatus::Failed => {
            "The run did not produce a pull request; its exit reason says which step gave out."
        }
        BuildStatus::Cancelled => {
            "Somebody stopped this on purpose. Its specs went back to approved with no attempt \
             charged against them."
        }
    };
    Term::new(status.as_str(), gloss)
}

/// Whether the *issue upstream* is open or closed.
///
/// The one term whose label is not simply its wire word title-cased: bare
/// "Open" beside a task state does not say what is open.
pub fn gh_state(state: GhState) -> Term {
    let (label, gloss) = match state {
        GhState::Open => (
            "Issue Open",
            "The GitHub issue behind this task is still open. Read at poll time, so it can lag a \
             close by up to one interval.",
        ),
        GhState::Closed => (
            "Issue Closed",
            "The GitHub issue behind this task is closed — which is the only thing that ever \
             makes a task done.",
        ),
    };
    Term {
        label: label.to_string(),
        gloss,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_term() -> Vec<Term> {
        let mut terms = Vec::new();
        for state in [
            TaskState::Backlog,
            TaskState::Queued,
            TaskState::Scouting,
            TaskState::InReview,
            TaskState::ReadyToBuild,
            TaskState::Building,
            TaskState::AwaitingMerge,
            TaskState::Done,
            TaskState::Rejected,
        ] {
            terms.push(task_state(state));
        }
        for status in [
            SpecQueueStatus::PendingReview,
            SpecQueueStatus::Approved,
            SpecQueueStatus::NeedsRevision,
            SpecQueueStatus::Blocked,
            SpecQueueStatus::Rejected,
            SpecQueueStatus::Built,
        ] {
            terms.push(spec_queue_status(status));
        }
        for status in [
            BuildStatus::Queued,
            BuildStatus::Running,
            BuildStatus::Succeeded,
            BuildStatus::Failed,
            BuildStatus::Cancelled,
        ] {
            terms.push(build_status(status));
        }
        for state in [GhState::Open, GhState::Closed] {
            terms.push(gh_state(state));
        }
        terms
    }

    /// Every term is defined, and defined as a sentence — the whole point of
    /// the module is that the word now carries something a reader can act on.
    #[test]
    fn every_term_has_a_gloss_that_is_a_sentence() {
        for term in every_term() {
            assert!(!term.label.is_empty(), "{term:?}");
            assert!(term.gloss.len() > 30, "{term:?}");
            assert!(term.gloss.ends_with('.'), "{term:?}");
            assert!(
                term.gloss.chars().next().is_some_and(char::is_uppercase),
                "{term:?}"
            );
        }
    }

    /// The failure that is easiest to reintroduce while looking like a fix: a
    /// "definition" that restates the label and explains nothing.
    #[test]
    fn no_gloss_merely_restates_its_own_label() {
        for term in every_term() {
            let gloss = term.gloss.to_lowercase();
            let label = term.label.to_lowercase();
            assert_ne!(gloss.trim_end_matches('.'), label, "{term:?}");
            // A gloss whose only content is its own words is the same defect
            // one step less obvious.
            let words: Vec<&str> = gloss.split_whitespace().collect();
            assert!(words.len() >= 8, "{term:?}");
        }
    }

    /// The label stays the machine's word, so this app and `tasks status`
    /// cannot come to call one state two things. `gh_state` is the stated
    /// exception, and it still contains its wire word.
    #[test]
    fn labels_are_the_wire_words_title_cased() {
        for state in [
            TaskState::Backlog,
            TaskState::Queued,
            TaskState::Scouting,
            TaskState::InReview,
            TaskState::ReadyToBuild,
            TaskState::Building,
            TaskState::AwaitingMerge,
            TaskState::Done,
            TaskState::Rejected,
        ] {
            assert_eq!(task_state(state).label, title_case(state.as_str()));
        }
        for status in [
            SpecQueueStatus::PendingReview,
            SpecQueueStatus::Approved,
            SpecQueueStatus::NeedsRevision,
            SpecQueueStatus::Blocked,
            SpecQueueStatus::Rejected,
            SpecQueueStatus::Built,
        ] {
            assert_eq!(spec_queue_status(status).label, title_case(status.as_str()));
        }
        for status in [
            BuildStatus::Queued,
            BuildStatus::Running,
            BuildStatus::Succeeded,
            BuildStatus::Failed,
            BuildStatus::Cancelled,
        ] {
            assert_eq!(build_status(status).label, title_case(status.as_str()));
        }
        // The exception, and why: "Open" alone does not say what is open.
        assert_eq!(gh_state(GhState::Open).label, "Issue Open");
        assert_eq!(gh_state(GhState::Closed).label, "Issue Closed");
        for state in [GhState::Open, GhState::Closed] {
            assert!(gh_state(state)
                .label
                .to_lowercase()
                .contains(state.as_str()));
        }
    }

    /// Two states sharing a definition means at least one of them is
    /// undefined — and a reader comparing two badges would learn nothing.
    #[test]
    fn no_two_glosses_are_identical() {
        let mut glosses: Vec<&str> = every_term().iter().map(|term| term.gloss).collect();
        let total = glosses.len();
        glosses.sort_unstable();
        glosses.dedup();
        assert_eq!(glosses.len(), total);
    }

    /// `Rejected` names two different things, and CLAUDE.md's own rule turns
    /// on exactly that distinction — a reader who does not know it cannot act
    /// on the state.
    #[test]
    fn rejected_says_it_names_two_outcomes() {
        let gloss = task_state(TaskState::Rejected).gloss;
        assert!(gloss.contains("decided against"), "{gloss}");
        assert!(gloss.contains("Scouts failed"), "{gloss}");
    }
}
