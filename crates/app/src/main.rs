//! Tasks platform — main entry point.
//!
//! Constructs all components and runs the platform.
//! This binary is intentionally thin — logic lives in the library crates.

mod config;
mod run_loop;
mod tui;
mod web;

use models::project::Project;
use tasks_store::Store;

use config::AppConfig;

fn data_dir() -> String {
    std::env::var("TASKS_DATA_DIR").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{home}/.tasks")
    })
}

fn open_store() -> Result<Store, String> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create data dir: {e}"))?;
    let db_path = format!("{dir}/db.sqlite");
    Store::open(&db_path).map_err(|e| format!("Failed to open store: {e}"))
}

fn cmd_add_project(repo: &str) -> Result<(), String> {
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2 {
        return Err(format!("Invalid repo format: {repo} (expected owner/repo)"));
    }

    let store = open_store()?;
    let project = Project::new(repo, repo);
    store
        .save_project(&project)
        .map_err(|e| format!("Failed to save project: {e}"))?;
    eprintln!("Added project: {repo}");
    Ok(())
}

fn cmd_remove_project(id: &str) -> Result<(), String> {
    let store = open_store()?;
    let deleted = store
        .delete_project(id)
        .map_err(|e| format!("Failed to delete project: {e}"))?;
    if deleted {
        eprintln!("Removed project: {id}");
    } else {
        eprintln!("Project not found: {id}");
    }
    Ok(())
}

fn cmd_list_projects() -> Result<(), String> {
    let store = open_store()?;
    let projects = store
        .list_projects()
        .map_err(|e| format!("Failed to list projects: {e}"))?;
    if projects.is_empty() {
        eprintln!("No projects configured. Add one with: tasks-app add-project owner/repo");
    } else {
        for p in &projects {
            println!("{} (branch: {})", p.repo, p.default_branch);
        }
    }
    Ok(())
}

fn print_usage() {
    eprintln!("Usage: tasks-app <command>");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  run [--tui] [--web]     Start the platform (default)");
    eprintln!("  add-project <owner/repo>  Add a project to track");
    eprintln!("  remove-project <id>       Remove a project");
    eprintln!("  list-projects             List configured projects");
}

#[tokio::main]
async fn main() {
    // Initialize tracing subscriber (controlled by RUST_LOG env var, default: info)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("run");

    let result = match cmd {
        "add-project" => {
            let repo = match args.get(2) {
                Some(r) => r,
                None => {
                    eprintln!("Usage: tasks-app add-project <owner/repo>");
                    std::process::exit(1);
                }
            };
            cmd_add_project(repo)
        }
        "remove-project" => {
            let id = match args.get(2) {
                Some(r) => r,
                None => {
                    eprintln!("Usage: tasks-app remove-project <id>");
                    std::process::exit(1);
                }
            };
            cmd_remove_project(id)
        }
        "list-projects" => cmd_list_projects(),
        "run" => {
            let mut config = match AppConfig::from_env() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Configuration error: {e}");
                    std::process::exit(1);
                }
            };
            if args.iter().any(|a| a == "--tui") {
                config.tui = true;
            }
            if args.iter().any(|a| a == "--web") {
                config.web = true;
            }
            run_loop::run(config).await.map_err(|e| e.to_string())
        }
        "--help" | "-h" | "help" => {
            print_usage();
            Ok(())
        }
        other => {
            eprintln!("Unknown command: {other}");
            print_usage();
            std::process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
