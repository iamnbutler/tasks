//! Server wire-up: the Diamond 1 loop.
//!
//! `tasks serve` composes three long-lived pieces over one store:
//!
//! - [`poll_loop`] — read-only GitHub intake. Every `TASKS_POLL_INTERVAL`
//!   seconds it upserts each project's open issues into tasks.
//! - [`dispatch_loop`] — picks the next task in queue order and hands it to a
//!   [`Scout`], up to `SCOUT_MAX_CONCURRENT` at a time.
//! - [`obligation_loop`] — recomputes what the pipeline is owed and says so,
//!   until a decision discharges it. The nudge loop tells the orchestrator
//!   something happened once; this one guarantees the work is not lost when
//!   that message dies with a failed turn.
//! - the HTTP control API ([`crate::server`]).
//!
//! Both loops read the operating mode from the store on every pass, so a
//! `POST /mode` takes effect within a tick:
//!
//! - `Play` — poll and dispatch.
//! - `Pause` — poll, but start no new scouts.
//! - `Stop` — neither poll nor dispatch.
//!
//! The mode a process *starts* in is configured, not remembered:
//! [`apply_startup_mode`] overwrites the stored value with
//! `TASKS_DEFAULT_MODE` (default [`DEFAULT_STARTUP_MODE`]) before the listener
//! binds. Starting a server is therefore never the same act as resuming
//! dispatch — a crash, a `launchd` restart or an infrastructure problem brings
//! the pipeline back quiet. The one exception is a deliberate upgrade:
//! [`crate::reload`] hands the running server's mode to its replacement
//! through the child's environment. See [`startup_mode_from_env`].
//!
//! Mode gates *new* dispatches only. A scout already in flight runs to
//! completion or to its deadline: there is no in-band cancel command (see
//! [`crate::protocol::ScoutCommand`]), so cancelling means deallocating the
//! VM. That is exactly what `SCOUT_TIMEOUT_SECS` does when a scout hangs —
//! a timeout is a dispatch failure like any other, not a mode concern.
//!
//! Crash consistency is the store's job, not memory's. Startup first calls
//! [`resume_in_flight`] — scouts and builds run inside VMs, and vm-pool is a
//! separate daemon that keeps those VMs alive across a restart, so what a
//! restart loses is the event stream and nothing else — and only then
//! [`reconcile_startup`], which writes off whatever genuinely is gone. A
//! task's failed dispatches are counted on its row, so restarts can't hand a
//! task that can never be scouted three fresh attempts every time.
//!
//! Nothing here is required for the API to work. A missing `GITHUB_TOKEN`
//! disables polling and an unreachable vm-pool socket disables dispatch (with
//! periodic reconnect); the server stays up either way so manual flows over
//! HTTP keep working.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};
use vm_pool_client::{Client, ClientError};
use vm_pool_protocol::{VmConfig, VmId};

use crate::brief::Brief;
use crate::builder::{Builder, BuilderConfig, BuilderError};
use crate::bundles::RejectedBundles;
use crate::events::EventPayload;
use crate::github::{GitHubClient, IntakeFilter, PrState};
use crate::models::{
    Actor, Capability, CharterLevel, ChatRole, CloseReason, DecisionAction, DecisionInput, GhState,
    Mode, Project, Spec, Task, TaskId, TaskState,
};
use crate::orchestrator::{self, Orchestrator, OrchestratorConfig};
use crate::protocol::TasksProtocol;
use crate::reattach;
use crate::scout::{Scout, ScoutConfig, ScoutError, ScoutTarget};
use crate::server;
use crate::store::{ReconcileReport, ResumedWork, Store, StoreError, Strike};

pub const DEFAULT_PORT: u16 = 4800;
const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;
const DEFAULT_SCOUT_MAX_CONCURRENT: usize = 2;
const DEFAULT_SCOUT_IMAGE: &str = "agent:v1";
const DEFAULT_BUILDER_IMAGE: &str = "builder:v1";
const DEFAULT_VM_POOL_SOCKET: &str = "/tmp/vm-pool.sock";
/// Builds get the same wall-clock budget as scouts, for the same reason: it
/// must sit below vm-pool's 7200s reaper so the app deadline fires first.
const DEFAULT_BUILDER_TIMEOUT_SECS: u64 = 3600;
/// Default agent command for orchestrator ticks. `--allowedTools` lets the
/// headless run curl the tasks API without permission prompts — and nothing
/// else beyond Claude Code's defaults. stream-json (+ partial messages) is
/// what feeds `/orchestrator/stream`: text deltas and tool calls surface live
/// instead of one opaque multi-minute wait. `--verbose` is mandatory with
/// `--print --output-format stream-json` (see images/scout/Dockerfile).
const DEFAULT_ORCHESTRATOR_CMD: &str = "claude --print --output-format stream-json --verbose \
     --include-partial-messages --allowedTools Bash(curl:*)";
/// Wall-clock budget for one orchestrator tick.
///
/// Fifteen minutes rather than ten because a turn that verifies a composition
/// may have to pay for one cold build, and Claude Code's own per-command
/// ceiling has to fit *below* the turn (see
/// [`orchestrator::command_budget`](crate::orchestrator::command_budget)) — at
/// 600s against a 600s ceiling a single command could consume the whole turn
/// and leave nothing to report in. Bounded above by `OBLIGATION_REMINDER`, so
/// a turn can never outlast the interval at which the pipeline re-states what
/// it is owed. This is *with* the warm build directory, not instead of it:
/// alone it would only spend more wall-clock on the same cold build.
const DEFAULT_ORCHESTRATOR_TIMEOUT_SECS: u64 = 900;
/// Where the orchestrator builds when it verifies, unless `ORCHESTRATOR_TARGET_DIR`
/// says otherwise. Shared and long-lived on purpose — the warmth is the whole
/// value.
const DEFAULT_ORCHESTRATOR_TARGET_DIR: &str = "verify-target";
/// How often the orchestrator loop checks for unanswered input turns.
const ORCHESTRATOR_TICK: Duration = Duration::from_secs(1);
/// Debounce for pipeline-event nudges: after the first nudge-worthy event,
/// wait for this much quiet so a burst (a poller ingesting ten issues, a
/// scout finishing + its spec landing) becomes ONE event turn — every nudge
/// costs an agent turn.
const NUDGE_DEBOUNCE: Duration = Duration::from_secs(5);
/// Hard cap on how long a steady event trickle can hold a nudge open.
const NUDGE_MAX_WAIT: Duration = Duration::from_secs(30);
/// Wall-clock budget for one scout. ~2.5x the observed 23-minute live run, and
/// deliberately below vm-pool's own `PoolConfig::vm_timeout` (7200s): if the
/// app deadline sat at or above the pool's reaper, infrastructure would tear
/// the VM down first and the dispatcher would report a stream error instead of
/// its own timeout — same recovery, worse diagnostics. Raising this past ~2h
/// means raising the pool's `vm_timeout` too.
const DEFAULT_SCOUT_TIMEOUT_SECS: u64 = 3600;
const DEFAULT_CLONE_URL_BASE: &str = "https://github.com";

/// The mode every boot starts in when `TASKS_DEFAULT_MODE` says nothing.
///
/// `Pause` rather than `Stop`: a server that came back on its own should not
/// dispatch, but it should still ingest issues and answer the API, so a human
/// arriving at it sees the current state of the world and one button to press.
pub const DEFAULT_STARTUP_MODE: Mode = Mode::Pause;

/// `source` on the breadcrumb [`apply_startup_mode`] writes.
const STARTUP: &str = "startup";

/// How often the dispatch loop re-reads mode + queue.
const DISPATCH_TICK: Duration = Duration::from_millis(500);

/// How long to wait before retrying a vm-pool connection.
const VM_POOL_RETRY: Duration = Duration::from_secs(10);

/// How long in-flight scouts and builds get to finish after ctrl_c before we
/// walk away.
///
/// Thirty seconds is right *because* of reattachment, not in spite of it:
/// walking away from a scout now costs the wait until the successor attaches,
/// not the run. A long drain would buy back something the successor already
/// provides, and it would buy it at the price of a slow restart — which is
/// the thing operators actually feel. Don't lengthen it.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// How long the *background* loops — poll, nudge, obligations — get, in total,
/// before they are abandoned and named.
///
/// Short, and safe by construction, which is why these get a leash where a
/// scout gets thirty seconds: a poll is idempotent, obligations are recomputed
/// from state on every pass, and a nudge is a latency optimization the answered
/// watermark makes good. Nothing here is work in progress.
///
/// The arithmetic matters. These three used to be awaited **unbounded**, and
/// [`crate::reload`] SIGKILLs a server that has not exited within its own
/// `STOP_GRACE` of 75s — so one loop that missed its shutdown flag turned into
/// a hard kill with nothing in `serve.log` naming it, which is exactly what
/// #883 cost a scout run to diagnose. Bounded, the whole drain is 10 + 30 + 30
/// = 70s and still fits inside the 75, so a graceful stop stays graceful even
/// when every stage runs out its clock.
const BACKGROUND_GRACE: Duration = Duration::from_secs(10);

/// `source` on the breadcrumbs the dispatcher writes to the event log.
/// `pub(crate)` because [`crate::scout`] emits the timeout note under the same
/// source — it has the vm id and the deadline that make the entry useful.
pub(crate) const DISPATCHER: &str = "dispatcher";

/// `source` on breadcrumbs about the orchestrator's own lifecycle.
const ORCHESTRATOR: &str = "orchestrator";

/// Consecutive failed dispatches after which a task is rejected outright.
/// Matches the re-explore cap the plan puts on the server rather than the
/// orchestrator. The count lives in the store (`tasks.dispatch_attempts`), so
/// restarts don't hand a poison task a fresh set of strikes.
const MAX_DISPATCH_ATTEMPTS: u32 = 3;

#[derive(Debug, Error)]
pub enum RunError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("config: {0}")]
    Config(#[from] ConfigError),
    #[error("http server: {0}")]
    Io(#[from] std::io::Error),
    /// Another server already owns this data dir. Named separately because
    /// the fix is a specific command, not a diagnosis.
    #[error(
        "a tasks server is already running (pid {pid}, port {port}); \
         use `tasks reload` to swap it, or `tasks stop` to shut it down"
    )]
    AlreadyRunning { pid: u32, port: u16 },
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{var} is not {expected}: {value}")]
    Invalid {
        var: &'static str,
        expected: &'static str,
        value: String,
    },
    #[error("HOME environment variable not set")]
    NoHome,
}

/// Everything `serve` reads from the environment, resolved once at startup.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where `tasks.db` lives (`TASKS_DATA_DIR`).
    pub data_dir: PathBuf,
    /// HTTP API port (`TASKS_SERVER_PORT`).
    pub port: u16,
    /// Gap between GitHub polls (`TASKS_POLL_INTERVAL`, seconds).
    pub poll_interval: Duration,
    /// The mode this boot puts the pipeline into (`TASKS_DEFAULT_MODE`),
    /// overwriting whatever the last process left behind. See
    /// [`apply_startup_mode`].
    pub startup_mode: Mode,
    /// Concurrent scout dispatches (`SCOUT_MAX_CONCURRENT`).
    pub scout_max_concurrent: usize,
    /// vm-pool image scouts run in (`SCOUT_IMAGE`).
    pub scout_image: String,
    /// Wall-clock budget for one scout (`SCOUT_TIMEOUT_SECS`). Past it the VM
    /// is deallocated and the attempt counts as a dispatch failure.
    pub scout_timeout: Duration,
    /// vm-pool service socket (`VM_POOL_SOCKET`).
    pub vm_pool_socket: PathBuf,
    /// `GITHUB_TOKEN`. Absent disables polling and leaves clone URLs anonymous.
    pub github_token: Option<String>,
    /// GraphQL endpoint override (`GITHUB_API_URL`) — GitHub Enterprise, tests.
    pub github_api_url: Option<String>,
    /// Which fetched issues intake accepts (`TASKS_INTAKE_LABEL`). Unset means
    /// every open issue, which is the historical behaviour.
    pub intake: IntakeFilter,
    /// Prefix for derived clone URLs (`GITHUB_CLONE_URL_BASE`); the full URL is
    /// `<base>/<owner>/<repo>.git`.
    pub clone_url_base: String,
    /// Branch scouts base their throwaway branch on (`SCOUT_BASE_BRANCH`).
    /// Future: per-project config.
    pub scout_base_branch: String,
    /// VM shape requested from vm-pool for each scout (`SCOUT_VM_*`).
    pub vm_config: VmConfig,
    /// VM shape requested from vm-pool for each build (`BUILDER_VM_*`).
    /// Deliberately larger than a scout's: builds are serial, so a Builder's
    /// memory is not multiplied by anything, and a Builder killed halfway
    /// costs a whole implementation rather than one exploration.
    pub builder_vm_config: VmConfig,
    /// vm-pool image builds run in (`BUILDER_IMAGE`).
    pub builder_image: String,
    /// Wall-clock budget for one build (`BUILDER_TIMEOUT_SECS`).
    pub builder_timeout: Duration,
    /// REST endpoint override (`GITHUB_REST_API_URL`) — PR creation only.
    pub github_rest_api_url: Option<String>,
    /// Agent command for orchestrator ticks (`ORCHESTRATOR_CMD`).
    pub orchestrator_cmd: String,
    /// Wall-clock budget for one orchestrator tick (`ORCHESTRATOR_TIMEOUT_SECS`).
    pub orchestrator_timeout: Duration,
    /// Working directory for the orchestrator agent (`ORCHESTRATOR_WORKDIR`).
    /// Default is a neutral dir under the data dir; point it at a dedicated
    /// repo clone to run the orchestrator as a full development agent
    /// (pair with `--dangerously-skip-permissions` in `ORCHESTRATOR_CMD`).
    pub orchestrator_workdir: Option<PathBuf>,
    /// Build directory for the orchestrator's own verification
    /// (`ORCHESTRATOR_TARGET_DIR`), set as `CARGO_TARGET_DIR` on the agent
    /// child and nowhere else.
    ///
    /// A `git worktree` gets its own empty `target/`, so verifying that N pull
    /// requests compose meant a cold workspace debug build — minutes before a
    /// single test could run, which is why a typecheck was the ceiling on what
    /// a merge decision could rest on. Shared and long-lived is the point.
    ///
    /// Scoped to the one child process rather than living in `<data dir>/.env`,
    /// which every `tasks` invocation reads: a `CARGO_TARGET_DIR` there would
    /// be inherited by `tasks reload`'s own build of the server and would
    /// silently redirect the Makefile's `TEST_BIN_DIR`.
    pub orchestrator_target_dir: Option<PathBuf>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        // Resolved once and shared by both roles: the apiKeyHelper path shells
        // out to a script, and it should not run twice per startup.
        let credentials = agent_credentials_env();
        Ok(Self {
            data_dir: data_dir()?,
            port: parse_env("TASKS_SERVER_PORT", "a port number", DEFAULT_PORT)?,
            poll_interval: Duration::from_secs(parse_env(
                "TASKS_POLL_INTERVAL",
                "a number of seconds",
                DEFAULT_POLL_INTERVAL_SECS,
            )?),
            startup_mode: startup_mode_from_env()?,
            scout_max_concurrent: parse_env(
                "SCOUT_MAX_CONCURRENT",
                "a positive integer",
                DEFAULT_SCOUT_MAX_CONCURRENT,
            )?
            .max(1),
            scout_image: env_string("SCOUT_IMAGE").unwrap_or_else(|| DEFAULT_SCOUT_IMAGE.into()),
            scout_timeout: Duration::from_secs(parse_env(
                "SCOUT_TIMEOUT_SECS",
                "a number of seconds",
                DEFAULT_SCOUT_TIMEOUT_SECS,
            )?),
            vm_pool_socket: env_string("VM_POOL_SOCKET")
                .unwrap_or_else(|| DEFAULT_VM_POOL_SOCKET.into())
                .into(),
            github_token: env_string("GITHUB_TOKEN"),
            github_api_url: env_string("GITHUB_API_URL"),
            intake: IntakeFilter::from_label(env_string("TASKS_INTAKE_LABEL")),
            clone_url_base: env_string("GITHUB_CLONE_URL_BASE")
                .unwrap_or_else(|| DEFAULT_CLONE_URL_BASE.into()),
            scout_base_branch: env_string("SCOUT_BASE_BRANCH").unwrap_or_else(|| "main".into()),
            vm_config: agent_vm_config(SCOUT_VM, &credentials)?,
            builder_vm_config: agent_vm_config(BUILDER_VM, &credentials)?,
            builder_image: env_string("BUILDER_IMAGE")
                .unwrap_or_else(|| DEFAULT_BUILDER_IMAGE.into()),
            builder_timeout: Duration::from_secs(parse_env(
                "BUILDER_TIMEOUT_SECS",
                "a number of seconds",
                DEFAULT_BUILDER_TIMEOUT_SECS,
            )?),
            github_rest_api_url: env_string("GITHUB_REST_API_URL"),
            orchestrator_cmd: env_string("ORCHESTRATOR_CMD")
                .unwrap_or_else(|| DEFAULT_ORCHESTRATOR_CMD.into()),
            orchestrator_timeout: Duration::from_secs(parse_env(
                "ORCHESTRATOR_TIMEOUT_SECS",
                "a number of seconds",
                DEFAULT_ORCHESTRATOR_TIMEOUT_SECS,
            )?),
            orchestrator_workdir: env_string("ORCHESTRATOR_WORKDIR").map(PathBuf::from),
            orchestrator_target_dir: env_string("ORCHESTRATOR_TARGET_DIR").map(PathBuf::from),
        })
    }

    /// The working directory the orchestrator agent runs in:
    /// `ORCHESTRATOR_WORKDIR` (the repo checkout, in production) or a neutral
    /// dir under the data dir.
    fn agent_workdir(&self) -> PathBuf {
        self.orchestrator_workdir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("orchestrator"))
    }

    /// The build directory the orchestrator verifies in:
    /// `ORCHESTRATOR_TARGET_DIR`, or `<data dir>/verify-target`.
    ///
    /// There is deliberately no `off` value. Every setting here is a path, so a
    /// sentinel that could also be a directory name is a worse ambiguity than
    /// the one it resolves; `ORCHESTRATOR_TARGET_DIR=<checkout>/target` restores
    /// the old behaviour exactly and is the escape hatch.
    pub fn orchestrator_target_dir(&self) -> PathBuf {
        self.orchestrator_target_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join(DEFAULT_ORCHESTRATOR_TARGET_DIR))
    }

    /// Where the orchestrator's actor credential is written.
    ///
    /// Under the data dir, never [`Config::agent_workdir`]: in production
    /// that is the repo checkout the agent commits from, so a secret there is
    /// one `git add -A` from being published.
    fn orchestrator_curl_config(&self) -> PathBuf {
        self.data_dir.join("orchestrator-curl.conf")
    }

    fn github_client(&self) -> Option<GitHubClient> {
        let token = self.github_token.as_ref()?;
        let client = match &self.github_api_url {
            Some(url) => GitHubClient::with_base_url(token, url),
            None => GitHubClient::new(token),
        };
        Some(match &self.github_rest_api_url {
            Some(url) => client.with_rest_base_url(url),
            None => client,
        })
    }
}

fn env_string(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.is_empty())
}

/// The env vars and defaults that shape one role's VM. A role is a set of
/// knobs, not a set of behaviours: Scout and Builder differ only in the
/// numbers, so they share [`agent_vm_config`] and the derivation below.
struct VmRole {
    cpus_var: &'static str,
    memory_var: &'static str,
    /// Overrides the derived `CARGO_BUILD_JOBS` outright, for a host that
    /// knows better than the formula.
    jobs_var: &'static str,
    default_cpus: u32,
    default_memory_mb: u32,
}

/// Scouts run `SCOUT_MAX_CONCURRENT` at a time, so their memory is the number
/// that gets multiplied on a small host. 6 GB derives 2 build jobs.
const SCOUT_VM: VmRole = VmRole {
    cpus_var: "SCOUT_VM_CPUS",
    memory_var: "SCOUT_VM_MEMORY_MB",
    jobs_var: "SCOUT_BUILD_JOBS",
    default_cpus: 4,
    default_memory_mb: 6144,
};

/// Builds are serial: exactly one of these exists at a time, and losing one
/// costs a whole implementation. 8 GB derives 3 build jobs.
const BUILDER_VM: VmRole = VmRole {
    cpus_var: "BUILDER_VM_CPUS",
    memory_var: "BUILDER_VM_MEMORY_MB",
    jobs_var: "BUILDER_BUILD_JOBS",
    default_cpus: 4,
    default_memory_mb: 8192,
};

/// Memory set aside for everything that is not a cargo job: the agent process
/// itself, the supervisor, git, and the page cache a build churns through.
const AGENT_RESERVE_MEMORY_MB: u32 = 2048;
/// Memory to budget per concurrent cargo job. Linking this workspace's test
/// binaries is the peak, and it is what the OOM reports were about.
const BUILD_JOB_MEMORY_MB: u32 = 2048;

/// How many concurrent cargo jobs a VM of this shape can afford.
///
/// Cargo defaults `-j` to the CPU count and knows nothing about the memory
/// limit, which is the whole bug: 4 CPUs against 4 GB runs four concurrent
/// links of this workspace and the kernel kills one. Deriving the job count
/// from memory instead — and injecting it per-VM as `CARGO_BUILD_JOBS` —
/// states the rule once for every role rather than once per image.
///
/// Both constants are worth revisiting as the workspace grows; they are
/// calibrated so the shape that was failing (4 CPU / 4 GB) derives the single
/// job the field reports say completes.
fn build_jobs(cpus: u32, memory_mb: u32) -> u32 {
    let for_jobs = memory_mb.saturating_sub(AGENT_RESERVE_MEMORY_MB);
    (for_jobs / BUILD_JOB_MEMORY_MB).clamp(1, cpus.max(1))
}

/// Build one role's VM shape: cpus, memory, and the environment its agent
/// runs with (credentials plus the derived `CARGO_BUILD_JOBS`).
///
/// The images pin `CARGO_BUILD_JOBS=1` as a floor for hand-started
/// containers; this env entry overrides it, so the server's arithmetic always
/// wins for a VM the server allocated.
fn agent_vm_config(
    role: VmRole,
    credentials: &[(String, String)],
) -> Result<VmConfig, ConfigError> {
    let cpus: u32 = parse_env(role.cpus_var, "a positive integer", role.default_cpus)?.max(1);
    let memory_mb: u32 = parse_env(role.memory_var, "a size in MB", role.default_memory_mb)?;
    let jobs: u32 = parse_env(
        role.jobs_var,
        "a positive integer",
        build_jobs(cpus, memory_mb),
    )?
    .max(1);

    let mut env = credentials.to_vec();
    env.push(("CARGO_BUILD_JOBS".into(), jobs.to_string()));
    Ok(VmConfig {
        cpus: Some(cpus),
        memory_mb: Some(memory_mb),
        env,
        ..VmConfig::default()
    })
}

/// Credentials for the agent inside a Scout or Builder VM, resolved host-side
/// once at startup: `ANTHROPIC_API_KEY` from the environment, else the output
/// of the host's Claude Code `apiKeyHelper` script
/// (`~/.claude/anthropic_key.sh`) when one exists. Injected per-VM via
/// `VmConfig.env`, never baked into images. Empty means agents will fail auth
/// — warned at startup.
fn agent_credentials_env() -> Vec<(String, String)> {
    if let Some(key) = env_string("ANTHROPIC_API_KEY") {
        return vec![("ANTHROPIC_API_KEY".into(), key)];
    }
    let helper = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".claude/anthropic_key.sh"));
    if let Some(helper) = helper.filter(|p| p.exists()) {
        match std::process::Command::new(&helper).output() {
            Ok(out) if out.status.success() => {
                let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !key.is_empty() {
                    tracing::info!("agent credentials: host apiKeyHelper");
                    return vec![("ANTHROPIC_API_KEY".into(), key)];
                }
            }
            Ok(out) => {
                tracing::warn!(status = %out.status, "apiKeyHelper failed");
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not run apiKeyHelper");
            }
        }
    }
    tracing::warn!("no agent credentials found — scouts and builds will fail agent auth");
    Vec::new()
}

fn parse_env<T: std::str::FromStr>(
    var: &'static str,
    expected: &'static str,
    default: T,
) -> Result<T, ConfigError> {
    match env_string(var) {
        Some(raw) => raw.parse().map_err(|_| ConfigError::Invalid {
            var,
            expected,
            value: raw,
        }),
        None => Ok(default),
    }
}

/// The mode a boot starts in, out of `TASKS_DEFAULT_MODE`.
///
/// Public because [`crate::reload`] resolves the same value from the same
/// environment, one step *before* it builds: it has to know what the server it
/// is about to start would come up in, and a typo here must cost nothing while
/// the old server is still up and unsignalled.
pub fn startup_mode_from_env() -> Result<Mode, ConfigError> {
    parse_startup_mode(env_string("TASKS_DEFAULT_MODE").as_deref())
}

/// The pure half of [`startup_mode_from_env`] — separated so it is testable
/// without mutating the process environment, which under `cargo test` is
/// shared by every test in the binary.
///
/// An unparseable value is a hard startup error rather than a fallback to the
/// default. This variable decides whether a machine comes back dispatching, so
/// "it was silently ignored" is the one outcome that must not be possible.
fn parse_startup_mode(raw: Option<&str>) -> Result<Mode, ConfigError> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_STARTUP_MODE);
    };
    Mode::from_str(raw.trim()).ok_or(ConfigError::Invalid {
        var: "TASKS_DEFAULT_MODE",
        expected: "play, pause or stop",
        value: raw.to_string(),
    })
}

/// Put the pipeline into `mode`, whatever the last process left in the store.
///
/// Called from [`run`] immediately after the store opens and **before**
/// [`server::bind`], so no client — and no `tasks reload` verifying a swap —
/// can ever observe the previous run's mode on this process.
///
/// The stored column is deliberately kept rather than deleted: it is still the
/// live mode for the rest of this process's life (`GET /mode`, `POST /mode`
/// and the three loops all read it every tick), it has just stopped being
/// consulted at boot. **Do not delete it** — a field that is written and never
/// read *at startup* is exactly the kind of thing a later cleanup removes
/// without noticing it is load-bearing at runtime.
///
/// The breadcrumb is a [`EventPayload::Note`] and not `ModeChanged` on
/// purpose: [`orchestrator::nudge_worthy`] treats a mode change as news, so a
/// boot-time transition would spend an agent turn on every single restart, on
/// something the orchestrator has no capability to act on. Clients lose
/// nothing, because the transition happens before the listener binds — it is
/// only ever reached through the reconnect-and-resnapshot a restart forces
/// anyway.
pub async fn apply_startup_mode(store: &Store, mode: Mode) -> Result<(), StoreError> {
    let stored = store.get_mode().await?;
    store.set_mode(mode).await?;
    if stored == mode {
        info!(mode = mode.as_str(), "startup mode");
        return Ok(());
    }
    info!(
        mode = mode.as_str(),
        was = stored.as_str(),
        "startup mode (the stored mode is not resumed; see TASKS_DEFAULT_MODE)"
    );
    store
        .append_event(EventPayload::Note {
            source: STARTUP.into(),
            message: format!(
                "startup mode {} (was {}); a restart does not resume the previous mode",
                mode.as_str(),
                stored.as_str()
            ),
        })
        .await?;
    Ok(())
}

/// `$TASKS_DATA_DIR`, else `$HOME/.local/state/tasks-v2`.
///
/// The rule itself lives in [`tasks_api::paths`] — clients resolve the same
/// dir to find the pidfile, and two answers to "which server?" would be one
/// too many. All this adds is the server's own way of saying "no `$HOME`".
pub fn data_dir() -> Result<PathBuf, ConfigError> {
    tasks_api::paths::data_dir().ok_or(ConfigError::NoHome)
}

/// Open (creating as needed) the store under `data_dir`.
///
/// Logs which migrations this open applied, so the answer exists in
/// `serve.log` even when nobody ran `tasks reload` to watch the swap.
pub async fn open_store(data_dir: &Path) -> Result<Store, RunError> {
    tokio::fs::create_dir_all(data_dir).await?;
    let db_path = data_dir.join("tasks.db");
    info!(db = %db_path.display(), "opening store");
    let store = Store::open(&db_path).await?;
    match store.migrations_applied() {
        [] => info!("schema already current"),
        applied => info!(
            count = applied.len(),
            migrations = %applied
                .iter()
                .map(|m| m.file_stem())
                .collect::<Vec<_>>()
                .join(", "),
            "applied migrations"
        ),
    }
    Ok(store)
}

/// Resolve on ctrl-c or SIGTERM, whichever comes first, and say which.
///
/// SIGTERM is the standard restart signal, and `serve` used to have no
/// handler for it: the default disposition kills the process outright, so a
/// plain `kill` meant no graceful drain, no pidfile cleanup, and every
/// in-flight scout lost. A graceful swap is only possible because this exists.
async fn stop_signal() -> &'static str {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(err) => {
            // Registering the handler failed — degrade to ctrl-c rather than
            // refusing to serve.
            warn!(error = %err, "could not install SIGTERM handler");
            let _ = tokio::signal::ctrl_c().await;
            return "ctrl-c";
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "ctrl-c",
        _ = term.recv() => "SIGTERM",
    }
}

/// Work this process is following that is not in either loop's own
/// bookkeeping: runs picked back up by [`resume_in_flight`].
///
/// Resumed runs are invisible to the loops that would otherwise account for
/// them — they live in their own spawned tasks, not in the dispatch loop's
/// `JoinSet`. Without these counters a restart with two scouts still going
/// would start two *more* and exhaust the pool, and the serial build lane
/// would stop being serial.
#[derive(Debug, Clone, Default)]
pub struct InFlight {
    scouts: Arc<AtomicUsize>,
    builds: Arc<AtomicUsize>,
}

impl InFlight {
    fn scouts(&self) -> usize {
        self.scouts.load(Ordering::SeqCst)
    }

    fn builds(&self) -> usize {
        self.builds.load(Ordering::SeqCst)
    }
}

/// Increments a counter for as long as it lives. A guard rather than a pair of
/// calls because the decrement must survive every exit path from the task that
/// holds it, panics included — a counter that only leaks upwards silently
/// throttles the pipeline to nothing.
struct InFlightGuard(Arc<AtomicUsize>);

impl InFlightGuard {
    fn hold(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter.clone())
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Run the server until ctrl_c.
///
/// Startup is [`resume_in_flight`] then [`reconcile_startup`], in that order:
/// pick up what is still alive, then write off only what is genuinely gone.
/// Reversed, reconciliation would kill the runs reattachment exists to save.
///
/// Two processes must never share a data dir, so the pidfile is checked
/// *before* the store is opened: `Store::open` runs migrations, and a second
/// process that refused a moment later would already have migrated the
/// running server's database on its way in.
///
/// The mode comes from [`apply_startup_mode`] and not from the store, before
/// anything binds: starting a server is not the same act as resuming dispatch.
///
/// Shutdown is a hand-over rather than an outage. The HTTP listener is held
/// through the whole drain — the API keeps answering while in-flight work
/// finishes — and is released last, when this function returns. That is a real
/// trade: a successor cannot bind the port until this process exits. It is the
/// right one, because two processes both driving dispatch would be far worse
/// than a wait.
///
/// The drain itself: the poll loop exits at once, scouts and builds get
/// [`SHUTDOWN_GRACE`] (short, because a successor reattaches to whatever is
/// abandoned), and the orchestrator's turn gets the same grace — it is a local
/// child so nothing can pick it up, but an unbounded wait only means `reload`
/// SIGKILLs us at its own deadline instead. An abandoned turn is reported at
/// the next boot. A second ctrl_c abandons the drain outright.
pub async fn run(config: Config) -> Result<(), RunError> {
    if let Some(existing) = crate::pidfile::read_live(&config.data_dir) {
        return Err(RunError::AlreadyRunning {
            pid: existing.pid,
            port: existing.port,
        });
    }

    let store = Arc::new(open_store(&config.data_dir).await?);

    // Before the listener binds: the mode this process runs in is decided by
    // configuration, not by what the last one left behind, and nobody should
    // ever be able to read the previous run's mode off this server.
    apply_startup_mode(&store, config.startup_mode).await?;

    // The port is taken here so a clash is a startup error, before any work is
    // resumed — but the listener is not dropped until this function returns.
    let listener = server::bind(config.port).await?;

    let in_flight = InFlight::default();
    let resumed = resume_in_flight(&store, &config, &in_flight).await;
    reconcile_startup_except(&store, &resumed).await?;
    report_interrupted_orchestrator_turn(&store).await;

    // After reconciliation: until that has run, this process is not yet the
    // owner of the work in the store.
    match crate::pidfile::write(&config.data_dir, config.port).await {
        Ok(file) => info!(pid = file.pid, port = file.port, exe = %file.exe.display(), "serving"),
        Err(err) => warn!(error = %err, "could not write pidfile"),
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let poll = tokio::spawn(poll_loop(
        store.clone(),
        config.clone(),
        shutdown_rx.clone(),
    ));
    let dispatch = tokio::spawn(dispatch_loop(
        store.clone(),
        config.clone(),
        in_flight.clone(),
        shutdown_rx.clone(),
    ));
    let build = tokio::spawn(build_loop(
        store.clone(),
        config.clone(),
        in_flight.clone(),
        shutdown_rx.clone(),
    ));
    let orchestrate = tokio::spawn(orchestrator_loop(
        store.clone(),
        config.clone(),
        shutdown_rx.clone(),
    ));
    let nudge = tokio::spawn(orchestrator_nudge_loop(
        store.clone(),
        config.clone(),
        NUDGE_DEBOUNCE,
        NUDGE_MAX_WAIT,
        shutdown_rx.clone(),
    ));
    let obligations = tokio::spawn(obligation_loop(
        store.clone(),
        config.clone(),
        OBLIGATION_GRACE,
        OBLIGATION_REMINDER,
        OBLIGATION_TICK,
        shutdown_rx.clone(),
    ));
    // The API's own GitHub client: issue writes go through the server, so
    // without a token those routes answer 503 rather than falling back to an
    // agent's credential.
    //
    // The drain runs *inside* the shutdown future, which is what keeps the API
    // answering (and the port held) until it is done.
    let served = server::serve_on(
        listener,
        store,
        server::Services {
            github: config.github_client().map(Arc::new),
            bundles: Some(Arc::new(RejectedBundles::under(
                builder_config(&config).scratch_root,
            ))),
        },
        async move {
            // `stop_signal`, not bare ctrl-c: SIGTERM is what `tasks reload`
            // sends to swap a running server, and the default disposition
            // kills the process outright — no drain, no pidfile cleanup, and
            // nothing handed to the successor. The graceful swap only works
            // because this waits on both.
            let signal = stop_signal().await;
            info!(
                signal,
                "shutdown requested; draining in-flight work (the API stays up)"
            );
            let _ = shutdown_tx.send(true);

            let drain = async {
                // The background loops, bounded and named. Unbounded, one of
                // them missing its shutdown flag is a 75s SIGKILL with nothing
                // in the log to say which — see `drain_background`.
                for name in drain_background(
                    BACKGROUND_GRACE,
                    vec![
                        ("poll", poll),
                        ("nudge", nudge),
                        ("obligations", obligations),
                    ],
                )
                .await
                {
                    warn!(
                        loop_name = name,
                        grace_secs = BACKGROUND_GRACE.as_secs(),
                        "background loop did not stop when told; abandoning it. \
                         These loops return on a flag, so this is a bug in that \
                         loop, not slow work"
                    );
                }
                // Scouts and builds first, on a short leash: whatever we walk
                // away from, the successor attaches to.
                if tokio::time::timeout(SHUTDOWN_GRACE, async {
                    let _ = dispatch.await;
                    let _ = build.await;
                })
                .await
                .is_err()
                {
                    warn!(
                        grace_secs = SHUTDOWN_GRACE.as_secs(),
                        "in-flight scouts/builds did not finish within the grace \
                         period; leaving them to be reattached"
                    );
                }
                // The orchestrator's turn is the one thing nothing can pick up
                // — it is a local child, so losing it costs the turn outright.
                // Worth waiting for, but on the same short leash as everything
                // else, for two reasons.
                //
                // `reload` SIGKILLs a server that has not exited within its own
                // `STOP_GRACE` (75s), and `is_destructible()` deliberately does
                // not count an orchestrator turn — so an unbounded wait here is
                // not "the turn finishes", it is "the swap kills us anyway,
                // 45 seconds later, skipping the pidfile cleanup and the rest
                // of this drain". A turn's own budget is `ORCHESTRATOR_TIMEOUT_
                // SECS` (600 by default), twenty times the grace a scout gets
                // for work that costs an hour.
                //
                // Abandoning it is already handled honestly: the turn marker
                // survives in the store and `report_interrupted_orchestrator_
                // turn` says so at the next boot. The answered watermark only
                // advances with the reply, so the next tick retakes it.
                if tokio::time::timeout(SHUTDOWN_GRACE, orchestrate)
                    .await
                    .is_err()
                {
                    warn!(
                        grace_secs = SHUTDOWN_GRACE.as_secs(),
                        "orchestrator turn did not finish within the grace \
                         period; abandoning it — the next boot reports it and \
                         the next tick retakes it"
                    );
                }
            };

            tokio::select! {
                _ = drain => info!("drain complete"),
                _ = stop_signal() => {
                    warn!("second interrupt; abandoning the drain");
                }
            }
        },
    )
    .await;

    // The listener is down, so this process is no longer serving — say so
    // before waiting out in-flight work, so a `tasks reload` blocked on "is
    // it gone yet?" learns the moment it is true rather than 30s later. Also
    // on the error path: a bind that failed (a port already taken) must not
    // leave a record claiming this pid is serving.
    crate::pidfile::remove_if_ours(&config.data_dir, std::process::id());
    served?;

    Ok(())
}

/// Wait out the background loops under one shared deadline, and return the
/// names of the ones that did not stop.
///
/// Two properties are load-bearing, and both come from the failure this exists
/// for — a loop that never observed the shutdown flag, awaited forever, and
/// reached the operator as a SIGKILL with an empty log.
///
/// The deadline is **shared**, not per-task: the total is `grace` however many
/// loops there are, which is what lets the whole drain fit inside `reload`'s
/// `STOP_GRACE`. And loops after the deadline passes are still *asked* — a
/// handle that has already finished answers immediately and is not reported,
/// while one still running is named. Reporting only whichever loop happened to
/// run the clock out would point a reader at the first handle awaited rather
/// than at the one that is stuck.
///
/// Split out of [`run`] so it is testable without a server, a signal, or a
/// real ten-second wait.
async fn drain_background(
    grace: Duration,
    handles: Vec<(&'static str, tokio::task::JoinHandle<()>)>,
) -> Vec<&'static str> {
    let deadline = tokio::time::Instant::now() + grace;
    let mut stuck = Vec::new();
    for (name, handle) in handles {
        if tokio::time::timeout_at(deadline, handle).await.is_err() {
            stuck.push(name);
        }
    }
    stuck
}

/// Clean up work a previous process abandoned mid-flight.
///
/// See [`Store::reconcile_orphaned_work`] for what that means row by row.
pub async fn reconcile_startup(store: &Store) -> Result<(), StoreError> {
    reconcile_startup_except(store, &ResumedWork::default()).await?;
    Ok(())
}

/// [`reconcile_startup`], minus the rows a reattach already owns.
///
/// Must run after [`resume_in_flight`] and before any loop starts. "Orphaned"
/// is no longer the same thing as "`running` at startup": a session whose VM
/// is still alive is running in the ordinary sense, and concluding it would
/// destroy exactly the run reattachment exists to save. What is left after the
/// resume — rows whose VM is gone, or that could not be picked up — is
/// orphaned in the old sense, and is treated exactly as before.
///
/// Returns what it wrote off, so a caller can assert on the *decision* rather
/// than on the state a reattached row happens to be in afterwards. Those are
/// not the same claim: `resume_in_flight` spawns the reattach, so on a loaded
/// machine a session it picked up can already have concluded by the time this
/// returns, and reading the row back is a race. The report is race-free and the
/// stronger statement — reconciliation wrote nothing off.
pub async fn reconcile_startup_except(
    store: &Store,
    resumed: &ResumedWork,
) -> Result<ReconcileReport, StoreError> {
    let report = store.reconcile_orphaned_work_except(resumed).await?;
    if !report.is_empty() {
        info!(
            sessions = report.sessions,
            tasks = report.tasks,
            builds = report.builds,
            "reconciled work orphaned by a previous run"
        );
    }
    Ok(report)
}

/// Say, once, that an orchestrator turn was cut off mid-flight.
///
/// Reattachment does not apply to it: the agent is a local child of `tasks
/// serve`, and it died with its parent. Nothing is retried off the back of
/// this — the conversation recovers by itself, because an interrupted turn
/// never advanced `answered_through` and its input is therefore still
/// unanswered. What was missing was any trace that it happened at all.
async fn report_interrupted_orchestrator_turn(store: &Store) {
    let started = match store.take_interrupted_orchestrator_turn().await {
        Ok(Some(started)) => started,
        Ok(None) => return,
        Err(e) => {
            warn!(error = %e, "could not check for an interrupted orchestrator turn");
            return;
        }
    };
    warn!(
        turn_started_at = %started,
        "an orchestrator turn was interrupted by the last shutdown"
    );
    if let Err(e) = store
        .append_event(EventPayload::Note {
            source: ORCHESTRATOR.into(),
            message: format!(
                "an orchestrator turn that started at {started} was interrupted by a \
                 restart; its input is still unanswered and the next tick will answer it"
            ),
        })
        .await
    {
        warn!(error = %e, "could not record the interrupted orchestrator turn");
    }
}

/// Pick up scouts and builds a previous process left in flight.
///
/// The server was never the thing doing the work: scouts and builds run under
/// their own supervisors inside VMs, and vm-pool is a separate daemon that
/// keeps those VMs alive across a restart. So the honest question at startup
/// is not "what did I abandon" but "what is still running", and the answer is
/// whatever `sessions`/`builds` still name a VM that vm-pool still has.
///
/// Everything here degrades to the old behaviour rather than to a failure: no
/// resumable rows, an unreachable pool, *a pool too old to understand
/// `attach`*, a missing task or project, or no `GITHUB_TOKEN` for a build —
/// each leaves the row untouched for [`reconcile_startup_except`], which
/// writes it off exactly as before.
///
/// Returns what it took ownership of. Every returned row *will* be concluded
/// by the reattach that owns it; that invariant is what makes it safe for
/// reconciliation to skip them.
///
/// The spawned reattaches are deliberately not waited for at shutdown. They
/// are charged against `in_flight` so the loops account for them, but if this
/// process is asked to stop before they finish, walking away costs the same as
/// walking away from a fresh dispatch — the next boot picks them up again, by
/// the same route.
pub async fn resume_in_flight(
    store: &Arc<Store>,
    config: &Config,
    in_flight: &InFlight,
) -> ResumedWork {
    let sessions = match store.resumable_sessions().await {
        Ok(sessions) => sessions,
        Err(e) => {
            warn!(error = %e, "could not look for resumable sessions");
            Vec::new()
        }
    };
    let builds = match store.resumable_builds().await {
        Ok(builds) => builds,
        Err(e) => {
            warn!(error = %e, "could not look for resumable builds");
            Vec::new()
        }
    };
    if sessions.is_empty() && builds.is_empty() {
        return ResumedWork::default();
    }

    let client = match Client::<TasksProtocol>::connect(&config.vm_pool_socket).await {
        Ok(client) => client,
        Err(e) => {
            warn!(
                socket = %config.vm_pool_socket.display(),
                error = %e,
                sessions = sessions.len(),
                builds = builds.len(),
                "vm-pool unavailable at startup — in-flight work is written off instead"
            );
            return ResumedWork::default();
        }
    };

    // Before anything is claimed. `ResumedWork` membership is a promise that
    // the reattach owning the row will conclude it, and a reattach against a
    // pool that cannot decode `attach` concludes it by *failing the run* —
    // work that was alive and recoverable, destroyed by the code path that
    // exists to save it. Asked here, the same skew costs only what a server
    // without reattachment always cost.
    let support = reattach::attach_support(&client.handle()).await;
    if !support.is_supported() {
        warn!(
            socket = %config.vm_pool_socket.display(),
            sessions = sessions.len(),
            builds = builds.len(),
            reason = %support,
            "vm-pool cannot be attached to — in-flight work is written off instead. \
             Restart vm-pool, then the server"
        );
        return ResumedWork::default();
    }

    info!(
        sessions = sessions.len(),
        builds = builds.len(),
        "work survived the restart; reattaching"
    );

    let mut resumed = ResumedWork::default();
    for session in sessions {
        let task = match store.get_task(&session.task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                warn!(session_id = %session.id, "session references a missing task");
                continue;
            }
            Err(e) => {
                warn!(session_id = %session.id, error = %e, "could not load a session's task");
                continue;
            }
        };
        let scout = Scout::new(store.clone(), client.handle(), scout_config(config));
        resumed.sessions.insert(session.id.clone());
        resumed.tasks.insert(task.id.clone());

        let store = store.clone();
        let counter = InFlightGuard::hold(&in_flight.scouts);
        tokio::spawn(async move {
            let _held = counter;
            let task_id = task.id.clone();
            let result = scout.reattach(session, task).await;
            if let Err(e) = record_outcome(&store, &task_id, result).await {
                warn!(task_id = %task_id, error = %e, "recording a resumed scout's outcome failed");
            }
        });
    }

    let github = config.github_client().map(Arc::new);
    for build in builds {
        let Some(github) = github.clone() else {
            warn!(build_id = %build.id, "no GITHUB_TOKEN; a resumed build could not push or open a PR");
            continue;
        };
        let project = match store.get_project(&build.project_id).await {
            Ok(Some(project)) => project,
            Ok(None) => {
                warn!(build_id = %build.id, "build references a missing project");
                continue;
            }
            Err(e) => {
                warn!(build_id = %build.id, error = %e, "could not load a build's project");
                continue;
            }
        };
        let url = clone_url(config, &project);
        let builder = Builder::new(
            store.clone(),
            client.handle(),
            github,
            builder_config(config),
        );
        resumed.builds.insert(build.id.clone());
        for task_id in build_task_ids(store, &build.id).await {
            resumed.tasks.insert(task_id);
        }

        let counter = InFlightGuard::hold(&in_flight.builds);
        tokio::spawn(async move {
            let _held = counter;
            let build_id = build.id.clone();
            match builder.reattach(build, &url).await {
                Ok(done) => {
                    info!(build_id = %done.id, pr = ?done.pr_number, "resumed build succeeded")
                }
                Err(e) => warn!(build_id = %build_id, error = %e, "resumed build did not land"),
            }
        });
    }

    // The connection outlives this function through the `ClientHandle`s the
    // reattach tasks hold; dropping the owning `Client` only gives up its own
    // event stream.
    drop(client);
    resumed
}

/// The tasks behind a build's batch, so reconciliation leaves them `building`
/// while the build it belongs to is still being followed. Best-effort: a task
/// that cannot be read is simply not protected, and is requeued as before.
async fn build_task_ids(store: &Store, build_id: &crate::models::BuildId) -> Vec<TaskId> {
    let spec_ids = match store.build_spec_ids(build_id).await {
        Ok(ids) => ids,
        Err(e) => {
            warn!(%build_id, error = %e, "could not read a resumed build's specs");
            return Vec::new();
        }
    };
    let mut tasks = Vec::new();
    for spec_id in spec_ids {
        match store.get_spec(&spec_id).await {
            Ok(Some(spec)) => tasks.push(spec.task_id),
            Ok(None) => {}
            Err(e) => warn!(%spec_id, error = %e, "could not read a resumed build's spec"),
        }
    }
    tasks
}

// --- GitHub intake ---

/// Poll every project's open issues on an interval until `shutdown` flips.
///
/// Does nothing at all without a token: intake is the only thing that needs
/// one, and the rest of the server is still useful without it.
pub async fn poll_loop(store: Arc<Store>, config: Config, mut shutdown: watch::Receiver<bool>) {
    let Some(github) = config.github_client() else {
        warn!("GITHUB_TOKEN not set — GitHub polling disabled");
        return;
    };
    // Said once, at startup: a typo in TASKS_INTAKE_LABEL ingests nothing, and
    // silence is the worst way to learn that.
    match config.intake.label() {
        Some(label) => info!(label, "intake restricted to issues carrying this label"),
        None => info!("intake accepts every open issue (TASKS_INTAKE_LABEL unset)"),
    }

    let bundles = RejectedBundles::under(builder_config(&config).scratch_root);

    loop {
        match store.get_mode().await {
            Ok(Mode::Stop) => {}
            Ok(_) => {
                match poll_once(&store, &github, &config.intake, &config.scout_base_branch).await {
                    Ok(0) => {}
                    Ok(n) => info!(ingested = n, "poll ingested new tasks"),
                    Err(e) => warn!(error = %e, "poll failed"),
                }
                // Immediately after the poll, deliberately: `poll_once` is
                // where superseding evidence arrives. `done` is written by the
                // closure-derived retirement inside it, and `done` is half the
                // retention predicate.
                reclaim_bundles(&store, &bundles).await;
            }
            Err(e) => warn!(error = %e, "could not read mode; skipping poll"),
        }

        tokio::select! {
            _ = tokio::time::sleep(config.poll_interval) => {}
            _ = shutdown.changed() => return,
        }
    }
}

/// One pass over every project. Returns the number of tasks ingested for the
/// first time.
///
/// A project whose fetch fails is logged and skipped so one unreachable repo
/// can't stall intake for the others. That skip is load-bearing for the second
/// half of the pass: GitHub only tells us an issue was closed by leaving it out
/// of the open set, so absence reconciliation
/// ([`Store::reconcile_closed_issues`]) is only sound on a complete open set. A
/// partial fetch would read as a mass closure.
///
/// `intake` narrows the *upsert* half of the pass and nothing else. Three cases
/// follow from that, and all three are deliberate:
///
/// - An issue that gains the label is ingested on the next poll, as an ordinary
///   first sighting. There is no special path.
/// - A task whose issue *loses* the label is kept exactly as it is — same row,
///   same queue position, same `state`, same `dispatch_attempts` — and simply
///   stops having its snapshot refreshed. Writing `gh_state = Closed` would
///   persist a label-derived fact into a field that means "the issue is
///   closed", and the skipped upsert would never correct it; writing
///   `state = Rejected` would be the poller overriding human-authoritative
///   state. Un-labelling is not a retraction mechanism — pulling work back is
///   the API's job.
/// - That task still tracks upstream closure correctly, because reconciliation
///   still sees the complete open set.
///
/// A project's [`ProjectStatus`] narrows the same half and nothing else: an
/// `archived` one keeps being fetched, keeps reconciling closures and keeps
/// having its merges watched, it just stops turning issues into new tasks.
///
/// `trunk` is the branch that ships ([`Config::scout_base_branch`]), and is
/// only used by [`watch_merges`], which cannot decide "shipped" without it.
pub async fn poll_once(
    store: &Store,
    github: &GitHubClient,
    intake: &IntakeFilter,
    trunk: &str,
) -> Result<usize, StoreError> {
    let mut ingested = 0;
    for project in store.list_projects().await? {
        let issues = match github
            .list_open_issues(&project.repo_owner, &project.repo_name)
            .await
        {
            Ok(issues) => issues,
            Err(e) => {
                warn!(
                    repo = format!("{}/{}", project.repo_owner, project.repo_name),
                    error = %e,
                    "fetching issues failed; skipping project"
                );
                continue;
            }
        };

        // Before the filter, deliberately: `reconcile_closed_issues` reads
        // absence from this list as an upstream closure, so it has to describe
        // every issue GitHub returned, not just the ones we ingest. Filtering
        // first would close every task whose issue merely lost the label.
        let open_numbers: Vec<u64> = issues.iter().map(|issue| issue.number).collect();
        for issue in issues {
            // An archived project stops *gaining* tasks and nothing else —
            // exactly the semantics an issue losing its `TASKS_INTAKE_LABEL`
            // already has. The fetch above still happened, and the
            // reconciliation below still runs, because closure is only ever
            // learned from absence in the open set: an archived project that
            // stopped being fetched would leave every task it already has
            // stuck at `gh_state = open` forever, and a Builder PR it already
            // opened would sit in `awaiting_merge` with nothing to resolve it.
            if !project.status.ingests() || !intake.admits(&issue) {
                continue;
            }
            let outcome = store.upsert_gh_issue(&project.id, issue).await?;
            if outcome.is_new() {
                let task = outcome.into_inner();
                store
                    .append_event(EventPayload::TaskIngested {
                        task_id: task.id,
                        project_id: project.id.clone(),
                    })
                    .await?;
                ingested += 1;
            }
        }

        let closed = store
            .reconcile_closed_issues(&project.id, &open_numbers)
            .await?;
        if !closed.is_empty() {
            info!(
                repo = format!("{}/{}", project.repo_owner, project.repo_name),
                count = closed.len(),
                "issues closed upstream since the last poll"
            );
        }
        for task_id in closed {
            store
                .append_event(EventPayload::TaskGhStateChanged {
                    task_id,
                    gh_state: GhState::Closed,
                })
                .await?;
        }

        retire_closed_issues(store, github, &project).await?;
        // After retirement, deliberately. A merge closes the issue, so the
        // poll that follows one finds the task retirable; running this first
        // would find it still `awaiting_merge` and close an already-closed
        // issue a second time — a wasted PATCH and a duplicate ledger row.
        // `list_builds_awaiting_merge` filters on the open issue as well, so
        // the two passes cannot claim the same row either way.
        watch_merges(store, github, &project, trunk).await?;
    }
    Ok(ingested)
}

/// Retention for preserved bundles: delete only what has demonstrably been
/// **reproduced and shipped**.
///
/// Never by age and never by disk usage. A bundle holds the only copy of a
/// finished implementation — the VM was deallocated before egress ran — so the
/// only safe reason to delete one is that the same work now exists somewhere
/// it cannot be lost from. [`Store::build_superseded`] is that predicate:
/// every spec in the batch carried by a *later* build that succeeded, and
/// every task in it `done`, which in this system means the issue is closed
/// upstream. An old bundle for work nobody ever rebuilt is kept forever, and
/// that is the intended behaviour rather than a leak — see
/// [`ObligationKind::LandBatch`][crate::store::ObligationKind] for the other
/// half of not losing work quietly.
///
/// A deletion that fails is logged and left for the next pass; a bundle whose
/// build row is gone is left alone, since nothing can show it was reproduced.
///
/// Rides [`poll_loop`], so it is gated on a GitHub token. That is correct
/// rather than a limitation: a tokenless server never retires anything, so
/// nothing can ever become superseded.
pub async fn reclaim_bundles(store: &Store, bundles: &RejectedBundles) {
    let files = match bundles.list().await {
        Ok(files) => files,
        Err(e) => {
            warn!(dir = %bundles.dir().display(), error = %e, "could not list preserved bundles");
            return;
        }
    };
    for file in files {
        match store.build_superseded(&file.build_id).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                warn!(build_id = %file.build_id, error = %e, "could not judge a preserved bundle");
                continue;
            }
        }
        match bundles.remove(&file.build_id).await {
            Ok(true) => {
                info!(
                    build_id = %file.build_id,
                    bytes = file.bytes,
                    "reclaimed a preserved bundle: every spec in its batch was rebuilt \
                     and shipped"
                );
                if let Err(e) = store
                    .append_event(EventPayload::BundleRemoved {
                        build_id: file.build_id.clone(),
                        superseded: true,
                        // The server acting on a fact it observed — the work
                        // shipped — and not a judgment anybody made. Never
                        // `Human`: a misattributed write escalates here,
                        // because the human is the one nothing gates.
                        actor: Actor::System,
                    })
                    .await
                {
                    warn!(build_id = %file.build_id, error = %e, "could not record a reclaim");
                }
            }
            // Somebody deleted it between the listing and here. Nothing to say.
            Ok(false) => {}
            Err(e) => {
                warn!(build_id = %file.build_id, error = %e, "could not reclaim a bundle; \
                      leaving it for the next pass");
            }
        }
    }
}

/// Closure-derived retirement: issue closure IS the "done" signal for picked-up
/// work — there is no manual mark-done. The close *reason* is GitHub-owned, so
/// it is queried here at decision time and never persisted.
///
/// A failed lookup leaves the candidates picked up and returns: they are still
/// candidates, and the next poll asks again.
async fn retire_closed_issues(
    store: &Store,
    github: &GitHubClient,
    project: &Project,
) -> Result<(), StoreError> {
    let retirable = store.list_retirable_tasks(&project.id).await?;
    if retirable.is_empty() {
        return Ok(());
    }
    let numbers: Vec<u64> = retirable.iter().map(|t| t.gh_issue_number).collect();
    let info = match github
        .issue_close_info(&project.repo_owner, &project.repo_name, &numbers)
        .await
    {
        Ok(info) => info,
        Err(e) => {
            warn!(error = %e, "fetching close reasons failed; retiring next poll");
            return Ok(());
        }
    };
    for task in retirable {
        let to = match info.get(&task.gh_issue_number) {
            // Reopened between the open-set fetch and this lookup; the
            // next poll's upsert refreshes gh_state and it flows again.
            Some(i) if i.state == GhState::Open => continue,
            Some(i) => match i.state_reason.as_deref() {
                Some("NOT_PLANNED") | Some("DUPLICATE") => TaskState::Rejected,
                _ => TaskState::Done,
            },
            // Deleted / converted to a discussion — it is never coming
            // back as an issue, so the work is concluded either way.
            None => {
                warn!(
                    issue = task.gh_issue_number,
                    "issue no longer resolvable; retiring as done"
                );
                TaskState::Done
            }
        };
        if let Some(retired) = store.retire_task(&task.id, to).await? {
            info!(
                issue = retired.gh_issue_number,
                to = to.as_str(),
                "issue closed upstream; retired its task"
            );
        }
    }
    Ok(())
}

/// Did this pull request's work actually reach the branch that ships?
///
/// **Not `pr.merged`.** `merged` is a statement about the PR's *base*, and the
/// pipeline stacks builds routinely — a PR based on another build's branch
/// reads merged the moment that branch takes it, whether or not the branch
/// ever lands. That is not hypothetical: PR #863 was `merged: true` and its
/// work never reached `main`, which is the failure this whole pass exists to
/// prevent, one level up.
///
/// The base is checked first and short-circuits, so the ordinary unstacked
/// case — `base_ref == trunk` — costs **no extra API call at all**; the
/// compare is spent only on a stacked PR.
///
/// Every unreadable answer returns `false`, because the two mistakes are not
/// symmetric. Saying "not yet" costs one REST call on the next poll and is
/// undone by it; saying "shipped" wrongly writes `done` over work that shipped
/// nothing, and no pass ever revisits `done`.
async fn shipped(
    github: &GitHubClient,
    project: &Project,
    trunk: &str,
    pr: &PrState,
    pr_number: u64,
) -> bool {
    if !pr.merged {
        return false;
    }
    if pr.base_ref.as_deref() == Some(trunk) {
        return true;
    }
    let Some(sha) = pr.merge_commit_sha.as_deref() else {
        warn!(
            pr = pr_number,
            base = pr.base_ref.as_deref().unwrap_or("(unknown)"),
            "merged into a branch that is not the trunk and named no merge commit; \
             staying parked"
        );
        return false;
    };
    match github
        .merge_reached_trunk(&project.repo_owner, &project.repo_name, trunk, sha)
        .await
    {
        Ok(true) => true,
        Ok(false) => {
            info!(
                pr = pr_number,
                base = pr.base_ref.as_deref().unwrap_or("(unknown)"),
                sha,
                trunk,
                "merged into a branch that has not reached the trunk; staying parked"
            );
            false
        }
        Err(e) => {
            warn!(
                pr = pr_number,
                sha,
                error = %e,
                "could not tell whether the merge reached the trunk; staying parked"
            );
            false
        }
    }
}

/// Resolve the pull requests behind `awaiting_merge` work: a PR whose work
/// reached the trunk closes the issues it implements, an unmerged close returns
/// the batch to `ready_to_build`, and anything else is left parked.
///
/// This is the other half of "`done` means shipped". `finalize_build_succeeded`
/// parks a batch at `awaiting_merge` because a PR that opened is a claim, not a
/// delivery; the fact that settles it lives on GitHub, so it is read here at
/// decision time and never stored. The ongoing cost is one REST call per
/// unresolved PR per poll, bounded by how many Builder PRs are open at once
/// (builds are serial). Caching the answer in a `last_checked` column would be
/// persisting a GitHub-owned fact with a timestamp on it.
///
/// Closing an issue is `retire_work`, so the charter governs it exactly as it
/// governs the endpoint — and it governs the whole pass, not just the GitHub
/// write: `off` does not even spend the read, `shadow` records what it would
/// have done and applies nothing (including the unwind), `live` acts. The
/// daily cap is deliberately not consulted: `orchestrator_actions_today`
/// counts the *orchestrator's* writes and exists to bound a runaway agent
/// loop, while one close per merged PR is bounded by how many PRs a human
/// merged.
///
/// A failed PR read or a failed close is a warning and a skip — the task stays
/// `awaiting_merge` and the next poll asks again.
///
/// A batch that merged but has *not* reached `trunk` stays parked rather than
/// unwinding, deliberately, and that handles both stack orders: when a stack
/// is merged correctly the base lands afterwards and the merge commit becomes
/// reachable on a later poll, closing normally. Reachability is monotone, so
/// polling can never un-ship something it already concluded had landed.
/// Nothing here notices that a batch has been parked *too long* — that is
/// [`ObligationKind::LandBatch`]'s job.
async fn watch_merges(
    store: &Store,
    github: &GitHubClient,
    project: &Project,
    trunk: &str,
) -> Result<(), StoreError> {
    let awaiting = store.list_builds_awaiting_merge(&project.id).await?;
    if awaiting.is_empty() {
        return Ok(());
    }
    // A standing configuration, not an event: at `info` this would repeat
    // every poll interval for as long as the charter says so.
    let level = store.charter_entry(Capability::RetireWork).await?.level;
    if level == CharterLevel::Off {
        debug!(
            builds = awaiting.len(),
            "retire_work is off; leaving merged PRs' issues open"
        );
        return Ok(());
    }

    for build in awaiting {
        let pr = match github
            .pull_request_state(&project.repo_owner, &project.repo_name, build.pr_number)
            .await
        {
            Ok(pr) => pr,
            Err(e) => {
                warn!(
                    pr = build.pr_number,
                    error = %e,
                    "reading the pull request failed; asking again next poll"
                );
                continue;
            }
        };

        // Not `pr.merged` — see [`shipped`]. `merged` says the PR reached its
        // base, which for a stacked build is another build's branch, and
        // `merge_commit_sha` is populated on *open* PRs too from GitHub's
        // speculative test merge. Only "the merge commit is on the trunk"
        // means the work shipped.
        if shipped(github, project, trunk, &pr, build.pr_number).await {
            let rationale = format!(
                "PR #{} merged and its commit is on {trunk} (build {}); \
                 closing the issue it implements",
                build.pr_number, build.build_id
            );
            let evidence = serde_json::json!({
                "build_id": build.build_id.as_str(),
                "pr_number": build.pr_number,
                "merge_commit_sha": pr.merge_commit_sha,
                "base_ref": pr.base_ref,
                "trunk": trunk,
            });
            for task in &build.tasks {
                if level == CharterLevel::Shadow {
                    // A shadowed close changes nothing, so this build is on
                    // the list again next poll. Record the judgment once.
                    if store
                        .has_decision("task", task.id.as_str(), DecisionAction::RetireWork, false)
                        .await?
                    {
                        continue;
                    }
                    store
                        .record_decision(
                            "task",
                            task.id.as_str(),
                            DecisionAction::RetireWork,
                            DecisionInput {
                                actor: Actor::System,
                                rationale: Some(rationale.clone()),
                                evidence: Some(evidence.clone()),
                            },
                            false,
                        )
                        .await?;
                    info!(
                        issue = task.gh_issue_number,
                        pr = build.pr_number,
                        "retire_work is in shadow; recorded the close, applied nothing"
                    );
                    continue;
                }

                if let Err(e) = github
                    .close_issue(
                        &project.repo_owner,
                        &project.repo_name,
                        task.gh_issue_number,
                        CloseReason::Completed,
                    )
                    .await
                {
                    warn!(
                        issue = task.gh_issue_number,
                        pr = build.pr_number,
                        error = %e,
                        "closing the issue failed; asking again next poll"
                    );
                    continue;
                }
                store
                    .record_issue_closed(
                        &task.id,
                        CloseReason::Completed,
                        DecisionInput {
                            actor: Actor::System,
                            rationale: Some(rationale.clone()),
                            evidence: Some(evidence.clone()),
                        },
                    )
                    .await?;
                info!(
                    issue = task.gh_issue_number,
                    pr = build.pr_number,
                    "pull request merged; closed the issue it implements"
                );
            }
        } else if !pr.merged && pr.state == GhState::Closed {
            // `!pr.merged` is load-bearing now that `shipped` can decline a
            // merged PR: a merged PR is also a *closed* one, so without it a
            // batch merged into a branch that has not landed yet would be
            // unwound to `ready_to_build` — which is precisely the legitimate
            // stack order this pass has to wait out, and it would rebuild work
            // that is sitting in an open stack.
            //
            // The charter gates the pass, not just its GitHub write: `off`
            // never looks, `shadow` looks and reports, `live` acts. A demoted
            // capability that still quietly rewrote pipeline state would be a
            // kill switch that does not switch anything off.
            if level == CharterLevel::Shadow {
                info!(
                    pr = build.pr_number,
                    build = %build.build_id,
                    "retire_work is in shadow; the PR closed unmerged and the batch stays parked"
                );
                continue;
            }
            let returned = store.unwind_unmerged_build(&build.build_id).await?;
            if !returned.is_empty() {
                info!(
                    pr = build.pr_number,
                    build = %build.build_id,
                    tasks = returned.len(),
                    "pull request closed unmerged; the batch is ready to build again"
                );
            }
        }
    }
    Ok(())
}

// --- scout dispatch ---

/// How a Scout VM is booted, in one place: the dispatch loop and
/// [`resume_in_flight`] must agree on it, and a resumed run's deadline is
/// measured against the same budget.
fn scout_config(config: &Config) -> ScoutConfig {
    ScoutConfig {
        image: config.scout_image.clone(),
        vm_config: config.vm_config.clone(),
        timeout: config.scout_timeout,
    }
}

/// How a Builder VM is booted. See [`scout_config`].
fn builder_config(config: &Config) -> BuilderConfig {
    BuilderConfig {
        image: config.builder_image.clone(),
        vm_config: config.builder_vm_config.clone(),
        timeout: config.builder_timeout,
        scratch_root: config.data_dir.join("build-scratch"),
    }
}

/// Keep up to `scout_max_concurrent` scouts running until `shutdown` flips.
///
/// Owns the vm-pool connection: if the socket is missing or the connection
/// drops, dispatch pauses and reconnects every [`VM_POOL_RETRY`] rather than
/// taking the process down.
///
/// `in_flight` carries scouts this loop did not start — runs
/// [`resume_in_flight`] picked back up — which are otherwise invisible to its
/// own accounting and would let a restart oversubscribe the pool.
pub async fn dispatch_loop(
    store: Arc<Store>,
    config: Config,
    in_flight: InFlight,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        let mut client = match Client::<TasksProtocol>::connect(&config.vm_pool_socket).await {
            Ok(client) => client,
            Err(e) => {
                warn!(
                    socket = %config.vm_pool_socket.display(),
                    error = %e,
                    retry_secs = VM_POOL_RETRY.as_secs(),
                    "vm-pool unavailable — scout dispatch disabled"
                );
                tokio::select! {
                    _ = tokio::time::sleep(VM_POOL_RETRY) => continue,
                    _ = shutdown.changed() => return,
                }
            }
        };
        info!(socket = %config.vm_pool_socket.display(), "connected to vm-pool");
        report_pool(&client, &config).await;
        sweep_leaked_vms(&store, &mut client).await;

        dispatch_connected(&store, &config, &in_flight, client, &mut shutdown).await;
    }
}

/// Say, on every connect, what this vm-pool can do for this server: whether it
/// could be reattached to, and whether it is big enough to hold the work this
/// configuration will ask it for.
///
/// Deliberately not a gate, in both halves. Dispatch needs nothing newer than
/// the original command set, and an old daemon runs scouts and builds
/// perfectly well; the only thing it cannot do is hand work back after a
/// restart. That bill arrives at the *next* restart, by which point the work
/// it costs is already in flight — so connect time is the last moment an
/// operator can act on it, which is the whole reason to say it out loud here.
/// Capacity is the same shape of fact: nothing here can resize a pool in
/// another process, and refusing to dispatch against a small one would turn a
/// survivable misconfiguration into an outage.
///
/// One `status` round trip answers both questions — which is what
/// [`reattach::support_of`] exists for. A `status` that errors keeps the
/// attach-support warning and skips the capacity half rather than guessing:
/// `status` is the oldest command in the protocol, so a pool that will not
/// answer it will not answer anything better.
async fn report_pool(client: &Client<TasksProtocol>, config: &Config) {
    let status = match client.handle().status().await {
        Ok(status) => status,
        Err(e) => return report_attach_support(&reattach::AttachSupport::Unknown(e)),
    };
    report_attach_support(&reattach::support_of(&status));
    report_capacity(Capacity::assess(status.total, config.scout_max_concurrent));
}

fn report_attach_support(support: &reattach::AttachSupport) {
    if support.is_supported() {
        info!(%support, "vm-pool can hand work back across a restart");
    } else {
        warn!(
            %support,
            "vm-pool cannot hand work back across a restart — a restart from here \
             will write off whatever is in flight"
        );
    }
}

/// The slot the serial build lane occupies.
///
/// One, and only ever one — builds are strictly serial, so nothing multiplies
/// it. `buildkit` is deliberately **not** in this sum: it is started by the
/// container runtime to service an image build, as an ordinary host process
/// this pool never allocated and does not count. It bills to host memory, not
/// to a slot.
const BUILD_LANE_SLOTS: usize = 1;

/// How this server's configuration fits the pool it just connected to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capacity {
    /// The pool cannot hold what a full complement of work would need. Some
    /// dispatch will be refused with `pool exhausted`.
    Short { needed: usize, total: usize },
    /// It fits exactly. Dispatches fine today; exhausts on the first leaked VM.
    NoSlack { needed: usize, total: usize },
    /// It fits with room over.
    Slack {
        needed: usize,
        total: usize,
        spare: usize,
    },
}

impl Capacity {
    /// Weigh a pool of `total` slots against what this server may ask of it:
    /// every scout it will run at once, plus the one serial build lane.
    fn assess(total: usize, scout_max_concurrent: usize) -> Self {
        let needed = scout_max_concurrent + BUILD_LANE_SLOTS;
        match total.checked_sub(needed) {
            None => Self::Short { needed, total },
            Some(0) => Self::NoSlack { needed, total },
            Some(spare) => Self::Slack {
                needed,
                total,
                spare,
            },
        }
    }
}

/// Log a [`Capacity`], picking the level.
///
/// `NoSlack` is a `warn!` rather than an `info!` on purpose: a pool sized
/// exactly to the steady state dispatches perfectly well right up until one VM
/// leaks, and then refuses everything. The operator reading this line is the
/// person who can act on it, so both warnings name the variable *and* the fix
/// — and the `Short` one names the alternative, since lowering
/// `SCOUT_MAX_CONCURRENT` is as good an answer as raising the pool.
fn report_capacity(capacity: Capacity) {
    match capacity {
        Capacity::Short { needed, total } => warn!(
            needed,
            total,
            "vm-pool is too small for this server: {needed} slots are needed \
             ({} scouts + the serial build lane) and it holds {total} — dispatch will \
             be refused with `pool exhausted`. Raise VM_POOL_MAX_VMS and restart \
             `tasks vm-pool`, or lower SCOUT_MAX_CONCURRENT",
            needed - BUILD_LANE_SLOTS,
        ),
        Capacity::NoSlack { needed, total } => warn!(
            needed,
            total,
            "vm-pool fits this server exactly ({needed} of {total} slots) — one leaked \
             VM exhausts it. Raise VM_POOL_MAX_VMS and restart `tasks vm-pool`"
        ),
        Capacity::Slack {
            needed,
            total,
            spare,
        } => info!(needed, total, spare, "vm-pool capacity is sufficient"),
    }
}

/// Hand back the VMs of work that already concluded.
///
/// Runs on every connect rather than once at startup, because the moment a
/// leak becomes fixable is the moment there is a connection — and startup
/// reconciliation happens before there is one. Cheap: the list is empty in the
/// steady state, and each entry is one round-trip.
///
/// Best-effort by construction. A failure here must not stop dispatch, since
/// the whole point is to get slots back so dispatch can proceed.
async fn sweep_leaked_vms(store: &Store, client: &mut Client<TasksProtocol>) {
    let leaked = match store.leaked_vm_ids().await {
        Ok(leaked) if leaked.is_empty() => return,
        Ok(leaked) => leaked,
        Err(e) => {
            warn!(error = %e, "could not look for leaked VMs");
            return;
        }
    };
    warn!(
        count = leaked.len(),
        "concluded work still holds VMs — handing them back"
    );
    // Bounded like every other teardown: this loop is sequential and sits on
    // the dispatch loop's connect path, so one unanswered deallocate would
    // stall *all* dispatch — and it runs precisely when the pool is already
    // in trouble. A failure or an expiry is logged; the row is cleared either
    // way, so the next sweep retries whatever did not land.
    let handle = client.handle();
    for vm_id in leaked {
        let id = VmId::new(vm_id.clone());
        crate::teardown::deallocate_bounded(
            &handle,
            store,
            &id,
            "leaked-VM sweep",
            crate::teardown::DEALLOCATE_TIMEOUT,
        )
        .await;
        if let Err(e) = store.forget_vm(&vm_id).await {
            warn!(vm_id, error = %e, "could not clear a leaked VM's row");
        }
    }
}

/// The dispatch loop for as long as one vm-pool connection lives. Returns when
/// shutdown is requested or the connection is lost (after draining whatever is
/// still in flight).
async fn dispatch_connected(
    store: &Arc<Store>,
    config: &Config,
    resumed: &InFlight,
    client: Client<TasksProtocol>,
    shutdown: &mut watch::Receiver<bool>,
) {
    let scout = Arc::new(Scout::new(
        store.clone(),
        client.handle(),
        scout_config(config),
    ));

    let mut in_flight: JoinSet<(TaskId, Result<Spec, ScoutError>)> = JoinSet::new();
    let mut in_flight_ids: HashSet<TaskId> = HashSet::new();
    // Set once we stop starting new work: either shutdown or a dead socket.
    let mut draining = false;

    loop {
        if !draining
            && let Err(e) = top_up(
                store,
                config,
                resumed,
                &scout,
                &mut in_flight,
                &mut in_flight_ids,
            )
            .await
        {
            warn!(error = %e, "could not read the task queue; retrying next tick");
        }

        if draining && in_flight.is_empty() {
            return;
        }

        tokio::select! {
            Some(joined) = in_flight.join_next(), if !in_flight.is_empty() => {
                match joined {
                    Ok((task_id, result)) => {
                        in_flight_ids.remove(&task_id);
                        match record_outcome(store, &task_id, result).await {
                            Ok(ConnectionLost(true)) => draining = true,
                            Ok(ConnectionLost(false)) => {}
                            Err(e) => warn!(task_id = %task_id, error = %e, "recording scout outcome failed"),
                        }
                    }
                    Err(e) => warn!(error = %e, "scout dispatch task panicked"),
                }
            }
            _ = shutdown.changed() => {
                draining = true;
                info!(in_flight = in_flight.len(), "shutdown: starting no new scouts");
            }
            _ = tokio::time::sleep(DISPATCH_TICK), if !draining => {}
        }
    }
}

/// Start scouts until we are at the concurrency limit or run out of eligible
/// tasks.
async fn top_up(
    store: &Arc<Store>,
    config: &Config,
    resumed: &InFlight,
    scout: &Arc<Scout>,
    in_flight: &mut JoinSet<(TaskId, Result<Spec, ScoutError>)>,
    in_flight_ids: &mut HashSet<TaskId>,
) -> Result<(), StoreError> {
    if store.get_mode().await? != Mode::Play {
        return Ok(());
    }

    // Scouts this loop started plus scouts it inherited. Counting only its own
    // would let a restart with two runs in flight start two more.
    while in_flight.len() + resumed.scouts() < config.scout_max_concurrent {
        let Some((task, project)) = next_dispatchable(store, in_flight_ids).await? else {
            break;
        };

        let target = ScoutTarget {
            repo_clone_url: clone_url(config, &project),
            base_branch: config.scout_base_branch.clone(),
        };
        let task_id = task.id.clone();
        info!(
            task_id = %task_id,
            repo = format!("{}/{}", project.repo_owner, project.repo_name),
            "dispatching scout"
        );
        // Reserve before spawning: `dispatch` moves the task out of `New`
        // asynchronously, so the id set is what keeps the next tick from
        // picking the same task twice.
        in_flight_ids.insert(task_id.clone());
        let scout = scout.clone();
        in_flight.spawn(async move {
            let result = scout.dispatch(task, &target).await;
            (task_id, result)
        });
    }
    Ok(())
}

/// The next task to scout: queue order (which [`Store::list_tasks`] already
/// applies), state `Queued` (explicitly picked up), still open on GitHub, not in flight, not past the
/// attempt cap, and belonging to a project the dispatcher is still working on.
///
/// A task at the cap is rejected the moment it gets there, so the attempt
/// filter here is belt-and-braces: it also covers rows an older build (or a
/// crash between the increment and the rejection) left `Queued` at three strikes.
async fn next_dispatchable(
    store: &Store,
    skip: &HashSet<TaskId>,
) -> Result<Option<(Task, Project)>, StoreError> {
    for task in store.list_tasks().await? {
        if task.state != TaskState::Queued
            || task.gh_state == GhState::Closed
            || skip.contains(&task.id)
            || task.dispatch_attempts >= MAX_DISPATCH_ATTEMPTS
        {
            continue;
        }
        let Some(project) = store.get_project(&task.project_id).await? else {
            warn!(task_id = %task.id, project_id = %task.project_id, "task references a missing project");
            continue;
        };
        // `continue`, not `break`: a paused repo at the head of the queue must
        // not starve the ones behind it — that is the whole difference between
        // pausing one repo and pausing the server.
        if !project.status.dispatches() {
            continue;
        }
        return Ok(Some((task, project)));
    }
    Ok(None)
}

/// Whether the finished dispatch took the vm-pool connection down with it.
struct ConnectionLost(bool);

/// Fold a finished dispatch back into the pipeline. The spec and session writes
/// already happened inside [`Scout::dispatch`]; what is left is the retry
/// accounting — count the failure, reject the task once it has used up its
/// attempts, and leave a breadcrumb on the event log either way.
async fn record_outcome(
    store: &Store,
    task_id: &TaskId,
    result: Result<Spec, ScoutError>,
) -> Result<ConnectionLost, StoreError> {
    let error = match result {
        Ok(spec) => {
            // The attempt count is cleared by `Scout::finalize_succeeded`,
            // alongside the rest of the success-path writes.
            info!(task_id = %task_id, spec_id = %spec.id, "scout produced a spec");
            return Ok(ConnectionLost(false));
        }
        Err(e) => e,
    };

    // A dead socket is the socket's fault, not the task's: leave the task's
    // attempt count alone and let the loop reconnect.
    if is_disconnect(&error) {
        warn!(task_id = %task_id, error = %error, "lost the vm-pool connection mid-dispatch");
        store
            .append_event(EventPayload::Note {
                source: DISPATCHER.into(),
                message: format!("vm-pool connection lost while scouting {task_id}"),
            })
            .await?;
        return Ok(ConnectionLost(true));
    }

    // A cancel is a decision, not a failure. It costs the task no attempt (the
    // run was stopped before it could show whether it would have worked) and
    // the task is already back in the backlog, put there by
    // `Scout::finalize_cancelled` — the one non-success path that does not
    // return work to the queue, because leaving it queued would have the loop
    // below start a replacement scout within the tick.
    if let ScoutError::Cancelled(request) = &error {
        let actor = request.actor.as_str();
        info!(task_id = %task_id, actor, "a scout was cancelled");
        store
            .append_event(EventPayload::Note {
                source: DISPATCHER.into(),
                message: format!(
                    "the scout for {task_id} was {}; the task is back in the backlog \
                     and keeps its attempts",
                    request.exit_reason()
                ),
            })
            .await?;
        return Ok(ConnectionLost(false));
    }

    // The one decision point: only a run that judged the work costs the task
    // an attempt. Read off the class the supervisor stamped on its terminal
    // event, never off the reason text — a reason is prose written for a
    // human, and a strike decision that greps it would change meaning the next
    // time someone improves a sentence. #825 burned five scout attempts in one
    // night without a single verdict among them.
    let class = error.failure_class();
    if Strike::for_class(class) == Strike::Waive {
        let waiver = class
            .waiver_reason()
            .expect("a waived strike has a reason to waive it");
        let attempts = store
            .get_task(task_id)
            .await?
            .map(|t| t.dispatch_attempts)
            .unwrap_or_default();
        warn!(task_id = %task_id, %class, error = %error, "a scout failed without judging the work");
        store
            .append_event(EventPayload::Note {
                source: DISPATCHER.into(),
                message: format!(
                    "the scout for {task_id} failed as {class}, so the task keeps its \
                     {attempts} attempt(s): {waiver} ({error})"
                ),
            })
            .await?;
        return Ok(ConnectionLost(false));
    }

    // Read off the error variant alone, never off the notes table: a task can
    // carry salvage from an *earlier* attempt, and the event log must not
    // credit this run for it.
    let outcome = match &error {
        ScoutError::StoppedEarly { .. } => "stopped early (notes salvaged)",
        _ => "failed",
    };

    let count = store.record_dispatch_failure(task_id).await?;
    warn!(task_id = %task_id, attempt = count, error = %error, "scout dispatch did not produce a spec");
    if count >= MAX_DISPATCH_ATTEMPTS {
        reject_exhausted(
            store,
            task_id,
            format!("scout for {task_id} {outcome} {count}x, rejecting the task: {error}"),
        )
        .await?;
    } else {
        store
            .append_event(EventPayload::Note {
                source: DISPATCHER.into(),
                message: format!("scout for {task_id} {outcome} (attempt {count}): {error}"),
            })
            .await?;
    }
    Ok(ConnectionLost(false))
}

/// Retire a task that has burned through [`MAX_DISPATCH_ATTEMPTS`].
///
/// [`Scout::dispatch`]'s failure path has already put the task back to `New` by
/// the time we get here, which is why this runs last and wins: a task left
/// `New` at the cap would be picked up again by the next process, which is
/// exactly the retry-forever loop the persisted count exists to stop.
async fn reject_exhausted(
    store: &Store,
    task_id: &TaskId,
    message: String,
) -> Result<(), StoreError> {
    let from = match store.get_task(task_id).await? {
        Some(task) => task.state,
        None => {
            warn!(task_id = %task_id, "task vanished before it could be rejected");
            return Ok(());
        }
    };
    store
        .update_task_state(task_id, TaskState::Rejected)
        .await?;
    store
        .append_event(EventPayload::TaskStateChanged {
            task_id: task_id.clone(),
            from,
            to: TaskState::Rejected,
        })
        .await?;
    store
        .append_event(EventPayload::Note {
            source: DISPATCHER.into(),
            message,
        })
        .await?;
    Ok(())
}

/// A scout error that means the vm-pool connection is gone, rather than the
/// scout run itself having failed.
///
/// This decides *reconnection* — whether to drop the client and rebuild it —
/// and, because [`record_outcome`] consults it first, it incidentally decides
/// the strike too. For `StreamClosed` the two answers must agree, and they do:
/// `ScoutError::failure_class` classifies it `Transport`.
///
/// The two must **not** be merged into one predicate. `Client(Closed |
/// Connect(_))` is a disconnect and is still classified a verdict, so folding
/// either into the other would start charging it — see the `Client(_)` comment
/// on `BuilderError::failure_class` for why that is a separate argument.
fn is_disconnect(error: &ScoutError) -> bool {
    matches!(
        error,
        ScoutError::StreamClosed
            | ScoutError::Client(ClientError::Closed | ClientError::Connect(_))
    )
}

// --- orchestrator loop ---

/// The build directory the orchestrator may verify in, or `None` if it cannot.
///
/// Resolved and created **once per boot** rather than per turn, and that is
/// what keeps the prompt honest: the verification section names this directory,
/// so it can never name one the agent will find missing. `None` whenever the
/// workdir is not a checkout (there is nothing to build) or the mkdir failed,
/// and the prompt then grows no verification heading at all.
async fn verify_target_dir(config: &Config) -> Option<PathBuf> {
    config.orchestrator_workdir.as_ref()?;
    let dir = config.orchestrator_target_dir();
    match tokio::fs::create_dir_all(&dir).await {
        Ok(()) => {
            info!(dir = %dir.display(), "orchestrator verification build directory");
            Some(dir)
        }
        Err(e) => {
            warn!(
                dir = %dir.display(),
                error = %e,
                "could not create the orchestrator's build directory — it will not be \
                 asked to run tests this boot"
            );
            None
        }
    }
}

/// Answer pending orchestrator turns until `shutdown` flips.
///
/// Not mode-gated on purpose: asking the orchestrator "what's the status?"
/// must work while everything else is paused — that's what pause is *for*.
pub async fn orchestrator_loop(
    store: Arc<Store>,
    config: Config,
    mut shutdown: watch::Receiver<bool>,
) {
    let workdir = config.agent_workdir();
    // Advertise the effective workdir so `GET /orchestrator/session` can
    // tell clients where to `cd` for an interactive resume.
    if let Err(e) = store
        .set_orchestrator_workdir(&workdir.display().to_string())
        .await
    {
        warn!(error = %e, "failed to record orchestrator workdir");
    }
    let orchestrator = Orchestrator::new(
        store.clone(),
        OrchestratorConfig {
            command: config.orchestrator_cmd.clone(),
            timeout: config.orchestrator_timeout,
            workdir,
            workdir_is_checkout: config.orchestrator_workdir.is_some(),
            target_dir: verify_target_dir(&config).await,
            github_configured: config.github_token.is_some(),
            api_port: config.port,
            curl_config: config.orchestrator_curl_config(),
        },
    );
    loop {
        if *shutdown.borrow() {
            return;
        }
        // While a human has the session checked out interactively, do not
        // touch it — CC sessions have no file locking, and a headless turn
        // would interleave with theirs. Input keeps accumulating as
        // unanswered turns and is answered once the checkout lapses.
        match store.orchestrator_checked_out().await {
            Ok(true) => {}
            Ok(false) => {
                if let Err(e) = orchestrator.tick().await {
                    warn!(error = %e, "orchestrator tick failed");
                }
            }
            Err(e) => warn!(error = %e, "orchestrator checkout state unreadable; skipping tick"),
        }
        tokio::select! {
            _ = tokio::time::sleep(ORCHESTRATOR_TICK) => {}
            _ = shutdown.changed() => return,
        }
    }
}

/// Turn significant pipeline events into `event` turns in the orchestrator
/// conversation, so the agent reacts to the pipeline instead of only to the
/// human. Bursts are debounced ([`NUDGE_DEBOUNCE`] of quiet, capped at
/// `max_wait`) into one turn; [`orchestrator_loop`]'s next tick answers it.
///
/// `debounce`/`max_wait` are parameters so tests can run in milliseconds;
/// production passes [`NUDGE_DEBOUNCE`]/[`NUDGE_MAX_WAIT`].
///
/// The `*shutdown.borrow()` at the top of the outer loop is load-bearing, not
/// symmetry with the other loops. [`watch::Receiver::changed`] marks the value
/// seen when it *returns*, so a shutdown consumed by the inner batch loop's
/// `select!` leaves the outer `changed()` waiting for a second change that
/// never comes — parking on `events.recv()` forever while the drain awaits
/// this task unbounded. One nudge-worthy event near a restart was enough to
/// wedge the whole process until its supervisor's SIGKILL, and `POST /mode` is
/// one such event, so "pause the pipeline, then restart it" hit it every time.
pub async fn orchestrator_nudge_loop(
    store: Arc<Store>,
    config: Config,
    debounce: Duration,
    max_wait: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    use tokio::sync::broadcast::error::RecvError;

    // Built once: the brief's GitHub half degrades to a stated omission when
    // there is no token, rather than disabling the loop.
    let github = config.github_client();
    let mut events = store.subscribe_events();
    loop {
        if *shutdown.borrow() {
            return;
        }
        let first = tokio::select! {
            _ = shutdown.changed() => return,
            received = events.recv() => match received {
                Ok(event) if orchestrator::nudge_worthy(&event.payload) => event,
                Ok(_) => continue,
                Err(RecvError::Lagged(missed)) => {
                    warn!(missed, "orchestrator nudge feed lagged; events skipped");
                    continue;
                }
                Err(RecvError::Closed) => return,
            },
        };

        // Collect the rest of the burst.
        let mut batch = vec![first];
        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            let quiet = tokio::time::sleep(debounce);
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = quiet => break,
                _ = tokio::time::sleep_until(deadline) => break,
                received = events.recv() => match received {
                    Ok(event) if orchestrator::nudge_worthy(&event.payload) => batch.push(event),
                    Ok(_) => {}
                    Err(RecvError::Lagged(missed)) => {
                        warn!(missed, "orchestrator nudge feed lagged; events skipped");
                    }
                    Err(RecvError::Closed) => break,
                },
            }
        }

        let brief = Brief::new(&store, github.as_ref(), &config.scout_base_branch);
        let content = orchestrator::format_nudge(&store, &brief, &batch).await;
        info!(events = batch.len(), "nudging orchestrator");
        if let Err(e) = store
            .append_orchestrator_message(ChatRole::Event, &content)
            .await
        {
            warn!(error = %e, "failed to append orchestrator nudge");
        }
    }
}

/// How long freshly-landed work gets before it counts as an obligation. Long
/// enough that the ordinary nudge, a tick, and a review all fit inside it —
/// an obligation surfacing means that path failed, which is the case worth
/// catching rather than the common one.
const OBLIGATION_GRACE: Duration = Duration::from_secs(15 * 60);
/// How often a still-open obligation is mentioned again. Standing work should
/// be persistent, not nagging.
const OBLIGATION_REMINDER: Duration = Duration::from_secs(30 * 60);
/// How often state is reconciled. Cheap (two indexed queries), and the
/// interval only bounds how quickly a *missed* nudge is noticed.
const OBLIGATION_TICK: Duration = Duration::from_secs(60);

/// Surface what the pipeline is owed, forever, until it is actually done.
///
/// This is the half of the orchestrator's input that cannot be lost. The
/// nudge loop is a latency optimization — it says something happened, once,
/// and if that turn dies with a timeout the message is still consumed by the
/// watermark. Obligations are recomputed from state every pass, so the worst
/// a failure costs is the reminder interval.
///
/// Not mode-gated: an obligation is a fact about the pipeline, and Pause
/// stopping *new* work is not a reason to stop saying that a spec has been
/// waiting since Tuesday.
pub async fn obligation_loop(
    store: Arc<Store>,
    config: Config,
    grace: Duration,
    reminder: Duration,
    tick: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let grace = chrono::Duration::from_std(grace).expect("grace fits");
    let interval = chrono::Duration::from_std(reminder).expect("interval fits");
    let github = config.github_client();
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(tick) => {}
        }

        let open = match store.open_obligations(grace).await {
            Ok(open) => open,
            Err(e) => {
                warn!(error = %e, "reconciling obligations failed");
                continue;
            }
        };
        if open.is_empty() {
            continue;
        }
        let due = match store.obligations_due_for_reminder(open, interval).await {
            Ok(due) => due,
            Err(e) => {
                warn!(error = %e, "filtering obligation reminders failed");
                continue;
            }
        };
        if due.is_empty() {
            continue;
        }

        info!(obligations = due.len(), "surfacing standing obligations");
        let brief = Brief::new(&store, github.as_ref(), &config.scout_base_branch);
        let content = orchestrator::format_obligations(&store, &brief, &due).await;
        if let Err(e) = store
            .append_orchestrator_message(ChatRole::Event, &content)
            .await
        {
            warn!(error = %e, "failed to append obligation turn");
            continue;
        }
        // Only after the turn is durable: a failed append must re-remind next
        // pass rather than going quiet for the full interval.
        if let Err(e) = store.mark_obligations_surfaced(&due).await {
            warn!(error = %e, "failed to record obligation reminders");
        }
    }
}

// --- serial build loop ---

/// Run queued builds one at a time until `shutdown` flips.
///
/// Mirrors [`dispatch_loop`]'s connection handling — own vm-pool connection,
/// reconnect every [`VM_POOL_RETRY`] — but awaits each build inline, which
/// *is* the serialization at the loop level;
/// [`Store::claim_next_queued_build`] is the serialization at the store.
/// Every batch is cut from a base branch that already contains the previous
/// batch. Do not relax this.
///
/// Requires GitHub credentials (the push and the PR are server-side writes);
/// without a token the loop is disabled with a warning and everything else
/// runs normally. Mode gates the *start* of a run only — `Pause` never
/// interrupts a running build, which has more at stake than a scout: a
/// half-built branch has no home.
pub async fn build_loop(
    store: Arc<Store>,
    config: Config,
    in_flight: InFlight,
    mut shutdown: watch::Receiver<bool>,
) {
    let Some(github) = config.github_client() else {
        warn!("no GITHUB_TOKEN; builds disabled (a build must push and open a PR)");
        return;
    };
    let github = Arc::new(github);

    loop {
        if *shutdown.borrow() {
            return;
        }
        let client = match Client::<TasksProtocol>::connect(&config.vm_pool_socket).await {
            Ok(client) => client,
            Err(e) => {
                warn!(
                    socket = %config.vm_pool_socket.display(),
                    error = %e,
                    retry_secs = VM_POOL_RETRY.as_secs(),
                    "vm-pool unavailable — builds disabled"
                );
                tokio::select! {
                    _ = tokio::time::sleep(VM_POOL_RETRY) => continue,
                    _ = shutdown.changed() => return,
                }
            }
        };

        let builder = Builder::new(
            store.clone(),
            client.handle(),
            github.clone(),
            builder_config(&config),
        );

        loop {
            match store.get_mode().await {
                // A build this process inherited is `running` in the store, so
                // `claim_next_queued_build` already refuses to start another.
                // The counter says the same thing in the loop's own terms,
                // where it can be read without a round trip.
                Ok(Mode::Play) if in_flight.builds() == 0 => {
                    match store.claim_next_queued_build().await {
                        Ok(Some(build)) => {
                            let project = match store.get_project(&build.project_id).await {
                                Ok(Some(p)) => p,
                                Ok(None) => {
                                    warn!(build_id = %build.id, "build references a missing project");
                                    let _ = store
                                        .finalize_build_failed(&build.id, "project not found")
                                        .await;
                                    continue;
                                }
                                Err(e) => {
                                    warn!(error = %e, "could not load the build's project");
                                    continue;
                                }
                            };
                            let url = clone_url(&config, &project);
                            // Inline await = serial by construction.
                            match builder.dispatch(build, &url).await {
                                Ok(done) => {
                                    info!(build_id = %done.id, pr = ?done.pr_number, "build succeeded");
                                }
                                Err(e) => {
                                    // Already finalized inside dispatch; a dead
                                    // socket additionally means reconnecting.
                                    if matches!(
                                        e,
                                        BuilderError::Client(_) | BuilderError::StreamClosed
                                    ) {
                                        warn!(error = %e, "lost the vm-pool connection mid-build");
                                        break;
                                    }
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(e) => warn!(error = %e, "could not read the build queue"),
                    }
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "could not read mode; skipping build tick"),
            }

            tokio::select! {
                _ = tokio::time::sleep(DISPATCH_TICK) => {}
                _ = shutdown.changed() => return,
            }
        }
    }
}

/// Where a scout clones from. Derived per project; the token, when set, rides
/// along as basic auth so private repos clone without a credential helper.
fn clone_url(config: &Config, project: &Project) -> String {
    clone_url_for(
        &config.clone_url_base,
        config.github_token.as_deref(),
        project,
    )
}

/// The credentialed clone URL for a project — shared by the scout and builder
/// paths so both sides of the diamond clone (and the builder pushes) the same
/// way. Non-https bases take no credentials; they'd be meaningless.
pub(crate) fn clone_url_for(base: &str, token: Option<&str>, project: &Project) -> String {
    let url = format!(
        "{}/{}/{}.git",
        base.trim_end_matches('/'),
        project.repo_owner,
        project.repo_name
    );
    match (token, url.strip_prefix("https://")) {
        (Some(token), Some(rest)) => format!("https://x-access-token:{token}@{rest}"),
        _ => url,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::{Value, json};

    use super::*;
    use crate::models::{ProjectId, ProjectStatus};

    #[test]
    fn the_shipped_defaults_leave_the_pool_slack() {
        // Asserted against the constants rather than literals, so this moves
        // when they do instead of quietly becoming a claim about the past.
        assert_eq!(
            Capacity::assess(
                vm_pool_manager::DEFAULT_MAX_VMS,
                DEFAULT_SCOUT_MAX_CONCURRENT
            ),
            Capacity::Slack {
                needed: DEFAULT_SCOUT_MAX_CONCURRENT + 1,
                total: vm_pool_manager::DEFAULT_MAX_VMS,
                spare: vm_pool_manager::DEFAULT_MAX_VMS - DEFAULT_SCOUT_MAX_CONCURRENT - 1,
            }
        );
    }

    #[test]
    fn buildkit_does_not_occupy_a_slot() {
        // The sum is scouts + the one serial build lane, and nothing else. A
        // `buildkit` VM is started by the container runtime as a host process
        // the pool never allocated, so counting it would size every pool one
        // too large and make this report wrong in the safe-looking direction.
        assert_eq!(BUILD_LANE_SLOTS, 1);
        assert_eq!(
            Capacity::assess(6, 3),
            Capacity::Slack {
                needed: 4,
                total: 6,
                spare: 2
            },
            "3 scouts + 1 build lane is 4 of 6, not 5 of 6"
        );
    }

    #[test]
    fn a_pool_too_small_is_short_by_what_it_is_missing() {
        assert_eq!(
            Capacity::assess(3, 4),
            Capacity::Short {
                needed: 5,
                total: 3
            }
        );
        // The degenerate case: a pool reporting nothing at all is short, not a
        // subtraction overflow.
        assert_eq!(
            Capacity::assess(0, 1),
            Capacity::Short {
                needed: 2,
                total: 0
            }
        );
    }

    #[test]
    fn an_exact_fit_is_reported_as_having_no_slack() {
        assert_eq!(
            Capacity::assess(3, 2),
            Capacity::NoSlack {
                needed: 3,
                total: 3
            }
        );
        assert_eq!(
            Capacity::assess(6, 5),
            Capacity::NoSlack {
                needed: 6,
                total: 6
            }
        );
    }

    fn config() -> Config {
        Config {
            data_dir: PathBuf::from("/tmp"),
            port: 0,
            poll_interval: Duration::from_secs(60),
            startup_mode: DEFAULT_STARTUP_MODE,
            scout_max_concurrent: 1,
            scout_image: DEFAULT_SCOUT_IMAGE.into(),
            scout_timeout: Duration::from_secs(DEFAULT_SCOUT_TIMEOUT_SECS),
            vm_pool_socket: PathBuf::from(DEFAULT_VM_POOL_SOCKET),
            github_token: None,
            github_api_url: None,
            intake: IntakeFilter::All,
            clone_url_base: DEFAULT_CLONE_URL_BASE.into(),
            scout_base_branch: "main".into(),
            vm_config: VmConfig::default(),
            builder_vm_config: VmConfig::default(),
            builder_image: DEFAULT_BUILDER_IMAGE.into(),
            builder_timeout: Duration::from_secs(DEFAULT_BUILDER_TIMEOUT_SECS),
            github_rest_api_url: None,
            orchestrator_cmd: "true".into(),
            orchestrator_timeout: Duration::from_secs(60),
            orchestrator_workdir: None,
            orchestrator_target_dir: None,
        }
    }

    fn project() -> Project {
        Project {
            id: ProjectId::new(),
            repo_owner: "iamnbutler".into(),
            repo_name: "tasks".into(),
            added_at: Utc::now(),
            status: ProjectStatus::Active,
        }
    }

    /// The shape that was OOM-killing builds: cargo would have taken its
    /// default of 4 concurrent jobs against 4 GB and lost a linker to the
    /// kernel. One job is what the field reports say completes.
    #[test]
    fn four_cpus_and_four_gb_derive_a_single_build_job() {
        assert_eq!(build_jobs(4, 4096), 1);
    }

    #[test]
    fn build_jobs_scale_with_memory_and_stop_at_the_cpu_count() {
        assert_eq!(build_jobs(4, 6144), 2, "the scout default");
        assert_eq!(build_jobs(4, 8192), 3, "the builder default");
        // Memory to spare, but there is no point running more jobs than CPUs.
        assert_eq!(build_jobs(4, 65536), 4);
        // vm-pool's own default shape, and anything smaller, still builds.
        assert_eq!(build_jobs(2, 2048), 1);
        assert_eq!(build_jobs(1, 65536), 1);
    }

    #[test]
    fn clone_url_is_anonymous_without_a_token() {
        assert_eq!(
            clone_url(&config(), &project()),
            "https://github.com/iamnbutler/tasks.git"
        );
    }

    #[test]
    fn clone_url_carries_the_token_as_basic_auth() {
        let mut config = config();
        config.github_token = Some("ghp_secret".into());
        assert_eq!(
            clone_url(&config, &project()),
            "https://x-access-token:ghp_secret@github.com/iamnbutler/tasks.git"
        );
    }

    /// A non-https base (a local clone in tests, a git:// mirror) takes no
    /// credentials — they'd be meaningless in the URL.
    #[test]
    fn clone_url_leaves_non_https_bases_alone() {
        let mut config = config();
        config.github_token = Some("ghp_secret".into());
        config.clone_url_base = "file:///srv/repos/".into();
        assert_eq!(
            clone_url(&config, &project()),
            "file:///srv/repos/iamnbutler/tasks.git"
        );
    }

    /// Unset is the whole point of the change: a machine that comes back on
    /// its own comes back quiet.
    #[test]
    fn an_unset_startup_mode_is_pause() {
        assert_eq!(parse_startup_mode(None).unwrap(), Mode::Pause);
        assert_eq!(DEFAULT_STARTUP_MODE, Mode::Pause);
    }

    #[test]
    fn a_configured_startup_mode_is_taken_verbatim() {
        assert_eq!(parse_startup_mode(Some("play")).unwrap(), Mode::Play);
        assert_eq!(parse_startup_mode(Some("pause")).unwrap(), Mode::Pause);
        assert_eq!(parse_startup_mode(Some("stop")).unwrap(), Mode::Stop);
        // `.env` files carry stray whitespace; a mode is still a mode.
        assert_eq!(parse_startup_mode(Some("  play  ")).unwrap(), Mode::Play);
    }

    /// This variable decides whether a machine comes back dispatching, so
    /// "silently ignored" is the one outcome that must not be possible.
    #[test]
    fn an_unparseable_startup_mode_refuses_to_boot() {
        let err = parse_startup_mode(Some("yes")).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("TASKS_DEFAULT_MODE"), "{message}");
        assert!(message.contains("play, pause or stop"), "{message}");
        assert!(message.contains("yes"), "{message}");
    }

    async fn mode_events(store: &Store) -> Vec<EventPayload> {
        store
            .all_events()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.payload)
            .filter(|p| {
                matches!(
                    p,
                    EventPayload::ModeChanged { .. } | EventPayload::Note { .. }
                )
            })
            .collect()
    }

    /// The stored mode is overwritten rather than resumed — and the breadcrumb
    /// is a `Note`, because `ModeChanged` is nudge-worthy and would spend an
    /// orchestrator turn on every single restart.
    #[tokio::test]
    async fn a_boot_overwrites_the_stored_mode_without_nudging() {
        let store = Store::open_in_memory().await.unwrap();
        store.set_mode(Mode::Play).await.unwrap();

        apply_startup_mode(&store, Mode::Pause).await.unwrap();

        assert_eq!(store.get_mode().await.unwrap(), Mode::Pause);
        let events = mode_events(&store).await;
        assert_eq!(events.len(), 1, "{events:?}");
        let EventPayload::Note { source, message } = &events[0] else {
            panic!("a boot must not emit ModeChanged: {events:?}");
        };
        assert_eq!(source, STARTUP);
        assert!(message.contains("pause"), "{message}");
        assert!(message.contains("was play"), "{message}");
    }

    /// `TASKS_DEFAULT_MODE=play` is the honest way to keep a host dispatching
    /// across restarts.
    #[tokio::test]
    async fn a_configured_play_boots_playing() {
        let store = Store::open_in_memory().await.unwrap();
        store.set_mode(Mode::Stop).await.unwrap();

        apply_startup_mode(&store, Mode::Play).await.unwrap();

        assert_eq!(store.get_mode().await.unwrap(), Mode::Play);
        assert_eq!(mode_events(&store).await.len(), 1, "the transition is news");
    }

    /// Nothing moved, so nothing is said: the common case is a restart of a
    /// paused server, and a breadcrumb per boot is noise in the feed.
    #[tokio::test]
    async fn a_boot_into_the_stored_mode_says_nothing() {
        let store = Store::open_in_memory().await.unwrap();
        store.set_mode(Mode::Pause).await.unwrap();

        apply_startup_mode(&store, Mode::Pause).await.unwrap();

        assert_eq!(store.get_mode().await.unwrap(), Mode::Pause);
        assert!(mode_events(&store).await.is_empty());
    }

    /// Fake GraphQL endpoint that pops canned responses in request order —
    /// poll_once issues the open-issues query first, then (if anything is
    /// retirable) the close-info query.
    async fn spawn_fake_github(responses: Vec<Value>) -> String {
        use axum::{Router, extract::State, response::Json, routing::post};
        use std::sync::Mutex;

        let queue = Arc::new(Mutex::new(responses));
        let app = Router::new()
            .route(
                "/graphql",
                post(
                    move |State(q): State<Arc<Mutex<Vec<Value>>>>, _body: String| async move {
                        let resp = {
                            let mut g = q.lock().unwrap();
                            if g.is_empty() {
                                json!({"data": {"repository": {"issues": {
                                    "pageInfo": {"hasNextPage": false, "endCursor": null},
                                    "nodes": []}}}})
                            } else {
                                g.remove(0)
                            }
                        };
                        Json(resp)
                    },
                ),
            )
            .with_state(queue);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/graphql", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    async fn seed_task(store: &Store, project: &Project, number: u64, state: TaskState) -> Task {
        let now = Utc::now();
        let task = Task {
            id: TaskId::new(),
            project_id: project.id.clone(),
            gh_issue_number: number,
            title: format!("issue {number}"),
            body: String::new(),
            labels: vec![],
            gh_state: GhState::Open,
            state,
            priority: 0,
            manual_rank: (state == TaskState::Queued).then_some(1),
            dispatch_attempts: 0,
            ingested_at: now,
            updated_at: now,
            scout_directions: None,
        };
        store.insert_task(&task).await.unwrap();
        task
    }

    /// The real `record_outcome`, against a real store, one round past
    /// `MAX_DISPATCH_ATTEMPTS`: a vm-pool that goes away must never reject a
    /// task, however many times it goes away.
    ///
    /// It also pins the *other* half of the answer — the call still reports
    /// `ConnectionLost(true)`, because this predicate decides reconnection as
    /// well as the strike, and a fix that waived the attempt by making the
    /// error look ordinary would silently stop the loop rebuilding its client.
    #[tokio::test]
    async fn a_closed_event_stream_costs_the_task_no_dispatch_attempt() {
        use crate::protocol::FailureClass;

        let store = Store::open_in_memory().await.unwrap();
        let project = project();
        store.insert_project(&project).await.unwrap();
        let task = seed_task(&store, &project, 1, TaskState::Queued).await;

        for round in 1..=MAX_DISPATCH_ATTEMPTS + 1 {
            let ConnectionLost(lost) =
                record_outcome(&store, &task.id, Err(ScoutError::StreamClosed))
                    .await
                    .unwrap();
            assert!(lost, "round {round} must still reconnect the client");

            let after = store.get_task(&task.id).await.unwrap().unwrap();
            assert_eq!(after.dispatch_attempts, 0, "round {round} charged a strike");
            assert_eq!(
                after.state,
                TaskState::Queued,
                "round {round} moved the task"
            );
        }

        // The negative half. Three verdicts and the task really is rejected —
        // without this, "attempts stayed 0" reads identically to the cap
        // having been switched off.
        let doomed = seed_task(&store, &project, 2, TaskState::Queued).await;
        for _ in 0..MAX_DISPATCH_ATTEMPTS {
            record_outcome(
                &store,
                &doomed.id,
                Err(ScoutError::StoppedEarly {
                    reason: "no spec".into(),
                    class: FailureClass::Verdict,
                }),
            )
            .await
            .unwrap();
        }
        let after = store.get_task(&doomed.id).await.unwrap().unwrap();
        assert_eq!(after.dispatch_attempts, MAX_DISPATCH_ATTEMPTS);
        assert_eq!(after.state, TaskState::Rejected);
    }

    /// A paused repo is a repo the dispatcher walks *past*, not one it stops
    /// at. That `continue` rather than `break` is the whole difference between
    /// pausing one repo and pausing the server.
    #[tokio::test]
    async fn next_dispatchable_skips_a_paused_repo_without_starving_the_queue() {
        let store = Store::open_in_memory().await.unwrap();
        let paused = project();
        store.insert_project(&paused).await.unwrap();
        let live = Project {
            id: ProjectId::new(),
            repo_owner: "iamnbutler".into(),
            repo_name: "other".into(),
            added_at: Utc::now(),
            status: ProjectStatus::Active,
        };
        store.insert_project(&live).await.unwrap();

        // The paused repo's task is at the head of the queue.
        let head = seed_task(&store, &paused, 1, TaskState::Queued).await;
        let behind = seed_task(&store, &live, 2, TaskState::Queued).await;
        store
            .set_queue_order(&[head.id.clone(), behind.id.clone()])
            .await
            .unwrap();
        store
            .set_project_status(&paused.id, ProjectStatus::Paused)
            .await
            .unwrap();

        let skip = HashSet::new();
        let (task, project) = next_dispatchable(&store, &skip)
            .await
            .unwrap()
            .expect("the repo behind the paused one is still dispatchable");
        assert_eq!(task.id, behind.id);
        assert_eq!(project.id, live.id);

        // Pause that one too and there is simply nothing to dispatch — the
        // head's task is still `queued`, not rejected or returned.
        store
            .set_project_status(&live.id, ProjectStatus::Paused)
            .await
            .unwrap();
        assert!(next_dispatchable(&store, &skip).await.unwrap().is_none());
        assert_eq!(
            store.get_task(&head.id).await.unwrap().unwrap().state,
            TaskState::Queued
        );
    }

    /// The whole closure-derived retirement path: issues vanish from the open
    /// set, the poller queries their close reason at decision time, and
    /// picked-up work concludes as done/rejected — while a scout in flight and
    /// untouched backlog rows are left alone.
    #[tokio::test]
    async fn poll_once_retires_picked_up_work_when_issues_close() {
        let store = Store::open_in_memory().await.unwrap();
        let project = project();
        store.insert_project(&project).await.unwrap();

        let queued = seed_task(&store, &project, 1, TaskState::Queued).await;
        let in_review = seed_task(&store, &project, 2, TaskState::InReview).await;
        let ready = seed_task(&store, &project, 3, TaskState::ReadyToBuild).await;
        let scouting = seed_task(&store, &project, 4, TaskState::Scouting).await;
        let backlog = seed_task(&store, &project, 5, TaskState::Backlog).await;

        let url = spawn_fake_github(vec![
            // Poll: the repository has no open issues left.
            json!({"data": {"repository": {"issues": {
                "pageInfo": {"hasNextPage": false, "endCursor": null},
                "nodes": []}}}}),
            // Close-info lookup for the three retirable tasks. Issue 3 no
            // longer resolves (deleted / converted) -> retired as done anyway.
            json!({
                "data": {"repository": {
                    "i1": {"number": 1, "state": "CLOSED", "stateReason": "COMPLETED"},
                    "i2": {"number": 2, "state": "CLOSED", "stateReason": "NOT_PLANNED"},
                    "i3": null,
                }},
                "errors": [{"message": "Could not resolve to an Issue with the number of 3."}]
            }),
        ])
        .await;
        let github = GitHubClient::with_base_url("token", url);

        poll_once(&store, &github, &IntakeFilter::All, "main")
            .await
            .unwrap();

        let state_of = async |id: &TaskId| store.get_task(id).await.unwrap().unwrap();
        let queued = state_of(&queued.id).await;
        assert_eq!(queued.state, TaskState::Done);
        assert_eq!(queued.manual_rank, None, "retirement frees the queue slot");
        assert_eq!(state_of(&in_review.id).await.state, TaskState::Rejected);
        assert_eq!(state_of(&ready.id).await.state, TaskState::Done);
        assert_eq!(
            state_of(&scouting.id).await.state,
            TaskState::Scouting,
            "a scout in flight is not interrupted; it retires from in_review next poll"
        );
        assert_eq!(state_of(&backlog.id).await.state, TaskState::Backlog);

        let payloads: Vec<EventPayload> = store
            .all_events()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.payload)
            .collect();
        assert!(payloads.contains(&EventPayload::TaskStateChanged {
            task_id: queued.id.clone(),
            from: TaskState::Queued,
            to: TaskState::Done,
        }));
        assert!(payloads.contains(&EventPayload::TaskStateChanged {
            task_id: in_review.id,
            from: TaskState::InReview,
            to: TaskState::Rejected,
        }));
    }

    /// A failed close-reason lookup must not strand the candidates — they stay
    /// picked up and the next poll tries again.
    #[tokio::test]
    async fn poll_once_leaves_retirement_for_next_poll_when_lookup_fails() {
        let store = Store::open_in_memory().await.unwrap();
        let project = project();
        store.insert_project(&project).await.unwrap();
        let task = seed_task(&store, &project, 1, TaskState::InReview).await;

        let url = spawn_fake_github(vec![
            json!({"data": {"repository": {"issues": {
                "pageInfo": {"hasNextPage": false, "endCursor": null},
                "nodes": []}}}}),
            json!({"errors": [{"message": "Bad credentials"}]}),
        ])
        .await;
        let github = GitHubClient::with_base_url("token", url);

        poll_once(&store, &github, &IntakeFilter::All, "main")
            .await
            .unwrap();

        let after = store.get_task(&task.id).await.unwrap().unwrap();
        assert_eq!(after.state, TaskState::InReview, "still a candidate");
        assert_eq!(
            after.gh_state,
            GhState::Closed,
            "closure was still recorded"
        );
        assert_eq!(
            store.list_retirable_tasks(&project.id).await.unwrap().len(),
            1
        );
    }

    /// The failure #883 cost a scout run: a loop that never observed the
    /// shutdown flag, awaited forever, and reached the operator as a SIGKILL
    /// 75 seconds later with nothing in the log naming it.
    #[tokio::test]
    async fn a_background_loop_that_ignores_shutdown_is_bounded_and_named() {
        let grace = Duration::from_millis(200);
        let wedged = || tokio::spawn(std::future::pending::<()>());

        let started = std::time::Instant::now();
        let stuck = drain_background(
            grace,
            vec![
                ("poll", wedged()),
                ("nudge", wedged()),
                ("obligations", wedged()),
            ],
        )
        .await;

        // Every one of them, not just whichever was awaited first — otherwise
        // the log points at a handle rather than at the bug.
        assert_eq!(stuck, vec!["poll", "nudge", "obligations"]);
        // And one shared deadline: per-task would be ~3x this.
        let elapsed = started.elapsed();
        assert!(
            elapsed < grace * 3,
            "the deadline is shared, not per task: {elapsed:?}"
        );
    }

    /// The other half, without which the assertion above is satisfied by
    /// reporting everything: a loop that stops when told is not reported, even
    /// when it is awaited after the deadline has already passed.
    #[tokio::test]
    async fn loops_that_stop_when_told_are_not_reported() {
        let done = || tokio::spawn(async {});
        // Give them a moment to actually finish, so this is not a race the
        // timeout wins by accident.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let stuck = drain_background(
            Duration::from_millis(100),
            vec![
                ("poll", tokio::spawn(std::future::pending::<()>())),
                ("nudge", done()),
                ("obligations", done()),
            ],
        )
        .await;

        assert_eq!(
            stuck,
            vec!["poll"],
            "a finished handle answers immediately, deadline or no deadline"
        );
    }
}
