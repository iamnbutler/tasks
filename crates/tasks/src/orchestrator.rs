//! Orchestrator: a persistent, server-owned Claude Code conversation.
//!
//! Per the load-bearing rule, this is not an agentic loop of our own — every
//! tick shells out to headless Claude Code and *resumes one long-lived CC
//! session*, so the orchestrator accumulates context across turns the same
//! way an interactive session would. Our side owns the chat projection
//! (`orchestrator_messages`), the session id, and the session *ledger*
//! (`orchestrator_sessions`) — one row per session it has lived in, with the
//! context size each reached. The ledger exists because that accumulated
//! context is the point of a long-lived conversation, and losing it used to
//! be invisible: the chat reads as continuous across a boundary the agent
//! itself does not survive.
//!
//! The tick condition is DB-derived ([`Store::unanswered_orchestrator_messages`]):
//! the loop answers whatever user turns arrived since the last assistant
//! turn, so a crash mid-reply just means the next pass answers again.
//!
//! What the orchestrator may *do* is decided by configuration, not code:
//! `ORCHESTRATOR_CMD` carries Claude Code's permission flags and
//! `ORCHESTRATOR_WORKDIR` its working directory. Pointed at the real repo
//! checkout with `--dangerously-skip-permissions`, it is a full development
//! agent; with the defaults it is an API-only controller. Either way,
//! *pipeline* writes go through the tasks HTTP API (curl against
//! `127.0.0.1:<port>`) so state changes stay inside the server's rules, and
//! the server's own `GITHUB_TOKEN` is stripped from the child env — when the
//! agent talks to GitHub it authenticates as itself (gh's keychain auth),
//! not with the server's credential.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::{info, warn};
use uuid::Uuid;

use crate::events::{Event, EventPayload};
use crate::models::{
    Actor, BuildStatus, Obligation, OrchestratorFeedEvent, SessionEndReason, SessionStatus,
    SpecQueueStatus,
};
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

    /// Answer the pending input turns (user + event), if any. Returns whether
    /// a reply was produced. One reply covers every unanswered turn — they
    /// are joined into one prompt, which is also what makes the tick
    /// idempotent.
    pub async fn tick(&self) -> Result<bool, OrchestratorError> {
        let pending = self.store.unanswered_orchestrator_messages().await?;
        if pending.is_empty() {
            return Ok(false);
        }
        let answered_through = pending.last().map(|m| m.seq).unwrap_or(0);
        let prompt = pending
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        info!(turns = pending.len(), "orchestrator tick");

        let (reply, session_id) = match self.run_agent(&prompt).await {
            Ok(turn) => {
                info!(
                    session = %turn.session_id,
                    context_tokens = ?turn.context_tokens,
                    "orchestrator turn complete"
                );
                (turn.text, Some(turn.session_id))
            }
            Err(e) => {
                // The error becomes the assistant turn: the chat must never
                // dangle silently, and persisting it also settles the tick
                // condition so the loop doesn't retry a poison prompt forever.
                warn!(error = %e, "orchestrator agent failed");
                (format!("(orchestrator error: {e})"), None)
            }
        };
        let trimmed = reply.trim();
        let content = if trimmed.is_empty() {
            "(the orchestrator returned nothing)"
        } else {
            trimmed
        };
        self.store
            .append_orchestrator_reply(content, answered_through, session_id.as_deref())
            .await?;
        // The durable message exists now — tell live-feed subscribers the
        // in-flight view is over (after the append, so a client reacting to
        // `done` finds the message already fetchable).
        self.store
            .publish_orchestrator_feed(OrchestratorFeedEvent::Done);
        Ok(true)
    }

    /// Run one headless Claude Code turn against the persistent session,
    /// creating the session on first use and healing a lost one by starting
    /// over with a fresh id.
    ///
    /// Healing is not free and must not be silent: the accumulated context is
    /// what makes the orchestrator worth having, so a failed resume closes the
    /// old session in the ledger and writes a seam into the conversation
    /// before the replacement takes its first turn. The chat projection
    /// survives either way — it is only the agent's memory of it that doesn't.
    ///
    /// The standing prompt rides along on every turn — resume included — so
    /// prompt updates reach a long-lived session without resetting it, and a
    /// fresh session is re-armed with its instructions on turn one.
    async fn run_agent(&self, prompt: &str) -> Result<Turn, OrchestratorError> {
        let system = system_prompt(self.config.api_port);
        match self.store.orchestrator_cc_session().await? {
            None => self.start_session(&system, prompt, None).await,
            Some(session) => match self
                .invoke(
                    &["--resume", &session, "--append-system-prompt", &system],
                    prompt,
                )
                .await
            {
                Ok((text, context_tokens)) => {
                    if let Some(tokens) = context_tokens {
                        self.store
                            .record_orchestrator_context_tokens(&session, tokens)
                            .await?;
                    }
                    Ok(Turn {
                        text,
                        context_tokens,
                        session_id: session,
                    })
                }
                Err(e @ OrchestratorError::AgentFailed { .. }) => {
                    warn!(error = %e, "resume failed; starting a fresh orchestrator session");
                    // The context is gone the moment resume fails, so the
                    // seam is recorded here rather than after a replacement
                    // succeeds — a fresh start that *also* fails must not
                    // erase the fact that memory was lost.
                    self.store
                        .end_orchestrator_session(&session, SessionEndReason::ResumeFailed)
                        .await?;
                    self.start_session(&system, prompt, Some(&session)).await
                }
                Err(e) => Err(e),
            },
        }
    }

    /// Take the first turn in a brand-new Claude Code session, adopting it as
    /// the live one only once that turn succeeds. `replacing` is `Some` when
    /// this is a seam rather than a first start.
    async fn start_session(
        &self,
        system: &str,
        prompt: &str,
        replacing: Option<&str>,
    ) -> Result<Turn, OrchestratorError> {
        let session = Uuid::new_v4().to_string();
        let (text, context_tokens) = self
            .invoke(
                &["--session-id", &session, "--append-system-prompt", system],
                prompt,
            )
            .await?;
        self.store
            .begin_orchestrator_session(
                &session,
                replacing,
                replacing.map(|_| SessionEndReason::ResumeFailed),
            )
            .await?;
        if let Some(tokens) = context_tokens {
            self.store
                .record_orchestrator_context_tokens(&session, tokens)
                .await?;
        }
        Ok(Turn {
            text,
            context_tokens,
            session_id: session,
        })
    }

    /// Run the agent, streaming its stdout as it arrives. stream-json lines
    /// become live-feed events (text deltas, tool-call labels) and the
    /// `result` record's text becomes the reply; anything that isn't
    /// stream-json is collected raw and returned whole, so plain-text agents
    /// (and test stubs) keep working — they just don't stream.
    async fn invoke(
        &self,
        extra_args: &[&str],
        prompt: &str,
    ) -> Result<(String, Option<i64>), OrchestratorError> {
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
            // The server's token stays the server's. When the agent talks to
            // GitHub it authenticates as itself (gh keychain auth).
            .env_remove("GITHUB_TOKEN")
            // How the agent proves its writes are its own. Passed through the
            // environment rather than argv (world-readable via `ps`) or the
            // prompt (persisted), and referenced by name in the standing
            // instructions so the value itself never lands in the transcript.
            .env("TASKS_ACTOR_TOKEN", self.store.actor_token())
            // A timeout drops the read future below, which drops the child —
            // this makes that drop kill the process instead of leaking it.
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(OrchestratorError::Spawn)?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let prompt_owned = prompt.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt_owned.as_bytes()).await;
            drop(stdin);
        });
        // Drain stderr concurrently so a chatty agent can't fill the pipe
        // and deadlock against our stdout read.
        let stderr_task = tokio::spawn(async move {
            let mut buf = String::new();
            let mut stderr = stderr;
            let _ = stderr.read_to_string(&mut buf).await;
            buf
        });

        let read = async {
            let mut lines = BufReader::new(stdout).lines();
            let mut raw = String::new();
            let mut result_text: Option<String> = None;
            let mut context_tokens: Option<i64> = None;
            while let Some(line) = lines.next_line().await.map_err(OrchestratorError::Spawn)? {
                match parse_stream_line(&line) {
                    StreamLine::Delta(text) => self
                        .store
                        .publish_orchestrator_feed(OrchestratorFeedEvent::Delta { text }),
                    StreamLine::Tools(labels) => {
                        for label in labels {
                            self.store
                                .publish_orchestrator_feed(OrchestratorFeedEvent::Tool { label });
                        }
                    }
                    StreamLine::Result { text, tokens } => {
                        result_text = Some(text);
                        context_tokens = tokens;
                    }
                    StreamLine::Other => {}
                    StreamLine::NotStreamJson => {
                        raw.push_str(&line);
                        raw.push('\n');
                    }
                }
            }
            let status = child.wait().await.map_err(OrchestratorError::Spawn)?;
            Ok::<_, OrchestratorError>((status, result_text, raw, context_tokens))
        };

        let secs = self.config.timeout.as_secs();
        let (status, result_text, raw, context_tokens) =
            tokio::time::timeout(self.config.timeout, read)
                .await
                .map_err(|_| OrchestratorError::Timeout { secs })??;

        if !status.success() {
            let stderr = stderr_task.await.unwrap_or_default();
            return Err(OrchestratorError::AgentFailed {
                status,
                stderr: stderr.chars().take(2000).collect(),
            });
        }
        Ok((result_text.unwrap_or(raw), context_tokens))
    }
}

/// One completed agent turn.
struct Turn {
    text: String,
    /// Absolute context size reported by the agent, when it reports one.
    context_tokens: Option<i64>,
    /// The Claude Code session this turn ran in, stamped onto the durable
    /// reply so a verdict can be traced back to the memory regime that
    /// produced it.
    session_id: String,
}

/// What one line of agent stdout means for the live feed.
enum StreamLine {
    /// Assistant text as it's generated (`--include-partial-messages`).
    Delta(String),
    /// Tool invocations from a completed assistant turn.
    Tools(Vec<String>),
    /// The final `result` record: the reply text, and the context size that
    /// produced it when the agent reports usage.
    Result { text: String, tokens: Option<i64> },
    /// A stream-json record with nothing for us (init, thinking, tool results).
    Other,
    /// Not stream-json at all — a plain-text agent's output.
    NotStreamJson,
}

fn parse_stream_line(line: &str) -> StreamLine {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return StreamLine::NotStreamJson;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("stream_event") => match v.pointer("/event/delta/text").and_then(|t| t.as_str()) {
            Some(text) => StreamLine::Delta(text.to_string()),
            None => StreamLine::Other,
        },
        Some("assistant") => {
            let labels: Vec<String> = v
                .pointer("/message/content")
                .and_then(|c| c.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                        .map(tool_label)
                        .collect()
                })
                .unwrap_or_default();
            if labels.is_empty() {
                StreamLine::Other
            } else {
                StreamLine::Tools(labels)
            }
        }
        Some("result") => match v.get("result").and_then(|r| r.as_str()) {
            Some(text) => StreamLine::Result {
                text: text.to_string(),
                tokens: context_tokens(v.get("usage")),
            },
            None => StreamLine::Other,
        },
        Some(_) => StreamLine::Other,
        // JSON, but not a stream record — treat like plain output.
        None => StreamLine::NotStreamJson,
    }
}

/// Context size from a stream-json `usage` object: every input-side token the
/// model had to read this turn, cached or not.
///
/// The sum is what matters. `input_tokens` alone under-reports by whatever
/// the cache served — which, on a long-lived resumed session, is nearly all
/// of it. And because this is an absolute measurement rather than a running
/// total, it stays honest across turns the server never drove (an interactive
/// checkout) without any reconciliation.
fn context_tokens(usage: Option<&serde_json::Value>) -> Option<i64> {
    let usage = usage?;
    let field = |key: &str| usage.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
    let total = field("input_tokens")
        + field("cache_read_input_tokens")
        + field("cache_creation_input_tokens");
    (total > 0).then_some(total)
}

/// One-line human label for a tool call, e.g. `Bash: curl -s .../tasks`.
fn tool_label(item: &serde_json::Value) -> String {
    let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
    let detail = item
        .pointer("/input/command")
        .and_then(|c| c.as_str())
        .or_else(|| item.pointer("/input/description").and_then(|d| d.as_str()))
        .unwrap_or("");
    let label = if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {detail}")
    };
    let one_line = label.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 120 {
        format!("{}…", one_line.chars().take(119).collect::<String>())
    } else {
        one_line
    }
}

/// Which pipeline events deserve a nudge into the orchestrator conversation.
///
/// Selective on purpose — every nudge costs an agent turn. Chosen: the
/// moments a human coworker would want flagged (work arriving, specs landing,
/// verdicts, builds concluding). Excluded: the orchestrator's own messages
/// (feedback loop), derivative state transitions, and dispatch-level noise
/// that the Activity feed already carries.
pub fn nudge_worthy(payload: &EventPayload) -> bool {
    match payload {
        EventPayload::TaskIngested { .. }
        | EventPayload::SpecCreated { .. }
        | EventPayload::BuildCompleted { .. }
        | EventPayload::PullRequestOpened { .. }
        | EventPayload::ModeChanged { .. } => true,
        // Success is conveyed by the SpecCreated that accompanies it.
        EventPayload::SessionCompleted { status, .. } => *status == SessionStatus::ScoutFailed,
        // Review verdicts — but never the orchestrator's own. Being told
        // what you just did is not news: it costs a turn to acknowledge, and
        // worse, invites second-guessing a verdict already rendered. This is
        // the one filter that gets more load-bearing as autonomy grows, since
        // every autonomous verdict would otherwise echo straight back.
        // PendingReview duplicates SpecCreated; Built duplicates
        // BuildCompleted; Blocked carries no actor (the attempt cap decided)
        // and is exactly when someone should hear about it.
        EventPayload::SpecQueueStatusChanged { to, actor, .. } => {
            *actor != Some(Actor::Orchestrator)
                && matches!(
                    to,
                    SpecQueueStatus::Approved
                        | SpecQueueStatus::NeedsRevision
                        | SpecQueueStatus::Rejected
                        | SpecQueueStatus::Blocked
                )
        }
        _ => false,
    }
}

/// Render a batch of nudge-worthy events as one `event` turn. Events are
/// identifier-only, so detail comes from store lookups at format time; a row
/// that has since vanished degrades to its id rather than failing the nudge.
pub async fn format_nudge(store: &Store, events: &[Event]) -> String {
    let mut lines = Vec::new();
    let mut ingested = 0usize;
    for event in events {
        match &event.payload {
            EventPayload::TaskIngested { task_id, .. } => {
                ingested += 1;
                lines.push(format!("- New task: {}", task_ref(store, task_id).await));
            }
            EventPayload::SpecCreated {
                spec_id, task_id, ..
            } => lines.push(format!(
                "- Spec landed for review: {} ({spec_id})",
                task_ref(store, task_id).await
            )),
            EventPayload::SessionCompleted {
                session_id,
                task_id,
                ..
            } => {
                let reason = match store.get_session(session_id).await {
                    Ok(Some(s)) => s.exit_reason.unwrap_or_else(|| "unknown".into()),
                    _ => "unknown".into(),
                };
                lines.push(format!(
                    "- Scout FAILED for {}: {reason}",
                    task_ref(store, task_id).await
                ));
            }
            EventPayload::SpecQueueStatusChanged { spec_id, to, .. } => {
                let task = match store.get_spec(spec_id).await {
                    Ok(Some(spec)) => task_ref(store, &spec.task_id).await,
                    _ => spec_id.to_string(),
                };
                lines.push(format!("- Review verdict on {task}: {}", to.as_str()));
            }
            EventPayload::BuildCompleted { build_id, status } => {
                let line = match store.get_build(build_id).await {
                    Ok(Some(build)) => match status {
                        BuildStatus::Succeeded => {
                            format!("- Build {build_id} succeeded (branch {})", build.branch)
                        }
                        _ => format!(
                            "- Build {build_id} FAILED: {}",
                            build.exit_reason.unwrap_or_else(|| "unknown".into())
                        ),
                    },
                    _ => format!("- Build {build_id}: {}", status.as_str()),
                };
                lines.push(line);
            }
            EventPayload::PullRequestOpened {
                build_id,
                pr_number,
            } => lines.push(format!("- PR #{pr_number} opened (build {build_id})")),
            EventPayload::ModeChanged { from, to } => lines.push(format!(
                "- Mode changed: {} → {}",
                from.as_str(),
                to.as_str()
            )),
            _ => {}
        }
    }
    if ingested > 1 {
        lines.push(format!("({ingested} tasks ingested in this batch)"));
    }
    format!(
        "[pipeline] Automated notification — not the human. Recent activity:\n{}",
        lines.join("\n")
    )
}

/// Render standing obligations as one `event` turn.
///
/// Worded to be unmistakable from a notification, because the two behave
/// differently and the orchestrator should treat them differently: a nudge is
/// news that happened once, an obligation is work still owed and will keep
/// coming back until it is discharged by an actual decision.
pub fn format_obligations(obligations: &[Obligation]) -> String {
    let lines: Vec<String> = obligations
        .iter()
        .map(|o| format!("- {}", o.summary))
        .collect();
    format!(
        "[pipeline] Standing obligations — not notifications. These are \
         derived from pipeline state and will keep appearing until they are \
         resolved, so act on them rather than acknowledging them:\n{}",
        lines.join("\n")
    )
}

/// `#<issue> "<title>"` for a task, degrading to the raw id if it's gone.
async fn task_ref(store: &Store, task_id: &crate::models::TaskId) -> String {
    match store.get_task(task_id).await {
        Ok(Some(task)) => format!("#{} \"{}\"", task.gh_issue_number, task.title),
        _ => task_id.to_string(),
    }
}

/// The orchestrator's standing instructions. Appended (not replacing) so
/// Claude Code's own tool discipline stays intact, and passed on every turn
/// (resume included) so edits here reach a long-lived session.
fn system_prompt(port: u16) -> String {
    format!(
        "You are the Orchestrator for Tasks — a human-in-the-loop platform \
         that turns GitHub issues into specs (via Scout agents) and approved \
         specs into PRs (via Builder agents). You are a persistent \
         conversation the human returns to, and a proactive teammate: besides \
         the human's messages, you receive automated pipeline notifications \
         (turns starting with \"[pipeline]\"). Treat those as your cue to act \
         on the human's behalf — investigate, summarize, prepare — not just \
         to acknowledge.\n\n\
         Two kinds of automated turn arrive, and they mean different things. \
         A *notification* reports something that happened, once. A *standing \
         obligation* is work the pipeline is still owed, derived from its \
         state — it will keep reappearing until it is actually resolved, so \
         acknowledging one changes nothing. Act on those.\n\n\
         On a [pipeline] turn:\n\
         - Spec landed → read it (GET /specs/{{id}}) and review it \
           ADVERSARIALLY: your value is finding what's wrong, not affirming \
           the work — the scout already believes in it. Hunt for missed \
           requirements, untested claims, wrong layers, scope creep. Then \
           zoom out: does this work fit the larger picture of what's in \
           flight and where the project is going, and is the underlying task \
           worth doing at all? You are the one place \"why are we doing \
           this?\" gets asked — did the agent miss the forest for the trees? \
           Lead with your strongest objection, then say whether you'd \
           approve. The verdict itself stays the human's.\n\
         - Scout or build FAILED → investigate (transcript, build row, \
           events) and report the cause and what you'd do about it.\n\
         - New tasks → note anything urgent or related to in-flight work; \
           otherwise a one-line summary is plenty.\n\
         - PR opened / mode changed / verdicts → keep it to a line unless \
           something needs attention.\n\
         The same adversarial posture applies whenever the human asks you to \
         review a spec, a PR, or an implementation: never be congratulatory — \
         praise is noise, defects and risks are signal. \"Looks solid\" is \
         only worth saying after you tried hard to break it and failed, and \
         then say what you tried.\n\
         Be brief on notifications — a quiet pipeline deserves a quiet \
         channel. Never fabricate activity, and never take the gated actions \
         below without the human.\n\n\
         Your working directory is the project checkout itself. Within your \
         permission settings you may read and edit code, run builds and \
         tests, and use `gh` (e.g. to file issues the human asks for). If a \
         tool call is denied, say what was denied instead of improvising \
         around it.\n\n\
         Pipeline control goes through the tasks HTTP API at \
         http://127.0.0.1:{port} (use curl) — not around it; API writes keep \
         state and the activity log honest.\n\n\
         Identify yourself on every write: pass \
         `-H \"X-Tasks-Actor: orchestrator $TASKS_ACTOR_TOKEN\"` (the token is \
         in your environment — never print it). Writes carrying it are \
         recorded as yours, which is what keeps you from being notified about \
         your own actions, and what makes the decisions ledger worth reading. \
         Every write you make that way must also carry a `rationale` — the \
         server rejects one without it, because a decision nobody can review \
         afterwards is not one you were trusted to make.\n\n\
         Endpoints:\n\
         - GET /tasks (working set; ?all=true for history), GET /tasks/{{id}}\n\
         - POST /tasks/{{id}}/queue | /dequeue | /scout — queue membership\n\
         - GET /sessions, GET /sessions/{{id}}/transcript?since=N — scout runs\n\
         - GET /specs/{{id}}, GET /spec-queue — specs and their review state\n\
         - POST /spec-queue/{{id}}/review \
           {{\"status\":\"approved|needs_revision|rejected\",\"feedback\",\"rationale\"}} \
           — ONLY when the human has explicitly given a verdict\n\
         - POST /builds {{\"spec_ids\":[...],\"rationale\"}} — batch approved \
           specs into one Builder run (serial; one at a time)\n\
         - GET /decisions[?spec=|?build=] — the ledger: who decided what, why, \
           and which turn of this conversation explains it\n\
         - GET /builds, GET /builds/{{id}} — build state, PR number\n\
         - GET /events?since=N — the activity log, newest last; your best \
           source for \"what happened\". Without ?since it returns only the \
           newest 100 — page from since=1 before counting anything. Retired \
           tasks are hidden from GET /tasks but reachable at GET /tasks/{{id}}. \
           Nothing on the wire counts merged PRs — pull_request_opened fires \
           at open; check merge state via gh, or say \"opened\", not \
           \"shipped\"\n\
         - GET /mode, POST /mode {{\"mode\":\"play|pause|stop\"}} — play runs \
           scouts+builds, pause only polls, stop is everything off\n\n\
         Rules:\n\
         - States: backlog → queued → scouting → in_review → ready_to_build → \
           building → done (rejected = terminal). Issue closure on GitHub \
           retires work automatically; there is no manual mark-done.\n\
         - Reviews are the human's: only submit a verdict they explicitly \
           stated, and quote their feedback verbatim.\n\
         - The checkout is shared with the human and other agents. Never \
           switch branches, stash, or discard changes you did not make; do \
           your own work on branches and leave the tree as you found it.\n\
         - Do not restart the tasks server or vm-pool unasked — a restart \
           orphans in-flight builds.\n\
         - Be concise. When you took actions, say exactly which calls or \
           commands you ran.\n\
         - When asked for status, lead with what needs the human's attention: \
           specs awaiting review, failed builds, then everything else."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nudges_are_selective() {
        use crate::models::{BuildId, ChatRole, Mode, SessionId, SpecId, TaskId};
        let task = || TaskId::from_raw("task_1");
        let sess = || SessionId::from_raw("sess_1");
        let spec = || SpecId::from_raw("spec_1");

        // The moments a coworker would flag:
        assert!(nudge_worthy(&EventPayload::SpecCreated {
            spec_id: spec(),
            task_id: task(),
            session_id: sess(),
        }));
        assert!(nudge_worthy(&EventPayload::SessionCompleted {
            session_id: sess(),
            task_id: task(),
            status: SessionStatus::ScoutFailed,
        }));
        assert!(nudge_worthy(&EventPayload::SpecQueueStatusChanged {
            spec_id: spec(),
            from: Some(SpecQueueStatus::PendingReview),
            to: SpecQueueStatus::Approved,
            actor: Some(Actor::Human),
            decision_seq: Some(1),
        }));
        assert!(nudge_worthy(&EventPayload::ModeChanged {
            from: Mode::Play,
            to: Mode::Pause,
        }));
        // Running out of build attempts has no actor — nobody chose it — and
        // is precisely when someone should hear about it.
        assert!(nudge_worthy(&EventPayload::SpecQueueStatusChanged {
            spec_id: spec(),
            from: Some(SpecQueueStatus::Approved),
            to: SpecQueueStatus::Blocked,
            actor: None,
            decision_seq: None,
        }));

        // Feedback loops, duplicates, and noise:
        assert!(!nudge_worthy(&EventPayload::OrchestratorMessage {
            seq: 1,
            role: ChatRole::Assistant,
        }));
        assert!(!nudge_worthy(&EventPayload::SessionCompleted {
            session_id: sess(),
            task_id: task(),
            status: SessionStatus::ScoutSucceeded, // SpecCreated covers it
        }));
        assert!(!nudge_worthy(&EventPayload::SpecQueueStatusChanged {
            spec_id: spec(),
            from: None,
            to: SpecQueueStatus::PendingReview, // SpecCreated covers it
            actor: None,
            decision_seq: None,
        }));
        // The echo: the orchestrator's own verdict coming back at it. Being
        // told what you just did costs a turn and invites second-guessing a
        // decision already made — and under autonomy every verdict would do
        // this.
        assert!(!nudge_worthy(&EventPayload::SpecQueueStatusChanged {
            spec_id: spec(),
            from: Some(SpecQueueStatus::PendingReview),
            to: SpecQueueStatus::Approved,
            actor: Some(Actor::Orchestrator),
            decision_seq: Some(7),
        }));
        assert!(!nudge_worthy(&EventPayload::BuildStarted {
            build_id: BuildId::from_raw("build_1"),
        }));
        assert!(!nudge_worthy(&EventPayload::QueueReordered {
            task_ids: vec![]
        }));
        // Briefings are generated ABOUT pipeline activity — nudging on them
        // would be a feedback loop (nudge → tick → activity → briefing →
        // nudge). Load-bearing exclusion, not an oversight.
        assert!(!nudge_worthy(&EventPayload::BriefingUpdated {
            section: crate::models::BriefingSection::Changes,
        }));
    }

    #[test]
    fn the_system_prompt_carries_the_port_and_the_guardrails() {
        let p = system_prompt(4800);
        assert!(p.contains("http://127.0.0.1:4800"));
        assert!(p.contains("[pipeline]"));
        assert!(p.contains("proactive"));
        assert!(p.contains("ADVERSARIALLY"));
        assert!(p.contains("why are we doing"));
        assert!(p.contains("Never switch branches"));
        assert!(p.contains("verdict they explicitly stated"));
    }
}
