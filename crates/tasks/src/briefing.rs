//! Home briefings: three narrative slots (state of the project / changes /
//! issues) written by one-shot headless agents, served stale-while-revalidate.
//!
//! Per the load-bearing rule this is not an agentic loop of our own: each
//! generation shells out to headless Claude Code (`BRIEFING_CMD`) with
//! *read-only* tool permissions and a fresh session — the inputs ARE the
//! context, so there is nothing to resume. The orchestrator is deliberately
//! not involved: its long-lived chat is a conversation, not a report mill.
//!
//! Demand-driven, not a cron: `GET /briefings` returns whatever is stored
//! immediately and, when a section is past `BRIEFING_TTL_SECS`, kicks a
//! single-flight background regeneration. App closed = zero generations.
//! The stored text is a cache with a visible date — GitHub facts inside it
//! were queried at generation time and are never read back as truth.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::events::EventPayload;
use crate::models::{Briefing, BriefingSection};
use crate::store::{Store, StoreError};

/// How long a failed generation blocks retries for its section. Short enough
/// that a transient failure self-heals on the next look, long enough that a
/// broken `BRIEFING_CMD` can't burn an agent run per refresh.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(120);

/// Default freshness window (`BRIEFING_TTL_SECS`).
pub const DEFAULT_TTL: Duration = Duration::from_secs(900);

#[derive(Debug, Error)]
pub enum BriefingError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("BRIEFING_CMD is empty")]
    EmptyCommand,
    #[error("spawn agent: {0}")]
    Spawn(std::io::Error),
    #[error("agent exited with {status}: {stderr}")]
    AgentFailed {
        status: std::process::ExitStatus,
        stderr: String,
    },
    #[error("agent timed out after {secs}s")]
    Timeout { secs: u64 },
    #[error("agent returned an empty briefing")]
    EmptyOutput,
}

#[derive(Debug, Clone)]
pub struct BriefingConfig {
    /// The agent command (`BRIEFING_CMD`), shell-style quoted. Must carry
    /// read-only permissions only — a briefing agent that can write is a
    /// misconfiguration, not a feature.
    pub command: String,
    /// Wall-clock budget per generation (`BRIEFING_TIMEOUT_SECS`).
    pub timeout: Duration,
    /// Freshness window (`BRIEFING_TTL_SECS`).
    pub ttl: Duration,
    /// Working directory — the repo checkout, so `git log` and `gh` resolve.
    pub workdir: PathBuf,
    /// Port the tasks API listens on; spliced into the prompt.
    pub api_port: u16,
}

/// One slot as `GET /briefings` serves it. All three sections are always
/// present; a never-generated one has no content and reads as stale.
#[derive(Debug, Clone, Serialize)]
pub struct BriefingStatus {
    pub section: BriefingSection,
    pub content: Option<String>,
    pub generated_at: Option<DateTime<Utc>>,
    pub stale: bool,
    pub regenerating: bool,
    /// The last generation failure, if the most recent attempt failed. The
    /// stored content (if any) is still served alongside it — never a blank
    /// slot, never a fabricated one.
    pub error: Option<String>,
}

/// In-memory regeneration bookkeeping. Deliberately not persisted: a crash
/// mid-generation just means the next look regenerates again.
#[derive(Default)]
struct RegenState {
    in_flight: HashSet<BriefingSection>,
    cooldown_until: HashMap<BriefingSection, Instant>,
    last_error: HashMap<BriefingSection, String>,
}

pub struct Briefings {
    store: Arc<Store>,
    config: BriefingConfig,
    state: Mutex<RegenState>,
}

impl Briefings {
    pub fn new(store: Arc<Store>, config: BriefingConfig) -> Self {
        Self {
            store,
            config,
            state: Mutex::new(RegenState::default()),
        }
    }

    /// The read side of stale-while-revalidate: return what is stored *now*,
    /// and for each stale section kick one background regeneration —
    /// single-flight per section, so three refreshes can't stampede.
    pub async fn get_all(self: &Arc<Self>) -> Result<Vec<BriefingStatus>, StoreError> {
        let mut statuses = snapshot(&self.store, self.config.ttl).await?;
        let mut state = self.state.lock().expect("briefing state lock");
        let now = Instant::now();
        for status in &mut statuses {
            let section = status.section;
            status.error = state.last_error.get(&section).cloned();
            if state.in_flight.contains(&section) {
                status.regenerating = true;
                continue;
            }
            let cooling = state
                .cooldown_until
                .get(&section)
                .is_some_and(|until| *until > now);
            if status.stale && !cooling {
                state.in_flight.insert(section);
                status.regenerating = true;
                let this = self.clone();
                tokio::spawn(async move { this.regenerate(section).await });
            }
        }
        Ok(statuses)
    }

    /// One generation: run the agent, persist the result, announce it on the
    /// event log. Failure keeps the last good briefing, records the error for
    /// the status projection, and sets a cooldown.
    async fn regenerate(self: Arc<Self>, section: BriefingSection) {
        let result = self.generate(section).await;
        let mut state = self.state.lock().expect("briefing state lock");
        state.in_flight.remove(&section);
        match result {
            Ok(()) => {
                state.last_error.remove(&section);
                info!(section = section.as_str(), "briefing regenerated");
            }
            Err(e) => {
                warn!(section = section.as_str(), error = %e, "briefing generation failed");
                state.last_error.insert(section, e.to_string());
                state
                    .cooldown_until
                    .insert(section, Instant::now() + FAILURE_COOLDOWN);
            }
        }
    }

    async fn generate(&self, section: BriefingSection) -> Result<(), BriefingError> {
        // High-water first: events that land *during* generation stay above
        // the mark, so later gating errs toward regenerating.
        let event_high_water = self.store.latest_event_seq().await?;
        let digest = pipeline_digest(&self.store).await;
        let prompt = prompt(section, &digest, self.config.api_port);
        let content = self.run_agent(&prompt).await?;
        let content = content.trim();
        if content.is_empty() {
            return Err(BriefingError::EmptyOutput);
        }
        self.store
            .upsert_briefing(&Briefing {
                section,
                content: content.to_string(),
                generated_at: Utc::now(),
                event_high_water,
            })
            .await?;
        self.store
            .append_event(EventPayload::BriefingUpdated { section })
            .await?;
        Ok(())
    }

    /// Run one headless agent turn: prompt on stdin, briefing on stdout.
    /// No streaming and no session — a briefing is a one-shot artifact.
    async fn run_agent(&self, prompt: &str) -> Result<String, BriefingError> {
        let args = split_command(&self.config.command);
        let (prog, rest) = args.split_first().ok_or(BriefingError::EmptyCommand)?;

        tokio::fs::create_dir_all(&self.config.workdir)
            .await
            .map_err(BriefingError::Spawn)?;

        let mut cmd = tokio::process::Command::new(prog);
        cmd.args(rest)
            .current_dir(&self.config.workdir)
            // The server's token stays the server's; `gh` authenticates as
            // itself (keychain auth), same as the orchestrator.
            .env_remove("GITHUB_TOKEN")
            // A timeout drops the read future, which drops the child — this
            // makes that drop kill the process instead of leaking it.
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(BriefingError::Spawn)?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let prompt_owned = prompt.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt_owned.as_bytes()).await;
            drop(stdin);
        });
        // Drain stderr concurrently so a chatty agent can't fill the pipe and
        // deadlock against our stdout read.
        let stderr_task = tokio::spawn(async move {
            let mut buf = String::new();
            let mut stderr = stderr;
            let _ = stderr.read_to_string(&mut buf).await;
            buf
        });

        let read = async {
            let mut out = String::new();
            stdout
                .read_to_string(&mut out)
                .await
                .map_err(BriefingError::Spawn)?;
            let status = child.wait().await.map_err(BriefingError::Spawn)?;
            Ok::<_, BriefingError>((status, out))
        };

        let secs = self.config.timeout.as_secs();
        let (status, out) = tokio::time::timeout(self.config.timeout, read)
            .await
            .map_err(|_| BriefingError::Timeout { secs })??;

        if !status.success() {
            let stderr = stderr_task.await.unwrap_or_default();
            return Err(BriefingError::AgentFailed {
                status,
                stderr: stderr.chars().take(2000).collect(),
            });
        }
        Ok(out)
    }
}

/// Pure projection of the stored rows onto all three sections — what a
/// server without a briefing service (tests, `router()` without one) serves:
/// stale copies, never regeneration.
pub async fn snapshot(store: &Store, ttl: Duration) -> Result<Vec<BriefingStatus>, StoreError> {
    let rows = store.list_briefings().await?;
    let now = Utc::now();
    Ok(BriefingSection::ALL
        .into_iter()
        .map(|section| {
            let row = rows.iter().find(|b| b.section == section);
            BriefingStatus {
                section,
                content: row.map(|b| b.content.clone()),
                generated_at: row.map(|b| b.generated_at),
                stale: row.is_none_or(|b| {
                    now.signed_duration_since(b.generated_at)
                        .to_std()
                        // A generated_at in the future is clock skew; count
                        // it as fresh rather than regenerating in a loop.
                        .is_ok_and(|age| age > ttl)
                }),
                regenerating: false,
                error: None,
            }
        })
        .collect())
}

/// Cheap server-known facts spliced into the prompt so the agent spends its
/// tool calls on the GitHub-side digging the server deliberately doesn't
/// persist. Degrades to less context, never to a failed generation.
async fn pipeline_digest(store: &Store) -> String {
    let mut lines = Vec::new();
    if let Ok(projects) = store.list_projects().await {
        let slugs: Vec<String> = projects
            .iter()
            .map(|p| format!("{}/{}", p.repo_owner, p.repo_name))
            .collect();
        if !slugs.is_empty() {
            lines.push(format!("Tracked repositories: {}", slugs.join(", ")));
        }
    }
    if let Ok(tasks) = store.list_tasks().await {
        let mut counts: std::collections::BTreeMap<&'static str, usize> =
            std::collections::BTreeMap::new();
        for task in &tasks {
            *counts.entry(task.state.as_str()).or_default() += 1;
        }
        let parts: Vec<String> = counts
            .iter()
            .map(|(state, n)| format!("{n} {state}"))
            .collect();
        lines.push(format!(
            "Pipeline working set ({} tasks): {}",
            tasks.len(),
            if parts.is_empty() {
                "empty".to_string()
            } else {
                parts.join(", ")
            }
        ));
    }
    if let Ok(queue) = store.list_spec_queue().await {
        let pending = queue
            .iter()
            .filter(|item| item.entry.status == crate::models::SpecQueueStatus::PendingReview)
            .count();
        if pending > 0 {
            lines.push(format!("Specs awaiting human review: {pending}"));
        }
    }
    if let Ok(builds) = store.list_builds().await {
        let running = builds
            .iter()
            .filter(|b| b.status == crate::models::BuildStatus::Running)
            .count();
        if running > 0 {
            lines.push(format!("Builder runs in flight: {running}"));
        }
    }
    lines.join("\n")
}

/// The full prompt for one section: shared context + the tone contract (the
/// design's verbatim rules — see docs/plans/2026-08-11-home-briefings.md) +
/// the section's own charge.
fn prompt(section: BriefingSection, digest: &str, port: u16) -> String {
    let charge = match section {
        BriefingSection::StateOfProject => {
            "Section: STATE OF THE PROJECT. Answer: is the project active or \
             quiet right now? What feature threads or meta-projects are \
             actually being tackled — named themes read out of recent \
             commits, PRs, issues, and pipeline activity, not counts. Where \
             is attention concentrated, and what is the current bottleneck? \
             Useful sources: `git log --oneline -30`, `gh pr list`, \
             `gh issue list --limit 20`, the pipeline snapshot above."
        }
        BriefingSection::Changes => {
            "Section: CHANGES. Cover open pull requests and their *real* \
             states — CI, mergeability, draft, waiting-on-review (`gh pr \
             list --json number,title,isDraft,mergeStateStatus,\
             reviewDecision,updatedAt,statusCheckRollup,files`). What has \
             gone stale and why that might be; branches or PRs that need \
             cleanup; risky overlaps where open PRs touch the same files and \
             land order matters. Judgment, not thresholds."
        }
        BriefingSection::Issues => {
            "Section: ISSUES. What came in recently and what actually looks \
             high-priority — inferred from issue content, not just labels \
             (`gh issue list --json number,title,labels,updatedAt,body \
             --limit 30`). Issue health: stale issues, probable duplicates, \
             things that look already fixed but still open. What is worth \
             queueing next, given what is already in flight."
        }
    };
    format!(
        "You are writing one section of a project briefing for Tasks — a \
         human-in-the-loop platform that turns GitHub issues into specs \
         (Scout agents) and approved specs into PRs (Builder agents). The \
         reader is the project's maintainer; they will read this on the \
         app's Home screen.\n\n\
         {digest}\n\n\
         The Tasks HTTP API is at http://127.0.0.1:{port} (curl). Useful \
         reads: GET /tasks, /spec-queue, /builds, /events?since=N (without \
         ?since it returns only the newest 100). Your working directory is a \
         checkout of the repository — use `gh`, `git log`, and `git diff` \
         for GitHub-side truth. Everything you run must be read-only: do not \
         write, push, or switch branches. Nothing on the wire counts merged \
         PRs — check merge state via gh, and say \"opened\", not \
         \"shipped\", unless you verified the merge.\n\n\
         {charge}\n\n\
         Rules for the text you produce:\n\
         - Factual and terse. No praise, no exclamation points, no \"great \
           progress\" hustle framing. Problems and risks lead.\n\
         - \"Nothing notable\" is a complete and welcome answer.\n\
         - Every PR, issue, or commit you mention becomes a markdown link to \
           its GitHub URL.\n\
         - Around 100-150 words, unless something is genuinely wrong.\n\
         - Never fabricate. If a lookup failed, say what you could not \
           check.\n\n\
         Output ONLY the briefing text as markdown prose — no heading, no \
         preamble, no code fences around the whole thing."
    )
}

/// Shell-style word splitting: whitespace separates, single or double quotes
/// group. `BRIEFING_CMD` needs this where `ORCHESTRATOR_CMD` doesn't because
/// read-only Bash permissions contain spaces (`Bash(git log:*)`).
fn split_command(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            None => {
                current.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventPayload;

    fn config(command: &str) -> BriefingConfig {
        BriefingConfig {
            command: command.into(),
            timeout: Duration::from_secs(10),
            ttl: DEFAULT_TTL,
            workdir: std::env::temp_dir(),
            api_port: 4800,
        }
    }

    #[test]
    fn split_command_groups_quoted_permissions() {
        assert_eq!(
            split_command(
                r#"claude --print --allowedTools "Bash(gh:*),Bash(git log:*)" --model x"#
            ),
            vec![
                "claude",
                "--print",
                "--allowedTools",
                "Bash(gh:*),Bash(git log:*)",
                "--model",
                "x",
            ]
        );
        assert_eq!(
            split_command("sh -c 'cat > /dev/null; echo hi'"),
            vec!["sh", "-c", "cat > /dev/null; echo hi"]
        );
        assert_eq!(split_command("  "), Vec::<String>::new());
    }

    /// The tone contract rides in every prompt, verbatim enough to grep for.
    #[test]
    fn the_prompt_carries_the_tone_contract_and_the_port() {
        for section in BriefingSection::ALL {
            let p = prompt(section, "Tracked repositories: a/b", 4811);
            assert!(p.contains("No praise"));
            assert!(p.contains("Nothing notable"));
            assert!(p.contains("Never fabricate"));
            assert!(p.contains("read-only"));
            assert!(p.contains("http://127.0.0.1:4811"));
            assert!(p.contains("Tracked repositories: a/b"));
        }
        assert!(prompt(BriefingSection::Changes, "", 1).contains("risky overlaps"));
        assert!(prompt(BriefingSection::Issues, "", 1).contains("probable duplicates"));
        assert!(prompt(BriefingSection::StateOfProject, "", 1).contains("bottleneck"));
    }

    /// The whole stale-while-revalidate loop against a real stub agent:
    /// empty store serves three stale slots and kicks single-flight
    /// regeneration; completion persists the row and announces it on the
    /// event log; the next read is fresh and quiet.
    #[tokio::test]
    async fn get_all_regenerates_stale_sections_once() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let briefings = Arc::new(Briefings::new(
            store.clone(),
            config("sh -c 'cat > /dev/null; echo the briefing'"),
        ));
        let mut events = store.subscribe_events();

        let first = briefings.get_all().await.unwrap();
        assert_eq!(first.len(), 3);
        for status in &first {
            assert!(status.stale);
            assert!(status.regenerating);
            assert_eq!(status.content, None);
        }
        // Immediately asking again must not double-spawn: each section is
        // either still in flight or already done — never spawned twice.
        for status in briefings.get_all().await.unwrap() {
            assert!(status.regenerating || !status.stale);
        }

        // All three completions arrive on the event log.
        let mut updated = HashSet::new();
        while updated.len() < 3 {
            let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("briefing events within 10s")
                .unwrap();
            if let EventPayload::BriefingUpdated { section } = event.payload {
                updated.insert(section);
            }
        }

        let fresh = briefings.get_all().await.unwrap();
        for status in fresh {
            assert!(!status.stale);
            assert!(!status.regenerating);
            assert_eq!(status.content.as_deref(), Some("the briefing"));
            assert_eq!(status.error, None);
        }
    }

    /// Failure keeps the slot honest: no row is written, the error surfaces
    /// on the status, and the cooldown stops a broken command from burning
    /// an agent run per refresh.
    #[tokio::test]
    async fn a_failed_generation_surfaces_and_cools_down() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let briefings = Arc::new(Briefings::new(
            store.clone(),
            config("sh -c 'cat > /dev/null; echo boom >&2; exit 1'"),
        ));

        briefings.get_all().await.unwrap();
        // Wait for all three failures to settle.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let empty = briefings
                .state
                .lock()
                .map(|s| s.in_flight.is_empty() && s.last_error.len() == 3)
                .unwrap();
            if empty {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "generations did not settle"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let statuses = briefings.get_all().await.unwrap();
        for status in statuses {
            assert!(status.stale, "no good copy exists");
            assert!(
                !status.regenerating,
                "cooldown must hold the retry, not respawn it"
            );
            let error = status.error.expect("failure surfaced");
            assert!(error.contains("boom"), "stderr rides the error: {error}");
            assert_eq!(status.content, None, "no fabricated content");
        }
        assert!(store.list_briefings().await.unwrap().is_empty());
    }

    /// `snapshot` (the service-less read) computes staleness off the row age.
    #[tokio::test]
    async fn snapshot_reads_rows_without_regenerating() {
        let store = Store::open_in_memory().await.unwrap();
        store
            .upsert_briefing(&Briefing {
                section: BriefingSection::Changes,
                content: "old".into(),
                generated_at: Utc::now() - chrono::Duration::hours(2),
                event_high_water: 7,
            })
            .await
            .unwrap();
        store
            .upsert_briefing(&Briefing {
                section: BriefingSection::Issues,
                content: "new".into(),
                generated_at: Utc::now(),
                event_high_water: 9,
            })
            .await
            .unwrap();

        let statuses = snapshot(&store, DEFAULT_TTL).await.unwrap();
        assert_eq!(statuses.len(), 3);
        let by_section = |s: BriefingSection| {
            statuses
                .iter()
                .find(|b| b.section == s)
                .expect("all sections present")
        };
        assert!(by_section(BriefingSection::StateOfProject).stale);
        assert_eq!(by_section(BriefingSection::StateOfProject).content, None);
        assert!(by_section(BriefingSection::Changes).stale);
        assert_eq!(
            by_section(BriefingSection::Changes).content.as_deref(),
            Some("old")
        );
        assert!(!by_section(BriefingSection::Issues).stale);
    }
}
