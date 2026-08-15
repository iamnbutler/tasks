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
use tracing::{info, warn};
use vm_pool_client::{Client, ClientError};
use vm_pool_protocol::{VmConfig, VmId};

use crate::brief::Brief;
use crate::briefing::{BriefingConfig, Briefings};
use crate::builder::{Builder, BuilderConfig, BuilderError};
use crate::events::EventPayload;
use crate::github::{GitHubClient, IntakeFilter};
use crate::models::{ChatRole, GhState, Mode, Project, Spec, Task, TaskId, TaskState};
use crate::orchestrator::{self, Orchestrator, OrchestratorConfig};
use crate::protocol::TasksProtocol;
use crate::scout::{Scout, ScoutConfig, ScoutError, ScoutTarget};
use crate::server;
use crate::store::{ResumedWork, Store, StoreError};

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
const DEFAULT_ORCHESTRATOR_TIMEOUT_SECS: u64 = 600;
/// Default agent command for Home briefing generations. Read-only on
/// purpose: gh/curl/git-log/git-diff and nothing else — a briefing agent
/// that can write is a misconfiguration. The quoted permission list is why
/// `BRIEFING_CMD` gets shell-style splitting (see `briefing::split_command`).
const DEFAULT_BRIEFING_CMD: &str = "claude --print --allowedTools \
     \"Bash(gh:*),Bash(curl:*),Bash(git log:*),Bash(git diff:*)\"";
/// Briefings stay fresh this long (`BRIEFING_TTL_SECS`).
const DEFAULT_BRIEFING_TTL_SECS: u64 = 900;
/// Wall-clock budget per briefing generation (`BRIEFING_TIMEOUT_SECS`).
const DEFAULT_BRIEFING_TIMEOUT_SECS: u64 = 300;
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
    /// Agent command for Home briefing generations (`BRIEFING_CMD`).
    pub briefing_cmd: String,
    /// Freshness window for briefings (`BRIEFING_TTL_SECS`).
    pub briefing_ttl: Duration,
    /// Wall-clock budget per briefing generation (`BRIEFING_TIMEOUT_SECS`).
    pub briefing_timeout: Duration,
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
            briefing_cmd: env_string("BRIEFING_CMD").unwrap_or_else(|| DEFAULT_BRIEFING_CMD.into()),
            briefing_ttl: Duration::from_secs(parse_env(
                "BRIEFING_TTL_SECS",
                "a number of seconds",
                DEFAULT_BRIEFING_TTL_SECS,
            )?),
            briefing_timeout: Duration::from_secs(parse_env(
                "BRIEFING_TIMEOUT_SECS",
                "a number of seconds",
                DEFAULT_BRIEFING_TIMEOUT_SECS,
            )?),
        })
    }

    /// The working directory both the orchestrator and briefing agents run
    /// in: `ORCHESTRATOR_WORKDIR` (the repo checkout, in production) or a
    /// neutral dir under the data dir.
    fn agent_workdir(&self) -> PathBuf {
        self.orchestrator_workdir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("orchestrator"))
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

pub fn data_dir() -> Result<PathBuf, ConfigError> {
    if let Some(dir) = env_string("TASKS_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").ok_or(ConfigError::NoHome)?;
    Ok(PathBuf::from(home).join(".local/state/tasks-v2"))
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
    let briefings = Arc::new(Briefings::new(
        store.clone(),
        BriefingConfig {
            command: config.briefing_cmd.clone(),
            timeout: config.briefing_timeout,
            ttl: config.briefing_ttl,
            workdir: config.agent_workdir(),
            api_port: config.port,
        },
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
        Some(briefings),
        config.github_client().map(Arc::new),
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
                let _ = poll.await;
                let _ = nudge.await;
                let _ = obligations.await;
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

/// Clean up work a previous process abandoned mid-flight.
///
/// See [`Store::reconcile_orphaned_work`] for what that means row by row.
pub async fn reconcile_startup(store: &Store) -> Result<(), StoreError> {
    reconcile_startup_except(store, &ResumedWork::default()).await
}

/// [`reconcile_startup`], minus the rows a reattach already owns.
///
/// Must run after [`resume_in_flight`] and before any loop starts. "Orphaned"
/// is no longer the same thing as "`running` at startup": a session whose VM
/// is still alive is running in the ordinary sense, and concluding it would
/// destroy exactly the run reattachment exists to save. What is left after the
/// resume — rows whose VM is gone, or that could not be picked up — is
/// orphaned in the old sense, and is treated exactly as before.
pub async fn reconcile_startup_except(
    store: &Store,
    resumed: &ResumedWork,
) -> Result<(), StoreError> {
    let report = store.reconcile_orphaned_work_except(resumed).await?;
    if !report.is_empty() {
        info!(
            sessions = report.sessions,
            tasks = report.tasks,
            builds = report.builds,
            "reconciled work orphaned by a previous run"
        );
    }
    Ok(())
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
/// resumable rows, an unreachable pool, a missing task or project, or no
/// `GITHUB_TOKEN` for a build — each leaves the row untouched for
/// [`reconcile_startup_except`], which writes it off exactly as before.
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

    loop {
        match store.get_mode().await {
            Ok(Mode::Stop) => {}
            Ok(_) => match poll_once(&store, &github, &config.intake).await {
                Ok(0) => {}
                Ok(n) => info!(ingested = n, "poll ingested new tasks"),
                Err(e) => warn!(error = %e, "poll failed"),
            },
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
pub async fn poll_once(
    store: &Store,
    github: &GitHubClient,
    intake: &IntakeFilter,
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
            if !intake.admits(&issue) {
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

        // Closure-derived retirement: issue closure IS the "done" signal for
        // picked-up work — there is no manual mark-done. The close *reason* is
        // GitHub-owned, so it's queried here at decision time, never persisted.
        let retirable = store.list_retirable_tasks(&project.id).await?;
        if retirable.is_empty() {
            continue;
        }
        let numbers: Vec<u64> = retirable.iter().map(|t| t.gh_issue_number).collect();
        let info = match github
            .issue_close_info(&project.repo_owner, &project.repo_name, &numbers)
            .await
        {
            Ok(info) => info,
            Err(e) => {
                warn!(error = %e, "fetching close reasons failed; retiring next poll");
                continue;
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
    }
    Ok(ingested)
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
        sweep_leaked_vms(&store, &mut client).await;

        dispatch_connected(&store, &config, &in_flight, client, &mut shutdown).await;
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
/// attempt cap.
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

    // A run that could not be picked up after a restart is the restart's
    // fault, not the task's. Reconciliation never charged a strike for
    // orphaning either, and charging one here would mean three restarts could
    // reject a task nobody has anything against.
    if let ScoutError::NotResumable(reason) = &error {
        warn!(task_id = %task_id, reason, "a scout could not be resumed");
        store
            .append_event(EventPayload::Note {
                source: DISPATCHER.into(),
                message: format!(
                    "the scout for {task_id} could not be resumed after a restart \
                     ({reason}); the task is back in the queue and keeps its attempts"
                ),
            })
            .await?;
        return Ok(ConnectionLost(false));
    }

    // Read off the error variant alone, never off the notes table: a task can
    // carry salvage from an *earlier* attempt, and the event log must not
    // credit this run for it.
    let outcome = match &error {
        ScoutError::StoppedEarly(_) => "stopped early (notes salvaged)",
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
fn is_disconnect(error: &ScoutError) -> bool {
    matches!(
        error,
        ScoutError::StreamClosed
            | ScoutError::Client(ClientError::Closed | ClientError::Connect(_))
    )
}

// --- orchestrator loop ---

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
    use crate::models::ProjectId;

    fn config() -> Config {
        Config {
            data_dir: PathBuf::from("/tmp"),
            port: 0,
            poll_interval: Duration::from_secs(60),
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
            briefing_cmd: "true".into(),
            briefing_ttl: Duration::from_secs(DEFAULT_BRIEFING_TTL_SECS),
            briefing_timeout: Duration::from_secs(DEFAULT_BRIEFING_TIMEOUT_SECS),
        }
    }

    fn project() -> Project {
        Project {
            id: ProjectId::new(),
            repo_owner: "iamnbutler".into(),
            repo_name: "tasks".into(),
            added_at: Utc::now(),
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
        };
        store.insert_task(&task).await.unwrap();
        task
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

        poll_once(&store, &github, &IntakeFilter::All)
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

        poll_once(&store, &github, &IntakeFilter::All)
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
}
