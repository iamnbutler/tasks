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
  SCOUT_MAX_CONCURRENT   scouts running at once (default 2). Each holds a
                         vm-pool slot, and the serial build lane holds one
                         more, so the pool must fit SCOUT_MAX_CONCURRENT + 1
                         with slack — see VM_POOL_MAX_VMS
  SCOUT_IMAGE            vm-pool image for scouts (default agent:v1)
  SCOUT_TIMEOUT_SECS     budget per scout (default 3600), measured on both
                         the monotonic and the wall clock, so a host that
                         slept through it reads as a suspend, not a timeout
  VM_POOL_SOCKET         vm-pool service socket (default /tmp/vm-pool.sock)
  VM_POOL_MAX_VMS        VMs `tasks vm-pool` holds at once (default 6). Read
                         by the pool, not by the server, so changing it means
                         restarting the pool
  GITHUB_TOKEN           required for polling; also used for repo clones
  GITHUB_API_URL         GraphQL endpoint override
  GITHUB_CLONE_URL_BASE  clone URL prefix (default https://github.com)
";

// Per-subcommand usage. The top-level `USAGE` answers "what commands are
// there"; these answer "what does this one take", which is what somebody
// typing `--help` at an unfamiliar subcommand is actually asking.

const SERVE_USAGE: &str = "\
usage: tasks serve [--port N]

Run the server in the foreground: GitHub poller, scout dispatcher, serial
build lane and the HTTP control API. Logs to this terminal.

  --port N    HTTP API port (default: TASKS_SERVER_PORT, else 4800)

Every boot starts in TASKS_DEFAULT_MODE (default pause), overwriting whatever
mode the last process left in the store.
";

const RELOAD_USAGE: &str = "\
usage: tasks reload [flags]   (alias: tasks restart)

Build, report, gate, drain, swap, verify — the upgrade loop. Refuses by
default when a scout or a build is in flight.

  --when-idle             wait for in-flight scouts/builds to finish (pauses
                          dispatch for the wait; the new server comes up in
                          the mode the old one was in)
  --drain-timeout SECS    how long --when-idle waits (default 3900)
  --force                 swap anyway, with work in flight
  --no-build              skip the build and swap in this binary
  --repo PATH             workspace to build in (default: detected)
  --foreground            exec the new server here instead of backgrounding it
  --port N                port for the new server (default: the running
                          server's, else 4800)

exit codes: 3 busy   4 drain timed out   5 the swap did not land
";

const STATUS_USAGE: &str = "\
usage: tasks status

Who is serving, since when, and what is in flight — plus the schema version
and the identity of each VM image a run has been observed in. Takes no
arguments. Exits 1 when nothing is serving.
";

const STOP_USAGE: &str = "\
usage: tasks stop [flags]

SIGTERM the running server and wait until it is actually gone.

  --when-idle             wait for in-flight scouts/builds to finish first
                          (pauses dispatch for the wait — and LEAVES it
                          paused, since no boot resumes the stored mode)
  --drain-timeout SECS    how long --when-idle waits (default 3900)

Plain `tasks stop` is unchanged: immediate and ungated.

exit codes: 3 --when-idle against a server that will not say what is in
flight   4 drain timed out (nothing was stopped)
";

const ADD_PROJECT_USAGE: &str = "\
usage: tasks add-project <owner/repo>

Track a GitHub repository, writing straight to the store. Exactly one repo
per invocation. Its issues are ingested into the backlog by the poller and
are never dispatched until something queues them explicitly.

Removal is archive, never delete: POST /projects/{id}/status.
";

const VM_POOL_USAGE: &str = "\
usage: tasks vm-pool

Run the vm-pool daemon specialized for Tasks (ContainerRuntime +
TasksProtocol) on VM_POOL_SOCKET (default /tmp/vm-pool.sock). Takes no
arguments; runs in the foreground until killed.

It REFUSES to start when something is already listening on that socket,
rather than taking the path over: the incumbent would go on holding its VMs
while becoming unreachable. Stop the running daemon first, or point this one
at a different VM_POOL_SOCKET. A socket file left behind by a dead daemon is
unlinked and reclaimed automatically.
";

/// Whether a subcommand's arguments are asking for help.
///
/// Checked in [`dispatch`] **before** the subcommand function is entered,
/// deliberately not inside each one: a check per command is one refactor away
/// from being skipped again by the next command someone adds — which is how
/// `tasks vm-pool --help` came to *start a daemon*, `vm_pool()` having taken
/// no arguments at all, so an unrecognized one meant "proceed".
///
/// It scans every argument rather than just the first, so `tasks reload --repo
/// --help` prints help instead of treating `--help` as a path, and `tasks
/// vm-pool help` prints help too. Both fall in the safe direction. The only
/// theoretical collision — a repo literally named `help` — cannot arise, since
/// `add-project` requires an `owner/repo` slash.
fn wants_help(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "--help" || a == "-h" || a == "help")
}

/// The usage text for a subcommand, or the top-level list for anything else.
fn usage_for(command: &str) -> &'static str {
    match command {
        "serve" => SERVE_USAGE,
        "reload" | "restart" => RELOAD_USAGE,
        "status" => STATUS_USAGE,
        "stop" => STOP_USAGE,
        "add-project" => ADD_PROJECT_USAGE,
        "vm-pool" => VM_POOL_USAGE,
        _ => USAGE,
    }
}

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

    // Ahead of every subcommand, so asking for help is a side-effect-free act
    // no matter which one is asked. This is the whole of the `tasks vm-pool
    // --help` defect: the daemon started before anything looked at the flag.
    if let Some(command) = args.first()
        && wants_help(&args[1..])
    {
        print!("{}", usage_for(command));
        return Ok(());
    }

    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]).await,
        Some("reload") | Some("restart") => reload_cmd(&args[1..]).await,
        Some("status") => status_cmd(&args[1..]).await,
        Some("stop") => stop_cmd(&args[1..]).await,
        Some("add-project") => add_project(&args[1..]).await,
        Some("vm-pool") => vm_pool(&args[1..]).await,
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
async fn status_cmd(args: &[String]) -> Result<()> {
    // Like `serve`/`reload`/`stop`, which already did: an unrecognized
    // argument never means "proceed".
    if let Some(other) = args.first() {
        bail!("unexpected argument: {other}\n\n{STATUS_USAGE}");
    }
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
async fn vm_pool(args: &[String]) -> Result<()> {
    use tracing::info;
    use vm_pool_manager::{ContainerRuntime, PoolConfig};
    use vm_pool_service::{MAX_VMS_ENV, Service, ServiceConfig, max_vms_from_env};

    // It took no arguments at all, which is why an unrecognized one meant
    // "start the daemon".
    if let Some(other) = args.first() {
        bail!("unexpected argument: {other}\n\n{VM_POOL_USAGE}");
    }

    let socket_path = std::env::var("VM_POOL_SOCKET")
        .unwrap_or_else(|_| "/tmp/vm-pool.sock".into())
        .into();
    let data_dir = run::data_dir()?;
    // This is the pool that actually runs scouts and builds, so it has to
    // honour VM_POOL_MAX_VMS too — it hand-builds its ServiceConfig (it needs
    // ContainerRuntime + TasksProtocol, which the stock binary cannot carry),
    // so it cannot inherit `ServiceConfig::from_env`. Resolved before the
    // socket is bound: an unusable value is an exit, not a daemon of a size
    // nobody chose.
    let max_vms = max_vms_from_env()?;
    info!(max_vms, var = MAX_VMS_ENV, "pool capacity");
    let config = ServiceConfig {
        socket_path,
        snapshot_dir: data_dir.join("snapshots"),
        pool: PoolConfig {
            max_vms,
            ..PoolConfig::default()
        },
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
    let spec = args.first().context(ADD_PROJECT_USAGE)?;
    // It used to take `args.first()` and drop the rest in silence, so
    // `add-project a/b c/d` tracked one repo and said nothing about the other.
    if let Some(extra) = args.get(1) {
        bail!("one repository at a time; unexpected argument: {extra}\n\n{ADD_PROJECT_USAGE}");
    }
    let (owner, name) = spec
        .split_once('/')
        .filter(|(o, n)| !o.is_empty() && !n.is_empty())
        .with_context(|| format!("expected owner/repo, got {spec}"))?;

    let store = open_store().await?;
    // Case-insensitively, and here as well as in the handler: this path writes
    // straight to the store, so it would otherwise be the hole in the check.
    // `Owner/Repo` beside `owner/repo` is two projects for one repo, which
    // costs `resolve_project` its answer and doubles every poll.
    if let Some(existing) = store
        .find_project_by_repo(owner, name)
        .await
        .with_context(|| format!("looking up {spec}"))?
    {
        bail!(
            "{} is already tracked as {} ({})",
            existing.slug(),
            existing.id,
            existing.status
        );
    }
    let project = Project {
        id: ProjectId::new(),
        repo_owner: owner.to_string(),
        repo_name: name.to_string(),
        added_at: Utc::now(),
        status: ProjectStatus::Active,
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
