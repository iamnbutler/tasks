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
//! # Three policies, one mechanism
//!
//! Re-invoking the agent on the conversation it already has is now the answer
//! to three different questions, and they must not be netted together:
//!
//! - **`{SCOUT,BUILDER}_MAX_RESUMES`** (this module's [`decide`]) — how often a
//!   *dropped API connection* may be picked back up.
//! - **the Builder's repair round** (`builder-supervisor`, one round,
//!   hardcoded) — how often the agent may be told *its own tests are red*.
//! - **`{SCOUT,BUILDER}_MAX_CONTINUATIONS`** ([`decide_continuation`]) — how
//!   often the agent may be told *its turn was the whole run and it produced
//!   nothing* (#962).
//!
//! Separate counters, because netting any two lets one exhaust the other: a run
//! that spent both resumes on dropped connections still gets its repair round
//! and still gets its continuation. What they share is the re-invocation
//! itself — [`resume_argv`] for the argv and [`command_selects_session`] for
//! the guard — so a third hand-written version of either cannot come to
//! disagree with the other two.
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

use crate::AgentRole;
use crate::budget::{RunBudget, command_budget};
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
    ResumeDecision::Resume {
        argv: resume_argv(argv, session_id),
        delay: resume_delay(attempt),
        attempt,
    }
}

/// The configured command, aimed at an existing conversation.
///
/// Factored out of [`decide`] so the *other* caller — a supervisor re-invoking
/// the agent to repair a red test suite — builds the same argv rather than a
/// second hand-written one beside it. It is only the argv: the guard that says
/// whether appending a selector is safe at all
/// ([`NoResume::CommandSelectsSession`]) stays where the decision is made, and
/// a caller reaching for this outside [`decide`] has to ask
/// [`command_selects_session`] itself.
///
/// `--session-id` is never what this appends. That flag would *impose* an id
/// on a new conversation; `--resume` picks up the one the agent announced,
/// which is the whole point — the worktree and the conversation both survive.
pub fn resume_argv(argv: &[String], session_id: &str) -> Vec<String> {
    let mut resumed = argv.to_vec();
    resumed.push("--resume".to_string());
    resumed.push(session_id.to_string());
    resumed
}

/// Whether the operator's command already chose the conversation, in which
/// case nothing may append a selector alongside it.
///
/// Public so a caller that resumes *outside* [`decide`] shares this guard
/// rather than restating it — restating it is how the two would come to
/// disagree about `--resume=<id>`.
pub fn command_selects_session(argv: &[String]) -> Option<String> {
    session_selecting_flag(argv)
}

/// Continuations allowed per run when `SCOUT_MAX_CONTINUATIONS` /
/// `BUILDER_MAX_CONTINUATIONS` is unset.
///
/// **One**, and one is a decision rather than a starting point. The message a
/// continuation carries is "there is no later, and you have produced nothing" —
/// a third telling of that is not a longer leash, it is a mechanism for wearing
/// an agent down until it invents something, and what it would invent is the
/// half-explored spec that reaches a reviewer looking finished.
pub const DEFAULT_MAX_CONTINUATIONS: u32 = 1;

/// Whether the run left anything behind that the supervisor can report.
///
/// Deliberately the *weakest* reading each supervisor can make — "is there
/// anything at all", never "is it any good". Asking whether the work was fully
/// done would fire a continuation on every ordinary run, and a supervisor that
/// cannot read its own repository answers [`Deliverable::Produced`], so an
/// unreadable state declines a continuation rather than spending one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deliverable {
    /// Something is on disk: a spec, or commits. Nothing to continue about.
    Produced,
    /// Nothing is. This is the state [`decide_continuation`] exists for.
    Nothing,
}

/// Why a run that produced nothing was not handed back to its agent.
///
/// One named reason per guard, on [`NoResume`]'s rule: a continuation that did
/// not happen has to be as readable as one that did — and here it is more than
/// readability, because the terminal reason these compose into is what a human
/// reads when deciding whether three attempts were fair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoContinuation {
    /// `*_MAX_CONTINUATIONS=0`: continuing is switched off.
    Disabled,
    /// The continuation budget is spent. With the default of one, this is what
    /// a second empty ending says.
    BudgetExhausted { used: u32, max: u32 },
    /// The run has something to show, so there is nothing to tell it about.
    DeliverableProduced,
    /// The agent did not end its own turn — its connection dropped, the VM is
    /// going away, or it never streamed anything to classify. A continuation
    /// says "you ended your turn and your background children are already
    /// dead"; in front of an agent whose turn was ended *for* it that message
    /// is simply false, and the whole mechanism rests on it being true.
    NotAConclusion { ending: &'static str },
    /// The kernel OOM-killed something during the run. Same argument as
    /// [`Self::NotAConclusion`] and it is the argument rather than #828 that
    /// settles it: the agent did not park and did not choose to stop, so the
    /// message would describe something that did not happen, to an agent that
    /// did not do it. The recovery this gives up — an agent writing down what
    /// it knew without rebuilding anything — has a home already, and it is
    /// `NOTES.md`, which survives without a continuation.
    MemoryKill,
    /// The agent never announced a session id, so there is no conversation to
    /// hand anything back to.
    NoSessionId,
    /// The operator's command already chooses its own conversation.
    CommandSelectsSession { flag: String },
    /// The host stated no run budget, so there is no way to know whether a
    /// continuation could have been acted on — and claiming the agent was told
    /// when it may have had seconds is the one thing this guard exists to
    /// prevent.
    BudgetUnstated,
    /// There is budget left, and not enough of it.
    TooLittle { remaining_secs: u64, needed_secs: u64 },
}

impl NoContinuation {
    /// One clause for a log line or a terminal reason.
    pub fn describe(&self) -> String {
        match self {
            Self::Disabled => "continuing is disabled (max continuations is 0)".to_string(),
            Self::BudgetExhausted { used, max } => {
                format!("the continuation budget is spent ({used} of {max} used)")
            }
            Self::DeliverableProduced => "the run produced something to report".to_string(),
            Self::NotAConclusion { ending } => format!(
                "the agent did not end its own turn ({ending}), so it was never asked to \
                 continue one"
            ),
            Self::MemoryKill => {
                "the kernel OOM-killed a process during the run, so the agent did not choose to \
                 stop and telling it that it had would be false"
                    .to_string()
            }
            Self::NoSessionId => {
                "the agent's output never carried a session id, so there is no conversation to \
                 continue"
                    .to_string()
            }
            Self::CommandSelectsSession { flag } => {
                format!("the configured agent command already selects a session ({flag})")
            }
            Self::BudgetUnstated => {
                "the host did not say how much run budget was left, so there was no way to know \
                 whether a further attempt could be acted on (the server predates this field — \
                 restart it)"
                    .to_string()
            }
            Self::TooLittle {
                remaining_secs,
                needed_secs,
            } => format!(
                "only {remaining_secs}s of run budget remained, less than the {needed_secs}s one \
                 command may run for, so a further attempt could not have been acted on"
            ),
        }
    }
}

/// A run that has just ended, and everything the continuation decision reads
/// about it.
///
/// A struct rather than eight positional arguments, because the two counters
/// and the two budgets are easy to transpose and a transposed pair here is a
/// wrong fact under an attempt cap.
#[derive(Debug, Clone, Copy)]
pub struct Continuation<'a> {
    /// The stream the last attempt produced.
    pub watcher: &'a ResultWatcher,
    /// The last attempt's exit status and memory accounting.
    pub outcome: &'a AgentOutcome,
    /// The operator's configured command, program first.
    pub argv: &'a [String],
    /// Whose deliverable is missing — it decides two nouns in the prompt.
    pub role: AgentRole,
    /// Whether anything was produced.
    pub deliverable: Deliverable,
    /// What is left of the run, as the host stated it and this VM has spent it.
    pub budget: RunBudget,
    /// Continuations this run has already spent.
    pub used: u32,
    /// [`DEFAULT_MAX_CONTINUATIONS`], or the configured override.
    pub max: u32,
}

/// Whether to hand a run that produced nothing back to its own agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuationDecision {
    /// Re-invoke the agent with this argv and this prompt. `attempt` counts
    /// continuations, so the first one is 1.
    Continue {
        argv: Vec<String>,
        prompt: String,
        attempt: u32,
    },
    /// Let the run end, for this reason.
    Stop(NoContinuation),
}

/// Decide whether to tell an agent that produced nothing that there is no
/// later.
///
/// This is the answer to #962, where a Scout finished its implementation, put a
/// cold 750-crate build behind three `until [ -f /tmp/test.log ]` waiters, said
/// it would pick the result up when the tests reported, and ended its turn 490s
/// into a 3600s budget. Under `claude --print` the turn ending *is* the run
/// ending: the children were killed a moment later, the supervisor collected a
/// `SPEC.md` that was never written, and the task was charged a dispatch
/// attempt for a verdict nothing had reached.
///
/// # It is a second policy over one mechanism, never a second mechanism
///
/// [`decide`] answers "the connection dropped, pick it back up"; the Builder's
/// repair round answers "your own tests are red, fix them"; this answers "your
/// turn was the whole run and you produced nothing". Three questions, three
/// counters — netting any two would let one exhaust the other. What they share
/// is the re-invocation underneath: [`resume_argv`] builds the argv and
/// [`command_selects_session`] is the guard, asked here rather than restated,
/// which is what keeps the three from disagreeing about `--resume=<id>`.
///
/// # Why this is not a fourth `FailureClass`
///
/// An agent that parked on a background command and an agent that explored
/// honestly and concluded it could not conclude write the *same* terminal
/// record (`terminal_reason: completed`) and leave the same empty directory.
/// They are not distinguishable by inspection, and separating them on how much
/// of the budget was spent is exactly the inference this codebase refuses
/// everywhere else. So the ambiguity is removed rather than classified: what
/// comes back is a verdict either way — a spec, or an agent that was told
/// plainly and still produced nothing.
///
/// # The budget guard is what keeps that claim true
///
/// [`AgentRun::continuations`] puts "it was told" into the terminal reason, and
/// that only holds when the agent had time to answer. A run parking at 3400s of
/// 3600 would otherwise spend a continuation the agent could not act on and
/// record it as a telling — the ledger asserting something untrue, in the
/// direction that charges a strike. So the threshold is [`command_budget`], the
/// same half-the-run arithmetic the harness enforces per command: a further
/// attempt that cannot run one command cannot verify anything. It errs toward
/// declining, deliberately — declining costs one run's recovery, while
/// recording a telling that never happened puts a wrong fact under an attempt
/// cap that rejects a task at three.
pub fn decide_continuation(c: &Continuation<'_>) -> ContinuationDecision {
    if c.max == 0 {
        return ContinuationDecision::Stop(NoContinuation::Disabled);
    }
    if c.used >= c.max {
        return ContinuationDecision::Stop(NoContinuation::BudgetExhausted {
            used: c.used,
            max: c.max,
        });
    }
    if c.deliverable == Deliverable::Produced {
        return ContinuationDecision::Stop(NoContinuation::DeliverableProduced);
    }
    match c.watcher.ending() {
        AgentEnding::Concluded { .. } => {}
        AgentEnding::Silent => {
            return ContinuationDecision::Stop(NoContinuation::NotAConclusion {
                ending: "it produced no stream-json to classify",
            });
        }
        AgentEnding::NoResult => {
            return ContinuationDecision::Stop(NoContinuation::NotAConclusion {
                ending: "it wrote no terminal record, so it was killed from outside",
            });
        }
        AgentEnding::Transport { .. } => {
            return ContinuationDecision::Stop(NoContinuation::NotAConclusion {
                ending: "its API connection dropped",
            });
        }
    }
    if c.outcome.verdict.is_some() {
        return ContinuationDecision::Stop(NoContinuation::MemoryKill);
    }
    let Some(session_id) = c.watcher.session_id() else {
        return ContinuationDecision::Stop(NoContinuation::NoSessionId);
    };
    if let Some(flag) = command_selects_session(c.argv) {
        return ContinuationDecision::Stop(NoContinuation::CommandSelectsSession { flag });
    }
    // Last, and last on purpose: every reason above is a fact about the run,
    // and reporting "there was no time" about a run that had produced its
    // deliverable anyway would be noise in the one place a human is deciding
    // whether a strike was earned.
    let (Some(total), Some(remaining)) = (c.budget.total(), c.budget.remaining()) else {
        return ContinuationDecision::Stop(NoContinuation::BudgetUnstated);
    };
    let needed = command_budget(total);
    if remaining < needed {
        return ContinuationDecision::Stop(NoContinuation::TooLittle {
            remaining_secs: remaining.as_secs(),
            needed_secs: needed.as_secs(),
        });
    }

    ContinuationDecision::Continue {
        argv: resume_argv(c.argv, session_id),
        prompt: continuation_prompt(c.role),
        attempt: c.used + 1,
    }
}

/// What the continued agent reads on stdin.
///
/// Like [`RESUME_PROMPT`] it does **not** restate the task: the task is above
/// this in the conversation the `--resume` reattaches to, and re-sending it is
/// how a re-invocation silently becomes a restart.
///
/// Three things it must say, and each is load-bearing.
///
/// That the turn ending *is* the run ending, in as many words — because under
/// `claude --print` it is, and an agent that has just backgrounded a build has
/// demonstrably not been told.
///
/// That the background children are already dead. Not "will be": the previous
/// turn ended, so they were killed before this message was written, and an
/// agent told they *would* die may reasonably go back to waiting for one.
///
/// That "I cannot" is available, in as many words, and where to write it.
/// Without that the only exit is to produce something, which for a Scout is the
/// half-explored spec that reaches a reviewer looking finished — the exact
/// failure the `SPEC.md`/`NOTES.md` split exists to prevent.
pub fn continuation_prompt(role: AgentRole) -> String {
    let deliverable = role.deliverable();
    let shortfall = role.shortfall_artifact();
    let finish = match role {
        AgentRole::Scout => {
            "If you have concluded, write `SPEC.md` now. If you have not, do the smallest \
             remaining thing that would let you conclude — run it in the foreground, watch it \
             finish, and then write the spec."
        }
        AgentRole::Builder => {
            "If the implementation is done, commit it now. If it is not, finish the smallest \
             coherent piece of it, run what verifies that piece in the foreground, watch it \
             finish, and commit."
        }
    };
    format!(
        "STOP. Your turn ended and this run produced nothing.\n\n\
         Read this carefully, because it is probably not what you assumed.\n\n\
         **Your turn ending is this run ending.** You are running under `claude --print`: there \
         is no later turn, nothing polls for you, and no one is going to read a file you have \
         not written yet. The supervisor looked for {deliverable} and found none.\n\n\
         **Anything you put in the background is already dead.** Not \"will be killed\" — the \
         moment your last turn ended, every child process you had started was killed. A build \
         you backgrounded is not still building. A test run you were waiting on will never \
         report. A file it was going to write will never appear. If you ended your turn \
         intending to collect a result, that result does not exist and cannot be collected.\n\n\
         You have EXACTLY ONE more attempt. Use it in the foreground: run the command, wait for \
         it, read its output. One command may run for a long time here — far longer than the \
         default you may be used to — and the run budget is what bounds you, not the command \
         timeout.\n\n\
         Everything you already wrote is still on disk, in the same working directory, \
         unchanged. Do not start over and do not repeat work you have already done. \
         {finish}\n\n\
         **If you cannot conclude, say that instead — it is a real answer and an honest one is \
         worth more than an invented one.** Write what you actually know into `{shortfall}`: \
         what you established, what you ruled out, where you got to, and what you would do \
         next. That is read by whoever picks this up, and it is worth far more than something \
         made to look finished. Do not manufacture a conclusion you have not reached."
    )
}

/// Read a `*_MAX_CONTINUATIONS` variable, falling back to
/// [`DEFAULT_MAX_CONTINUATIONS`] when it is unset or unparseable. `0` disables
/// continuing.
pub fn max_continuations_from_env(var: &str) -> u32 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_MAX_CONTINUATIONS)
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
    /// The conversation the agent announced, if it announced one.
    ///
    /// Carried on the run rather than left in the [`ResultWatcher`] because a
    /// caller that wants to re-invoke the agent *after* the run concluded —
    /// the repair round for a red test suite — has the run and not the
    /// watcher. `None` is an agent that never streamed a session id, and the
    /// only honest response to it is not to resume.
    pub session_id: Option<String>,
    /// How many times the agent was told its turn was the whole run and handed
    /// the conversation back. `0` on an ordinary run.
    ///
    /// This is what makes a strike legible as earned: an empty run that was
    /// told plainly and still produced nothing is a verdict, where the same
    /// empty run untold is #962.
    pub continuations: u32,
    /// Why the run was not continued. Meaningful only when the agent ended its
    /// own turn — on a transport death "not continued: its API connection
    /// dropped" is noise beside [`Self::no_resume`], which already said it.
    pub no_continuation: Option<NoContinuation>,
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
            session_id: None,
            continuations: 0,
            no_continuation: None,
        }
    }

    /// Whether this run judged the work, for the supervisor to stamp on its
    /// terminal event (#884).
    ///
    /// Read off the *final* ending only. A run that was resumed twice and then
    /// concluded on its own terms is a verdict: the transport deaths cost it
    /// nothing that the resume did not give back, and the thing it finally
    /// said is what the host is being asked about.
    pub fn failure_class(&self) -> crate::FailureClass {
        if self.ending.is_transport() {
            crate::FailureClass::Transport
        } else {
            crate::FailureClass::Verdict
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
        out.push_str(&self.continuation_context());
        out.push_str(&self.outcome.failure_context());
        out
    }

    /// The clause that says whether this run was handed back to its agent, and
    /// on what terms.
    ///
    /// Three states, deliberately kept apart, because a human deciding whether
    /// three attempts were fair needs them apart: **told** (and still produced
    /// nothing — that is a verdict), **not told for want of time** (the
    /// continuation could not have been acted on, so nothing was claimed of the
    /// agent), and **not told for some other reason** (which names itself).
    ///
    /// Two silences. A run that ended in anything but a conclusion of the
    /// agent's own says nothing here, because [`Self::no_resume`] has already
    /// described that ending and repeating it under a second heading reads as
    /// two findings. And a run that produced its deliverable says nothing,
    /// because "the run produced something to report" is not a fact about a
    /// failure.
    fn continuation_context(&self) -> String {
        if self.continuations > 0 {
            return format!(
                " — told {} time(s) that its turn was the whole run and given a further \
                 attempt, and still produced nothing",
                self.continuations
            );
        }
        if self.ending.is_transport() {
            return String::new();
        }
        match &self.no_continuation {
            None | Some(NoContinuation::DeliverableProduced) => String::new(),
            Some(NoContinuation::NotAConclusion { .. }) => String::new(),
            Some(reason) => format!(" — not continued: {}", reason.describe()),
        }
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
    use std::time::Instant;

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
            resumes: 2,
            no_resume: Some(NoResume::BudgetExhausted { used: 2, max: 2 }),
            ..AgentRun::single(
                AgentOutcome::default(),
                AgentEnding::Transport {
                    terminal_reason: "api_error".into(),
                    api_error_status: Some("529".into()),
                },
            )
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
            no_resume: Some(NoResume::Concluded {
                terminal_reason: "completed".into(),
            }),
            ..AgentRun::single(
                AgentOutcome::default(),
                AgentEnding::Concluded {
                    terminal_reason: "completed".into(),
                },
            )
        };
        assert_eq!(run.failure_context(), "");
        assert_eq!(AgentRun::default().failure_context(), "");
    }

    /// The classification is a property of the *ending*, never of the prose.
    /// Both directions, because this is the guard against the thing #884
    /// forbids: a strike decision that greps a reason string would change
    /// meaning the next time someone improves a sentence.
    #[test]
    fn the_class_is_independent_of_the_reason_text() {
        use crate::FailureClass;

        // A run that concluded, whose terminal reason is full of the transport
        // vocabulary. Still a verdict: the agent said how it ended.
        let verdict = AgentRun::single(
            AgentOutcome::default(),
            AgentEnding::Concluded {
                terminal_reason: "the connection to the API failed, allegedly (api_error 529)"
                    .into(),
            },
        );
        assert_eq!(verdict.failure_class(), FailureClass::Verdict);

        // And a transport death whose reason text names nothing of the sort.
        let transport = AgentRun::single(
            AgentOutcome::default(),
            AgentEnding::Transport {
                terminal_reason: "completed, honest".into(),
                api_error_status: None,
            },
        );
        assert_eq!(transport.failure_class(), FailureClass::Transport);

        // A run with no stream-json at all — every shell-script agent, and the
        // shape a SIGKILL leaves — is a verdict. An OOM kill is deliberately
        // charged: a memory limit is a real property of the work in that VM.
        assert_eq!(
            AgentRun::single(
                AgentOutcome {
                    signal: Some("killed by signal 9 (SIGKILL)".into()),
                    ..Default::default()
                },
                AgentEnding::Silent,
            )
            .failure_class(),
            FailureClass::Verdict
        );

        // The *final* ending decides: a run resumed twice that then concluded
        // badly is a verdict, whatever killed the earlier attempts.
        let resumed_then_concluded = AgentRun {
            resumes: 2,
            ..AgentRun::single(
                AgentOutcome::default(),
                AgentEnding::Concluded {
                    terminal_reason: "completed".into(),
                },
            )
        };
        assert_eq!(
            resumed_then_concluded.failure_class(),
            FailureClass::Verdict
        );
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

    // ---- continuations (#962) -------------------------------------------

    /// A run that ended cleanly, produced nothing, and has most of an hour
    /// left — the #962 shape, where every guard is satisfied.
    fn parked() -> (ResultWatcher, AgentOutcome) {
        (watch(&[INIT, CLEAN_RESULT]), AgentOutcome::default())
    }

    fn continuation<'a>(
        watcher: &'a ResultWatcher,
        outcome: &'a AgentOutcome,
        argv: &'a [String],
    ) -> Continuation<'a> {
        Continuation {
            watcher,
            outcome,
            argv,
            role: AgentRole::Scout,
            deliverable: Deliverable::Nothing,
            budget: RunBudget::starting_now(Some(3600)),
            used: 0,
            max: DEFAULT_MAX_CONTINUATIONS,
        }
    }

    /// The failure the whole mechanism is for: a clean ending, an empty
    /// directory, and 3110s of the budget still unspent.
    #[test]
    fn an_empty_clean_ending_with_budget_left_is_continued_once() {
        let (w, o) = parked();
        let argv = base_argv();
        let ContinuationDecision::Continue {
            argv: next,
            prompt,
            attempt,
        } = decide_continuation(&continuation(&w, &o, &argv))
        else {
            panic!("the #962 shape must be continued");
        };
        assert_eq!(attempt, 1);
        assert_eq!(
            next.last().map(String::as_str),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(next[next.len() - 2], "--resume");
        assert!(prompt.contains("already dead"));

        // And exactly once: the second empty ending finds the budget spent.
        let spent = Continuation {
            used: 1,
            ..continuation(&w, &o, &argv)
        };
        assert_eq!(
            decide_continuation(&spent),
            ContinuationDecision::Stop(NoContinuation::BudgetExhausted { used: 1, max: 1 })
        );
    }

    /// Every gate names itself, so a continuation that did not happen is as
    /// readable as one that did — and so a reader can tell which guard fired.
    #[test]
    fn each_continuation_guard_stops_with_its_own_reason() {
        let (w, o) = parked();
        let argv = base_argv();
        let base = continuation(&w, &o, &argv);

        let cases: Vec<(Continuation<'_>, NoContinuation)> = vec![
            (Continuation { max: 0, ..base }, NoContinuation::Disabled),
            (
                Continuation {
                    used: 3,
                    max: 1,
                    ..base
                },
                NoContinuation::BudgetExhausted { used: 3, max: 1 },
            ),
            (
                Continuation {
                    deliverable: Deliverable::Produced,
                    ..base
                },
                NoContinuation::DeliverableProduced,
            ),
            (
                Continuation {
                    budget: RunBudget::starting_now(None),
                    ..base
                },
                NoContinuation::BudgetUnstated,
            ),
        ];
        for (case, expected) in cases {
            assert_eq!(
                decide_continuation(&case),
                ContinuationDecision::Stop(expected)
            );
        }

        // An ending that is not the agent's own, three ways. Each says which.
        let silent = watch(&["not json at all"]);
        let ContinuationDecision::Stop(NoContinuation::NotAConclusion { ending }) =
            decide_continuation(&Continuation {
                watcher: &silent,
                ..base
            })
        else {
            panic!("a silent agent is not a conclusion");
        };
        assert!(ending.contains("stream-json"), "{ending}");

        let no_result = watch(&[INIT]);
        let ContinuationDecision::Stop(NoContinuation::NotAConclusion { ending }) =
            decide_continuation(&Continuation {
                watcher: &no_result,
                ..base
            })
        else {
            panic!("a missing terminal record is not a conclusion");
        };
        assert!(ending.contains("killed from outside"), "{ending}");

        let dropped = watch(&[INIT, API_ERROR_RESULT]);
        let ContinuationDecision::Stop(NoContinuation::NotAConclusion { ending }) =
            decide_continuation(&Continuation {
                watcher: &dropped,
                ..base
            })
        else {
            panic!("a transport death is decide()'s question, never this one");
        };
        assert!(ending.contains("connection dropped"), "{ending}");

        // The kernel stopped it, so it did not choose to stop, so it must not
        // be told that it did.
        let oomed = AgentOutcome {
            verdict: Some("the kernel OOM-killed a process".into()),
            ..Default::default()
        };
        assert_eq!(
            decide_continuation(&Continuation {
                outcome: &oomed,
                ..base
            }),
            ContinuationDecision::Stop(NoContinuation::MemoryKill)
        );

        // No conversation to continue.
        let anonymous = watch(&[
            r#"{"type":"system","subtype":"init"}"#,
            r#"{"subtype":"success","terminal_reason":"completed","api_error_status":null,"type":"result"}"#,
        ]);
        assert_eq!(
            decide_continuation(&Continuation {
                watcher: &anonymous,
                ..base
            }),
            ContinuationDecision::Stop(NoContinuation::NoSessionId)
        );

        // The operator already chose one.
        let operators: Vec<String> = "claude --print --resume abc"
            .split_whitespace()
            .map(str::to_string)
            .collect();
        assert_eq!(
            decide_continuation(&Continuation {
                argv: &operators,
                ..base
            }),
            ContinuationDecision::Stop(NoContinuation::CommandSelectsSession {
                flag: "--resume".into()
            })
        );
    }

    /// A continuation started with no budget left is not a telling, and the
    /// terminal reason must not record it as one.
    ///
    /// The threshold is [`command_budget`] — half the run, floor 60s — because
    /// a further attempt that cannot run one command cannot verify anything.
    #[test]
    fn a_continuation_needs_enough_budget_left_to_be_acted_on() {
        let (w, o) = parked();
        let argv = base_argv();
        let base = continuation(&w, &o, &argv);

        // An hour's budget with ten minutes left: half the run is 1800s, so a
        // further attempt could not run one command.
        let starved = Continuation {
            budget: RunBudget::anchored(Instant::now() - Duration::from_secs(3000), Some(3600)),
            ..base
        };
        let ContinuationDecision::Stop(NoContinuation::TooLittle {
            remaining_secs,
            needed_secs,
        }) = decide_continuation(&starved)
        else {
            panic!("600s of an hour is not enough to be told anything about");
        };
        assert_eq!(needed_secs, 1800);
        assert!((595..=600).contains(&remaining_secs), "{remaining_secs}");

        // The #962 run parked at 490s of 3600 — 3110s left, comfortably over.
        let roomy = Continuation {
            budget: RunBudget::anchored(Instant::now() - Duration::from_secs(490), Some(3600)),
            ..base
        };
        assert!(matches!(
            decide_continuation(&roomy),
            ContinuationDecision::Continue { .. }
        ));

        // A host that stated nothing is its own answer, never a guess.
        assert_eq!(
            decide_continuation(&Continuation {
                budget: RunBudget::starting_now(None),
                ..base
            }),
            ContinuationDecision::Stop(NoContinuation::BudgetUnstated)
        );

        // The budget guard is asked LAST: a run that produced its deliverable
        // with no time left reports the deliverable, not the clock, because
        // "there was no time" is noise where nothing was owed.
        assert_eq!(
            decide_continuation(&Continuation {
                deliverable: Deliverable::Produced,
                budget: RunBudget::starting_now(Some(0)),
                ..base
            }),
            ContinuationDecision::Stop(NoContinuation::DeliverableProduced)
        );
    }

    /// A resume and a continuation name the same conversation the same way,
    /// which is the whole reason [`resume_argv`] is shared rather than copied.
    #[test]
    fn a_resume_and_a_continuation_name_the_same_conversation_the_same_way() {
        let argv = base_argv();
        let dropped = watch(&[INIT, API_ERROR_RESULT]);
        let ResumeDecision::Resume {
            argv: resumed_argv, ..
        } = decide(&dropped, &AgentOutcome::default(), &argv, 0, 2)
        else {
            panic!("a dropped connection resumes");
        };

        let (w, o) = parked();
        let ContinuationDecision::Continue {
            argv: continued_argv,
            ..
        } = decide_continuation(&continuation(&w, &o, &argv))
        else {
            panic!("the #962 shape continues");
        };

        assert_eq!(resumed_argv, continued_argv);
    }

    /// The prompt has to be *true* about the thing it is telling the agent, and
    /// it has to leave "I cannot" available with somewhere to write it.
    #[test]
    fn the_continuation_prompt_is_true_and_leaves_an_honest_exit() {
        for role in [AgentRole::Scout, AgentRole::Builder] {
            let p = continuation_prompt(role);
            assert!(
                p.contains("already dead"),
                "the children are dead now, not later: {p}"
            );
            assert!(
                p.contains("the moment your last turn ended"),
                "past tense, not future — an agent told its children *would* die \
                 may reasonably go back to waiting for one: {p}"
            );
            assert!(p.contains("EXACTLY ONE more attempt"), "{p}");
            assert!(p.contains("If you cannot conclude"), "{p}");
            assert!(p.contains(role.shortfall_artifact()), "{p}");
            assert!(
                p.contains("Do not start over"),
                "a continuation is not a restart: {p}"
            );
        }
        assert!(continuation_prompt(AgentRole::Scout).contains("NOTES.md"));
        assert!(continuation_prompt(AgentRole::Builder).contains("SUMMARY.md"));
    }

    /// Told, not-told-for-want-of-time, and not-told-for-another-reason are
    /// three different sentences, because a human deciding whether three
    /// attempts were fair has to tell them apart.
    #[test]
    fn the_terminal_reason_keeps_the_three_continuation_states_apart() {
        let concluded = || AgentEnding::Concluded {
            terminal_reason: "completed".into(),
        };

        let told = AgentRun {
            continuations: 1,
            ..AgentRun::single(AgentOutcome::default(), concluded())
        };
        assert!(
            told.failure_context()
                .contains("told 1 time(s) that its turn was the whole run"),
            "{}",
            told.failure_context()
        );

        let starved = AgentRun {
            no_continuation: Some(NoContinuation::TooLittle {
                remaining_secs: 600,
                needed_secs: 1800,
            }),
            ..AgentRun::single(AgentOutcome::default(), concluded())
        };
        let text = starved.failure_context();
        assert!(text.contains("not continued: only 600s"), "{text}");
        assert!(!text.contains("told"), "{text}");

        let never = AgentRun {
            no_continuation: Some(NoContinuation::Disabled),
            ..AgentRun::single(AgentOutcome::default(), concluded())
        };
        assert!(
            never.failure_context().contains("continuing is disabled"),
            "{}",
            never.failure_context()
        );

        // Two silences. A transport death already said why it ended, so it does
        // not also report that it was not continued...
        let dropped = AgentRun {
            no_continuation: Some(NoContinuation::NotAConclusion {
                ending: "its API connection dropped",
            }),
            ..AgentRun::single(
                AgentOutcome::default(),
                AgentEnding::Transport {
                    terminal_reason: "api_error".into(),
                    api_error_status: Some("529".into()),
                },
            )
        };
        assert!(!dropped.failure_context().contains("not continued"));

        // ...and neither does a run that had something to report.
        let produced = AgentRun {
            no_continuation: Some(NoContinuation::DeliverableProduced),
            ..AgentRun::single(AgentOutcome::default(), concluded())
        };
        assert_eq!(produced.failure_context(), "");
    }

    /// The env knob, and the direction a typo falls in.
    #[test]
    fn the_continuation_budget_defaults_to_one() {
        assert_eq!(DEFAULT_MAX_CONTINUATIONS, 1);
        // SAFETY: single-threaded test process, and the var is read once here.
        unsafe {
            std::env::set_var("TASKS_TEST_MAX_CONTINUATIONS", "0");
        }
        assert_eq!(max_continuations_from_env("TASKS_TEST_MAX_CONTINUATIONS"), 0);
        unsafe {
            std::env::set_var("TASKS_TEST_MAX_CONTINUATIONS", "not a number");
        }
        assert_eq!(
            max_continuations_from_env("TASKS_TEST_MAX_CONTINUATIONS"),
            DEFAULT_MAX_CONTINUATIONS,
            "an unparseable value falls back to the default rather than to zero"
        );
        unsafe {
            std::env::remove_var("TASKS_TEST_MAX_CONTINUATIONS");
        }
        assert_eq!(
            max_continuations_from_env("TASKS_TEST_MAX_CONTINUATIONS"),
            DEFAULT_MAX_CONTINUATIONS
        );
    }
}
