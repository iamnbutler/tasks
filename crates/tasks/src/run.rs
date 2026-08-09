//! Server wire-up: the Diamond 1 loop.
//!
//! `tasks serve` composes three long-lived pieces over one store:
//!
//! - [`poll_loop`] — read-only GitHub intake. Every `TASKS_POLL_INTERVAL`
//!   seconds it upserts each project's open issues into tasks.
//! - [`dispatch_loop`] — picks the next task in queue order and hands it to a
//!   [`Scout`], up to `SCOUT_MAX_CONCURRENT` at a time.
//! - the HTTP control API ([`crate::server`]).
//!
//! Both loops read the operating mode from the store on every pass, so a
//! `POST /mode` takes effect within a tick:
//!
//! - `Play` — poll and dispatch.
//! - `Pause` — poll, but start no new scouts.
//! - `Stop` — neither poll nor dispatch.
//!
//! Mode gates *new* dispatches only. A scout already in flight always runs to
//! completion: there is no in-band cancel command (see
//! [`crate::protocol::ScoutCommand`]) — cancelling means deallocating the VM,
//! and that path is deferred.
//!
//! Nothing here is required for the API to work. A missing `GITHUB_TOKEN`
//! disables polling and an unreachable vm-pool socket disables dispatch (with
//! periodic reconnect); the server stays up either way so manual flows over
//! HTTP keep working.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{info, warn};
use vm_pool_client::{Client, ClientError};
use vm_pool_protocol::VmConfig;

use crate::events::EventPayload;
use crate::github::GitHubClient;
use crate::models::{GhState, Mode, Project, Spec, Task, TaskId, TaskState};
use crate::protocol::TasksProtocol;
use crate::scout::{Scout, ScoutConfig, ScoutError, ScoutTarget};
use crate::server;
use crate::store::{Store, StoreError};

pub const DEFAULT_PORT: u16 = 4800;
const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;
const DEFAULT_SCOUT_MAX_CONCURRENT: usize = 2;
const DEFAULT_SCOUT_IMAGE: &str = "agent:v1";
const DEFAULT_VM_POOL_SOCKET: &str = "/tmp/vm-pool.sock";
const DEFAULT_CLONE_URL_BASE: &str = "https://github.com";

/// How often the dispatch loop re-reads mode + queue.
const DISPATCH_TICK: Duration = Duration::from_millis(500);

/// How long to wait before retrying a vm-pool connection.
const VM_POOL_RETRY: Duration = Duration::from_secs(10);

/// How long in-flight scouts get to finish after ctrl_c before we walk away.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// `source` on the breadcrumbs this module writes to the event log.
const DISPATCHER: &str = "dispatcher";

/// Consecutive failed dispatches after which the loop stops retrying a task
/// for the lifetime of the process. Matches the re-explore cap the plan puts
/// on the server rather than the orchestrator.
const MAX_DISPATCH_ATTEMPTS: u32 = 3;

#[derive(Debug, Error)]
pub enum RunError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("config: {0}")]
    Config(#[from] ConfigError),
    #[error("http server: {0}")]
    Io(#[from] std::io::Error),
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
    /// vm-pool service socket (`VM_POOL_SOCKET`).
    pub vm_pool_socket: PathBuf,
    /// `GITHUB_TOKEN`. Absent disables polling and leaves clone URLs anonymous.
    pub github_token: Option<String>,
    /// GraphQL endpoint override (`GITHUB_API_URL`) — GitHub Enterprise, tests.
    pub github_api_url: Option<String>,
    /// Prefix for derived clone URLs (`GITHUB_CLONE_URL_BASE`); the full URL is
    /// `<base>/<owner>/<repo>.git`.
    pub clone_url_base: String,
    /// Branch scouts base their throwaway branch on (`SCOUT_BASE_BRANCH`).
    /// Future: per-project config.
    pub scout_base_branch: String,
    /// VM shape requested from vm-pool for each scout.
    pub vm_config: VmConfig,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
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
            vm_pool_socket: env_string("VM_POOL_SOCKET")
                .unwrap_or_else(|| DEFAULT_VM_POOL_SOCKET.into())
                .into(),
            github_token: env_string("GITHUB_TOKEN"),
            github_api_url: env_string("GITHUB_API_URL"),
            clone_url_base: env_string("GITHUB_CLONE_URL_BASE")
                .unwrap_or_else(|| DEFAULT_CLONE_URL_BASE.into()),
            scout_base_branch: env_string("SCOUT_BASE_BRANCH").unwrap_or_else(|| "main".into()),
            vm_config: VmConfig {
                env: scout_vm_env(),
                ..VmConfig::default()
            },
        })
    }

    fn github_client(&self) -> Option<GitHubClient> {
        let token = self.github_token.as_ref()?;
        Some(match &self.github_api_url {
            Some(url) => GitHubClient::with_base_url(token, url),
            None => GitHubClient::new(token),
        })
    }
}

fn env_string(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.is_empty())
}

/// Credentials for the agent inside Scout VMs, resolved host-side once at
/// startup: `ANTHROPIC_API_KEY` from the environment, else the output of the
/// host's Claude Code `apiKeyHelper` script (`~/.claude/anthropic_key.sh`)
/// when one exists. Injected per-VM via `VmConfig.env`, never baked into
/// images. Empty means scouts will fail agent auth — warned at startup.
fn scout_vm_env() -> Vec<(String, String)> {
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
                    tracing::info!("scout credentials: host apiKeyHelper");
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
    tracing::warn!("no scout credentials found — scouts will fail agent auth");
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
pub async fn open_store(data_dir: &Path) -> Result<Store, RunError> {
    tokio::fs::create_dir_all(data_dir).await?;
    let db_path = data_dir.join("tasks.db");
    info!(db = %db_path.display(), "opening store");
    Ok(Store::open(&db_path).await?)
}

/// Run the server until ctrl_c.
///
/// Shutdown is best-effort: ctrl_c stops the HTTP listener and signals both
/// loops. The poll loop exits at once; the dispatch loop starts nothing new
/// and waits out its in-flight scouts, up to [`SHUTDOWN_GRACE`]. Past that we
/// stop waiting — the abandoned VMs are reaped by vm-pool's health loop, and
/// their sessions stay `running` in the store until a scout retries the task.
pub async fn run(config: Config) -> Result<(), RunError> {
    let store = Arc::new(open_store(&config.data_dir).await?);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let poll = tokio::spawn(poll_loop(
        store.clone(),
        config.clone(),
        shutdown_rx.clone(),
    ));
    let dispatch = tokio::spawn(dispatch_loop(
        store.clone(),
        config.clone(),
        shutdown_rx.clone(),
    ));

    server::serve_with_shutdown(store, config.port, async {
        let _ = tokio::signal::ctrl_c().await;
        info!("shutdown requested");
    })
    .await?;

    let _ = shutdown_tx.send(true);
    let _ = poll.await;
    if tokio::time::timeout(SHUTDOWN_GRACE, dispatch)
        .await
        .is_err()
    {
        warn!(
            grace_secs = SHUTDOWN_GRACE.as_secs(),
            "in-flight scouts did not finish within the grace period; exiting anyway"
        );
    }
    Ok(())
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

    loop {
        match store.get_mode().await {
            Ok(Mode::Stop) => {}
            Ok(_) => match poll_once(&store, &github).await {
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
/// can't stall intake for the others.
pub async fn poll_once(store: &Store, github: &GitHubClient) -> Result<usize, StoreError> {
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

        for issue in issues {
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
    }
    Ok(ingested)
}

// --- scout dispatch ---

/// Keep up to `scout_max_concurrent` scouts running until `shutdown` flips.
///
/// Owns the vm-pool connection: if the socket is missing or the connection
/// drops, dispatch pauses and reconnects every [`VM_POOL_RETRY`] rather than
/// taking the process down.
pub async fn dispatch_loop(store: Arc<Store>, config: Config, mut shutdown: watch::Receiver<bool>) {
    let mut attempts: HashMap<TaskId, u32> = HashMap::new();
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
                    "vm-pool unavailable — scout dispatch disabled"
                );
                tokio::select! {
                    _ = tokio::time::sleep(VM_POOL_RETRY) => continue,
                    _ = shutdown.changed() => return,
                }
            }
        };
        info!(socket = %config.vm_pool_socket.display(), "connected to vm-pool");

        dispatch_connected(&store, &config, client, &mut attempts, &mut shutdown).await;
    }
}

/// The dispatch loop for as long as one vm-pool connection lives. Returns when
/// shutdown is requested or the connection is lost (after draining whatever is
/// still in flight).
async fn dispatch_connected(
    store: &Arc<Store>,
    config: &Config,
    client: Client<TasksProtocol>,
    attempts: &mut HashMap<TaskId, u32>,
    shutdown: &mut watch::Receiver<bool>,
) {
    let scout = Arc::new(Scout::new(
        store.clone(),
        client.handle(),
        ScoutConfig {
            image: config.scout_image.clone(),
            vm_config: config.vm_config.clone(),
        },
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
                &scout,
                attempts,
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
                        match record_outcome(store, attempts, &task_id, result).await {
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
    scout: &Arc<Scout>,
    attempts: &HashMap<TaskId, u32>,
    in_flight: &mut JoinSet<(TaskId, Result<Spec, ScoutError>)>,
    in_flight_ids: &mut HashSet<TaskId>,
) -> Result<(), StoreError> {
    if store.get_mode().await? != Mode::Play {
        return Ok(());
    }

    while in_flight.len() < config.scout_max_concurrent {
        let Some((task, project)) = next_dispatchable(store, in_flight_ids, attempts).await? else {
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
/// applies), state `New`, still open on GitHub, not in flight, not past the
/// attempt cap.
async fn next_dispatchable(
    store: &Store,
    skip: &HashSet<TaskId>,
    attempts: &HashMap<TaskId, u32>,
) -> Result<Option<(Task, Project)>, StoreError> {
    for task in store.list_tasks().await? {
        if task.state != TaskState::New
            || task.gh_state == GhState::Closed
            || skip.contains(&task.id)
            || attempts.get(&task.id).copied().unwrap_or(0) >= MAX_DISPATCH_ATTEMPTS
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

/// Fold a finished dispatch back into the loop's bookkeeping. The store writes
/// themselves already happened inside [`Scout::dispatch`]; this only tracks
/// retries and leaves a breadcrumb on the event log.
async fn record_outcome(
    store: &Store,
    attempts: &mut HashMap<TaskId, u32>,
    task_id: &TaskId,
    result: Result<Spec, ScoutError>,
) -> Result<ConnectionLost, StoreError> {
    let error = match result {
        Ok(spec) => {
            attempts.remove(task_id);
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

    let count = attempts.entry(task_id.clone()).or_insert(0);
    *count += 1;
    warn!(task_id = %task_id, attempt = *count, error = %error, "scout dispatch failed");
    let message = if *count >= MAX_DISPATCH_ATTEMPTS {
        format!("scout for {task_id} failed {count}x, giving up: {error}")
    } else {
        format!("scout for {task_id} failed (attempt {count}): {error}")
    };
    store
        .append_event(EventPayload::Note {
            source: DISPATCHER.into(),
            message,
        })
        .await?;
    Ok(ConnectionLost(false))
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

/// Where a scout clones from. Derived per project; the token, when set, rides
/// along as basic auth so private repos clone without a credential helper.
fn clone_url(config: &Config, project: &Project) -> String {
    let url = format!(
        "{}/{}/{}.git",
        config.clone_url_base.trim_end_matches('/'),
        project.repo_owner,
        project.repo_name
    );
    match (&config.github_token, url.strip_prefix("https://")) {
        (Some(token), Some(rest)) => format!("https://x-access-token:{token}@{rest}"),
        _ => url,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::models::ProjectId;

    fn config() -> Config {
        Config {
            data_dir: PathBuf::from("/tmp"),
            port: 0,
            poll_interval: Duration::from_secs(60),
            scout_max_concurrent: 1,
            scout_image: DEFAULT_SCOUT_IMAGE.into(),
            vm_pool_socket: PathBuf::from(DEFAULT_VM_POOL_SOCKET),
            github_token: None,
            github_api_url: None,
            clone_url_base: DEFAULT_CLONE_URL_BASE.into(),
            scout_base_branch: "main".into(),
            vm_config: VmConfig::default(),
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
}
