//! vm-pool integration: TasksProtocol defines the command/event vocabulary
//! flowing between the tasks server and a Scout VM's supervisor.
//!
//! Infrastructure-level traffic (Ping/Pong/Shutdown/Ready) is handled by vm-pool
//! itself. TasksProtocol only carries the application-level Scout messages.

use serde::{Deserialize, Serialize};
use vm_pool_protocol::AppProtocol;

/// The application protocol tasks uses with vm-pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TasksProtocol;

impl AppProtocol for TasksProtocol {
    type Command = ScoutCommand;
    type Event = ScoutEvent;
}

/// Commands the tasks server sends to a Scout VM.
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
    /// Abort in-flight exploration. The supervisor should exit cleanly; the
    /// host will deallocate the VM.
    Cancel,
}

/// Events a Scout VM streams back to the tasks server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScoutEvent {
    /// Supervisor has received `Start` and is setting up (clone, branch).
    Started { branch: String },
    /// A stdout/stderr line from Claude Code. Best-effort — may be dropped
    /// under load. Useful for breadcrumbs / live log tailing.
    Progress { stream: LogStream, line: String },
    /// Claude Code finished. Implementation branch state at this point is
    /// whatever the agent produced.
    ImplementationFinished { exit_code: i32 },
    /// The supervisor has distilled the SPEC.md written by Claude Code.
    /// Terminal success.
    Completed {
        spec_markdown: String,
        files_touched: Vec<String>,
    },
    /// Terminal failure. `reason` is a short diagnostic; the supervisor has
    /// already exited or is about to.
    Failed { reason: String },
}

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

    #[test]
    fn scout_command_roundtrip() {
        let cmd = ScoutCommand::Start {
            task_id: "task_abc".into(),
            repo_clone_url: "https://github.com/o/r.git".into(),
            base_branch: "main".into(),
            prompt: "## Issue\nFix the thing.".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"kind\":\"start\""));
        let back: ScoutCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn scout_event_roundtrip() {
        let evt = ScoutEvent::Completed {
            spec_markdown: "## Spec\nstuff".into(),
            files_touched: vec!["src/lib.rs".into()],
        };
        let json = serde_json::to_string(&evt).unwrap();
        let back: ScoutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, evt);
    }

    #[test]
    fn service_command_with_tasks_protocol_composes() {
        // Verify that tasks' ScoutCommand plugs cleanly into vm-pool's
        // generic ServiceCommand<P> without extra glue.
        let cmd: ServiceCommand<TasksProtocol> = ServiceCommand::Send {
            vm_id: VmId::new("vm-abc"),
            command: ScoutCommand::Cancel,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: ServiceCommand<TasksProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn service_event_with_tasks_protocol_composes() {
        let evt: ServiceEvent<TasksProtocol> = ServiceEvent::VmApp {
            vm_id: VmId::new("vm-abc"),
            event: ScoutEvent::Started {
                branch: "scout/42-uuid".into(),
            },
        };
        let json = serde_json::to_string(&evt).unwrap();
        let back: ServiceEvent<TasksProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, evt);
    }

    #[test]
    fn vm_command_wraps_scout_command() {
        let wrapped: VmCommand<TasksProtocol> = VmCommand::App {
            payload: ScoutCommand::Cancel,
        };
        let json = serde_json::to_string(&wrapped).unwrap();
        let back: VmCommand<TasksProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, wrapped);
    }

    #[test]
    fn vm_event_wraps_scout_event() {
        let wrapped: VmEvent<TasksProtocol> = VmEvent::App {
            payload: ScoutEvent::Failed {
                reason: "clone timed out".into(),
            },
        };
        let json = serde_json::to_string(&wrapped).unwrap();
        let back: VmEvent<TasksProtocol> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, wrapped);
    }
}
