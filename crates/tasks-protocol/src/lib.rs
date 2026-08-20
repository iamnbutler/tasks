//! Protocol types shared between the `tasks` server and its agent VMs.
//!
//! The server sends [`TaskCommand`] messages (wrapped in `VmCommand<P>` by
//! vm-pool) to a VM's supervisor. The supervisor streams back [`TaskEvent`]
//! messages (wrapped in `VmEvent<P>`). Infrastructure-level traffic
//! (Ping/Pong/Shutdown/Ready) is handled by vm-pool itself.
//!
//! # The role union
//!
//! A vm-pool `Service` is monomorphised over exactly one `AppProtocol`, so
//! Scouts and Builders share one wire type, tagged by `role`:
//! `{"role":"scout","kind":"start",...}`. Each supervisor binary answers only
//! its own role and rejects the other with a terminal `Failed` event. This is
//! the Scout/Builder information barrier at the wire: a Builder VM cannot be
//! handed Scout traffic by accident, because the mismatch is a rejection at
//! the supervisor, not a convention in the caller.

use serde::{Deserialize, Deserializer, Serialize};
use vm_pool_protocol::{AppProtocol, ServiceErrorKind};

pub mod agent_run;
pub mod budget;
pub mod redact;
pub mod verify;
pub mod vm_memory;

/// Which of the two agents a run belongs to.
///
/// Not a wire type — the wire already tags a role on [`TaskCommand`] and
/// [`TaskEvent`], and this is deliberately not that. It exists because two
/// things the server and the supervisors both render — the harness section of
/// an agent's prompt, and [`crate::agent_run::continuation_prompt`] — differ by
/// exactly one noun (the artifact the agent is asked to produce), and a shared
/// sentence with a role-shaped hole in it is one statement rather than two that
/// can drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    /// Explores, and concludes by writing `SPEC.md` — with `NOTES.md` as the
    /// honest fallback when it cannot.
    Scout,
    /// Implements, and concludes by committing — with `SUMMARY.md` as where it
    /// says what it could not do.
    Builder,
}

impl AgentRole {
    /// What this agent is asked to produce, as the prompt names it.
    pub fn deliverable(&self) -> &'static str {
        match self {
            Self::Scout => "a finished `SPEC.md`",
            Self::Builder => "committed work on this branch",
        }
    }

    /// Where this agent writes down what it could *not* do.
    ///
    /// Every prompt that tells an agent it has one attempt left has to name
    /// this in the same breath, or the only exit it leaves is to produce
    /// something — which for a Scout is the half-explored spec that reaches a
    /// reviewer looking finished, the exact failure the `SPEC.md`/`NOTES.md`
    /// split exists to prevent.
    pub fn shortfall_artifact(&self) -> &'static str {
        match self {
            Self::Scout => "NOTES.md",
            Self::Builder => "SUMMARY.md",
        }
    }
}

/// Whether a terminal failure is a verdict on the *work*, or something that
/// happened *to* the run.
///
/// `dispatch_attempts` and `build_attempts` exist so that work which genuinely
/// cannot be done stops consuming the pipeline after three tries. That cap is
/// only meaningful for a run that actually judged the work: a dropped API
/// connection, a deliberate cancel or a server restart says nothing about the
/// task, and charging one identically means three infrastructure deaths reject
/// a good task or `blocked` a good spec having learned nothing (#884, and #825
/// where five scout attempts burned in one night without a single verdict
/// among them).
///
/// The class is stamped by the supervisor — the only thing that knows how the
/// agent died — and read off the *field* by the host. Never off the reason
/// text: a reason is prose written for a human, and a strike decision that
/// greps it would change meaning the next time someone improves a sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The run judged the work: an agent that ran to completion and produced
    /// nothing usable, a clone that cannot succeed, a budget spent in full.
    /// This is the case the attempt cap was written for, and the default —
    /// including for every event that predates the field.
    #[default]
    Verdict,
    /// Something below the run failed, rather than the run failing: the agent's
    /// connection to the API dropping mid-response (#845), a finished branch
    /// that could not be pushed, a vm-pool restart that took the event stream
    /// away, or a host that suspended for most of the budget (#929). Nothing
    /// about the work is implicated in any of them, and the note the waiver
    /// writes names the specific error beside the class.
    Transport,
    /// Somebody stopped the run on purpose, before it could show whether it
    /// would have worked.
    Cancelled,
    /// The host could not pick the run back up after a restart. Never crosses
    /// the wire — no supervisor is alive to send it — but it is the same
    /// question, so it is the same enum.
    Orphaned,
}

impl FailureClass {
    /// The wire form, and what a note or a log line prints.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Verdict => "verdict",
            Self::Transport => "transport",
            Self::Cancelled => "cancelled",
            Self::Orphaned => "orphaned",
        }
    }

    /// Whether this failure judged the work, and therefore costs an attempt.
    pub fn is_verdict(&self) -> bool {
        matches!(self, Self::Verdict)
    }

    /// Why the attempt was not charged, in a clause a note can carry. `None`
    /// for a verdict, which is charged.
    pub fn waiver_reason(&self) -> Option<&'static str> {
        match self {
            Self::Verdict => None,
            Self::Transport => Some(
                "the run was interrupted by something underneath it rather than by the \
                 work, which is an infrastructure failure and not a verdict",
            ),
            Self::Cancelled => Some(
                "the run was stopped deliberately, before it could show whether it would \
                 have worked",
            ),
            Self::Orphaned => Some(
                "the run could not be picked up after a server restart, which is the \
                 restart's fault and not the work's",
            ),
        }
    }

    /// What a vm-pool refusal means for the attempt cap — one statement, so a
    /// Scout and a Builder cannot give different answers about the same "no".
    ///
    /// Only [`ServiceErrorKind::Capacity`] is waived, and the line is **whether
    /// the condition clears by itself**: a full pool is a property of the
    /// moment, and the same dispatch of the same task succeeds once something
    /// is handed back. A reference that does not resolve refuses identically
    /// forever, and waiving *that* is the retry-forever loop the cap exists to
    /// stop — which is why [`ServiceErrorKind::Image`] and everything beside it
    /// stays charged, [`ServiceErrorKind::Unspecified`] included. Unspecified
    /// is the routine reading against a vm-pool older than the field (a
    /// separate daemon, upgraded separately), so defaulting it to a waiver
    /// would silently waive every permanent misconfiguration on every old
    /// daemon.
    ///
    /// The waiver of `Capacity` is only safe while something *else* stops the
    /// refused task being re-attempted twice a second: `Scout::dispatch` now
    /// returns it to `Queued`, and `crate::run`'s pool hold is what keeps that
    /// requeue from becoming a 500 ms loop against a pool that stays full
    /// (#967). Waiving a strike removes the backstop that used to bound that
    /// loop, so the hold is mandatory rather than preferable — if it is ever
    /// removed, this arm has to be revisited with it.
    ///
    /// The match is exhaustive rather than `_ => Transport`, so a kind added to
    /// vm-pool tomorrow cannot widen a waiver without somebody deciding it
    /// here.
    pub fn for_service_error(kind: ServiceErrorKind) -> Self {
        match kind {
            ServiceErrorKind::Capacity => Self::Transport,
            // Not a typo, and not the same word twice: `ServiceErrorKind::
            // Transport` is vm-pool's stdio link to a VM failing, which is a
            // real failure of *this* run's infrastructure with nothing to
            // suggest the next attempt goes better. `FailureClass::Transport`
            // means "nothing here judged the work", and this does not qualify.
            ServiceErrorKind::Transport
            | ServiceErrorKind::Unspecified
            | ServiceErrorKind::NoSuchVm
            | ServiceErrorKind::NotReady
            | ServiceErrorKind::Image
            | ServiceErrorKind::Runtime
            | ServiceErrorKind::BadRequest
            | ServiceErrorKind::Other => Self::Verdict,
        }
    }

    /// The wire form, read forgivingly: anything unrecognised is a
    /// [`FailureClass::Verdict`]. See the [`Deserialize`] impl for why.
    fn from_wire(raw: &str) -> Self {
        match raw {
            "transport" => Self::Transport,
            "cancelled" => Self::Cancelled,
            "orphaned" => Self::Orphaned,
            _ => Self::Verdict,
        }
    }
}

impl std::fmt::Display for FailureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Skew runs in both directions, and only one of them is obvious.
///
/// An *older* supervisor image omitting the field is the routine one, and
/// `#[serde(default)]` on the field covers it. The other direction is why this
/// impl is hand-written: a *newer* supervisor sending a class this binary has
/// never heard of must not make the terminal event undecodable, because a lost
/// terminal event does not cost a strike — it costs the run its outcome, and
/// hangs it until the deadline. Unknown decays to `Verdict`: today's
/// behaviour, and never a silent waive.
///
/// `#[serde(other)]` cannot express this — it is rejected on a plain
/// externally-tagged unit-variant enum — and reading the value as a
/// `serde_json::Value` first means a class sent as a number or a null decays
/// the same way a misspelt string does.
impl<'de> Deserialize<'de> for FailureClass {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(deserializer)?;
        Ok(raw
            .as_str()
            .map(FailureClass::from_wire)
            .unwrap_or_default())
    }
}

/// The application protocol tasks uses with vm-pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TasksProtocol;

impl AppProtocol for TasksProtocol {
    type Command = TaskCommand;
    type Event = TaskEvent;
}

/// Role-tagged command union: everything the tasks server can send to a VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum TaskCommand {
    Scout(ScoutCommand),
    Build(BuildCommand),
}

/// Role-tagged event union: everything a VM can stream back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum TaskEvent {
    Scout(ScoutEvent),
    Build(BuildEvent),
}

/// Commands the tasks server sends to a Scout VM.
///
/// There is deliberately no cancel command: the host cancels a scout by
/// deallocating its VM, which tears down the supervisor and everything
/// under it. In-band cancellation would race the supervisor's
/// single-threaded command loop for no benefit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScoutCommand {
    /// Begin exploration. The supervisor clones the repo, creates a throwaway
    /// branch, runs Claude Code with the provided prompt, and reports back.
    Start {
        task_id: String,
        repo_clone_url: String,
        base_branch: String,
        /// The prompt Claude Code sees. Includes the issue body and spec
        /// template instructions, rendered host-side.
        prompt: String,
        /// Seconds of this run's budget still unspent when the command was
        /// sent, so the VM can tell whether a further attempt at the agent
        /// could be acted on at all (see
        /// [`crate::agent_run::decide_continuation`]) — the Builder's field of
        /// the same name, for the same reason one level along.
        ///
        /// `#[serde(default)]` in the direction that actually happens: a
        /// supervisor image rebuilt ahead of the server it talks to sees no
        /// field, and `None` declines a continuation rather than guessing at
        /// how much time it would have had. A telling the agent could not act
        /// on is a wrong fact under an attempt cap.
        #[serde(default)]
        budget_secs: Option<u64>,
    },
}

/// Which build of a supervisor binary is inside a VM, as it reports itself.
///
/// A VM exists only while a run is inside it, so the `Started` event is the
/// only moment there is to ask. The two fields are `build-stamp`'s, computed
/// the same way the server's own are — which is the whole reason the two
/// numbers can be compared at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorBuild {
    /// `0.1.<commit count>`, or the crate version with no git in reach.
    pub version: String,
    /// Short SHA, `-dirty` for an uncommitted tree, or `unknown`.
    pub commit: String,
}

/// Events a Scout VM streams back to the tasks server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScoutEvent {
    /// Supervisor has received `Start` and is setting up (clone, branch).
    ///
    /// `supervisor` is **`#[serde(default)]` and that is load-bearing**, not
    /// decorative. Images are rebuilt by hand, so the host is routinely newer
    /// than the supervisor talking to it — that skew *is* #909. A `Started`
    /// from an image built before this field existed carries no `supervisor`
    /// key and must still decode.
    ///
    /// `None` is therefore the **loudest** reading, not the quietest: it means
    /// "built before there was an identity to send", which is strictly staler
    /// than any version a supervisor could report. See
    /// `tasks_api::version::ImageFreshness::Unstamped`.
    Started {
        branch: String,
        #[serde(default)]
        supervisor: Option<SupervisorBuild>,
    },
    /// A stdout/stderr line from the agent process. Best-effort — may be
    /// dropped under load. Useful for breadcrumbs / live log tailing.
    Progress { stream: LogStream, line: String },
    /// The agent's `NOTES.md` as of now — exploration so far, not a spec.
    /// Non-terminal and pushed periodically while the agent works.
    ///
    /// Pushed rather than pulled because the host cancels a scout by
    /// destroying its VM: at the deadline there is no supervisor left to ask,
    /// and nothing on that disk is recoverable. Whatever the host already
    /// holds is all it will ever have. Capped at [`MAX_NOTES_BYTES`].
    Checkpoint { notes_markdown: String },
    /// Agent process finished. Implementation branch state at this point is
    /// whatever the agent produced.
    ImplementationFinished { exit_code: i32 },
    /// The supervisor has read SPEC.md produced by the agent. Terminal success.
    Completed {
        spec_markdown: String,
        files_touched: Vec<String>,
    },
    /// Terminal, and **never a spec**: the run ended without concluding, but
    /// left something written down. `notes_markdown` is salvage — an
    /// explicitly unverified lead for the next attempt, with no review path
    /// and no queue entry. Anything that would treat this as a deliverable is
    /// a bug; a half-explored spec that looks finished is worse than the lost
    /// run this exists to prevent.
    ///
    /// `reason` says why the run ended (agent exit, an unfinished SPEC.md and
    /// what it was missing). Capped at [`MAX_NOTES_BYTES`].
    StoppedEarly {
        reason: String,
        notes_markdown: String,
        files_touched: Vec<String>,
        /// Whether the run judged the work. The host reads this field and
        /// never `reason`, which is prose. See [`FailureClass`].
        #[serde(default)]
        class: FailureClass,
    },
    /// Terminal failure with *nothing to salvage*; `reason` is a short
    /// diagnostic and the supervisor has already exited or is about to.
    ///
    /// Distinct from [`ScoutEvent::StoppedEarly`] on purpose: "we salvaged
    /// something" and "there was nothing" are different facts, and a retry
    /// that cannot tell them apart re-derives what it already had.
    Failed {
        reason: String,
        /// Whether the run judged the work. See [`FailureClass`].
        #[serde(default)]
        class: FailureClass,
    },
}

/// Hard cap on the notes carried by [`ScoutEvent::Checkpoint`] and
/// [`ScoutEvent::StoppedEarly`]. Past this the supervisor trims (notes are
/// written top-down, so the head is the part worth keeping) rather than
/// letting one runaway file dominate the JSON-lines transport.
///
/// This is a *transport* cap, not a prompt cap: 256 KiB is fine on the wire
/// and ruinous in a retry's context window. The dispatcher trims much harder
/// again on its way into a prompt.
pub const MAX_NOTES_BYTES: usize = 256 * 1024;

/// Commands the tasks server sends to a Builder VM.
///
/// Like scouts, there is no cancel command — the host cancels a build by
/// deallocating the VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BuildCommand {
    /// Begin a build. The supervisor clones the repo at full depth, creates
    /// `branch` (the *host* chooses the name — the host is what pushes it),
    /// runs the agent with the provided prompt, sweeps any uncommitted work
    /// into a final commit, and reports the commits back as a git bundle.
    Start {
        build_id: String,
        repo_clone_url: String,
        base_branch: String,
        /// Branch the supervisor creates and the server later pushes.
        branch: String,
        /// The prompt the agent sees: concatenated approved spec markdown and
        /// issue titles, rendered host-side. Nothing Scout-code-derived.
        prompt: String,
        /// Seconds of this run's budget still unspent when the command was
        /// sent, so the VM can size its test suite's budget to expire *first*
        /// (see [`crate::verify::suite_budget_secs`]). Not the configured
        /// budget: a resumed build is bounded by the remainder, and a suite
        /// sized against the wrong number is a suite the outer deadline kills
        /// mid-run — which is a `Verdict` against work that may be fine.
        ///
        /// `#[serde(default)]` in the direction that actually happens: a
        /// supervisor image rebuilt ahead of the server it talks to sees no
        /// field, and reports `Unavailable` rather than guessing.
        #[serde(default)]
        budget_secs: Option<u64>,
        /// The project's trunk (`SCOUT_BASE_BRANCH`), so the VM can say which
        /// gate ruled when this build is stacked on another build's branch.
        ///
        /// `base_branch` is not it and cannot be made into it: this pipeline
        /// stacks builds routinely, so a stacked build's base *is* another
        /// build's branch, and a `.tasks/verify` weakened by that earlier build
        /// is already in this one's base commit. Comparing against the trunk is
        /// what notices; comparing against the base is what missed it.
        #[serde(default)]
        trunk_branch: Option<String>,
    },
}

/// Events a Builder VM streams back to the tasks server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BuildEvent {
    /// Supervisor cloned and branched; `base_sha` is the commit the branch
    /// grew from (and the thin bundle's prerequisite).
    ///
    /// `supervisor` carries the image's build identity, under the same skew
    /// rule as [`ScoutEvent::Started`] — read that one.
    Started {
        base_sha: String,
        #[serde(default)]
        supervisor: Option<SupervisorBuild>,
    },
    /// A stdout/stderr line from the agent process. Best-effort.
    Progress { stream: LogStream, line: String },
    /// Agent process finished; the sweep commit (if any) hasn't happened yet.
    ImplementationFinished { exit_code: i32 },
    /// Terminal success. `bundle_base64` is a *thin* `git bundle` of
    /// `base_sha..head_sha` — the server unbundles, verifies the tip matches
    /// `head_sha`, and pushes. Capped at [`MAX_BUNDLE_BASE64_BYTES`] encoded;
    /// an oversized bundle is a `Failed`, not a truncation.
    Completed {
        base_sha: String,
        head_sha: String,
        bundle_base64: String,
        /// SUMMARY.md if the agent wrote one — becomes the PR body. Missing
        /// prose is not a failure; the code is the deliverable.
        summary: Option<String>,
        files_touched: Vec<String>,
        /// What the project's own test suite said about the tree this bundle
        /// carries, run by the **supervisor** rather than claimed by the agent.
        ///
        /// `None` means the image predates the check entirely, which reads as
        /// "no run on record" everywhere and is never green — so an image
        /// nobody has rebuilt degrades to the behaviour that shipped before
        /// this existed rather than breaking. See [`crate::verify`].
        ///
        /// There is no way for this field to say the suite *failed*: a red
        /// suite fails the build inside the VM and no bundle is packaged at
        /// all.
        #[serde(default)]
        verification: Option<crate::verify::Verification>,
    },
    /// Terminal failure. An empty branch (`head == base`) lands here — the
    /// Builder analogue of a Scout's missing SPEC.md.
    Failed {
        reason: String,
        /// Whether the run judged the work. The host reads this field and
        /// never `reason`. See [`FailureClass`].
        #[serde(default)]
        class: FailureClass,
    },
}

/// Hard cap on the base64-encoded bundle carried in [`BuildEvent::Completed`].
/// Past this the supervisor fails the build loudly rather than shipping a
/// blob that would dominate the JSON-lines transport.
pub const MAX_BUNDLE_BASE64_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_pool_protocol::{ServiceCommand, ServiceEvent, VmCommand, VmEvent, VmId};

    fn start_cmd() -> TaskCommand {
        TaskCommand::Scout(ScoutCommand::Start {
            task_id: "task_abc".into(),
            repo_clone_url: "https://github.com/o/r.git".into(),
            base_branch: "main".into(),
            prompt: "go".into(),
        })
    }

    #[test]
    fn scout_command_roundtrip_carries_both_tags() {
        let cmd = start_cmd();
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"role\":\"scout\""));
        assert!(json.contains("\"kind\":\"start\""));
        let back: TaskCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn build_command_roundtrip() {
        let cmd = TaskCommand::Build(BuildCommand::Start {
            build_id: "build_abc".into(),
            repo_clone_url: "https://github.com/o/r.git".into(),
            base_branch: "main".into(),
            branch: "build/build_abc".into(),
            prompt: "## Spec 1 of 1".into(),
            budget_secs: Some(3600),
            trunk_branch: Some("main".into()),
        });
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"role\":\"build\""));
        let back: TaskCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn build_event_roundtrip() {
        let evt = TaskEvent::Build(BuildEvent::Completed {
            base_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
            bundle_base64: "AAAA".into(),
            summary: Some("Did the thing.".into()),
            files_touched: vec!["src/lib.rs".into()],
            verification: Some(crate::verify::Verification::new(
                crate::verify::VerificationStatus::Passed,
                ".tasks/verify passed (gate abc1234, same as main)",
            )),
        });
        let json = serde_json::to_string(&evt).unwrap();
        let back: TaskEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, evt);
    }

    /// The barrier is a deserialization error, not a convention: a scout
    /// payload relabelled `build` does not decode into a BuildCommand.
    #[test]
    fn a_relabelled_scout_payload_does_not_cross_the_barrier() {
        let scout = serde_json::to_string(&start_cmd()).unwrap();
        let forged = scout.replace("\"role\":\"scout\"", "\"role\":\"build\"");
        assert!(
            serde_json::from_str::<TaskCommand>(&forged).is_err(),
            "a scout start is not a build start"
        );

        let evt = TaskEvent::Scout(ScoutEvent::Completed {
            spec_markdown: "## Spec".into(),
            files_touched: vec![],
        });
        let json = serde_json::to_string(&evt).unwrap();
        let forged = json.replace("\"role\":\"scout\"", "\"role\":\"build\"");
        assert!(serde_json::from_str::<TaskEvent>(&forged).is_err());
    }

    /// A checkpoint and a stopped-early run are distinct wire kinds, and
    /// neither one is `completed`. Nothing downstream should be able to reach
    /// a spec by reading either.
    #[test]
    fn checkpoint_and_stopped_early_roundtrip_and_are_not_completions() {
        let checkpoint = TaskEvent::Scout(ScoutEvent::Checkpoint {
            notes_markdown: "# Notes\n\nHalf of an idea.".into(),
        });
        let json = serde_json::to_string(&checkpoint).unwrap();
        assert!(json.contains("\"kind\":\"checkpoint\""));
        assert!(!json.contains("spec_markdown"));
        assert_eq!(
            serde_json::from_str::<TaskEvent>(&json).unwrap(),
            checkpoint
        );

        let stopped = TaskEvent::Scout(ScoutEvent::StoppedEarly {
            reason: "agent exited 1 with an unfinished SPEC.md".into(),
            notes_markdown: "# Notes".into(),
            files_touched: vec!["src/lib.rs".into()],
            class: FailureClass::Transport,
        });
        let json = serde_json::to_string(&stopped).unwrap();
        assert!(json.contains("\"kind\":\"stopped_early\""));
        assert!(!json.contains("spec_markdown"));
        assert_eq!(serde_json::from_str::<TaskEvent>(&json).unwrap(), stopped);
    }

    /// Skew in the direction that happens every time a supervisor change is
    /// made: the image still runs the *old* binary, which knows nothing about
    /// `class`. The field defaults, and the default is what the host did
    /// before the field existed — charge the attempt.
    #[test]
    fn an_older_supervisor_omitting_the_class_still_reads_as_a_verdict() {
        let old_failed = r#"{"role":"scout","kind":"failed","reason":"SPEC.md not found"}"#;
        assert_eq!(
            serde_json::from_str::<TaskEvent>(old_failed).unwrap(),
            TaskEvent::Scout(ScoutEvent::Failed {
                reason: "SPEC.md not found".into(),
                class: FailureClass::Verdict,
            })
        );

        let old_stopped = r##"{"role":"scout","kind":"stopped_early","reason":"no spec",
                              "notes_markdown":"# Notes","files_touched":[]}"##;
        assert!(matches!(
            serde_json::from_str::<TaskEvent>(old_stopped).unwrap(),
            TaskEvent::Scout(ScoutEvent::StoppedEarly {
                class: FailureClass::Verdict,
                ..
            })
        ));

        let old_build = r#"{"role":"build","kind":"failed","reason":"agent produced no commits"}"#;
        assert_eq!(
            serde_json::from_str::<TaskEvent>(old_build).unwrap(),
            TaskEvent::Build(BuildEvent::Failed {
                reason: "agent produced no commits".into(),
                class: FailureClass::Verdict,
            })
        );
    }

    /// One statement of what a vm-pool refusal costs, so the two dispatchers
    /// cannot disagree. `Capacity` is the only waiver — it is the kind that
    /// clears by itself — and the negative half is every other kind still
    /// charging, `Unspecified` above all: that is what an old daemon says, and
    /// a waiver there would silently spare every permanent misconfiguration.
    #[test]
    fn only_a_full_pool_is_waived_and_every_other_refusal_is_charged() {
        assert_eq!(
            FailureClass::for_service_error(ServiceErrorKind::Capacity),
            FailureClass::Transport
        );
        assert!(!FailureClass::for_service_error(ServiceErrorKind::Capacity).is_verdict());

        for kind in [
            ServiceErrorKind::Unspecified,
            ServiceErrorKind::NoSuchVm,
            ServiceErrorKind::NotReady,
            ServiceErrorKind::Image,
            ServiceErrorKind::Runtime,
            ServiceErrorKind::Transport,
            ServiceErrorKind::BadRequest,
            ServiceErrorKind::Other,
        ] {
            assert_eq!(
                FailureClass::for_service_error(kind),
                FailureClass::Verdict,
                "{kind} must cost an attempt"
            );
        }
    }

    /// The other direction, and the one that has to be hand-written: a
    /// *newer* supervisor naming a class this binary has never heard of. The
    /// event must still decode — a lost terminal event does not cost a
    /// strike, it costs the run its outcome and hangs it until the deadline —
    /// and it must decay to `Verdict`, never to a silent waive.
    #[test]
    fn an_unknown_class_decays_to_a_verdict_rather_than_failing_the_event() {
        for raw in [
            r#""quantum_flux""#,
            r#""VERDICT""#,
            r#""""#,
            "17",
            "null",
            "true",
        ] {
            let json =
                format!(r#"{{"role":"scout","kind":"failed","reason":"whatever","class":{raw}}}"#);
            assert_eq!(
                serde_json::from_str::<TaskEvent>(&json).unwrap(),
                TaskEvent::Scout(ScoutEvent::Failed {
                    reason: "whatever".into(),
                    class: FailureClass::Verdict,
                }),
                "class: {raw}"
            );
        }

        // And the classes that *are* known still round-trip by name.
        for class in [
            FailureClass::Verdict,
            FailureClass::Transport,
            FailureClass::Cancelled,
            FailureClass::Orphaned,
        ] {
            let evt = TaskEvent::Build(BuildEvent::Failed {
                reason: "r".into(),
                class,
            });
            let json = serde_json::to_string(&evt).unwrap();
            assert!(
                json.contains(&format!("\"class\":\"{}\"", class.as_str())),
                "{json}"
            );
            assert_eq!(serde_json::from_str::<TaskEvent>(&json).unwrap(), evt);
        }
    }

    /// Only a verdict is charged, and every waiver says why in words a note
    /// can carry.
    #[test]
    fn only_a_verdict_is_charged() {
        assert!(FailureClass::Verdict.is_verdict());
        assert_eq!(FailureClass::Verdict.waiver_reason(), None);
        for class in [
            FailureClass::Transport,
            FailureClass::Cancelled,
            FailureClass::Orphaned,
        ] {
            assert!(!class.is_verdict(), "{class}");
            assert!(class.waiver_reason().is_some(), "{class}");
        }
        assert_eq!(FailureClass::default(), FailureClass::Verdict);
    }

    #[test]
    fn service_command_composes() {
        let cmd: ServiceCommand<TasksProtocol> = ServiceCommand::Send {
            vm_id: VmId::new("vm-abc"),
            command: start_cmd(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: ServiceCommand<TasksProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cmd);
    }

    /// The skew this field has to survive is the one it exists to report:
    /// images are rebuilt by hand, so a `Started` from an image built before
    /// this field existed reaches a newer host and **must decode**. Without
    /// `serde(default)` that is not a failing test, it is a broken pipeline.
    #[test]
    fn a_started_from_a_pre_stamping_image_still_decodes() {
        let scout: ScoutEvent =
            serde_json::from_str(r#"{"kind":"started","branch":"scout/42-uuid"}"#).unwrap();
        assert_eq!(
            scout,
            ScoutEvent::Started {
                branch: "scout/42-uuid".into(),
                supervisor: None,
            }
        );

        let build: BuildEvent =
            serde_json::from_str(r#"{"kind":"started","base_sha":"abc123"}"#).unwrap();
        assert_eq!(
            build,
            BuildEvent::Started {
                base_sha: "abc123".into(),
                supervisor: None,
            }
        );
    }

    #[test]
    fn a_stamped_started_round_trips() {
        let evt = TaskEvent::Build(BuildEvent::Started {
            base_sha: "abc123".into(),
            supervisor: Some(SupervisorBuild {
                version: "0.1.163".into(),
                commit: "def5678".into(),
            }),
        });
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"version\":\"0.1.163\""), "{json}");
        assert_eq!(serde_json::from_str::<TaskEvent>(&json).unwrap(), evt);
    }

    #[test]
    fn service_event_composes() {
        let evt: ServiceEvent<TasksProtocol> = ServiceEvent::VmApp {
            vm_id: VmId::new("vm-abc"),
            event: TaskEvent::Scout(ScoutEvent::Started {
                branch: "scout/42-uuid".into(),
                supervisor: None,
            }),
            seq: 3,
        };
        let json = serde_json::to_string(&evt).unwrap();
        let back: ServiceEvent<TasksProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, evt);
    }

    #[test]
    fn vm_command_wraps_task_command() {
        let wrapped: VmCommand<TasksProtocol> = VmCommand::App {
            payload: start_cmd(),
        };
        let json = serde_json::to_string(&wrapped).unwrap();
        let back: VmCommand<TasksProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, wrapped);
    }

    #[test]
    fn vm_event_wraps_task_event() {
        let wrapped: VmEvent<TasksProtocol> = VmEvent::App {
            payload: TaskEvent::Scout(ScoutEvent::Failed {
                reason: "clone timed out".into(),
                class: FailureClass::Verdict,
            }),
        };
        let json = serde_json::to_string(&wrapped).unwrap();
        let back: VmEvent<TasksProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, wrapped);
    }
}
