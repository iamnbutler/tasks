# Session Manager & Run Loop Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build the session management layer and the main run loop that connects GitHub polling → task creation → dispatch → container sessions → event bus, making the platform actually run.

**Architecture:** Two new crates. `crates/session/` is a library containing `SessionManager` — it spawns and monitors container sessions, maps supervisor events to platform events, and enforces time limits. `crates/app/` is a thin binary that constructs all components, starts the GitHub poll loop and dispatch tick loop, and wires SessionManager into the dispatch flow. The runtime crate's `Session` uses synchronous I/O (std::sync::mpsc), so session monitors use `spawn_blocking` to bridge into async.

**Tech Stack:** Rust, tokio (spawn, spawn_blocking, interval, select, signal), existing crates (events, github, server, runtime)

---

### Task 1: Create session crate with SessionManager skeleton

**Files:**
- Create: `crates/session/Cargo.toml`
- Create: `crates/session/src/lib.rs`
- Create: `crates/session/src/manager.rs`
- Modify: `Cargo.toml` (workspace — members already includes `crates/*` via glob, so no change needed)
- Modify: `CLAUDE.md` (add session crate to project structure)

**Step 1: Create Cargo.toml**

```toml
[package]
name = "tasks-session"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[dependencies]
events = { path = "../events" }
runtime = { path = "../runtime" }
server = { path = "../server" }
serde.workspace = true
serde_json.workspace = true
tokio.workspace = true
chrono.workspace = true
thiserror.workspace = true
uuid.workspace = true
```

**Step 2: Create lib.rs**

```rust
//! Session management for the Tasks platform.
//!
//! Manages active container sessions — the bridge between dispatcher
//! decisions and running agent containers.

mod manager;

pub use manager::{SessionManager, SessionManagerError, SessionHandle};
```

**Step 3: Create manager.rs with the SessionManager skeleton**

The SessionManager should:
- Be generic over `ContainerRuntime` (for testing with mocks)
- Hold a `HashMap<String, SessionHandle>` mapping task_id → active session info
- Hold an `Arc<events::EventBus>` for publishing platform events
- Hold a `Arc<tokio::sync::RwLock<server::ServerState>>` for reading task/project state (or take a reference to the Server)

Actually, to keep it simple and avoid circular deps, the SessionManager takes:
- An `Arc<EventBus>` for event publishing
- The container runtime (generic `R: ContainerRuntime`)
- A default `ContainerConfig` (image, env) that can be overridden per project

Key types:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use events::{Actor, Event as PlatformEvent, EventBus, EventType};
use runtime::{ContainerConfig, ContainerRuntime, Session, SessionError as RuntimeSessionError};
use runtime::protocol::Event as SupervisorEvent;

#[derive(Debug, Error)]
pub enum SessionManagerError {
    #[error("session error: {0}")]
    Session(#[from] RuntimeSessionError),
    #[error("event store error: {0}")]
    EventStore(#[from] events::StoreError),
    #[error("session already exists for task: {0}")]
    AlreadyExists(String),
    #[error("no session for task: {0}")]
    NotFound(String),
}

/// Handle for a running session — tracks metadata and the monitoring task.
pub struct SessionHandle {
    pub task_id: String,
    pub container_id: String,
    pub started_at: Instant,
    monitor_handle: JoinHandle<()>,
}

/// Manages active container sessions (spec §9).
pub struct SessionManager<R: ContainerRuntime> {
    runtime: Arc<R>,
    event_bus: Arc<EventBus>,
    sessions: Arc<RwLock<HashMap<String, SessionHandle>>>,
    default_config: ContainerConfig,
    ready_timeout: Duration,
    soft_time_limit: Duration,
    hard_time_limit: Duration,
}
```

Implement:
- `new()` constructor
- `active_count()` → number of active sessions
- `has_session()` → bool
- `session_ids()` → list of active task IDs

Do NOT implement `start_session` yet — that's Task 2.

**Step 4: Verify it compiles**

Run: `cargo check -p tasks-session`
Expected: Compiles with no errors.

**Step 5: Update CLAUDE.md**

Add under crates:
```
  - `session/` — Session management: lifecycle, monitoring, event bridging
```

**Step 6: Commit**

```
git add crates/session/ CLAUDE.md
git commit -m "Add session crate with SessionManager skeleton"
```

---

### Task 2: Session start and monitoring loop

**Files:**
- Modify: `crates/session/src/manager.rs`

**Step 1: Implement start_session**

```rust
impl<R: ContainerRuntime + Send + Sync + 'static> SessionManager<R> {
    /// Start a new session for a task (spec §9.1).
    ///
    /// Creates a container, starts the agent with the given prompt,
    /// and spawns a monitoring task that bridges supervisor events
    /// to the platform event bus.
    pub async fn start_session(
        &self,
        task_id: String,
        repo_url: String,
        branch: String,
        prompt: String,
        config: Option<ContainerConfig>,
    ) -> Result<(), SessionManagerError>
```

This method should:
1. Check if a session already exists for this task_id (return AlreadyExists if so)
2. Create a `Session<R>` with the runtime and config
3. Call `session.start(ready_timeout)` to create/start container and wait for ready
4. Call `session.start_agent(repo_url, branch, prompt)` to launch the agent
5. Get the container_id from the session
6. Spawn a monitoring task (see below)
7. Insert a SessionHandle into the sessions map
8. Return Ok

**The monitoring task** is a tokio task spawned with `tokio::spawn`. Since `Session::recv()` is blocking (uses std::sync::mpsc), the monitor needs to use `spawn_blocking` for the recv loop:

```rust
// Inside the spawned monitoring task:
async fn monitor_session(
    task_id: String,
    session: Session<R>,  // moved into this task
    event_bus: Arc<EventBus>,
    sessions: Arc<RwLock<HashMap<String, SessionHandle>>>,
    soft_limit: Duration,
    hard_limit: Duration,
)
```

The monitor:
1. Loops, receiving events via `spawn_blocking(move || session.recv_timeout(1s))`
2. Maps each supervisor event to platform events and publishes them
3. Tracks elapsed time — when soft_limit is exceeded, emits `orchestrator:escalation`
4. When hard_limit is exceeded, stops the agent
5. When `agent:exit` is received, handles success/failure
6. On exit, removes the task from the sessions map

**Event mapping (in the monitor):**

```rust
fn map_supervisor_event(task_id: &str, event: &SupervisorEvent) -> Option<(EventType, serde_json::Value)> {
    match event {
        SupervisorEvent::AgentStarted(e) => Some((
            EventType::TaskStateRunning,
            serde_json::json!({ "pid": e.pid }),
        )),
        SupervisorEvent::AgentStdout(e) => Some((
            EventType::AgentMessage,
            serde_json::json!({ "text": e.data }),
        )),
        SupervisorEvent::AgentStderr(e) => Some((
            EventType::AgentMessage,
            serde_json::json!({ "text": e.data, "stream": "stderr" }),
        )),
        SupervisorEvent::AgentExit(e) => None, // handled separately
        SupervisorEvent::SystemReady(_) => None, // already handled during start
        SupervisorEvent::ExecResult(e) => None, // passthrough, not mapped to platform events
    }
}
```

**Step 2: Implement stop_session and send_chat**

```rust
/// Send a chat message to a running session.
pub async fn send_chat(&self, task_id: &str, message: String) -> Result<(), SessionManagerError>

/// Stop a session's agent process.
pub async fn stop_session(&self, task_id: &str) -> Result<(), SessionManagerError>

/// Stop all active sessions (for shutdown).
pub async fn stop_all(&self)
```

Note: send_chat and stop need access to the Session object. Since the Session is moved into the monitoring task, we need a way to communicate with it. Use a `tokio::sync::mpsc` channel per session:

Define a command enum:
```rust
enum SessionCommand {
    Chat(String),
    Stop,
}
```

The monitoring task holds the `Session` and a `mpsc::Receiver<SessionCommand>`. The SessionHandle holds the `mpsc::Sender<SessionCommand>`. When `send_chat` or `stop_session` is called, it sends a command through the channel.

Update `SessionHandle` to include the sender:
```rust
pub struct SessionHandle {
    pub task_id: String,
    pub container_id: String,
    pub started_at: Instant,
    command_tx: tokio::sync::mpsc::Sender<SessionCommand>,
    monitor_handle: JoinHandle<()>,
}
```

**Step 3: Verify compilation**

Run: `cargo check -p tasks-session`

**Step 4: Commit**

```
git add crates/session/src/manager.rs
git commit -m "Add session start, monitoring loop, and event bridging"
```

---

### Task 3: Session monitor exit handling and time limits

**Files:**
- Modify: `crates/session/src/manager.rs`

**Step 1: Implement exit handling in the monitor**

When `AgentExit` is received:
- **Exit code 0:** Emit `task:state:awaiting_merge`. Session ends normally.
- **Exit non-zero or signal:** This is a failure. The monitor emits `task:state:failed` with failure details in the event data. The caller (app crate run loop) is responsible for checking retry eligibility and transitioning back to `Waiting` — the session crate doesn't own the retry logic (that's in the server crate's dispatcher).

Actually, to keep the session crate focused, the monitor should:
1. Emit an event with the exit details
2. Clean up the session (remove from map)
3. Return a `SessionOutcome` that the app layer can act on

But since the monitor is a spawned task, it can't return values easily. Instead, the monitor emits events that the app's event subscriber reacts to. This is the event-driven architecture — the monitor publishes `task:state:awaiting_merge` or `task:state:failed`, and the dispatch event subscriber picks those up.

For exit code 0: emit `task:state:awaiting_merge`
For exit non-zero: emit `task:state:failed` with `{ "exit_code": N, "signal": S }`

The retry logic (incrementing retry_count, checking max retries, transitioning back to Waiting) happens in the server/app layer when it sees `task:state:failed` events — not in the session crate.

**Step 2: Implement time limit enforcement**

In the monitor loop, track elapsed time:
- `started_at: Instant` is passed in
- Each iteration of the recv loop checks elapsed time
- At `soft_limit`: emit `orchestrator:escalation` event (once)
- At `hard_limit`: send Stop command to the session, then handle the exit normally

**Step 3: Write tests**

The monitoring loop is hard to unit test directly (it needs a real container). Instead, test the helper functions:

1. `map_supervisor_event` — test each variant mapping
2. `backoff and time calculations` — if any pure functions exist

For the SessionManager itself, we'd need a mock ContainerRuntime. Create one in tests:

```rust
#[cfg(test)]
mod tests {
    // MockContainerRuntime that returns fake container IDs
    // and a mock transport
    // Test: start_session creates entry in sessions map
    // Test: stop_session removes entry
    // Test: already_exists error
    // Test: active_count
}
```

Actually, mocking the full container runtime + transport is complex. Focus on testing:
1. `map_supervisor_event` function (pure, easy to test)
2. SessionManager construction and state tracking (active_count, has_session)
3. Integration tests deferred to container e2e tests

**Step 4: Verify**

Run: `cargo test -p tasks-session`

**Step 5: Commit**

```
git add crates/session/src/manager.rs
git commit -m "Add session exit handling, time limits, and event mapping"
```

---

### Task 4: Create app binary crate with startup

**Files:**
- Create: `crates/app/Cargo.toml`
- Create: `crates/app/src/main.rs`
- Create: `crates/app/src/config.rs`
- Modify: `CLAUDE.md`

**Step 1: Create Cargo.toml**

```toml
[package]
name = "tasks-app"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[dependencies]
events = { path = "../events" }
runtime = { path = "../runtime" }
server = { path = "../server" }
tasks-session = { path = "../session" }
tasks-github = { path = "../github" }
serde.workspace = true
serde_json.workspace = true
tokio.workspace = true
chrono.workspace = true
```

**Step 2: Create config.rs**

Simple config read from environment variables:

```rust
//! App configuration — reads from environment.

use std::time::Duration;

/// Top-level app configuration.
pub struct AppConfig {
    /// GitHub personal access token.
    pub github_token: String,
    /// Global max concurrent sessions (default: 5).
    pub max_sessions: u32,
    /// GitHub poll interval (default: 60s).
    pub poll_interval: Duration,
    /// Dispatch tick interval (default: 30s).
    pub dispatch_interval: Duration,
    /// Container image for sessions.
    pub container_image: String,
    /// Session soft time limit (default: 1h).
    pub session_soft_limit: Duration,
    /// Session hard time limit (default: 1h15m).
    pub session_hard_limit: Duration,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, String> {
        let github_token = std::env::var("GITHUB_TOKEN")
            .map_err(|_| "GITHUB_TOKEN not set")?;
        let container_image = std::env::var("TASKS_CONTAINER_IMAGE")
            .unwrap_or_else(|_| "tasks-agent:latest".to_string());
        let max_sessions = std::env::var("TASKS_MAX_SESSIONS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        Ok(Self {
            github_token,
            max_sessions,
            poll_interval: Duration::from_secs(60),
            dispatch_interval: Duration::from_secs(30),
            container_image,
            session_soft_limit: Duration::from_secs(3600),
            session_hard_limit: Duration::from_secs(4500),
        })
    }
}
```

**Step 3: Create main.rs — thin startup**

```rust
//! Tasks platform — main entry point.
//!
//! Constructs all components and runs the platform loops.

mod config;
mod run_loop;

use config::AppConfig;

#[tokio::main]
async fn main() {
    let config = match AppConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = run_loop::run(config).await {
        eprintln!("Fatal error: {e}");
        std::process::exit(1);
    }
}
```

**Step 4: Create a stub run_loop.rs**

```rust
//! Main run loop — wires all components together.

use crate::config::AppConfig;

pub async fn run(config: AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Tasks platform starting...");
    eprintln!("  max_sessions: {}", config.max_sessions);
    eprintln!("  poll_interval: {:?}", config.poll_interval);
    eprintln!("  dispatch_interval: {:?}", config.dispatch_interval);

    // TODO: construct components and start loops
    // For now, just validate the binary builds.

    Ok(())
}
```

**Step 5: Verify**

Run: `cargo build -p tasks-app`

**Step 6: Update CLAUDE.md**

Add under crates:
```
  - `app/` — Binary entry point: startup, run loops, component wiring
```

**Step 7: Commit**

```
git add crates/app/ CLAUDE.md
git commit -m "Add app binary crate with configuration and startup skeleton"
```

---

### Task 5: Implement the run loop — GitHub poll and dispatch tick

**Files:**
- Modify: `crates/app/src/run_loop.rs`

**Step 1: Implement the full run loop**

Replace the stub with the real implementation:

```rust
pub async fn run(config: AppConfig) -> Result<(), Box<dyn std::error::Error>>
```

The function should:

1. **Create infrastructure:**
   - `EventStore` (tempdir for now — persistence is future work)
   - `EventBus`
   - `Server`

2. **Create session manager:**
   - `AppleContainerRuntime`
   - Default `ContainerConfig` from app config
   - `SessionManager::new(runtime, event_bus, config, ...)`

3. **Register projects:**
   - For now, read `TASKS_PROJECTS` env var as comma-separated `owner/repo` list
   - For each, create a `Project`, add to server, create a `RepoPoller`

4. **Emit system:started**

5. **Spawn GitHub poll loop:**
   ```rust
   tokio::spawn(async move {
       let mut interval = tokio::time::interval(config.poll_interval);
       loop {
           interval.tick().await;
           // poll each project's poller
           // for each new issue/PR, create task if not exists
           // emit system:scheduler:tick
       }
   })
   ```

6. **Spawn dispatch tick loop:**
   ```rust
   tokio::spawn(async move {
       let mut interval = tokio::time::interval(config.dispatch_interval);
       let mut event_rx = event_bus.subscribe();

       loop {
           tokio::select! {
               _ = interval.tick() => {
                   // run dispatch
               }
               Ok(event) = event_rx.recv() => {
                   // if event is a dispatch trigger, run dispatch
               }
           }
       }
   })
   ```

7. **Dispatch handler:** When dispatch returns new_work, call `session_manager.start_session()` for each. When it returns resumes, call `session_manager.send_chat()`.

8. **Wait for shutdown signal:**
   ```rust
   tokio::signal::ctrl_c().await?;
   // stop all sessions
   // set mode to Stop
   ```

**Step 2: Verify**

Run: `cargo build -p tasks-app`

**Step 3: Commit**

```
git add crates/app/src/run_loop.rs
git commit -m "Implement run loop with GitHub polling, dispatch ticks, and session management"
```

---

### Task 6: Full workspace verification and CLAUDE.md cleanup

**Step 1: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All tests pass.

**Step 2: Run clippy**

Run: `cargo clippy --workspace`

**Step 3: Fix any issues**

**Step 4: Commit**

```
git commit -m "Workspace verification and cleanup"
```
