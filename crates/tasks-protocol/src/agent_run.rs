//! How an agent process ended, and whether the supervisor should resume it.
//!
//! Agent processes die intermittently at around 380 seconds elapsed (#845),
//! across both scouts and builders, in the middle of generating a response.
//! Nothing in this repository can prevent that: the drop happens below the
//! agent, in the VM's network path — which is why two scouts started one
//! second apart in the same image on the same host both crossed the boundary
//! and only the one that was mid-generation died.
//!
//! What *can* be fixed is that the drop currently costs the whole run. Claude
//! Code sessions are resumable by id, and the id is already in the stream the
//! supervisor is reading, so a supervisor that sees its agent die of
//! `terminal_reason: api_error` can re-invoke it with `--resume <session_id>`
//! in the same VM — same conversation, same worktree, same `NOTES.md` — and
//! the run continues instead of ending.
//!
//! This module is the pure half of that: [`ResultWatcher`] reads the agent's
//! stream-json and classifies the ending, [`decide`] says whether to resume,
//! and [`AgentRun`] carries the whole run's story into the terminal reason.
//! No process is spawned here, which is what keeps `tasks-protocol`'s
//! dependency set light — it sits beside [`crate::vm_memory`], which set the
//! precedent that VM-side helpers shared by both supervisors live here.
//!
//! # Read the session id, don't inject one
//!
//! `claude --session-id <uuid>` exists and would let a supervisor name the
//! conversation up front. But the agent command belongs to the operator
//! (`SCOUT_AGENT_CMD` / `BUILDER_AGENT_CMD`): reading the id out of the stream
//! is additive and works with whatever they configured, injecting one is not.
//!
//! # Resume in the supervisor, never re-dispatch from the host
//!
//! A host-side retry gets a new VM and a fresh clone. The supervisor keeps the
//! conversation *and* the worktree — and for a Builder that difference is the
//! implementation itself.
//!
//! # The guards are the load-bearing part
//!
//! The failures you must not retry look superficially like the one you must.
//! Do not resume an OOM kill: same memory limit, larger conversation, same
//! kill. Do not resume when no terminal record arrived at all: that is the
//! host deallocating the VM at the deadline, and this process is about to die
//! with it. Do not resume a command that already selects its own session.
//! Each of those has its own [`NoResume`] reason, so a resume that *didn't*
//! happen is as legible in the transcript as one that did.

use std::time::Duration;

use serde_json::Value;

use crate::vm_memory::AgentOutcome;

/// Resumes allowed per run when `SCOUT_MAX_RESUMES` / `BUILDER_MAX_RESUMES`
/// is unset. Two covers the observed shape (a single mid-response drop, very
/// occasionally a second) without turning a genuinely broken network into a
/// budget-burning loop.
pub const DEFAULT_MAX_RESUMES: u32 = 2;

/// What the resumed agent reads on stdin.
///
/// It deliberately does **not** restate the task. The task is above this in
/// the conversation the `--resume` reattaches to, and re-sending it is exactly
/// how a resume silently becomes a restart — the agent reads a fresh
/// instruction, decides it is starting over, and throws away the work it can
/// still see on disk.
pub const RESUME_PROMPT: &str = "\
Your previous message was cut off because the connection to the API dropped. \
That was an infrastructure failure in the VM's network path — it was not \
caused by anything you did, and nothing you produced was rejected.

You are resuming the same session, in the same working directory, with the \
same worktree: every file you created or edited is still there, unchanged, \
including any notes you were keeping. Nothing has been reverted.

Do not start over and do not repeat work you have already done. Re-read \
anything you need to re-orient (your own notes on disk are the fastest way), \
then continue from where you were interrupted and finish the task you were \
already given.";

/// How the agent's own output stream ended.
///
/// The fourth state is the one that is easy to miss and the third is the one
/// that is easy to fabricate — see [`AgentEnding::Silent`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AgentEnding {
    /// No stream-json at all. A plain-text agent — including the supervisors'
    /// own `claude --print` fallback, and every shell-script agent in the
    /// tests — has no postmortem to be missing, so nothing is invented for it.
    ///
    /// An early draft reported "the agent wrote no final result record, so it
    /// was killed" for every one of them.
    #[default]
    Silent,
    /// stream-json was seen, but no terminal `result` record: the agent was
    /// killed before it could explain itself. Almost always the host
    /// deallocating the VM at the deadline.
    NoResult,
    /// The agent reached a terminal record and said how it ended.
    Concluded { terminal_reason: String },
    /// The #845 ending: the terminal record names a transport failure.
    Transport {
        terminal_reason: String,
        /// The HTTP status when there was one (`529`, `429`, …). Numeric on
        /// the wire, rendered here as text.
        api_error_status: Option<String>,
    },
}

impl AgentEnding {
    /// Whether the run ended because the connection to the API failed, rather
    /// than because the agent decided anything.
    pub fn is_transport(&self) -> bool {
        matches!(self, Self::Transport { .. })
    }

    /// One clause naming the transport failure, for a terminal reason.
    /// `None` for every other ending.
    pub fn transport_summary(&self) -> Option<String> {
        let Self::Transport {
            terminal_reason,
            api_error_status,
        } = self
        else {
            return None;
        };
        let status = match api_error_status {
            Some(status) => format!(", HTTP {status}"),
            None => String::new(),
        };
        Some(format!(
            "the agent's connection to the API failed \
             (terminal_reason: {terminal_reason}{status}); \
             this is an infrastructure failure, not a verdict on the work"
        ))
    }
}

/// Reads an agent's stdout stream-json as it goes by, keeping the newest
/// `session_id` and classifying how the stream ended.
///
/// It rides the *same* loop that forwards output as `Progress` events, so its
/// classification reads the identical bytes that were reported and cannot
/// disagree with them.
#[derive(Debug, Clone, Default)]
pub struct ResultWatcher {
    session_id: Option<String>,
    /// Set by the first stream-json *object*. Never by a bare scalar: an
    /// ordinary agent that prints `42` or `"done"` is still a plain-text
    /// agent, and promoting it out of [`AgentEnding::Silent`] would invent a
    /// diagnosis for it.
    saw_stream_json: bool,
    /// The terminal record's classification, once one has been seen.
    result: Option<AgentEnding>,
}

impl ResultWatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line of the agent's stdout. Anything that is not a JSON object
    /// is ignored, silently and cheaply — most agents produce mostly prose.
    pub fn observe(&mut self, line: &str) {
        let line = line.trim();
        if !line.starts_with('{') {
            return;
        }
        let Ok(Value::Object(record)) = serde_json::from_str::<Value>(line) else {
            return;
        };
        self.saw_stream_json = true;

        // Every record type carries session_id, so the newest one wins and a
        // stream that ends mid-record still leaves the id behind.
        if let Some(Value::String(id)) = record.get("session_id")
            && !id.is_empty()
        {
            self.session_id = Some(id.clone());
        }

        if record.get("type").and_then(Value::as_str) != Some("result") {
            return;
        }
        self.result = Some(classify_result(&Value::Object(record)));
    }

    /// The newest session id the stream announced, if any.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// How the stream ended.
    pub fn ending(&self) -> AgentEnding {
        match &self.result {
            Some(ending) => ending.clone(),
            None if self.saw_stream_json => AgentEnding::NoResult,
            None => AgentEnding::Silent,
        }
    }
}

/// Classify a terminal `result` record.
///
/// `terminal_reason` wins over `subtype`. The subtype fallback exists for a
/// Claude Code old enough to predate `terminal_reason`, and applies only when
/// `terminal_reason` is absent *entirely* — a newer CLI that says `completed`
/// must never be overridden by its own subtype.
fn classify_result(record: &Value) -> AgentEnding {
    let api_error_status = read_api_error_status(record);
    let terminal_reason = record
        .get("terminal_reason")
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty());

    match terminal_reason {
        Some(reason) => {
            if reason == "api_error" || api_error_status.is_some() {
                AgentEnding::Transport {
                    terminal_reason: reason.to_string(),
                    api_error_status,
                }
            } else {
                AgentEnding::Concluded {
                    terminal_reason: reason.to_string(),
                }
            }
        }
        None => {
            let subtype = record
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if subtype == "error_during_execution" || api_error_status.is_some() {
                AgentEnding::Transport {
                    terminal_reason: subtype.to_string(),
                    api_error_status,
                }
            } else {
                AgentEnding::Concluded {
                    terminal_reason: subtype.to_string(),
                }
            }
        }
    }
}

/// `api_error_status` as text, or `None` when the field is absent or null.
///
/// Both halves matter. The field is a *number* on a real HTTP status, so
/// reading it with `as_str()` alone silently drops a genuine 529. And it is
/// present-and-`null` on a healthy run, so reading it without the null check
/// makes every clean run look like a transport death.
fn read_api_error_status(record: &Value) -> Option<String> {
    match record.get("api_error_status")? {
        Value::Null => None,
        Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}

/// Why a dead agent was not resumed. One named reason per guard, because a
/// resume that did not happen has to be as readable as one that did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoResume {
    /// `*_MAX_RESUMES=0`: resuming is switched off.
    Disabled,
    /// Every resume in the budget has been spent.
    BudgetExhausted { used: u32, max: u32 },
    /// The agent produced no stream-json, so there is no ending to diagnose
    /// and no session id to resume. See [`AgentEnding::Silent`].
    NotStreamJson,
    /// stream-json but no terminal record — the VM is being torn down under
    /// us, and this process is about to die too. Resuming would spawn a child
    /// into a machine that is going away.
    NoResultRecord,
    /// The agent reached a conclusion. Resuming would relitigate it.
    Concluded { terminal_reason: String },
    /// The kernel OOM-killed something during the run. A resume replays the
    /// same memory limit against a *larger* conversation: same kill, one
    /// budget later.
    MemoryKill,
    /// stream-json arrived but never carried a session id, so there is no
    /// conversation to name.
    NoSessionId,
    /// The operator's command already chooses its own conversation; appending
    /// another selector would change which one runs.
    CommandSelectsSession { flag: String },
}

impl NoResume {
    /// One clause for a log line or a terminal reason.
    pub fn describe(&self) -> String {
        match self {
            Self::Disabled => "resuming is disabled (max resumes is 0)".to_string(),
            Self::BudgetExhausted { used, max } => {
                format!("the resume budget is spent ({used} of {max} used)")
            }
            Self::NotStreamJson => {
                "the agent produced no stream-json, so its ending cannot be classified".to_string()
            }
            Self::NoResultRecord => {
                "the agent wrote no terminal record, which means it was killed from outside \
                 (usually the VM being deallocated at the deadline) rather than losing a \
                 connection"
                    .to_string()
            }
            Self::Concluded { terminal_reason } => {
                format!("the agent concluded on its own (terminal_reason: {terminal_reason})")
            }
            Self::MemoryKill => {
                "the kernel OOM-killed a process during the run, and a resume would meet the \
                 same memory limit with a larger conversation"
                    .to_string()
            }
            Self::NoSessionId => {
                "the agent's output never carried a session id, so there is no conversation to \
                 resume"
                    .to_string()
            }
            Self::CommandSelectsSession { flag } => {
                format!("the configured agent command already selects a session ({flag})")
            }
        }
    }
}

/// What to do about an agent process that just exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeDecision {
    /// Re-invoke the agent with this argv after this delay. `attempt` counts
    /// resumes, so the first one is 1.
    Resume {
        argv: Vec<String>,
        delay: Duration,
        attempt: u32,
    },
    /// Let the run end, for this reason.
    Stop(NoResume),
}

/// Flags that mean the operator already chose the conversation.
///
/// `--resume` alone is not enough: every one of these selects a session, and
/// appending our own selector alongside would change which conversation runs.
/// Matched against the flag *name*, so `--resume=<id>` is caught too.
const SESSION_SELECTING_FLAGS: &[&str] = &[
    "--resume",
    "-r",
    "--continue",
    "-c",
    "--session-id",
    "--fork-session",
];

/// The first session-selecting flag in the configured command, if any.
fn session_selecting_flag(argv: &[String]) -> Option<String> {
    argv.iter().skip(1).find_map(|arg| {
        let name = arg.split_once('=').map(|(name, _)| name).unwrap_or(arg);
        SESSION_SELECTING_FLAGS
            .contains(&name)
            .then(|| name.to_string())
    })
}

/// How long to wait before the nth resume (1-based).
///
/// Rising, not flat. A per-connection lifetime cap is gone the instant it
/// fires — the next connection is fine — so the first wait only has to be
/// non-zero. But `api_error_status` can also be 429 or 529, where the only
/// remedy is time; by the second failure in a row that is the likelier
/// reading, so the wait grows into it.
pub fn resume_delay(attempt: u32) -> Duration {
    match attempt {
        0 | 1 => Duration::from_secs(2),
        2 => Duration::from_secs(15),
        _ => Duration::from_secs(30),
    }
}

/// Decide whether to resume the agent, given how its process ended.
///
/// `argv` is the operator's configured command (program first). `resumes_used`
/// is how many resumes this run has already spent.
pub fn decide(
    watcher: &ResultWatcher,
    outcome: &AgentOutcome,
    argv: &[String],
    resumes_used: u32,
    max_resumes: u32,
) -> ResumeDecision {
    if max_resumes == 0 {
        return ResumeDecision::Stop(NoResume::Disabled);
    }
    if resumes_used >= max_resumes {
        return ResumeDecision::Stop(NoResume::BudgetExhausted {
            used: resumes_used,
            max: max_resumes,
        });
    }
    match watcher.ending() {
        AgentEnding::Silent => return ResumeDecision::Stop(NoResume::NotStreamJson),
        AgentEnding::NoResult => return ResumeDecision::Stop(NoResume::NoResultRecord),
        AgentEnding::Concluded { terminal_reason } => {
            return ResumeDecision::Stop(NoResume::Concluded { terminal_reason });
        }
        AgentEnding::Transport { .. } => {}
    }
    // The OOM check comes after the transport check on purpose: a run that was
    // both OOM-killed and dropped is reported as the kill, because that is the
    // one a resume cannot survive.
    if outcome.verdict.is_some() {
        return ResumeDecision::Stop(NoResume::MemoryKill);
    }
    let Some(session_id) = watcher.session_id() else {
        return ResumeDecision::Stop(NoResume::NoSessionId);
    };
    if let Some(flag) = session_selecting_flag(argv) {
        return ResumeDecision::Stop(NoResume::CommandSelectsSession { flag });
    }

    let attempt = resumes_used + 1;
    let mut resumed = argv.to_vec();
    resumed.push("--resume".to_string());
    resumed.push(session_id.to_string());
    ResumeDecision::Resume {
        argv: resumed,
        delay: resume_delay(attempt),
        attempt,
    }
}

/// A whole agent run — every attempt of it — and how it ended.
///
/// The reported exit code is the *last* attempt's, not the death's, so a run
/// that was resumed and then finished cleanly reports 0. That is what a reader
/// wants (`ImplementationFinished` describes the run, not the first process),
/// but it is worth knowing before staring at it.
#[derive(Debug, Clone, Default)]
pub struct AgentRun {
    /// The last attempt's exit status, plus the memory accounting bracketing
    /// the whole run.
    pub outcome: AgentOutcome,
    /// How the last attempt's output stream ended.
    pub ending: AgentEnding,
    /// How many times the agent was resumed. `0` on an ordinary run.
    pub resumes: u32,
    /// Why the loop stopped. Meaningful mainly when [`Self::ending`] is a
    /// transport failure — on a healthy run it is just
    /// [`NoResume::Concluded`].
    pub no_resume: Option<NoResume>,
}

impl AgentRun {
    /// A run that was never resumed — the shape every non-looping caller and
    /// every test that does not care about resuming wants.
    pub fn single(outcome: AgentOutcome, ending: AgentEnding) -> Self {
        Self {
            outcome,
            ending,
            resumes: 0,
            no_resume: None,
        }
    }

    /// Suffix for a terminal failure reason, composing with
    /// [`AgentOutcome::failure_context`].
    ///
    /// This is what stops #845 from being reported only as its symptom. A
    /// dropped connection used to reach a human as "SPEC.md not found" or
    /// "agent produced no commits", which reads as a verdict on the work.
    pub fn failure_context(&self) -> String {
        let mut out = String::new();
        if let Some(transport) = self.ending.transport_summary() {
            out.push_str(&format!(" — {transport}"));
        }
        if self.resumes > 0 {
            out.push_str(&format!(
                " — resumed {} time(s) after an interrupted API connection",
                self.resumes
            ));
        }
        // Only when the ending was a transport failure: on a healthy run
        // "not resumed: the agent concluded on its own" is noise.
        if self.ending.is_transport()
            && let Some(reason) = &self.no_resume
        {
            out.push_str(&format!(" — not resumed: {}", reason.describe()));
        }
        out.push_str(&self.outcome.failure_context());
        out
    }
}

/// Read a `*_MAX_RESUMES` variable, falling back to
/// [`DEFAULT_MAX_RESUMES`] when it is unset or unparseable. `0` disables
/// resuming.
pub fn max_resumes_from_env(var: &str) -> u32 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_MAX_RESUMES)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(cmd: &str) -> Vec<String> {
        cmd.split_whitespace().map(str::to_string).collect()
    }

    fn base_argv() -> Vec<String> {
        argv("claude --print --output-format stream-json --verbose")
    }

    /// A watcher fed the given lines.
    fn watch(lines: &[&str]) -> ResultWatcher {
        let mut w = ResultWatcher::new();
        for line in lines {
            w.observe(line);
        }
        w
    }

    const INIT: &str =
        r#"{"type":"system","subtype":"init","session_id":"11111111-2222-3333-4444-555555555555"}"#;

    /// The clean ending, as the real CLI writes it: `terminal_reason` present,
    /// `api_error_status` present *and null*.
    const CLEAN_RESULT: &str = r#"{"subtype":"success","terminal_reason":"completed","api_error_status":null,"session_id":"11111111-2222-3333-4444-555555555555","type":"result"}"#;

    /// The #845 ending.
    const API_ERROR_RESULT: &str = r#"{"subtype":"error_during_execution","terminal_reason":"api_error","api_error_status":529,"session_id":"11111111-2222-3333-4444-555555555555","type":"result"}"#;

    #[test]
    fn a_plain_text_agent_is_silent_and_never_diagnosed() {
        let w = watch(&[
            "[stub-agent] starting",
            "building...",
            "done",
            // A bare scalar is still not stream-json — an ordinary agent that
            // prints a number must not be promoted out of Silent.
            "42",
            "\"done\"",
        ]);
        assert_eq!(w.ending(), AgentEnding::Silent);
        assert_eq!(w.session_id(), None);
    }

    #[test]
    fn stream_json_without_a_terminal_record_is_no_result() {
        let w = watch(&[INIT, r#"{"type":"assistant","message":{"content":[]}}"#]);
        assert_eq!(w.ending(), AgentEnding::NoResult);
        assert_eq!(w.session_id(), Some("11111111-2222-3333-4444-555555555555"));
    }

    /// Both directions of the `api_error_status` pitfall in one test: a
    /// present-and-null field must not make a clean run look like a transport
    /// death, and a *numeric* status must not be dropped by a string-only read.
    #[test]
    fn api_error_status_is_numeric_when_set_and_null_when_healthy() {
        assert_eq!(
            watch(&[INIT, CLEAN_RESULT]).ending(),
            AgentEnding::Concluded {
                terminal_reason: "completed".into()
            }
        );
        assert_eq!(
            watch(&[INIT, API_ERROR_RESULT]).ending(),
            AgentEnding::Transport {
                terminal_reason: "api_error".into(),
                api_error_status: Some("529".into()),
            }
        );
        // An api_error with no HTTP status at all is still a transport death.
        let no_status = r#"{"type":"result","terminal_reason":"api_error"}"#;
        assert_eq!(
            watch(&[INIT, no_status]).ending(),
            AgentEnding::Transport {
                terminal_reason: "api_error".into(),
                api_error_status: None,
            }
        );
    }

    /// `terminal_reason` wins over `subtype`: the clean record above says
    /// `completed` while its own subtype says `success`, and a *newer* CLI
    /// reporting `completed` must never be reclassified by a subtype the
    /// fallback would have read as an error.
    #[test]
    fn terminal_reason_beats_subtype_but_subtype_is_the_fallback() {
        let newer =
            r#"{"type":"result","subtype":"error_during_execution","terminal_reason":"completed"}"#;
        assert_eq!(
            watch(&[INIT, newer]).ending(),
            AgentEnding::Concluded {
                terminal_reason: "completed".into()
            }
        );

        // No terminal_reason at all — an older CLI. Now the subtype speaks.
        let older = r#"{"type":"result","subtype":"error_during_execution"}"#;
        assert_eq!(
            watch(&[INIT, older]).ending(),
            AgentEnding::Transport {
                terminal_reason: "error_during_execution".into(),
                api_error_status: None,
            }
        );
        let older_ok = r#"{"type":"result","subtype":"success"}"#;
        assert_eq!(
            watch(&[INIT, older_ok]).ending(),
            AgentEnding::Concluded {
                terminal_reason: "success".into()
            }
        );
    }

    #[test]
    fn the_newest_session_id_wins() {
        let w = watch(&[
            INIT,
            r#"{"type":"assistant","session_id":"aaaa","message":{}}"#,
            r#"{"type":"assistant","session_id":"bbbb","message":{}}"#,
        ]);
        assert_eq!(w.session_id(), Some("bbbb"));
    }

    #[test]
    fn a_transport_death_resumes_with_the_session_id_appended() {
        let w = watch(&[INIT, API_ERROR_RESULT]);
        match decide(&w, &AgentOutcome::default(), &base_argv(), 0, 2) {
            ResumeDecision::Resume {
                argv,
                delay,
                attempt,
            } => {
                assert_eq!(attempt, 1);
                assert_eq!(delay, Duration::from_secs(2));
                assert_eq!(
                    argv.last().map(String::as_str),
                    Some("11111111-2222-3333-4444-555555555555")
                );
                assert_eq!(argv[argv.len() - 2], "--resume");
                // The operator's command is preserved ahead of it, verbatim.
                assert_eq!(argv[..base_argv().len()], base_argv()[..]);
            }
            other => panic!("expected a resume, got {other:?}"),
        }
    }

    #[test]
    fn the_backoff_rises() {
        assert_eq!(resume_delay(1), Duration::from_secs(2));
        assert_eq!(resume_delay(2), Duration::from_secs(15));
        assert_eq!(resume_delay(3), Duration::from_secs(30));
        assert_eq!(resume_delay(9), Duration::from_secs(30));
    }

    /// Every guard, each naming itself. These are the cases where resuming is
    /// worse than stopping, and they all look superficially like the case
    /// where it is better.
    #[test]
    fn each_guard_stops_with_its_own_reason() {
        let transport = watch(&[INIT, API_ERROR_RESULT]);
        let clean = AgentOutcome::default();

        assert_eq!(
            decide(&transport, &clean, &base_argv(), 0, 0),
            ResumeDecision::Stop(NoResume::Disabled)
        );
        assert_eq!(
            decide(&transport, &clean, &base_argv(), 2, 2),
            ResumeDecision::Stop(NoResume::BudgetExhausted { used: 2, max: 2 })
        );
        assert_eq!(
            decide(&watch(&["plain output"]), &clean, &base_argv(), 0, 2),
            ResumeDecision::Stop(NoResume::NotStreamJson)
        );
        assert_eq!(
            decide(&watch(&[INIT]), &clean, &base_argv(), 0, 2),
            ResumeDecision::Stop(NoResume::NoResultRecord)
        );
        assert_eq!(
            decide(&watch(&[INIT, CLEAN_RESULT]), &clean, &base_argv(), 0, 2),
            ResumeDecision::Stop(NoResume::Concluded {
                terminal_reason: "completed".into()
            })
        );

        // An OOM kill: same limit, larger conversation, same kill.
        let oom = AgentOutcome {
            verdict: Some("the kernel OOM-killed 1 process(es)".into()),
            ..Default::default()
        };
        assert_eq!(
            decide(&transport, &oom, &base_argv(), 0, 2),
            ResumeDecision::Stop(NoResume::MemoryKill)
        );

        // A transport death with no session id anywhere in the stream.
        let anonymous = watch(&[
            r#"{"type":"system","subtype":"init"}"#,
            r#"{"type":"result","terminal_reason":"api_error"}"#,
        ]);
        assert_eq!(
            decide(&anonymous, &clean, &base_argv(), 0, 2),
            ResumeDecision::Stop(NoResume::NoSessionId)
        );
    }

    /// `--resume` alone is not the guard list: every one of these means the
    /// operator already chose the conversation.
    #[test]
    fn any_session_selecting_flag_blocks_a_resume() {
        let transport = watch(&[INIT, API_ERROR_RESULT]);
        for (cmd, expected) in [
            ("claude --print --resume abc", "--resume"),
            ("claude --print --resume=abc", "--resume"),
            ("claude --print -r abc", "-r"),
            ("claude --print --continue", "--continue"),
            ("claude --print -c", "-c"),
            ("claude --print --session-id abc", "--session-id"),
            ("claude --print --session-id=abc", "--session-id"),
            ("claude --print --fork-session", "--fork-session"),
        ] {
            assert_eq!(
                decide(&transport, &AgentOutcome::default(), &argv(cmd), 0, 2),
                ResumeDecision::Stop(NoResume::CommandSelectsSession {
                    flag: expected.into()
                }),
                "command: {cmd}"
            );
        }

        // And a command that merely mentions the word does not trip it.
        match decide(
            &transport,
            &AgentOutcome::default(),
            &argv("claude --print --append-system-prompt resume-nothing"),
            0,
            2,
        ) {
            ResumeDecision::Resume { .. } => {}
            other => panic!("a non-selecting flag must not block a resume: {other:?}"),
        }
    }

    #[test]
    fn a_transport_failure_names_itself_in_the_terminal_reason() {
        let run = AgentRun {
            outcome: AgentOutcome::default(),
            ending: AgentEnding::Transport {
                terminal_reason: "api_error".into(),
                api_error_status: Some("529".into()),
            },
            resumes: 2,
            no_resume: Some(NoResume::BudgetExhausted { used: 2, max: 2 }),
        };
        let context = run.failure_context();
        assert!(
            context.contains("connection to the API failed"),
            "{context}"
        );
        assert!(context.contains("HTTP 529"), "{context}");
        assert!(context.contains("resumed 2 time(s)"), "{context}");
        assert!(context.contains("resume budget is spent"), "{context}");
        // The point of the sentence: this is not a verdict on the work.
        assert!(context.contains("not a verdict on the work"), "{context}");
    }

    /// A healthy run says nothing new — the whole change stays off the happy
    /// path, exactly as `AgentOutcome::failure_context` does.
    #[test]
    fn a_healthy_run_adds_nothing_to_the_reason() {
        let run = AgentRun {
            outcome: AgentOutcome::default(),
            ending: AgentEnding::Concluded {
                terminal_reason: "completed".into(),
            },
            resumes: 0,
            no_resume: Some(NoResume::Concluded {
                terminal_reason: "completed".into(),
            }),
        };
        assert_eq!(run.failure_context(), "");
        assert_eq!(AgentRun::default().failure_context(), "");
    }

    /// Signal and OOM reporting is unchanged: `AgentRun` composes with
    /// `AgentOutcome` rather than replacing it.
    #[test]
    fn the_outcomes_own_context_still_composes() {
        let run = AgentRun::single(
            AgentOutcome {
                signal: Some("killed by signal 9 (SIGKILL)".into()),
                ..Default::default()
            },
            AgentEnding::Silent,
        );
        assert_eq!(
            run.failure_context(),
            " — agent killed by signal 9 (SIGKILL)"
        );
    }
}
