//! tasks CLI entry point.
//!
//! `serve` runs the server (GitHub poller + scout dispatcher + HTTP control
//! API — see [`tasks::run`]); `reload` / `status` / `stop` are the upgrade
//! loop around it (see [`tasks::reload`]); `add-project` writes straight to
//! the store. Everything else is driven over the API — see [`tasks::server`]
//! for the route list.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use tasks::events::EventPayload;
use tasks::models::{Project, ProjectId, ProjectStatus};
use tasks::reload::{self, ReloadOptions, StopOptions};
use tasks::run::{self, Config};
use tasks::store::Store;

const USAGE: &str = "\
tasks — human-in-the-loop agent orchestration

usage:
  tasks serve [--port N]        run the server: GitHub poller, scout
                                dispatcher and HTTP control API (default
                                port 4800, override with TASKS_SERVER_PORT)
  tasks reload [flags]          build, then swap the running server for the
                                new binary (alias: restart)
  tasks status                  who is serving, since when, what is in flight
  tasks stop [flags]            SIGTERM the running server and wait for it
  tasks add-project <owner/repo>  track a GitHub repository
  tasks vm-pool                 run the vm-pool service specialized for
                                scouts (ContainerRuntime + TasksProtocol)
                                on VM_POOL_SOCKET

reload flags:
  --when-idle                   wait for in-flight scouts/builds to finish
                                (pauses dispatch for the wait; the new server
                                comes up in the mode the old one was in)
  --drain-timeout SECS          how long --when-idle waits (default 3900)
  --force                       swap even with work in flight
  --no-build                    skip the build and swap in this binary
  --repo PATH                   workspace to build in (default: detected)
  --foreground                  exec the new server here instead of
                                backgrounding it
  --port N                      port for the new server (default: the
                                running server's, else 4800)

reload exit codes:
  3 busy (work in flight)   4 drain timed out   5 the swap did not land

stop flags:
  --when-idle                   wait for in-flight scouts/builds to finish
                                before stopping (pauses dispatch for the wait
                                — and leaves it paused, since no boot resumes
                                the stored mode). Plain `tasks stop` is
                                unchanged: immediate and ungated
  --drain-timeout SECS          how long --when-idle waits (default 3900)

stop exit codes:
  3 --when-idle against a server that will not say what is in flight
  4 drain timed out (nothing was stopped)

environment (also read from .env — the data dir's, then the nearest one at or
above the cwd, then the nearest above this binary; the real environment wins):
  TASKS_DATA_DIR         where tasks.db lives (default ~/.local/state/tasks-v2)
  TASKS_SERVER_PORT      default port for `serve`
  TASKS_POLL_INTERVAL    seconds between GitHub polls (default 60)
  TASKS_DEFAULT_MODE     mode every boot starts in: play/pause/stop (default
                         pause). The stored mode is never resumed — only
                         `tasks reload` carries it to the new server
  TASKS_INTAKE_LABEL     ingest only issues carrying this label (default: all)
  SCOUT_MAX_CONCURRENT   scouts running at once (default 2)
  SCOUT_IMAGE            vm-pool image for scouts (default agent:v1)
  SCOUT_TIMEOUT_SECS     wall-clock budget per scout (default 3600)
  VM_POOL_SOCKET         vm-pool service socket (default /tmp/vm-pool.sock)
  GITHUB_TOKEN           required for polling; also used for repo clones
  GITHUB_API_URL         GraphQL endpoint override
  GITHUB_CLONE_URL_BASE  clone URL prefix (default https://github.com)
";

/// Two orderings here are load-bearing, which is why `main` is not itself the
/// `#[tokio::main]` function.
///
/// `.env` is read *before* the subscriber exists, because a `.env` may set
/// `RUST_LOG` — so what it did is reported afterwards rather than logged as it
/// happens. And it is read *before* the runtime starts, because
/// [`std::env::set_var`] is unsafe for exactly one reason, another thread
/// reading the environment concurrently, and an `#[tokio::main]` body is
/// already running on a thread pool by the time its first statement does.
///
/// It runs for every subcommand, not just `serve`: `reload` and `status`
/// resolve `TASKS_DATA_DIR` too, and a `.env` that moved the data dir for one
/// and not the others would mean two answers to "which server?".
///
/// `TASKS_ENV_FILES=off` skips it — and an unreadable value there stops the
/// process before anything is configured, rather than being ignored back into
/// the behaviour it was turning off.
fn main() -> Result<()> {
    let env_sources = tasks::env_file::load()?;

    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .init();
    tasks::env_file::report(&env_sources);

    dispatch()
}

#[tokio::main]
async fn dispatch() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]).await,
        Some("reload") | Some("restart") => reload_cmd(&args[1..]).await,
        Some("status") => status_cmd().await,
        Some("stop") => stop_cmd(&args[1..]).await,
        Some("add-project") => add_project(&args[1..]).await,
        Some("vm-pool") => vm_pool().await,
        Some("-h") | Some("--help") | Some("help") | None => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => {
            eprint!("unknown subcommand: {other}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

async fn serve(args: &[String]) -> Result<()> {
    let mut config = Config::from_env()?;

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--port" => {
                let raw = rest.next().context("--port requires a value")?;
                config.port = raw
                    .parse()
                    .with_context(|| format!("not a port number: {raw}"))?;
            }
            other => bail!("unexpected argument: {other}"),
        }
    }

    run::run(config).await?;
    Ok(())
}

/// `tasks reload` / `tasks restart`: build, report, drain, swap, verify.
///
/// Exits with [`ReloadError::exit_code`](tasks::reload::ReloadError::exit_code)
/// so a script can branch on *why* it failed without parsing prose.
async fn reload_cmd(args: &[String]) -> Result<()> {
    let mut opts = ReloadOptions::new(run::data_dir()?);

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--when-idle" => opts.when_idle = true,
            "--force" => opts.force = true,
            "--no-build" => opts.build = false,
            "--foreground" => opts.foreground = true,
            "--repo" => {
                opts.repo = Some(PathBuf::from(
                    rest.next().context("--repo requires a path")?,
                ));
            }
            "--drain-timeout" => {
                let raw = rest.next().context("--drain-timeout requires a value")?;
                opts.drain_timeout = Duration::from_secs(
                    raw.parse()
                        .with_context(|| format!("not a number of seconds: {raw}"))?,
                );
            }
            "--port" => {
                let raw = rest.next().context("--port requires a value")?;
                opts.port = Some(
                    raw.parse()
                        .with_context(|| format!("not a port number: {raw}"))?,
                );
            }
            other => bail!("unexpected argument: {other}"),
        }
    }

    if let Err(err) = reload::reload(opts).await {
        eprintln!("{err}");
        std::process::exit(err.exit_code());
    }
    Ok(())
}

/// `tasks status`: exits 1 when nothing is serving, so `tasks status &&
/// something` reads the way it looks.
async fn status_cmd() -> Result<()> {
    let (report, serving) = reload::report(&run::data_dir()?).await;
    print!("{report}");
    if !serving {
        std::process::exit(1);
    }
    Ok(())
}

/// `tasks stop`: SIGTERM and wait until it is actually gone — the same
/// implementation the swap uses.
///
/// `--when-idle` waits for a drain point first, on the same predicate
/// `reload --when-idle` waits on, and exits with the same codes for the same
/// reasons (3 busy, 4 drain timed out). The one thing it leaves behind is a
/// paused pipeline, and that is the last thing it prints.
async fn stop_cmd(args: &[String]) -> Result<()> {
    let data_dir = run::data_dir()?;
    let mut opts = StopOptions::default();

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--when-idle" => opts.when_idle = true,
            "--drain-timeout" => {
                let raw = rest.next().context("--drain-timeout requires a value")?;
                opts.drain_timeout = Duration::from_secs(
                    raw.parse()
                        .with_context(|| format!("not a number of seconds: {raw}"))?,
                );
            }
            other => bail!("unexpected argument: {other}"),
        }
    }

    match reload::stop(&data_dir, opts).await {
        Ok(Some(stopped)) => {
            println!(
                "stopped pid {} (port {})",
                stopped.file.pid, stopped.file.port
            );
            if stopped.left_paused {
                println!("{}", reload::render_left_paused(stopped.file.port));
            }
            Ok(())
        }
        Ok(None) => {
            println!("not serving");
            Ok(())
        }
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(err.exit_code());
        }
    }
}

/// The vm-pool service, specialized for Tasks: real containers via the
/// `container` CLI, ScoutCommand/ScoutEvent passthrough. The stock
/// vm-pool-service binary is NoRuntime + ShellProtocol and can't carry our
/// protocol.
async fn vm_pool() -> Result<()> {
    use vm_pool_manager::{ContainerRuntime, PoolConfig};
    use vm_pool_service::{Service, ServiceConfig};

    let socket_path = std::env::var("VM_POOL_SOCKET")
        .unwrap_or_else(|_| "/tmp/vm-pool.sock".into())
        .into();
    let data_dir = run::data_dir()?;
    let config = ServiceConfig {
        socket_path,
        snapshot_dir: data_dir.join("snapshots"),
        pool: PoolConfig::default(),
    };
    let service = Service::<ContainerRuntime, tasks_protocol::TasksProtocol>::with_runtime(
        config,
        ContainerRuntime::new(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("starting vm-pool service: {e}"))?;
    service
        .run()
        .await
        .map_err(|e| anyhow::anyhow!("vm-pool service: {e}"))
}

async fn add_project(args: &[String]) -> Result<()> {
    let spec = args
        .first()
        .context("usage: tasks add-project <owner/repo>")?;
    let (owner, name) = spec
        .split_once('/')
        .filter(|(o, n)| !o.is_empty() && !n.is_empty())
        .with_context(|| format!("expected owner/repo, got {spec}"))?;

    let store = open_store().await?;
    // Case-insensitively, and here rather than only in the handler: this path
    // writes straight to the store, so leaving the check to `POST /projects`
    // would leave the hole open on the one door that bypasses it.
    // `UNIQUE(repo_owner, repo_name)` is case-*sensitive*, so `Owner/Repo`
    // beside `owner/repo` is two projects for one repository.
    if let Some(existing) = store.find_project_by_repo(owner, name).await? {
        bail!("{} is already tracked as {}", existing.slug(), existing.id);
    }
    let project = Project {
        id: ProjectId::new(),
        repo_owner: owner.to_string(),
        repo_name: name.to_string(),
        status: ProjectStatus::Active,
        added_at: Utc::now(),
    };
    store
        .insert_project(&project)
        .await
        .with_context(|| format!("adding project {spec}"))?;
    store
        .append_event(EventPayload::ProjectAdded {
            project_id: project.id.clone(),
        })
        .await?;

    println!(
        "{} {}/{}",
        project.id, project.repo_owner, project.repo_name
    );
    Ok(())
}

async fn open_store() -> Result<Store> {
    Ok(run::open_store(&run::data_dir()?).await?)
}
