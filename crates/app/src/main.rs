//! Tasks platform — main entry point.
//!
//! Constructs all components and runs the platform.
//! This binary is intentionally thin — logic lives in the library crates.

mod automation_runner;
mod config;
mod memory;
mod problem_tracker;
mod run_loop;
mod scheduler;
mod update;
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

/// Rebuild state from GitHub (issue #256).
///
/// This is a standalone CLI operation that:
/// 1. Clears tasks and merge_queue tables (preserving accounting and projects)
/// 2. Polls all tracked projects from scratch
/// 3. Re-creates tasks from all open issues
/// 4. Re-creates merge queue entries from all open PRs
async fn cmd_rebuild() -> Result<(), String> {
    use server::workflow::LabelConfig;
    use tasks_github::client::GitHubClient;
    use tasks_github::poller::RepoPoller;

    let github_token = std::env::var("GITHUB_TOKEN")
        .map_err(|_| "GITHUB_TOKEN environment variable not set")?;

    let store = open_store()?;

    // Get list of projects before clearing
    let projects = store
        .list_projects()
        .map_err(|e| format!("Failed to list projects: {e}"))?;

    if projects.is_empty() {
        eprintln!("No projects configured. Nothing to rebuild.");
        return Ok(());
    }

    // Clear tasks and merge_queue tables
    let tasks_cleared = store
        .clear_tasks()
        .map_err(|e| format!("Failed to clear tasks: {e}"))?;
    let merge_cleared = store
        .clear_merge_queue()
        .map_err(|e| format!("Failed to clear merge queue: {e}"))?;

    eprintln!(
        "Cleared {} tasks and {} merge queue entries",
        tasks_cleared, merge_cleared
    );

    let label_config = LabelConfig::default();
    let mut total_tasks = 0;
    let mut total_merge_entries = 0;

    // Poll each project and recreate state
    for project in &projects {
        let parts: Vec<&str> = project.repo.split('/').collect();
        if parts.len() != 2 {
            eprintln!("Skipping invalid project repo format: {}", project.repo);
            continue;
        }

        let (owner, repo_name) = (parts[0], parts[1]);
        eprintln!("Polling {}/{}...", owner, repo_name);

        let client = GitHubClient::new(&github_token);
        let mut poller = RepoPoller::new(client, owner, repo_name);

        match poller.poll().await {
            Ok(result) => {
                // Create tasks from issues
                for issue in &result.issues {
                    if let Some(task) =
                        server::scheduler::issue_to_task(issue, &project.id, &label_config)
                    {
                        if let Err(e) = store.save_task(&task) {
                            eprintln!(
                                "  Warning: failed to save task for issue #{}: {}",
                                issue.number, e
                            );
                        } else {
                            total_tasks += 1;
                        }
                    }
                }

                // Create merge queue entries from open, non-draft PRs
                for pr in &result.pull_requests {
                    if pr.state == tasks_github::model::PullRequestState::Open && !pr.is_draft {
                        let pr_url = format!(
                            "https://github.com/{}/{}/pull/{}",
                            pr.owner, pr.repo, pr.number
                        );
                        let entry_id = format!("mq-{}-{}-pr-{}", pr.owner, pr.repo, pr.number);

                        // Try to find linked task by branch name.
                        // Skip PRs with no linked task — a merge queue entry with
                        // an empty task_id is unusable.
                        let task_id = match find_task_by_branch_in_store(&store, &pr.head_ref) {
                            Some(id) => id,
                            None => {
                                eprintln!(
                                    "  Skipping PR #{}: no linked task found for branch {}",
                                    pr.number, pr.head_ref
                                );
                                continue;
                            }
                        };

                        let entry = server::model::merge_queue::MergeQueueEntry::new(
                            entry_id,
                            task_id,
                            &pr_url,
                        );

                        if let Err(e) = store.save_merge_entry(&entry) {
                            eprintln!(
                                "  Warning: failed to save merge entry for PR #{}: {}",
                                pr.number, e
                            );
                        } else {
                            total_merge_entries += 1;
                        }
                    }
                }

                eprintln!(
                    "  {} issues, {} PRs processed",
                    result.issues.len(),
                    result.pull_requests.len()
                );
            }
            Err(e) => {
                eprintln!("  Error polling {}/{}: {}", owner, repo_name, e);
            }
        }
    }

    eprintln!();
    eprintln!(
        "Rebuild complete: {} tasks, {} merge queue entries created",
        total_tasks, total_merge_entries
    );

    Ok(())
}

/// Find a task ID by its branch name from the store.
fn find_task_by_branch_in_store(store: &tasks_store::Store, branch: &str) -> Option<String> {
    // Strip the "tasks/" prefix
    let branch_suffix = branch.strip_prefix("tasks/")?;

    let tasks = store.list_tasks().ok()?;

    // New format: "tasks/{task_id}--{unique_suffix}"
    if let Some((task_id, _suffix)) = branch_suffix.split_once("--") {
        if tasks.iter().any(|t| t.id == task_id) {
            return Some(task_id.to_string());
        }
    }

    // Legacy format: "tasks/{task_id}" (exact match)
    if tasks.iter().any(|t| t.id == branch_suffix) {
        return Some(branch_suffix.to_string());
    }

    None
}

fn print_usage() {
    eprintln!("Usage: tasks-app <command>");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  run [--web]               Start the platform (default)");
    eprintln!("  add-project <owner/repo>  Add a project to track");
    eprintln!("  remove-project <id>       Remove a project");
    eprintln!("  list-projects             List configured projects");
    eprintln!("  rebuild                   Rebuild state from GitHub (clears tasks/merge queue)");
}

#[tokio::main]
async fn main() {
    // Initialize tracing: stderr + rotating log file at ~/.tasks/server.log
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let log_dir = data_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = format!("{log_dir}/server.log");

    // Truncate log file if it exceeds ~3000 lines
    if let Ok(contents) = std::fs::read_to_string(&log_path) {
        let lines: Vec<&str> = contents.lines().collect();
        if lines.len() > 3000 {
            let trimmed = lines[lines.len() - 2000..].join("\n");
            let _ = std::fs::write(&log_path, trimmed + "\n");
        }
    }

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("failed to open log file");

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
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
        "rebuild" => cmd_rebuild().await,
        "run" => {
            let mut config = match AppConfig::from_env() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Configuration error: {e}");
                    std::process::exit(1);
                }
            };
            if args.iter().any(|a| a == "--web") {
                config.web = true;
            }
            match run_loop::run(config).await {
                Ok(run_loop::RunResult::Normal) => Ok(()),
                Ok(run_loop::RunResult::UpdateRestart) => {
                    eprintln!("Exiting for update (code 100)");
                    std::process::exit(update::UPDATE_EXIT_CODE);
                }
                Err(e) => Err(e.to_string()),
            }
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
