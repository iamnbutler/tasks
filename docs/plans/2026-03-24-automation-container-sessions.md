# Automation Container Sessions Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace direct LLM calls in automation execution with container sessions, giving automations the same capabilities as tasks (GitHub access, file editing, tool use).

**Architecture:** Introduce an `AutomationRunner` in the `app` crate that bridges automation runs to `SessionManager::start_session()`. The runner uses a prefixed session ID (`automation-run:{run_id}`) to distinguish automation sessions from task sessions. A dedicated event listener monitors session events for automation runs and updates the run record on completion. The existing `AutomationExecutor` (direct LLM) is kept as a fallback when containers are unavailable.

**Tech Stack:** Rust, tokio, existing `SessionManager` / container runtime infrastructure.

**Future considerations:** Automations will eventually have their own config layer (safe outputs, frontmatter rules, permissions) layered on top of this. The runner is kept separate from task dispatch to support this divergence.

---

### Task 1: Add `automation_runner` module with `execute_automation_run`

**Files:**
- Create: `crates/app/src/automation_runner.rs`
- Modify: `crates/app/src/main.rs` (add `mod automation_runner;`)

**Step 1: Create the module**

```rust
//! Automation runner — executes automation runs via container sessions.
//!
//! Uses the same container + Claude Code infrastructure as tasks,
//! giving automations full tool access (GitHub, file system, etc.).
//! Kept separate from task dispatch to support future automation-specific
//! config (safe outputs, frontmatter rules, permissions).

use std::sync::Arc;
use std::time::Duration;

use tracing::{error, info, warn};

use tasks_session::SessionManager;
use crate::runtime;
use server::{self, Server};

/// Prefix for automation session IDs to distinguish from task sessions.
const SESSION_ID_PREFIX: &str = "automation-run:";

/// Execute an automation run in a container session.
///
/// This spawns a container, clones the project repo, and runs Claude Code
/// with the automation prompt. Session events are monitored and the run
/// record is updated on completion.
pub async fn execute_automation_run(
    session_manager: Arc<SessionManager<runtime::AppleContainerRuntime>>,
    server: Arc<Server>,
    run_id: String,
    automation_id: String,
    repo_url: String,
    prompt: String,
) {
    let session_id = format!("{}{}", SESSION_ID_PREFIX, run_id);
    let branch = "main".to_string();

    info!(
        run_id = %run_id,
        automation_id = %automation_id,
        session_id = %session_id,
        "Starting automation run in container session"
    );

    match session_manager
        .start_session(
            session_id.clone(),
            repo_url,
            branch,
            prompt,
            None,  // default container config
            None,  // no custom progress threshold
        )
        .await
    {
        Ok(()) => {
            info!(run_id = %run_id, "Automation container session started");
            // Session is now running. The monitor loop in SessionManager
            // will emit events. Task 2 wires up an event listener that
            // detects completion and updates the run record.
        }
        Err(e) => {
            error!(run_id = %run_id, error = %e, "Failed to start automation session");
            if let Err(e2) = server
                .fail_automation_run(&run_id, format!("Failed to start container: {e}"))
                .await
            {
                error!(run_id = %run_id, error = %e2, "Failed to record automation run failure");
            }
        }
    }
}

/// Check whether a session ID belongs to an automation run.
pub fn is_automation_session(session_id: &str) -> bool {
    session_id.starts_with(SESSION_ID_PREFIX)
}

/// Extract the run ID from an automation session ID.
pub fn run_id_from_session(session_id: &str) -> Option<&str> {
    session_id.strip_prefix(SESSION_ID_PREFIX)
}
```

**Step 2: Register the module**

In `crates/app/src/main.rs`, add:
```rust
mod automation_runner;
```

**Step 3: Verify it compiles**

Run: `cargo build -p tasks-app`
Expected: compiles (warnings about unused code are fine)

**Step 4: Commit**

```bash
git add crates/app/src/automation_runner.rs crates/app/src/main.rs
git commit -m "Add automation_runner module for container-based execution"
```

---

### Task 2: Wire automation run event listener into the run loop

**Files:**
- Modify: `crates/app/src/run_loop.rs`
- Modify: `crates/app/src/automation_runner.rs`

The existing `monitor_session` in `SessionManager` emits events like `task:state:completed` and `task:state:failed` keyed by the session's task_id. For automation sessions, these events will have the `automation-run:{run_id}` session ID.

**Step 1: Add event handler function to `automation_runner.rs`**

```rust
use events::{EventBus, EventType};

/// Subscribe to session events and update automation run records on completion.
///
/// Listens for task:state:completed and task:state:failed events where the
/// task_id matches an automation session prefix. Updates the automation run
/// record accordingly.
pub fn spawn_automation_event_listener(
    event_bus: Arc<EventBus>,
    server: Arc<Server>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = event_bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(event) => {
                    // Only handle events for automation sessions
                    if !is_automation_session(&event.task_id) {
                        continue;
                    }

                    let Some(run_id) = run_id_from_session(&event.task_id) else {
                        continue;
                    };

                    match event.event_type {
                        EventType::TaskStateCompleted => {
                            info!(run_id = %run_id, "Automation run completed via session");
                            // Extract output from event data if available
                            let output = event.data
                                .get("summary")
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            if let Err(e) = server
                                .complete_automation_run(run_id, output)
                                .await
                            {
                                error!(run_id = %run_id, error = %e, "Failed to complete automation run");
                            }
                        }
                        EventType::TaskStateFailed => {
                            let error_msg = event.data
                                .get("error")
                                .and_then(|v| v.as_str())
                                .unwrap_or("Session failed")
                                .to_string();
                            warn!(run_id = %run_id, error = %error_msg, "Automation run failed via session");
                            if let Err(e) = server
                                .fail_automation_run(run_id, error_msg)
                                .await
                            {
                                error!(run_id = %run_id, error = %e, "Failed to record automation run failure");
                            }
                        }
                        _ => {
                            // Forward other events as automation run output events
                            // (e.g., agent:message events become visible in the UI)
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "Automation event listener lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    })
}
```

**Step 2: Spawn the listener in run_loop.rs**

After the scheduler is started (around line 1944), add:
```rust
// Start automation event listener to track container session completion
let automation_listener = crate::automation_runner::spawn_automation_event_listener(
    event_bus.clone(),
    server.clone(),
);
```

And abort it on shutdown alongside the other handles.

**Step 3: Verify it compiles**

Run: `cargo build -p tasks-app`

**Step 4: Commit**

```bash
git add crates/app/src/automation_runner.rs crates/app/src/run_loop.rs
git commit -m "Add automation event listener for session completion tracking"
```

---

### Task 3: Update `trigger_automation` handler to use container sessions

**Files:**
- Modify: `crates/app/src/web.rs` (the `trigger_automation` handler)

**Step 1: Replace the executor call with session dispatch**

In the `trigger_automation` handler, replace the executor block with:

```rust
// Dispatch to container session if session manager is available
if let Some(ref session_manager) = state.session_manager {
    let run_id = run.id.clone();
    let automation_id = automation.id.clone();
    let server = state.server.clone();
    let sm = session_manager.clone();
    let repo_url = format!("https://github.com/{}", project.repo);

    // Build the automation prompt with context
    let prompt = format!(
        "You are running an automation for the {} project.\n\n\
         ## Automation: {}\n\n\
         {}\n\n\
         Execute this automation and report results. \
         You have full access to the repository, GitHub, and development tools.",
        project.repo, automation.name, automation.prompt
    );

    tokio::spawn(async move {
        crate::automation_runner::execute_automation_run(
            sm,
            server,
            run_id,
            automation_id,
            repo_url,
            prompt,
        )
        .await;
    });
} else if let Some(executor) = &state.automation_executor {
    // Fallback to direct LLM execution when containers aren't available
    // (existing code — keep as-is)
    ...
} else {
    // No executor available
    ...
}
```

Keep the existing `automation_executor` path as an `else if` fallback for when `session_manager` is `None` (e.g., running without container support).

**Step 2: Verify it compiles**

Run: `cargo build -p tasks-app`

**Step 3: Test manually**

1. Start the server: `make run`
2. Create an automation with prompt "Create a test issue titled 'Automation Test'"
3. Trigger it
4. Verify in the events tab that container session events appear
5. Verify the run completes with actual output (not "I can't interact with GitHub")

**Step 4: Commit**

```bash
git add crates/app/src/web.rs
git commit -m "Use container sessions for automation execution

Automations now run in the same container environment as tasks,
giving them full tool access (GitHub, file system, Claude Code).
Falls back to direct LLM execution when containers unavailable."
```

---

### Task 4: Update scheduler to use container sessions

**Files:**
- Modify: `crates/app/src/scheduler.rs`

The scheduler currently creates runs and immediately auto-completes them (stub). Update it to dispatch via `automation_runner::execute_automation_run` instead.

**Step 1: Update `trigger_run` to dispatch a session**

The scheduler needs access to `SessionManager`. Add it to the `AutomationScheduler` struct and update `trigger_run`:

```rust
// In the scheduler, after creating the run:
if let Some(ref sm) = self.session_manager {
    crate::automation_runner::execute_automation_run(
        sm.clone(),
        self.server.clone(),
        run.id.clone(),
        automation.id.clone(),
        repo_url,
        prompt,
    ).await;
} else {
    // No session manager — auto-complete as before (stub)
    ...
}
```

**Step 2: Update scheduler construction in run_loop.rs**

Pass `session_manager` to `AutomationScheduler::new()`.

**Step 3: Verify it compiles and tests pass**

Run: `cargo build -p tasks-app && cargo test -p tasks-app`

**Step 4: Commit**

```bash
git add crates/app/src/scheduler.rs crates/app/src/run_loop.rs
git commit -m "Wire scheduler to use container sessions for cron automations"
```

---

### Task 5: Clean up and create PR

**Step 1: Run full test suite**

```bash
cargo test --workspace
cd web && npx tsc --noEmit
```

**Step 2: Create PR**

```bash
gh pr create --title "Use container sessions for automation execution" --body "..."
```
