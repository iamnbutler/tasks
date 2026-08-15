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
use tasks::models::{Project, ProjectId};
use tasks::reload::{self, ReloadOptions};
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
  tasks stop                    SIGTERM the running server and wait for it
  tasks add-project <owner/repo>  track a GitHub repository
  tasks vm-pool                 run the vm-pool service specialized for
                                scouts (ContainerRuntime + TasksProtocol)
                                on VM_POOL_SOCKET

reload flags:
  --when-idle                   wait for in-flight scouts/builds to finish
                                (pauses dispatch for the wait, restores the
                                mode once the new server is up)
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

environment:
  TASKS_DATA_DIR         where tasks.db lives (default ~/.local/state/tasks-v2)
  TASKS_SERVER_PORT      default port for `serve`
  TASKS_POLL_INTERVAL    seconds between GitHub polls (default 60)
  TASKS_INTAKE_LABEL     ingest only issues carrying this label (default: all)
  SCOUT_MAX_CONCURRENT   scouts running at once (default 2)
  SCOUT_IMAGE            vm-pool image for scouts (default agent:v1)
  SCOUT_TIMEOUT_SECS     wall-clock budget per scout (default 3600)
  VM_POOL_SOCKET         vm-pool service socket (default /tmp/vm-pool.sock)
  GITHUB_TOKEN           required for polling; also used for repo clones
  GITHUB_API_URL         GraphQL endpoint override
  GITHUB_CLONE_URL_BASE  clone URL prefix (default https://github.com)
";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]).await,
        Some("reload") | Some("restart") => reload_cmd(&args[1..]).await,
        Some("status") => status_cmd().await,
        Some("stop") => stop_cmd().await,
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
async fn stop_cmd() -> Result<()> {
    let data_dir = run::data_dir()?;
    match reload::stop(&data_dir).await {
        Ok(Some(file)) => {
            println!("stopped pid {} (port {})", file.pid, file.port);
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
    let project = Project {
        id: ProjectId::new(),
        repo_owner: owner.to_string(),
        repo_name: name.to_string(),
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
