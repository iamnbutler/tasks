# Scheduler & Dispatcher Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build the core scheduling loop that connects GitHub issue discovery to session dispatch, completing the server's ability to pick up work and run agents on it.

**Architecture:** The scheduler polls GitHub via `RepoPoller`, converts discovered issues/PRs into tasks, and feeds them to the dispatcher. The dispatcher evaluates candidates (resume vs new work), applies priority sorting, enforces concurrency limits, and creates sessions. Both live in the server crate. A new `prompt` module handles prompt construction. Task gets `retry_count` and `last_failure_at` fields for backoff. A `workflow` module reads `workflow.toml` from repos.

**Tech Stack:** Rust, tokio (timers, spawn), existing crates (events, github, server, runtime), toml (new dep for config parsing)

---

### Task 1: Add retry fields to Task model

**Files:**
- Modify: `crates/server/src/model/task.rs`

**Step 1: Add fields to Task struct**

Add `retry_count` and `last_failure_at` to `Task`:

```rust
// After workspace_id field:
/// Number of times this task has been retried (spec §13.2).
pub retry_count: u32,
/// When the most recent failure occurred (spec §13.2).
pub last_failure_at: Option<DateTime<Utc>>,
```

Initialize both in `Task::new()`:
```rust
retry_count: 0,
last_failure_at: None,
```

**Step 2: Run tests to verify nothing broke**

Run: `cargo test -p server`
Expected: All 21 existing tests pass (fields have defaults).

**Step 3: Commit**

```
git add crates/server/src/model/task.rs
git commit -m "Add retry_count and last_failure_at fields to Task model"
```

---

### Task 2: Add `toml` dependency and workflow config module

**Files:**
- Modify: `crates/server/Cargo.toml`
- Create: `crates/server/src/workflow.rs`
- Modify: `crates/server/src/lib.rs`

**Step 1: Add toml dependency**

In `Cargo.toml`, add under `[dependencies]`:
```toml
toml = "0.8"
```

**Step 2: Write the workflow config module with tests**

Create `crates/server/src/workflow.rs`:

```rust
//! Workflow configuration — spec §14.
//!
//! Reads `workflow.toml` from the project repository root.

use serde::Deserialize;

/// Top-level workflow configuration (spec §14.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WorkflowConfig {
    pub project: ProjectConfig,
    pub dispatch: DispatchConfig,
    pub labels: LabelConfig,
    pub prompt: PromptConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    /// Per-project concurrency limit (spec §12.4).
    pub max_sessions: Option<u32>,
    /// Override project default branch.
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DispatchConfig {
    /// Task retry limit (spec §13.2). Default: 3.
    pub max_retries: u32,
    /// Base backoff delay in seconds (spec §13.2). Default: 5.
    pub retry_base_delay: u64,
    /// Minimum runtime (seconds) to count as "progress" (spec §13.1). Default: 60.
    pub progress_threshold: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LabelConfig {
    /// Issues with these labels are not imported (spec §14.2).
    pub ignore: Vec<String>,
    /// Issues with these labels start in blocked state (spec §14.2).
    pub blocked: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PromptConfig {
    /// Path to system prompt file, relative to repo root (spec §14.1).
    pub system_prompt: Option<String>,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            project: ProjectConfig::default(),
            dispatch: DispatchConfig::default(),
            labels: LabelConfig::default(),
            prompt: PromptConfig::default(),
        }
    }
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            max_sessions: None,
            default_branch: None,
        }
    }
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            retry_base_delay: 5,
            progress_threshold: 60,
        }
    }
}

impl Default for LabelConfig {
    fn default() -> Self {
        Self {
            ignore: Vec::new(),
            blocked: Vec::new(),
        }
    }
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
        }
    }
}

impl WorkflowConfig {
    /// Parse a workflow config from TOML string.
    pub fn parse(toml_str: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let config = WorkflowConfig::parse("").unwrap();
        assert_eq!(config.dispatch.max_retries, 3);
        assert_eq!(config.dispatch.retry_base_delay, 5);
        assert_eq!(config.dispatch.progress_threshold, 60);
        assert!(config.labels.ignore.is_empty());
        assert!(config.prompt.system_prompt.is_none());
        assert!(config.project.max_sessions.is_none());
    }

    #[test]
    fn full_config_parses() {
        let toml = r#"
[project]
max_sessions = 3
default_branch = "develop"

[dispatch]
max_retries = 5
retry_base_delay = 10
progress_threshold = 120

[labels]
ignore = ["wontfix", "duplicate"]
blocked = ["blocked"]

[prompt]
system_prompt = "system-prompt.md"
"#;
        let config = WorkflowConfig::parse(toml).unwrap();
        assert_eq!(config.project.max_sessions, Some(3));
        assert_eq!(config.project.default_branch.as_deref(), Some("develop"));
        assert_eq!(config.dispatch.max_retries, 5);
        assert_eq!(config.labels.ignore, vec!["wontfix", "duplicate"]);
        assert_eq!(config.labels.blocked, vec!["blocked"]);
        assert_eq!(config.prompt.system_prompt.as_deref(), Some("system-prompt.md"));
    }

    #[test]
    fn partial_config_fills_defaults() {
        let toml = r#"
[dispatch]
max_retries = 10
"#;
        let config = WorkflowConfig::parse(toml).unwrap();
        assert_eq!(config.dispatch.max_retries, 10);
        assert_eq!(config.dispatch.retry_base_delay, 5); // default
        assert!(config.labels.ignore.is_empty()); // default
    }
}
```

**Step 3: Register module in lib.rs**

Add `pub mod workflow;` to `crates/server/src/lib.rs`.

**Step 4: Run tests**

Run: `cargo test -p server`
Expected: All existing tests + 3 new workflow tests pass.

**Step 5: Commit**

```
git add crates/server/Cargo.toml crates/server/src/workflow.rs crates/server/src/lib.rs
git commit -m "Add workflow configuration module (spec §14)"
```

---

### Task 3: Prompt construction module

**Files:**
- Create: `crates/server/src/prompt.rs`
- Modify: `crates/server/src/lib.rs`

**Step 1: Write prompt module with tests**

Create `crates/server/src/prompt.rs` implementing spec §15. The module builds prompts from task details. It takes a task, its GitHub issue/PR data, and optional system prompt content, and produces a Markdown string.

Key function:
```rust
pub fn build_prompt(params: &PromptParams) -> String
```

Where `PromptParams` contains:
- `system_prompt: Option<&str>` — project system prompt contents
- `title: &str`, `number: u64`, `body: Option<&str>` — issue/PR details
- `comments: &[CommentInfo]` — comment history (will be truncated to first 10 + last 10)
- `labels: &[String]`, `assignees: &[String]`
- `sub_issues: &[SubIssueInfo]`, `linked_prs: &[LinkedInfo]` or `linked_issues: &[LinkedInfo]`
- `branch: &str`
- `parent: Option<ParentInfo>` — parent task if sub-task
- `related_tasks: &[RelatedTaskInfo]` — other in-progress tasks
- `retry: Option<RetryContext>` — retry info if this is a retry

Tests:
- Basic prompt includes title, body, branch, instructions
- Comments truncated to first 10 + last 10 with gap message when >20
- Comments not truncated when <=20
- System prompt prepended when present
- Retry context prepended when present
- Sub-issues and linked items rendered
- Empty optional fields omitted cleanly

**Step 2: Register module in lib.rs**

Add `pub mod prompt;` to `crates/server/src/lib.rs`.

**Step 3: Run tests**

Run: `cargo test -p server`
Expected: All tests pass.

**Step 4: Commit**

```
git add crates/server/src/prompt.rs crates/server/src/lib.rs
git commit -m "Add prompt construction module (spec §15)"
```

---

### Task 4: Dispatcher — candidate selection and priority sorting

**Files:**
- Create: `crates/server/src/dispatcher.rs`
- Modify: `crates/server/src/lib.rs`

**Step 1: Write dispatcher module with tests**

Create `crates/server/src/dispatcher.rs` implementing spec §12. This is the pure logic layer — no async, no sessions, just candidate selection and sorting.

Key types and functions:
```rust
/// Configuration for the dispatcher.
pub struct DispatcherConfig {
    pub max_sessions: u32,          // global limit
    pub reconciliation_interval: Duration,
}

/// A candidate for dispatch, with computed sort key.
struct Candidate { ... }

/// Select and sort dispatch candidates from current task state.
/// Returns task IDs in priority order, split into resume and new work.
pub fn evaluate(
    tasks: &HashMap<String, Task>,
    project_limits: &HashMap<String, u32>,  // project_id -> max_sessions
    global_max: u32,
) -> DispatchPlan

pub struct DispatchPlan {
    /// Tasks in question state with pending answers — resume immediately.
    pub resume: Vec<String>,
    /// Tasks in waiting state — start new sessions, in priority order.
    pub new_work: Vec<String>,
}
```

The `evaluate` function:
1. Counts active slots (tasks in Running/Question/Testing) globally and per-project
2. Collects resume candidates (question tasks with pending messages)
3. Collects new work candidates (waiting tasks with elapsed backoff)
4. Sorts new work by: explicit priority → unblocking value → recency (newest first)
5. Filters to those that fit within concurrency limits

Tests:
- No candidates when mode is Stop (caller checks this, but evaluate should handle empty input)
- Resume candidates returned before new work
- Priority sorting: explicit priority wins
- Priority sorting: unblocking value breaks ties
- Priority sorting: recency (newest first) breaks remaining ties
- Global concurrency limit enforced
- Per-project concurrency limit enforced
- Backoff not elapsed → candidate excluded
- Tasks in terminal states not selected
- Tasks in running/question/testing not selected as new work

**Step 2: Register module in lib.rs**

Add `pub mod dispatcher;` to `crates/server/src/lib.rs`.

**Step 3: Run tests**

Run: `cargo test -p server`
Expected: All tests pass.

**Step 4: Commit**

```
git add crates/server/src/dispatcher.rs crates/server/src/lib.rs
git commit -m "Add dispatcher with candidate selection and priority sorting (spec §12)"
```

---

### Task 5: Scheduler — GitHub polling loop and task creation

**Files:**
- Modify: `crates/server/Cargo.toml` (add tasks-github dependency)
- Create: `crates/server/src/scheduler.rs`
- Modify: `crates/server/src/lib.rs`

**Step 1: Add github dependency**

In `crates/server/Cargo.toml`, add:
```toml
tasks-github = { path = "../github" }
```

**Step 2: Write scheduler module**

Create `crates/server/src/scheduler.rs`. The scheduler:

1. Holds a `RepoPoller` per project
2. On each tick: polls all projects, converts new issues/PRs to tasks
3. Skips issues matching ignore labels
4. Sets blocked state for issues matching blocked labels
5. Skips issues that already have corresponding tasks (dedup by source)
6. Emits `system:scheduler:tick` event
7. Triggers dispatch evaluation after each tick

Key type:
```rust
pub struct Scheduler {
    pollers: HashMap<String, RepoPoller>,  // project_id -> poller
    server: Arc<Server>,                    // not Server itself, but needs access to state + event_bus
}
```

Key methods:
```rust
/// Run a single poll + dispatch tick. Called by the main loop on timer or event.
pub async fn tick(&mut self) -> Result<(), SchedulerError>

/// Register a project for polling.
pub fn add_project(&mut self, project_id: String, poller: RepoPoller)
```

The `tick` method:
1. Check mode — if Stop, return early
2. For each poller, call `poll()`
3. For each returned issue/PR, check if task exists (by source match)
4. If new: create Task from issue/PR data, call `server.add_task()`
5. Apply label rules (ignore, blocked)
6. Emit `system:scheduler:tick`
7. Run `dispatcher::evaluate()` and process the dispatch plan

Tests (unit, no network):
- Tick in Stop mode does nothing
- New issue creates a task
- Existing issue (matching source) is skipped
- Issue with ignore label is skipped
- Issue with blocked label creates task in blocked state
- PR creates a task with GithubPr source
- Multiple projects polled in one tick

**Step 3: Register module in lib.rs**

Add `pub mod scheduler;` to `crates/server/src/lib.rs`.

**Step 4: Run tests**

Run: `cargo test -p server`
Expected: All tests pass.

**Step 5: Commit**

```
git add crates/server/Cargo.toml crates/server/src/scheduler.rs crates/server/src/lib.rs
git commit -m "Add scheduler with GitHub polling and task creation (spec §3.2, §12)"
```

---

### Task 6: Integrate dispatcher with server — the run loop

**Files:**
- Modify: `crates/server/src/server.rs`

**Step 1: Add dispatch integration to Server**

Add methods to Server:

```rust
/// Run a dispatch evaluation and process the results.
/// Called by the scheduler after a tick, or by event handlers.
pub async fn run_dispatch(&self) -> Result<(), ServerError>

/// Check if a task source already exists (dedup for scheduler).
pub async fn has_task_for_source(&self, source: &TaskSource) -> bool

/// Get the dispatch config for a project (merging workflow config with global defaults).
pub async fn project_session_limit(&self, project_id: &str) -> u32
```

The `run_dispatch` method:
1. Read current mode — if Stop, return
2. Call `dispatcher::evaluate()` with current tasks and limits
3. For resume candidates: log/emit (actual session message delivery is future work)
4. For new work candidates: transition to Running, log/emit (actual session creation is future work)

For now, the dispatcher selects candidates and transitions their state. Actual session creation (container + agent) will be wired up when we integrate the full pipeline. This keeps the unit testable without container dependencies.

**Step 2: Add server-level tests**

- Dispatch evaluation respects mode
- Dispatch transitions waiting tasks to running
- Dispatch respects concurrency limits

**Step 3: Run tests**

Run: `cargo test -p server`
Expected: All tests pass.

**Step 4: Commit**

```
git add crates/server/src/server.rs
git commit -m "Integrate dispatcher into server with run_dispatch method"
```

---

### Task 7: Full workspace build verification

**Step 1: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All tests pass across all crates.

**Step 2: Run clippy**

Run: `cargo clippy --workspace`
Expected: No warnings.

**Step 3: Commit any fixes if needed**

---

### Task 8: Update server doc comment and clean up TODOs

**Files:**
- Modify: `crates/server/src/server.rs`

**Step 1: Update the doc comment on Server struct**

Remove the "Not yet implemented" list items for scheduler and dispatch since they're now implemented. Keep the remaining TODOs (web GUI, websockets, orchestrator).

**Step 2: Commit**

```
git add crates/server/src/server.rs
git commit -m "Update server doc comments to reflect implemented features"
```
