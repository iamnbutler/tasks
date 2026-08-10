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
    /// Agent process finished. Implementation branch state at this point is
    /// whatever the agent produced.
    ImplementationFinished { exit_code: i32 },
    /// The supervisor has read SPEC.md produced by the agent. Terminal success.
    Completed {
        spec_markdown: String,
        files_touched: Vec<String>,
    },
    /// Terminal failure. `reason` is a short diagnostic; the supervisor has
    /// already exited or is about to.
    Failed { reason: String },
}

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
