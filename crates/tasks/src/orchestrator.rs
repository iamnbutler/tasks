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

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::{info, warn};
use uuid::Uuid;

use crate::brief::Brief;
use crate::events::{Event, EventPayload};
use crate::models::{
    Actor, BuildStatus, CharterEntry, CharterLevel, Obligation, ObligationKind,
    OrchestratorFeedEvent, SessionEndReason, SessionStatus, SpecQueueStatus,
};
use crate::store::{ACTOR_HEADER, Store, StoreError};

#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("spawn agent: {0}")]
    Spawn(std::io::Error),
    #[error("writing the actor credential to {path}: {source}")]
    ActorConfig {
        path: String,
        source: std::io::Error,
    },
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
    /// Whether [`Self::workdir`] is a repo checkout (`ORCHESTRATOR_WORKDIR`
    /// was set) rather than the neutral dir under the data dir.
    ///
    /// It exists only to keep the system prompt honest, and that is not a
    /// cosmetic concern. The prompt used to assert "your working directory is
    /// the project checkout itself" unconditionally, so a server booted
    /// without `ORCHESTRATOR_WORKDIR` told a curl-only agent it could read
    /// code, edit files and run tests. The agent believed it — spent a turn
    /// reaching for `python3`, `Write` and a heredoc, had all three denied,
    /// and reported the denials as a tooling failure. It was right; the
    /// prompt was lying to it. Anything the prompt claims about the
    /// environment has to be derived from the environment.
    pub workdir_is_checkout: bool,
    /// Whether the server booted with a GitHub credential.
    ///
    /// Same principle as [`Self::workdir_is_checkout`], applied to the other
    /// half of the same incident: without a token every GitHub-backed write
    /// 500s, intake is off, and a build that succeeds cannot open its PR —
    /// while the charter still says the orchestrator may file, close, comment
    /// and merge. Charter authority and server capability are different
    /// facts, and an agent that only learns the difference from a failed call
    /// spends a turn diagnosing infrastructure instead of reporting it.
    pub github_configured: bool,
    /// Port the tasks API listens on; spliced into the system prompt.
    pub api_port: u16,
    /// Where the agent's curl config — its actor credential — is written
    /// before every turn. Under the data dir, never the workdir: in
    /// production the workdir is a repo checkout the agent commits from, and
    /// a secret there is one `git add -A` from being published.
    pub curl_config: PathBuf,
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
        // Before any of the slow work (charter read, prompt build, agent
        // spawn): the whole point is to cover the silence in front of the
        // first token. A no-op tick publishes nothing — a client must never
        // be told a tick began when none did.
        self.store
            .publish_orchestrator_feed(OrchestratorFeedEvent::Started);
        let answered_through = pending.last().map(|m| m.seq).unwrap_or(0);
        let prompt = pending
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        info!(turns = pending.len(), "orchestrator tick");

        // A turn is a local child process: a restart kills it, and unlike a
        // scout or a build there is nothing left to reattach to. The marker
        // makes that loss reportable at the next boot instead of silent. Set
        // best-effort — failing to mark a turn must not stop it happening.
        if let Err(e) = self.store.begin_orchestrator_turn().await {
            warn!(error = %e, "could not mark the orchestrator turn as in flight");
        }
        let (reply, session_id) = match self.run_agent(&prompt).await {
            Ok(turn) => {
                info!(
                    session = %turn.session_id,
                    context_tokens = ?turn.usage.context_tokens,
                    tick_tokens = ?turn.usage.tick_tokens,
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
        // Cleared here, before the reply is persisted: a turn that produced an
        // answer — even an error one — ran to its end and was not interrupted.
        if let Err(e) = self.store.end_orchestrator_turn().await {
            warn!(error = %e, "could not clear the orchestrator turn marker");
        }
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
        // Re-read every turn: the charter is the one statement of what the
        // orchestrator may do, and it reaches a long-lived session only
        // through the prompt. A human flipping a capability takes effect on
        // the next turn, without restarting anything.
        let charter = self.store.charter().await?;
        let system = system_prompt(
            self.config.api_port,
            &charter,
            &self.config.curl_config,
            self.config.workdir_is_checkout,
            self.config.github_configured,
        );
        match self.store.orchestrator_cc_session().await? {
            None => self.start_session(&system, prompt, None).await,
            Some(session) => match self
                .invoke(
                    &["--resume", &session, "--append-system-prompt", &system],
                    prompt,
                )
                .await
            {
                Ok((text, usage)) => {
                    self.store
                        .record_orchestrator_usage(
                            &session,
                            usage.context_tokens,
                            usage.tick_tokens,
                        )
                        .await?;
                    Ok(Turn {
                        text,
                        usage,
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
        let (text, usage) = self
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
        self.store
            .record_orchestrator_usage(&session, usage.context_tokens, usage.tick_tokens)
            .await?;
        Ok(Turn {
            text,
            usage,
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
    ) -> Result<(String, TurnUsage), OrchestratorError> {
        let mut parts = self.config.command.split_whitespace();
        let prog = parts.next().unwrap_or("claude").to_string();
        let base_args: Vec<String> = parts.map(str::to_string).collect();

        tokio::fs::create_dir_all(&self.config.workdir)
            .await
            .map_err(OrchestratorError::Spawn)?;

        // Rewritten before every spawn: the token is minted per boot, so a
        // stale file is useless. An agent that cannot identify itself must
        // not run at all — an unattributed write is recorded as the human's,
        // which is the charter going silently unenforced.
        write_curl_config(&self.config.curl_config, self.store.actor_token())
            .await
            .map_err(|source| OrchestratorError::ActorConfig {
                path: self.config.curl_config.display().to_string(),
                source,
            })?;

        let mut cmd = tokio::process::Command::new(&prog);
        cmd.args(&base_args)
            .args(extra_args)
            .current_dir(&self.config.workdir)
            // The server's token stays the server's. When the agent talks to
            // GitHub it authenticates as itself (gh keychain auth).
            .env_remove("GITHUB_TOKEN")
            // The credential is a file now (`curl -K`), and this is not just
            // "stop setting it": the server's own environment could carry a
            // `TASKS_ACTOR_TOKEN`, and inheriting one would revive the shell
            // expansion that a static allowlist cannot run.
            .env_remove("TASKS_ACTOR_TOKEN")
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
            let mut usage = TurnUsage::default();
            while let Some(line) = lines.next_line().await.map_err(OrchestratorError::Spawn)? {
                match parse_stream_line(&line) {
                    StreamLine::Delta(text) => self
                        .store
                        .publish_orchestrator_feed(OrchestratorFeedEvent::Delta { text }),
                    StreamLine::Assistant {
                        tools,
                        context_tokens,
                    } => {
                        for label in tools {
                            self.store
                                .publish_orchestrator_feed(OrchestratorFeedEvent::Tool { label });
                        }
                        // Last main-chain reading wins: the final assistant
                        // turn is the context the next tick resumes from.
                        if context_tokens.is_some() {
                            usage.context_tokens = context_tokens;
                        }
                    }
                    StreamLine::Result { text, tick_tokens } => {
                        result_text = Some(text);
                        usage.tick_tokens = tick_tokens;
                    }
                    StreamLine::Other => {}
                    StreamLine::NotStreamJson => {
                        raw.push_str(&line);
                        raw.push('\n');
                    }
                }
            }
            let status = child.wait().await.map_err(OrchestratorError::Spawn)?;
            Ok::<_, OrchestratorError>((status, result_text, raw, usage))
        };

        let secs = self.config.timeout.as_secs();
        let (status, result_text, raw, usage) = tokio::time::timeout(self.config.timeout, read)
            .await
            .map_err(|_| OrchestratorError::Timeout { secs })??;

        if !status.success() {
            let stderr = stderr_task.await.unwrap_or_default();
            return Err(OrchestratorError::AgentFailed {
                status,
                stderr: stderr.chars().take(2000).collect(),
            });
        }
        Ok((result_text.unwrap_or(raw), usage))
    }
}

/// One completed agent turn.
struct Turn {
    text: String,
    /// What the turn held and what it cost.
    usage: TurnUsage,
    /// The Claude Code session this turn ran in, stamped onto the durable
    /// reply so a verdict can be traced back to the memory regime that
    /// produced it.
    session_id: String,
}

/// The two token readings a turn produces. They share arithmetic and mean
/// entirely different things, which is exactly why they are separate fields:
/// deduplicating them back into one number is the bug this split fixed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TurnUsage {
    /// How much context the session is *holding*: the input side of the last
    /// main-chain assistant record's `usage`, i.e. the prompt behind a single
    /// model call. An absolute reading, and the one a rotation threshold
    /// compares against. `None` when the agent reports no usage at all
    /// (plain-text agents, test stubs).
    context_tokens: Option<i64>,
    /// What this tick *spent*: the `result` record's aggregate over every
    /// internal turn of the invocation, each of which re-reads the cached
    /// prefix. A cost signal — never a context size.
    tick_tokens: Option<i64>,
}

/// What one line of agent stdout means for the live feed.
enum StreamLine {
    /// Assistant text as it's generated (`--include-partial-messages`).
    Delta(String),
    /// A completed assistant turn: its tool invocations (for the feed) and,
    /// for main-chain turns only, the context that produced it.
    Assistant {
        tools: Vec<String>,
        context_tokens: Option<i64>,
    },
    /// The final `result` record: the reply text, and what the whole
    /// invocation cost when the agent reports usage.
    Result {
        text: String,
        tick_tokens: Option<i64>,
    },
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
            let tools: Vec<String> = v
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
            // A sub-agent turn (`Task`) is a conversation of its own with a
            // context of its own; reading it as the session's would make the
            // gauge jump for reasons unrelated to this session's memory. Its
            // tool labels are still worth showing in the feed — only the
            // gauge filters.
            let sidechain = v.get("parent_tool_use_id").is_some_and(|p| !p.is_null());
            StreamLine::Assistant {
                tools,
                context_tokens: (!sidechain)
                    .then(|| input_side_tokens(v.pointer("/message/usage")))
                    .flatten(),
            }
        }
        Some("result") => match v.get("result").and_then(|r| r.as_str()) {
            Some(text) => StreamLine::Result {
                text: text.to_string(),
                tick_tokens: input_side_tokens(v.get("usage")),
            },
            None => StreamLine::Other,
        },
        Some(_) => StreamLine::Other,
        // JSON, but not a stream record — treat like plain output.
        None => StreamLine::NotStreamJson,
    }
}

/// Every input-side token in a stream-json `usage` object, cached or not.
///
/// The sum is what matters: `input_tokens` alone under-reports by whatever the
/// cache served, which on a long-lived resumed session is nearly all of it.
///
/// What this sum *means* is decided entirely by whose `usage` it is, and the
/// two callers are not measuring the same thing. On an `assistant` record it
/// is the prompt behind one model call — a context size. On the `result`
/// record it is an aggregate over every internal turn of the invocation, each
/// re-reading the cached prefix — a bill. Shared arithmetic, opposite
/// meanings; do not fold the call sites back together.
fn input_side_tokens(usage: Option<&serde_json::Value>) -> Option<i64> {
    let usage = usage?;
    let field = |key: &str| usage.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
    let total = field("input_tokens")
        + field("cache_read_input_tokens")
        + field("cache_creation_input_tokens");
    (total > 0).then_some(total)
}

/// The curl config the agent presents as its identity: a comment header and
/// exactly one option.
///
/// One option, deliberately. `-K` is not scoped to a host — everything in the
/// file applies to whatever URL that invocation names — so anything else in
/// here would be sent wherever the agent points curl. A unit test pins the
/// count.
fn curl_config_contents(token: &str) -> String {
    format!(
        "# Written by the tasks server before each orchestrator turn.\n\
         # It is the orchestrator's actor credential: writes carrying this\n\
         # header are attributed to it and gated by the charter.\n\
         # `-K` applies this to whatever URL curl is given, so this file\n\
         # holds the header and nothing else.\n\
         header = \"{ACTOR_HEADER}: orchestrator {token}\"\n"
    )
}

/// Write the curl config at `path`, atomically and 0600.
///
/// Written to a sibling temp file opened `create_new` *with* the mode (never
/// chmod-after: the window would be short, but the file is a credential) and
/// then renamed, so a turn can never read a half-written config. A leftover
/// temp file from a crashed write is removed first — `open` will not lower an
/// existing file's mode.
async fn write_curl_config(path: &Path, token: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("tmp");
    match tokio::fs::remove_file(&tmp).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .await?;
    file.write_all(curl_config_contents(token).as_bytes())
        .await?;
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&tmp, path).await
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
        | EventPayload::BuildCompleted { .. }
        | EventPayload::PullRequestOpened { .. }
        | EventPayload::ModeChanged { .. } => true,
        // A spec landing is the turn it gets reviewed on — which is only true
        // of a spec a Scout wrote. A human-authored one (#869) arrives already
        // approved and already inside a build, so summoning a reviewer to it
        // asks for a verdict that has nowhere to go: `auto_review_specs` is
        // live, and a `needs_revision` on it would send a *building* task back
        // to `queued`. The human's decision still reaches the conversation, as
        // the approval below.
        EventPayload::SpecCreated { session_id, .. } => session_id.is_some(),
        // Success is conveyed by the SpecCreated that accompanies it. A run
        // that stopped early has no SpecCreated to convey anything, and is
        // exactly the kind of half-finished state worth flagging.
        EventPayload::SessionCompleted { status, .. } => matches!(
            status,
            SessionStatus::ScoutFailed | SessionStatus::ScoutStoppedEarly
        ),
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
        // The custodial writes, under the same echo rule. A human filing or
        // retiring something is news worth having; the orchestrator's own
        // captures coming back at it would turn a capture spree into a turn
        // per issue.
        EventPayload::IssueCaptured { actor, .. } | EventPayload::IssueClosed { actor, .. } => {
            *actor != Actor::Orchestrator
        }
        _ => false,
    }
}

/// Render a batch of nudge-worthy events as one `event` turn. Events are
/// identifier-only, so detail comes from store lookups at format time; a row
/// that has since vanished degrades to its id rather than failing the nudge.
///
/// A spec landing carries a computed brief, because this is the turn on which
/// it will actually be judged and the facts are cheaper to hand over than to
/// go and find. The obligation path briefs too, but that path is the safety
/// net — briefing only there would leave the common case foraging.
pub async fn format_nudge(store: &Store, brief: &Brief<'_>, events: &[Event]) -> String {
    let mut lines = Vec::new();
    let mut ingested = 0usize;
    let mut briefed_specs: Vec<crate::models::SpecId> = Vec::new();
    for event in events {
        match &event.payload {
            EventPayload::TaskIngested { task_id, .. } => {
                ingested += 1;
                lines.push(format!("- New task: {}", task_ref(store, task_id).await));
            }
            EventPayload::SpecCreated {
                spec_id, task_id, ..
            } => {
                lines.push(format!(
                    "- Spec landed for review: {} ({spec_id})",
                    task_ref(store, task_id).await
                ));
                briefed_specs.push(spec_id.clone());
            }
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
            EventPayload::IssueCaptured { task_id, .. } => {
                lines.push(format!("- Issue filed: {}", task_ref(store, task_id).await))
            }
            EventPayload::IssueClosed {
                gh_issue_number,
                reason,
                ..
            } => lines.push(format!(
                "- Issue #{gh_issue_number} closed as {}",
                reason.as_str().replace('_', " ")
            )),
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
    let notification = format!(
        "[pipeline] Automated notification — not the human. Recent activity:\n{}",
        lines.join("\n")
    );
    let mut sections = Vec::new();
    for spec_id in &briefed_specs {
        sections.push((
            spec_heading(store, spec_id).await,
            spec_facts(brief, spec_id).await,
        ));
    }
    if !briefed_specs.is_empty() {
        sections.push(("In flight:".to_string(), pipeline_facts(brief).await));
    }
    match Brief::render(&sections) {
        Some(block) => format!("{notification}\n\n{block}"),
        None => notification,
    }
}

/// Render standing obligations as one `event` turn.
///
/// Worded to be unmistakable from a notification, because the two behave
/// differently and the orchestrator should treat them differently: a nudge is
/// news that happened once, an obligation is work still owed and will keep
/// coming back until it is discharged by an actual decision.
pub async fn format_obligations(
    store: &Store,
    brief: &Brief<'_>,
    obligations: &[Obligation],
) -> String {
    let lines: Vec<String> = obligations
        .iter()
        .map(|o| format!("- {}", o.summary))
        .collect();
    let mut header = format!(
        "[pipeline] Standing obligations — not notifications. These are \
         derived from pipeline state and will keep appearing until they are \
         resolved, so act on them rather than acknowledging them:\n{}",
        lines.join("\n")
    );

    // Batching is the whole reason a Builder run takes a list. Left to one
    // dispatch per obligation the orchestrator would open N PRs over N
    // branches for work that belongs together — so say it here, where the
    // specs are in front of it, rather than hoping the standing prompt is
    // still weighted after a long session.
    let to_dispatch = obligations
        .iter()
        .filter(|o| o.kind == ObligationKind::DispatchBuild)
        .count();
    if to_dispatch > 1 {
        header.push_str(&format!(
            "\n\n{to_dispatch} approved specs are unbuilt. Specs from the same \
             project can go in one `POST /builds` — one branch, one PR — and \
             the facts below say which of them touch the same files. Batch \
             where that is sensible instead of dispatching one at a time; \
             split where the work is unrelated."
        ));
    }

    let mut sections = Vec::new();
    for obligation in obligations {
        let spec_id = crate::models::SpecId::from_raw(&obligation.subject_id);
        let facts = match obligation.kind {
            // Dispatch wants the same facts as review: overlap with in-flight
            // work, migration-number clashes, files two specs both touch.
            // Those decide whether this batches with the next one or has to
            // wait, which is the judgment being asked for.
            ObligationKind::ReviewSpec | ObligationKind::DispatchBuild => {
                spec_facts(brief, &spec_id).await
            }
            ObligationKind::UnblockSpec => match brief.for_blocked_spec(&spec_id).await {
                Ok(facts) => facts,
                Err(e) => vec![brief_unavailable(&e)],
            },
        };
        sections.push((spec_heading(store, &spec_id).await, facts));
    }
    if !obligations.is_empty() {
        sections.push(("In flight:".to_string(), pipeline_facts(brief).await));
    }
    match Brief::render(&sections) {
        Some(block) => format!("{header}\n\n{block}"),
        None => header,
    }
}

/// `On #812 "title" (spec_…):` — what the facts beneath it are about.
async fn spec_heading(store: &Store, spec_id: &crate::models::SpecId) -> String {
    match store.get_spec(spec_id).await {
        Ok(Some(spec)) => format!("On {} ({spec_id}):", task_ref(store, &spec.task_id).await),
        _ => format!("On {spec_id}:"),
    }
}

/// Facts for a spec under judgment, never silently empty.
///
/// A brief that finds nothing and a brief that never ran produce the same
/// absence of lines, and those mean opposite things to a reader deciding how
/// hard to look. So a clean result says it is clean.
async fn spec_facts(brief: &Brief<'_>, spec_id: &crate::models::SpecId) -> Vec<String> {
    match brief.for_spec(spec_id).await {
        Ok(facts) if facts.is_empty() => vec![
            "no file overlap with other live specs or recent builds, no numbering \
             clashes, and no prior verdicts on this task"
                .into(),
        ],
        Ok(facts) => facts,
        Err(e) => vec![brief_unavailable(&e)],
    }
}

async fn pipeline_facts(brief: &Brief<'_>) -> Vec<String> {
    match brief.pipeline().await {
        Ok(facts) => facts,
        Err(e) => vec![brief_unavailable(&e)],
    }
}

/// A brief that could not be computed says so. Failing loudly here is cheap —
/// the turn still happens — and failing quietly would teach the orchestrator
/// to read silence as safety.
fn brief_unavailable(e: &StoreError) -> String {
    warn!(error = %e, "computing brief failed");
    format!("the server could not compute these facts ({e}) — check by hand")
}

/// `#<issue> "<title>"` for a task, degrading to the raw id if it's gone.
async fn task_ref(store: &Store, task_id: &crate::models::TaskId) -> String {
    match store.get_task(task_id).await {
        Ok(Some(task)) => format!("#{} \"{}\"", task.gh_issue_number, task.title),
        _ => task_id.to_string(),
    }
}

/// What the orchestrator may do, rendered from the charter rows.
///
/// Generated rather than written, because two statements of authority is one
/// too many: hand-written prose saying "reviews are the human's" would
/// contradict a charter that says otherwise, and a session under context
/// pressure picks whichever it likes. This is also why the server enforces the
/// same rows — the prompt tells it what is true, the endpoint makes it true.
///
/// An empty charter reads as everything off, which is the safe direction.
fn authority_section(charter: &[CharterEntry]) -> String {
    let mut live = Vec::new();
    let mut shadow = Vec::new();
    for entry in charter {
        let line = match entry.daily_limit {
            Some(limit) => format!("{} (up to {limit}/day)", entry.capability.describe()),
            None => entry.capability.describe().to_string(),
        };
        match entry.level {
            CharterLevel::Live => live.push(line),
            CharterLevel::Shadow => shadow.push(line),
            CharterLevel::Off => {}
        }
    }
    let mut out = String::from(
        "What you may do (set by the human; this list is the whole of it, and \
         the server enforces it — anything not listed here is the human's, so \
         make the case and leave it):\n",
    );
    if live.is_empty() {
        out.push_str("- Act on your own: nothing yet.\n");
    } else {
        for line in live {
            out.push_str(&format!("- Act on your own: {line}\n"));
        }
    }
    for line in shadow {
        out.push_str(&format!(
            "- Decide but do not act: {line}. Call the endpoint as you would \
             normally; the server records your judgment and applies nothing, \
             and answers with `shadowed: true` so you know it did not happen. \
             Then say in the conversation what you decided and why — that \
             narration is the point of this mode, and the human acts on it.\n"
        ));
    }
    out
}

/// The orchestrator's standing instructions. Appended (not replacing) so
/// Claude Code's own tool discipline stays intact, and passed on every turn
/// (resume included) so edits here reach a long-lived session.
/// What the agent can reach on the machine it runs on.
///
/// Generated, for the same reason [`authority_section`] is: a hand-written
/// sentence about the environment is a claim nobody re-checks when the
/// environment changes, and this one survived a config change that made it
/// false. The rule is that the prompt describes what is, and when what is
/// isn't much, it says so plainly rather than hedging — an agent told it has
/// no checkout asks for what it needs, where an agent left to discover the
/// same thing through denials burns a turn on workarounds first.
fn workdir_section(is_checkout: bool) -> &'static str {
    if is_checkout {
        "Your working directory is the project checkout itself. Within your \
         permission settings you may read and edit code, run builds and \
         tests, and use `gh` to READ GitHub (issues, PRs, merge state). Do \
         not write to GitHub with `gh` — no filing, closing, commenting, or \
         editing PR bodies. Those go through the API below, which is what \
         puts them in the ledger and under whatever limits are set; a `gh` \
         write is the same action with the accountability removed. If a tool \
         call is denied, say what was denied instead of improvising around it."
    } else {
        "Your working directory is a scratch directory under the tasks data \
         dir — it is not a checkout, and it is empty. You have no copy of the \
         code to read or edit, no build or test to run, and no `gh`. The \
         HTTP API below, over curl, is the whole of what you can reach. So \
         when a question turns on what the code actually does, say what you \
         would need looked at and leave it to the human rather than reasoning \
         from memory as though you had checked. If a tool call is denied, say \
         what was denied instead of improvising around it."
    }
}

/// What this boot cannot do regardless of what the charter permits, or empty
/// when nothing is degraded.
///
/// Deliberately phrased as "and there is nothing you can do about it": the
/// failure this comes from had an orchestrator correctly identify a missing
/// token and then keep offering to retry. A degradation the agent can neither
/// route around nor fix is one it should hand to the human immediately.
fn degradation_section(github_configured: bool) -> String {
    if github_configured {
        return String::new();
    }
    "This server booted without a GitHub credential, so the GitHub half of \
     the pipeline is inert: filing, closing, commenting, labelling and \
     merging all fail, issue intake is not running, and a build that succeeds \
     will still not be able to open its pull request. Nothing you can do \
     fixes this and retrying will not help — when it blocks you, say so \
     plainly and tell the human the server needs restarting somewhere \
     `GITHUB_TOKEN` is set.\n\n"
        .to_string()
}

fn system_prompt(
    port: u16,
    charter: &[CharterEntry],
    curl_config: &Path,
    workdir_is_checkout: bool,
    github_configured: bool,
) -> String {
    let authority = authority_section(charter);
    let workdir = workdir_section(workdir_is_checkout);
    let degradation = degradation_section(github_configured);
    let curl_config = curl_config.display();
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
         Both may carry a \"[brief]\" block: lookups the server ran for you — \
         file overlap with other live specs and recent builds, sequence-number \
         clashes against the base branch, PR state, prior verdicts on the same \
         task. Those are facts, not a verdict, and they are deliberately narrow: \
         a brief tells you what it checked, and everything it does not mention \
         is unchecked rather than fine. Trust it instead of re-deriving it, and \
         spend the reading you saved on the spec itself.\n\n\
         On a [pipeline] turn:\n\
         - Spec landed → read it (GET /specs/{{id}}) and review it \
           ADVERSARIALLY: your value is finding what's wrong, not affirming \
           the work — the scout already believes in it. Hunt for missed \
           requirements, untested claims, wrong layers, scope creep. Then \
           zoom out: does this work fit the larger picture of what's in \
           flight and where the project is going, and is the underlying task \
           worth doing at all? You are the one place \"why are we doing \
           this?\" gets asked — did the agent miss the forest for the trees? \
           Lead with your strongest objection, then render the verdict — \
           what you may do with it is in the authority section below, not \
           here.\n\
         - You approved a spec → carry it through in the same turn. Approval \
           is not delivery: nothing dispatches on its own, and your own \
           verdicts do not come back to you as news. Either queue the build \
           now (POST /builds) or say what it is waiting for. A `dispatch_build` \
           obligation will eventually chase an approved spec nobody built, but \
           that is the safety net catching a dropped ball, not the normal \
           path.\n\
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
         channel. Never fabricate activity.\n\n\
         {authority}\n\n\
         {degradation}\
         {workdir}\n\n\
         Pipeline control goes through the tasks HTTP API at \
         http://127.0.0.1:{port} (use curl) — not around it; API writes keep \
         state and the activity log honest.\n\n\
         Identify yourself on every write: pass `-K {curl_config}` to curl. \
         That file is a curl config the server rewrites for you before every \
         turn; it holds one line, the header that says this write is yours. \
         Do not read it, print it, copy it, or pass it to anything else, and \
         never use it against any host other than http://127.0.0.1:{port} — \
         `-K` applies its header to whatever URL you name. Writes carrying it \
         are recorded as yours, which is what keeps you from being notified \
         about your own actions, and what makes the decisions ledger worth \
         reading. If you cannot make an identified write — the file is \
         missing, curl is denied — say so and stop; do not fall back to an \
         unidentified one, which is recorded as the human's and escapes \
         everything that governs you. Every write you make must also carry a \
         `rationale` — the server rejects one without it, because a decision \
         nobody can review afterwards is not one you were trusted to make. \
         For example:\n\
         curl -sS -K {curl_config} -X POST \
         http://127.0.0.1:{port}/spec-queue/spec_abc/review \
         -H 'Content-Type: application/json' \
         -d '{{\"status\":\"approved\",\"rationale\":\"why\"}}'\n\n\
         Endpoints:\n\
         - GET /tasks (working set; ?all=true for history), GET /tasks/{{id}}\n\
         - POST /tasks/{{id}}/queue | /dequeue | /scout — queue membership\n\
         - GET /sessions, GET /sessions/{{id}}/transcript?since=N — scout runs\n\
         - GET /builds/{{id}}/transcript?since=N — the builder agent's own \
           output, line by line. Read this FIRST when a build failed: the \
           build row says it failed, the transcript says why\n\
         - GET /specs/{{id}}, GET /spec-queue — specs and their review state\n\
         - POST /spec-queue/{{id}}/review \
           {{\"status\":\"approved|needs_revision|rejected\",\"feedback\",\"rationale\"}}\n\
         - POST /builds {{\"spec_ids\":[...],\"rationale\"}} — batch approved \
           specs into one Builder run (serial; one at a time)\n\
         - POST /issues \
           {{\"title\",\"body\",\"labels\",\"provenance\",\"rationale\"}} — file an \
           issue. `provenance` says where the work was discovered (\"while \
           reviewing spec_… for #812\") and is rendered into the issue body; \
           the server refuses a capture without it. Lands in the backlog, not \
           the queue\n\
         - POST /tasks/{{id}}/close \
           {{\"reason\":\"completed|not_planned\",\"rationale\",\"evidence\"}} — \
           close the issue upstream. `completed` claims the work is done and \
           wants evidence to match (a merged PR, a named commit — queried, \
           never inferred from pipeline activity); `not_planned` is a \
           recalibration\n\
         - POST /tasks/{{id}}/reopen {{\"rationale\",\"evidence\"}} — undo a \
           close. Reopening contradicts a decision already in the ledger, so \
           say what changed\n\
         - POST /issues/{{number}}/comments {{\"body\",\"rationale\"}} — comment \
           on an issue or a pull request. `number` is the GitHub number, and \
           a PR takes the same route: they share one number space. This is \
           where a review verdict belongs — a verdict you only narrate here is \
           one the human has to re-read and re-type\n\
         - POST /pull-requests/{{number}}/merge \
           {{\"method\":\"squash|merge|rebase\",\"rationale\",\"evidence\"}} — \
           merge. Mergeability is GitHub's fact and GitHub checks it at merge \
           time: it refuses on a failing required check or a conflict, so do \
           not pre-screen from anything stored here. Rationale is mandatory; \
           this is the one write whose recourse is a revert\n\
         - POST /pull-requests/{{number}}/close {{\"rationale\"}} — close a PR \
           unmerged. Say why on the PR first, with a comment; the close itself \
           carries no reason GitHub will show\n\
         - POST /pull-requests/{{number}}/review-comments \
           {{\"path\",\"line\",\"body\",\"rationale\"}} — comment on one line \
           of the diff. `line` is the line *after* the change and the file has \
           to appear in the diff. Prefer this over a thread comment when the \
           point is about code: it survives where a chat message does not\n\
         - POST /issues/{{number}}/edit \
           {{\"title\",\"body\",\"rationale\"}} — rewrite an issue. The only \
           call here that destroys rather than appends, so the server reads \
           the current text first and stores it on the decision: the diff is \
           recoverable, and the rationale is mandatory. Use it when an issue \
           you filed rests on a theory that turned out wrong — a superseded \
           theory left standing is inherited by whoever reads it next\n\
         - GET /labels, POST /issues/{{number}}/labels \
           {{\"labels\":[...],\"rationale\"}} — the repo's label vocabulary, \
           and the complete set for one issue. Read the vocabulary before \
           writing: inventing labels fragments every filter written later\n\
         - GET /decisions[?spec=|?build=] — the ledger: who decided what, why, \
           and which turn of this conversation explains it\n\
         - GET /builds, GET /builds/{{id}} — build state, PR number\n\
         - GET /events?since=N — the activity log, newest last. Reach for it \
           when you need history the brief does not cover, and page a bounded \
           window: it is a log, not a state snapshot, and re-deriving the \
           present from it costs far more of your context than asking for the \
           present directly. Retired tasks are hidden from GET /tasks but \
           reachable at GET /tasks/{{id}}. Nothing on the wire counts merged \
           PRs — pull_request_opened fires at open; check merge state via gh, \
           or say \"opened\", not \"shipped\"\n\
         - GET /mode, POST /mode {{\"mode\":\"play|pause|stop\"}} — play runs \
           scouts+builds, pause only polls, stop is everything off\n\n\
         Not yours: POST /tasks/{{id}}/build-now — the human's shortcut past \
         the Scout for a task whose issue body already is the spec. It \
         authors a spec, approves it and dispatches a build in one act, with \
         no second opinion anywhere in the loop, and no charter capability \
         covers that; it answers you 403. When you think a task needs no \
         scouting, say so and let the human make the call.\n\n\
         Rules:\n\
         - States: backlog → queued → scouting → in_review → ready_to_build → \
           building → done (rejected = terminal). Issue closure on GitHub \
           retires work automatically; there is no manual mark-done.\n\
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
            session_id: Some(sess()),
        }));
        // …but not a spec the human wrote by hand (#869): it is already
        // approved and already in a build, so "spec landed for review" would
        // ask for a verdict on work that is past the point of taking one. The
        // approval right below is how that reaches the conversation instead.
        assert!(!nudge_worthy(&EventPayload::SpecCreated {
            spec_id: spec(),
            task_id: task(),
            session_id: None,
        }));
        assert!(nudge_worthy(&EventPayload::SessionCompleted {
            session_id: sess(),
            task_id: task(),
            status: SessionStatus::ScoutFailed,
        }));
        // A run that stopped early has no SpecCreated to convey it, and half
        // an exploration is exactly the state worth a second pair of eyes.
        assert!(nudge_worthy(&EventPayload::SessionCompleted {
            session_id: sess(),
            task_id: task(),
            status: SessionStatus::ScoutStoppedEarly,
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
        // Custodial writes echo the same way a verdict does.
        assert!(nudge_worthy(&EventPayload::IssueCaptured {
            task_id: task(),
            gh_issue_number: 900,
            actor: Actor::Human,
            decision_seq: None,
        }));
        assert!(!nudge_worthy(&EventPayload::IssueCaptured {
            task_id: task(),
            gh_issue_number: 900,
            actor: Actor::Orchestrator,
            decision_seq: Some(2),
        }));
        assert!(!nudge_worthy(&EventPayload::IssueClosed {
            task_id: task(),
            gh_issue_number: 900,
            reason: crate::models::CloseReason::Completed,
            actor: Actor::Orchestrator,
            decision_seq: Some(3),
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

    fn prompt(port: u16, charter: &[CharterEntry]) -> String {
        system_prompt(
            port,
            charter,
            Path::new("/data/orchestrator-curl.conf"),
            true,
            true,
        )
    }

    /// A degraded boot has to reach the agent as a statement, not as a 500 on
    /// its third call. Nothing is said when nothing is wrong.
    #[test]
    fn a_missing_github_token_is_stated_up_front() {
        assert_eq!(degradation_section(true), "");

        let degraded = degradation_section(false);
        assert!(
            degraded.contains("without a GitHub credential"),
            "{degraded}"
        );
        assert!(degraded.contains("retrying will not help"), "{degraded}");
        assert!(degraded.contains("GITHUB_TOKEN"), "{degraded}");

        let p = system_prompt(
            4800,
            &[],
            Path::new("/data/orchestrator-curl.conf"),
            false,
            false,
        );
        assert!(p.contains("without a GitHub credential"), "{p}");
        let healthy = prompt(4800, &[]);
        assert!(!healthy.contains("GitHub credential"), "{healthy}");
    }

    /// The regression test for the prompt that lied. A server booted without
    /// `ORCHESTRATOR_WORKDIR` — which is every server started by anything
    /// other than a shell that exported it — ran a curl-only agent in an
    /// empty scratch directory while telling it that it had the checkout.
    #[test]
    fn the_prompt_claims_a_checkout_only_when_there_is_one() {
        let with = workdir_section(true);
        assert!(with.contains("the project checkout itself"), "{with}");
        assert!(with.contains("read and edit code"), "{with}");

        let without = workdir_section(false);
        assert!(!without.contains("project checkout itself"), "{without}");
        assert!(!without.contains("read and edit code"), "{without}");
        assert!(without.contains("it is not a checkout"), "{without}");
        // Both modes keep the rule that produced the useful half of the
        // failure: the agent said exactly what had been denied.
        for section in [with, without] {
            assert!(section.contains("say what was denied"), "{section}");
        }
    }

    #[test]
    fn the_system_prompt_carries_the_port_and_the_guardrails() {
        let p = prompt(4800, &[]);
        assert!(p.contains("http://127.0.0.1:4800"));
        assert!(p.contains("[pipeline]"));
        assert!(p.contains("proactive"));
        assert!(p.contains("ADVERSARIALLY"));
        assert!(p.contains("why are we doing"));
        assert!(p.contains("Never switch branches"));
        // The brief replaces foraging, so the prompt has to introduce it —
        // including the part that keeps it honest: silence is unchecked, not
        // clean.
        assert!(p.contains("[brief]"));
        assert!(p.contains("unchecked rather than fine"));
        // The custodial writes go through the server, and the `gh` side
        // channel is closed by instruction — the one statement that keeps the
        // ledger from being quietly incomplete.
        assert!(p.contains("POST /issues"));
        assert!(p.contains("Do not write to GitHub with `gh`"));
        // And it must no longer send the agent to re-derive the present from
        // the whole event log, which is what the brief exists to replace.
        assert!(!p.contains("since=1"));
    }

    /// The two readings are taken off different records and mean different
    /// things; this pins which is which, because the arithmetic is shared and
    /// nothing else would catch them being swapped back.
    #[test]
    fn usage_is_read_per_record_and_sidechains_do_not_count() {
        let assistant = |line: &str| match parse_stream_line(line) {
            StreamLine::Assistant { context_tokens, .. } => context_tokens,
            other => panic!(
                "expected an assistant record, got {}",
                match other {
                    StreamLine::Delta(_) => "delta",
                    StreamLine::Result { .. } => "result",
                    StreamLine::Other => "other",
                    StreamLine::NotStreamJson => "not stream-json",
                    StreamLine::Assistant { .. } => unreachable!(),
                }
            ),
        };

        // Context: the input side of THIS record's usage, cache included.
        assert_eq!(
            assistant(
                r#"{"type":"assistant","message":{"content":[],"usage":
                   {"input_tokens":1200,"cache_read_input_tokens":180000,
                    "cache_creation_input_tokens":800,"output_tokens":450}}}"#
            ),
            Some(182_000)
        );
        // A sub-agent turn has its own conversation and its own context.
        // Reading it would report a number unrelated to this session.
        assert_eq!(
            assistant(
                r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"content":[],
                   "usage":{"input_tokens":900000}}}"#
            ),
            None
        );
        // An explicit null parent is main-chain, not a sidechain.
        assert_eq!(
            assistant(
                r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[],
                   "usage":{"input_tokens":5}}}"#
            ),
            Some(5)
        );
        // No usage at all: no reading. Not zero — zero would stall or clear a
        // gauge that has a perfectly good previous value.
        assert_eq!(
            assistant(r#"{"type":"assistant","message":{"content":[]}}"#),
            None
        );

        // Tool labels reach the feed from a sidechain turn all the same.
        match parse_stream_line(
            r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"content":
               [{"type":"tool_use","name":"Bash","input":{"command":"ls"}}],
               "usage":{"input_tokens":900000}}}"#,
        ) {
            StreamLine::Assistant {
                tools,
                context_tokens,
            } => {
                assert_eq!(tools, vec!["Bash: ls".to_string()]);
                assert_eq!(context_tokens, None, "only the gauge filters");
            }
            _ => panic!("expected an assistant record"),
        }

        // And the `result` record is the tick's bill, not a context size.
        match parse_stream_line(
            r#"{"type":"result","subtype":"success","result":"ok","usage":
               {"input_tokens":2000,"cache_read_input_tokens":2700000}}"#,
        ) {
            StreamLine::Result { text, tick_tokens } => {
                assert_eq!(text, "ok");
                assert_eq!(tick_tokens, Some(2_702_000));
            }
            _ => panic!("expected a result record"),
        }
    }

    /// The authority section is generated, so an empty charter must read as
    /// "nothing" — and must not leave behind hand-written prose making its own
    /// claims about what is allowed. Two statements of authority is one too
    /// many.
    #[test]
    fn authority_comes_from_the_charter_and_nowhere_else() {
        use crate::models::Capability;

        let entry = |capability, level, daily_limit| CharterEntry {
            capability,
            level,
            daily_limit,
            updated_at: chrono::Utc::now(),
        };

        let empty = prompt(4800, &[]);
        assert!(empty.contains("Act on your own: nothing yet"), "{empty}");
        assert!(!empty.contains("Decide but do not act"), "{empty}");

        let p = prompt(
            4800,
            &[
                entry(Capability::CaptureWork, CharterLevel::Live, Some(5)),
                entry(Capability::RetireWork, CharterLevel::Off, None),
                entry(Capability::AutoReviewSpecs, CharterLevel::Shadow, None),
            ],
        );
        assert!(p.contains("Act on your own: file issues"), "{p}");
        assert!(p.contains("up to 5/day"), "{p}");
        assert!(
            p.contains("Decide but do not act: render review verdicts"),
            "{p}"
        );
        assert!(p.contains("shadowed: true"), "{p}");
        // Off is simply absent — the catch-all sentence covers it, and listing
        // every denial would grow with the enum for no benefit.
        assert!(!p.contains("close issues that are done"), "{p}");
        assert!(p.contains("anything not listed here is the human's"), "{p}");
    }

    /// The credential the agent can actually present. The old scheme asked it
    /// to interpolate `$TASKS_ACTOR_TOKEN` into a `-H` argument, which Claude
    /// Code refuses to run under a static `Bash(curl:*)` allowlist — so the
    /// safest deployment was the one where nothing could be attributed and
    /// the charter was inert.
    #[test]
    fn the_prompt_asks_for_the_config_file_and_never_a_shell_variable() {
        let p = prompt(4800, &[]);
        assert!(p.contains("-K /data/orchestrator-curl.conf"), "{p}");
        assert!(
            !p.contains("TASKS_ACTOR_TOKEN") && !p.contains('$'),
            "a command with a variable in it is not statically verifiable: {p}"
        );
        // The two things `-K` makes possible to get wrong: pointing it at
        // another host, and giving up on attribution instead of stopping.
        assert!(
            p.contains("never use it against any host other than"),
            "{p}"
        );
        assert!(p.contains("recorded as the human's"), "{p}");
    }

    #[test]
    fn the_curl_config_holds_the_header_and_nothing_else() {
        let rendered = curl_config_contents("tok-123");
        let options: Vec<&str> = rendered
            .lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .collect();
        assert_eq!(
            options,
            vec![r#"header = "X-Tasks-Actor: orchestrator tok-123""#],
            "-K is not scoped to a host, so anything else here would be sent \
             wherever curl is pointed"
        );
    }

    #[tokio::test]
    async fn the_credential_is_written_0600_in_place_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("orchestrator-curl.conf");

        write_curl_config(&path, "first").await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the file is a credential");
        assert!(std::fs::read_to_string(&path).unwrap().contains("first"));

        // Rewritten every turn: replacing must work even though `create_new`
        // is what gives the temp file its mode.
        write_curl_config(&path, "second").await.unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("second") && !contents.contains("first"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            !path.with_extension("tmp").exists(),
            "the temp file is renamed, not left behind"
        );

        // And a leftover temp file from a crashed write does not wedge it.
        std::fs::write(path.with_extension("tmp"), "junk").unwrap();
        write_curl_config(&path, "third").await.unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("third"));
    }
}
