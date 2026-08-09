//! tasks CLI entry point.
//!
//! Two subcommands for now: `serve` runs the HTTP control API, `add-project`
//! writes straight to the store. Everything else is driven over the API — see
//! [`tasks::server`] for the route list.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use tracing::info;

use tasks::events::EventPayload;
use tasks::models::{Project, ProjectId};
use tasks::server;
use tasks::store::Store;

const DEFAULT_PORT: u16 = 4800;

const USAGE: &str = "\
tasks — human-in-the-loop agent orchestration

usage:
  tasks serve [--port N]        run the HTTP control API (default port 4800,
                                override with TASKS_SERVER_PORT)
  tasks add-project <owner/repo>  track a GitHub repository

environment:
  TASKS_DATA_DIR      where tasks.db lives (default ~/.local/state/tasks-v2)
  TASKS_SERVER_PORT   default port for `serve`
";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]).await,
        Some("add-project") => add_project(&args[1..]).await,
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
    let mut port = match std::env::var("TASKS_SERVER_PORT") {
        Ok(raw) => raw
            .parse()
            .with_context(|| format!("TASKS_SERVER_PORT is not a port number: {raw}"))?,
        Err(_) => DEFAULT_PORT,
    };

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--port" => {
                let raw = rest.next().context("--port requires a value")?;
                port = raw
                    .parse()
                    .with_context(|| format!("not a port number: {raw}"))?;
            }
            other => bail!("unexpected argument: {other}"),
        }
    }

    let store = Arc::new(open_store().await?);
    server::serve(store, port).await?;
    Ok(())
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
    let data_dir = data_dir()?;
    tokio::fs::create_dir_all(&data_dir)
        .await
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    let db_path = data_dir.join("tasks.db");
    info!(db = %db_path.display(), "opening store");
    Ok(Store::open(&db_path).await?)
}

fn data_dir() -> Result<PathBuf> {
    if let Ok(s) = std::env::var("TASKS_DATA_DIR") {
        return Ok(PathBuf::from(s));
    }
    let home = dirs_home()?;
    Ok(home.join(".local/state/tasks-v2"))
}

fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME environment variable not set")
}
