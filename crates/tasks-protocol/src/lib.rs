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

use serde::{Deserialize, Serialize};
use vm_pool_protocol::AppProtocol;

pub mod agent_run;
pub mod vm_memory;

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
    },
}

/// Events a Scout VM streams back to the tasks server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScoutEvent {
    /// Supervisor has received `Start` and is setting up (clone, branch).
    Started { branch: String },
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
    },
    /// Terminal failure with *nothing to salvage*; `reason` is a short
    /// diagnostic and the supervisor has already exited or is about to.
    ///
    /// Distinct from [`ScoutEvent::StoppedEarly`] on purpose: "we salvaged
    /// something" and "there was nothing" are different facts, and a retry
    /// that cannot tell them apart re-derives what it already had.
    Failed { reason: String },
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
    },
}

/// Events a Builder VM streams back to the tasks server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BuildEvent {
    /// Supervisor cloned and branched; `base_sha` is the commit the branch
    /// grew from (and the thin bundle's prerequisite).
    Started { base_sha: String },
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
    },
    /// Terminal failure. An empty branch (`head == base`) lands here — the
    /// Builder analogue of a Scout's missing SPEC.md.
    Failed { reason: String },
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
        });
        let json = serde_json::to_string(&stopped).unwrap();
        assert!(json.contains("\"kind\":\"stopped_early\""));
        assert!(!json.contains("spec_markdown"));
        assert_eq!(serde_json::from_str::<TaskEvent>(&json).unwrap(), stopped);
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

    #[test]
    fn service_event_composes() {
        let evt: ServiceEvent<TasksProtocol> = ServiceEvent::VmApp {
            vm_id: VmId::new("vm-abc"),
            event: TaskEvent::Scout(ScoutEvent::Started {
                branch: "scout/42-uuid".into(),
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
            }),
        };
        let json = serde_json::to_string(&wrapped).unwrap();
        let back: VmEvent<TasksProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, wrapped);
    }
}
