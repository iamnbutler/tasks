# Orchestrator Think Loop

**Issues:** #539, #536, #530

## Design

The orchestrator is an actor that periodically surveys system state, identifies
patterns, and decides what to do. The event bus is a data source it can consult,
not its driver.

### Core concept: `think()`

```rust
/// Periodic orchestrator reasoning pass.
/// Called every ~30s by a dedicated task in the run loop.
async fn think(
    &self,
    context: &SystemContext,
) -> Result<Vec<OrchestratorAction>, OrchestratorError>;
```

The orchestrator receives a full snapshot of system state and returns actions.
It is NOT reactive to individual events — it stands outside the event stream
and sees patterns across events, tasks, and merge queue state.

### SystemContext (what the orchestrator sees)

```rust
pub struct SystemContext {
    pub mode: OperatingMode,
    pub projects: Vec<Project>,
    pub tasks: Vec<Task>,
    pub merge_queue: Vec<MergeQueueEntry>,
    pub active_sessions: Vec<SessionInfo>,
    pub human_present: bool,
    pub recent_events: Vec<Event>,    // last N events for pattern detection
    pub last_think_at: Option<DateTime<Utc>>,  // when we last ran think()
}
```

### OrchestratorAction (what the orchestrator can do)

```rust
pub enum OrchestratorAction {
    /// Stream-of-consciousness narration → emits orchestrator:thought event
    EmitThought(String),
    /// Change a task's state
    UpdateTaskState { task_id: String, state: TaskState },
    /// Request priority dispatch for a task
    PrioritizeTask { task_id: String, reason: String },
    // Future:
    // DispatchAgent { prompt: String, context: AgentContext },
    // CreateIssue { repo: String, title: String, body: String },
    // CloseIssue { repo: String, number: u64, reason: String },
    // SendChat { task_id: String, message: String },
}
```

### Implementation phases

**Phase 1 (this PR):**
- Trait expansion with `think()` method (default impl returns empty vec)
- `SystemContext` and `OrchestratorAction` types
- Rule-based `think()` in ClaudeOrchestrator:
  - Narrate tasks that changed state since last think
  - Flag tasks running longer than expected
  - Summarize merge queue state changes
  - Spot patterns: repeated failures, stuck tasks
- Wire into run_loop as a periodic tick (30s interval)
- Process returned actions (EmitThought → publish orchestrator:thought event)

**Phase 2 (future):**
- LLM-powered think() for complex pattern detection
- DispatchAgent action type
- CreateIssue / CloseIssue actions
- Priority management

### Run loop wiring

```rust
// In run_loop.rs, new spawned task:
let orchestrator_think = tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    let mut last_think_at = None;
    loop {
        interval.tick().await;
        if *mode.read().await != Mode::Stop {
            let context = build_system_context(&server, last_think_at).await;
            match orchestrator.think(&context).await {
                Ok(actions) => {
                    for action in actions {
                        process_orchestrator_action(&server, &event_bus, action).await;
                    }
                }
                Err(e) => tracing::warn!("orchestrator think failed: {e}"),
            }
            last_think_at = Some(Utc::now());
        }
    }
});
```

### What the orchestrator narrates (rule-based, no LLM)

- "Task X completed — PR ready for review"
- "Task Y failed after 3 retries — may need manual investigation"
- "3 tasks in auth code all failed with timeout errors — possible systemic issue"
- "Merge queue clear — all approved PRs merged"
- "Session for task Z has been running for 45 minutes (soft limit: 25m)"
- "PR #123 approved and merged"
- "PR #456 rejected — feedback sent to agent"

### Key design decisions

1. **No LLM for narration** — rule-based pattern matching is fast and predictable.
   LLM reasoning comes in phase 2 for complex pattern detection.
2. **Periodic, not reactive** — the orchestrator surveys state on a tick, not per-event.
   This lets it see patterns across events.
3. **Actions are requests** — the run loop decides how to fulfill them. The orchestrator
   doesn't directly mutate state.
4. **Default impl is no-op** — existing code doesn't break.
