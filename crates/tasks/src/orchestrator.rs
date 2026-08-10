//! Orchestrator: a persistent, server-owned Claude Code conversation.
//!
//! Per the load-bearing rule, this is not an agentic loop of our own — every
//! tick shells out to headless Claude Code and *resumes one long-lived CC
//! session*, so the orchestrator accumulates context across turns the same
//! way an interactive session would. Our side owns only the chat projection
//! (`orchestrator_messages`) and the session id.
//!
//! The tick condition is DB-derived ([`Store::unanswered_orchestrator_messages`]):
//! the loop answers whatever user turns arrived since the last assistant
//! turn, so a crash mid-reply just means the next pass answers again.
//!
//! The orchestrator acts through the tasks HTTP API (curl against
//! `127.0.0.1:<port>`), which keeps every write inside the server's rules —
//! notably "GitHub writes go through the server". Its child environment has
//! `GITHUB_TOKEN` explicitly removed: the API is its only pair of hands.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};
use uuid::Uuid;

use crate::models::ChatRole;
use crate::store::{Store, StoreError};

#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("spawn agent: {0}")]
    Spawn(std::io::Error),
    #[error("agent exited with {status}: {stderr}")]
    AgentFailed {
        status: std::process::ExitStatus,
        stderr: String,
    },
    #[error("agent timed out after {secs}s")]
    Timeout { secs: u64 },
}

#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    /// The agent command, space-separated (`ORCHESTRATOR_CMD`). Session and
    /// prompt flags are appended per tick. Tests point this at a stub.
    pub command: String,
    /// Wall-clock budget for one tick (`ORCHESTRATOR_TIMEOUT_SECS`).
    pub timeout: Duration,
    /// Working directory for the agent process — somewhere neutral under the
    /// data dir, not a checkout it could edit.
    pub workdir: PathBuf,
    /// Port the tasks API listens on; spliced into the system prompt.
    pub api_port: u16,
}

pub struct Orchestrator {
    store: Arc<Store>,
    config: OrchestratorConfig,
}

impl Orchestrator {
    pub fn new(store: Arc<Store>, config: OrchestratorConfig) -> Self {
        Self { store, config }
    }

    /// Answer the pending user turns, if any. Returns whether a reply was
    /// produced. One reply covers every unanswered turn — they are joined
    /// into one prompt, which is also what makes the tick idempotent.
    pub async fn tick(&self) -> Result<bool, OrchestratorError> {
        let pending = self.store.unanswered_orchestrator_messages().await?;
        if pending.is_empty() {
            return Ok(false);
        }
        let prompt = pending
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        info!(turns = pending.len(), "orchestrator tick");

        let reply = match self.run_agent(&prompt).await {
            Ok(reply) => reply,
            Err(e) => {
                // The error becomes the assistant turn: the chat must never
                // dangle silently, and persisting it also settles the tick
                // condition so the loop doesn't retry a poison prompt forever.
                warn!(error = %e, "orchestrator agent failed");
                format!("(orchestrator error: {e})")
            }
        };
        let trimmed = reply.trim();
        let content = if trimmed.is_empty() {
            "(the orchestrator returned nothing)"
        } else {
            trimmed
        };
        self.store
            .append_orchestrator_message(ChatRole::Assistant, content)
            .await?;
        Ok(true)
    }

    /// Run one headless Claude Code turn against the persistent session,
    /// creating the session on first use and healing a lost one by starting
    /// over with a fresh id (context is lost, the chat projection is not).
    async fn run_agent(&self, prompt: &str) -> Result<String, OrchestratorError> {
        match self.store.orchestrator_cc_session().await? {
            None => self.run_fresh(prompt).await,
            Some(session) => match self.invoke(&["--resume", &session], prompt).await {
                Ok(reply) => Ok(reply),
                Err(e @ OrchestratorError::AgentFailed { .. }) => {
                    warn!(error = %e, "resume failed; starting a fresh orchestrator session");
                    self.run_fresh(prompt).await
                }
                Err(e) => Err(e),
            },
        }
    }

    async fn run_fresh(&self, prompt: &str) -> Result<String, OrchestratorError> {
        let session = Uuid::new_v4().to_string();
        let system = system_prompt(self.config.api_port);
        let reply = self
            .invoke(
                &["--session-id", &session, "--append-system-prompt", &system],
                prompt,
            )
            .await?;
        self.store
            .set_orchestrator_cc_session(Some(&session))
            .await?;
        Ok(reply)
    }

    async fn invoke(&self, extra_args: &[&str], prompt: &str) -> Result<String, OrchestratorError> {
        let mut parts = self.config.command.split_whitespace();
        let prog = parts.next().unwrap_or("claude").to_string();
        let base_args: Vec<String> = parts.map(str::to_string).collect();

        tokio::fs::create_dir_all(&self.config.workdir)
            .await
            .map_err(OrchestratorError::Spawn)?;

        let mut cmd = tokio::process::Command::new(&prog);
        cmd.args(&base_args)
            .args(extra_args)
            .current_dir(&self.config.workdir)
            // The API is the orchestrator's only pair of hands. No direct
            // GitHub writes, so no token.
            .env_remove("GITHUB_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(OrchestratorError::Spawn)?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let prompt_owned = prompt.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt_owned.as_bytes()).await;
            drop(stdin);
        });

        let secs = self.config.timeout.as_secs();
        let output = tokio::time::timeout(self.config.timeout, child.wait_with_output())
            .await
            .map_err(|_| OrchestratorError::Timeout { secs })?
            .map_err(OrchestratorError::Spawn)?;

        if !output.status.success() {
            return Err(OrchestratorError::AgentFailed {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(2000)
                    .collect(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// The orchestrator's standing instructions. Appended (not replacing) so
/// Claude Code's own tool discipline stays intact.
fn system_prompt(port: u16) -> String {
    format!(
        "You are the Orchestrator for a Tasks server — a human-in-the-loop \
         platform that turns GitHub issues into specs (via Scout agents) and \
         approved specs into PRs (via Builder agents).\n\n\
         You are a persistent conversation the human returns to. Answer \
         questions about pipeline state, and take pipeline actions when — and \
         only when — the human asks for them. Never invent work for yourself.\n\n\
         Your only tool for acting is the tasks HTTP API at \
         http://127.0.0.1:{port} (use curl). Endpoints:\n\
         - GET /tasks (working set; ?all=true for history), GET /tasks/{{id}}\n\
         - POST /tasks/{{id}}/queue | /dequeue | /scout — queue membership\n\
         - GET /sessions, GET /sessions/{{id}}/transcript?since=N — scout runs\n\
         - GET /specs/{{id}}, GET /spec-queue — specs and their review state\n\
         - POST /spec-queue/{{id}}/review {{\"status\":\"approved|needs_revision|rejected\",\"feedback\"}} \
           — ONLY when the human has explicitly given a verdict\n\
         - POST /builds {{\"spec_ids\":[...]}} — batch approved specs into one \
           Builder run (serial; one at a time)\n\
         - GET /builds, GET /builds/{{id}} — build state, PR number\n\
         - GET /events?since=N — the activity log, newest last; your best \
           source for \"what happened\"\n\
         - GET /mode, POST /mode {{\"mode\":\"play|pause|stop\"}} — play runs \
           scouts+builds, pause only polls, stop is everything off\n\n\
         Rules:\n\
         - States: backlog → queued → scouting → in_review → ready_to_build → \
           building → done (rejected = terminal). Issue closure on GitHub \
           retires work automatically; there is no manual mark-done.\n\
         - Never touch GitHub directly — the server is the only thing that \
           writes there. You have no credentials for it anyway.\n\
         - Reviews are the human's: only submit a verdict they explicitly \
           stated, and quote their feedback verbatim.\n\
         - Be concise. Plain sentences, not markdown headers. When you took \
           actions, say exactly which calls you made.\n\
         - When asked for status, lead with what needs the human's attention: \
           specs awaiting review, failed builds, then everything else."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_prompt_carries_the_port_and_the_guardrails() {
        let p = system_prompt(4800);
        assert!(p.contains("http://127.0.0.1:4800"));
        assert!(p.contains("Never touch GitHub directly"));
        assert!(p.contains("only when — the human asks"));
    }
}
