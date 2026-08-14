//! SQLite-backed persistence.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use thiserror::Error;
use tokio::sync::broadcast;

use crate::events::{Event, EventPayload};
use crate::github::GhIssue;
use crate::models::{
    Briefing, BriefingSection, Build, BuildId, BuildStatus, ChatRole, Complexity, GhState, Mode,
    OrchestratorFeedEvent, OrchestratorMessage, OrchestratorSession, OrchestratorSessionInfo,
    Project, ProjectId, ReviewedSpec, Session, SessionEndReason, SessionId, SessionStatus,
    SessionUsage, Spec, SpecId, SpecQueueEntry, SpecQueueItem, SpecQueueStatus, Task, TaskId,
    TaskState, TranscriptLine, TranscriptStream,
};

/// Result of upserting an external record into our domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpsertOutcome<T> {
    Inserted(T),
    Existing(T),
}

impl<T> UpsertOutcome<T> {
    pub fn into_inner(self) -> T {
        match self {
            UpsertOutcome::Inserted(t) | UpsertOutcome::Existing(t) => t,
        }
    }

    pub fn is_new(&self) -> bool {
        matches!(self, UpsertOutcome::Inserted(_))
    }
}

/// Capacity of the in-memory event broadcast channel. Slow subscribers that
/// fall this far behind receive `RecvError::Lagged` and must catch up via
/// `events_since`.
const EVENT_BROADCAST_CAPACITY: usize = 1024;

/// Capacity of the transcript broadcast channel. Larger than the event one:
/// agent output is far higher-rate, and a session-detail view that lags briefly
/// should resync rather than lose its place.
const TRANSCRIPT_BROADCAST_CAPACITY: usize = 4096;

/// Capacity of the orchestrator live-feed channel. Sized like the transcript
/// one (token deltas are high-rate); a lagged subscriber just misses deltas —
/// the durable message arrives via `orchestrator_messages` regardless.
const ORCHESTRATOR_FEED_CAPACITY: usize = 4096;

/// `exit_reason` written to sessions that were still `running` when the server
/// came back up.
const ORPHANED_EXIT_REASON: &str = "orphaned by server restart";

/// How long an interactive checkout of the orchestrator session stays fresh
/// without a heartbeat renewal. The wrapper script renews every minute, so
/// this only expires when the interactive client died without releasing —
/// a killed terminal un-wedges the tick loop by itself within this window.
pub const ORCHESTRATOR_CHECKOUT_TTL: Duration = Duration::from_secs(5 * 60);

/// Whether a checkout heartbeat timestamp is still within
/// [`ORCHESTRATOR_CHECKOUT_TTL`]. An unparseable timestamp counts as stale —
/// failing open (ticks resume) beats wedging the loop on bad data.
fn checkout_heartbeat_fresh(ts: &str) -> bool {
    DateTime::parse_from_rfc3339(ts)
        .map(|t| {
            Utc::now().signed_duration_since(t.with_timezone(&Utc))
                < chrono::Duration::from_std(ORCHESTRATOR_CHECKOUT_TTL).expect("ttl fits")
        })
        .unwrap_or(false)
}

/// What [`Store::reconcile_orphaned_work`] cleaned up: rows that a previous
/// process left mid-flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    /// `running` sessions failed as orphaned.
    pub sessions: usize,
    /// `scouting` tasks put back in the queue as `new`.
    pub tasks: usize,
    /// `running` builds failed as orphaned (queued builds are durable intent
    /// and survive a restart untouched). A wedged running build would block
    /// the serial queue forever, strictly worse than an orphaned session.
    pub builds: usize,
}

impl ReconcileReport {
    /// Whether anything at all was reconciled — the only case worth logging.
    pub fn is_empty(&self) -> bool {
        self.sessions == 0 && self.tasks == 0 && self.builds == 0
    }
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migrate: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid enum value in column {column}: {value}")]
    BadEnum { column: &'static str, value: String },
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid request: {0}")]
    Invalid(String),
}

pub struct Store {
    pool: SqlitePool,
    event_tx: broadcast::Sender<Event>,
    transcript_tx: broadcast::Sender<TranscriptLine>,
    orchestrator_feed_tx: broadcast::Sender<OrchestratorFeedEvent>,
}

impl Store {
    /// Open (creating if necessary) a SQLite database at the given path and run migrations.
    ///
    /// WAL + a busy timeout are load-bearing: the pool holds 8 connections
    /// shared by the poller, the dispatch/build/orchestrator loops, transcript
    /// batch writes from live scouts, and the API's parallel reads. With the
    /// defaults (rollback journal, busy_timeout 0) any overlap fails instantly
    /// with `SQLITE_BUSY` ("database is locked") instead of waiting its turn —
    /// seen live the first time a scout streamed transcripts while the app
    /// refreshed.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};

        let options = SqliteConnectOptions::new()
            .filename(path.as_ref())
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            // NORMAL is durable enough under WAL (a power cut can lose the
            // tail of the log, never corrupt) and avoids an fsync per commit.
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
        let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let (transcript_tx, _) = broadcast::channel(TRANSCRIPT_BROADCAST_CAPACITY);
        let (orchestrator_feed_tx, _) = broadcast::channel(ORCHESTRATOR_FEED_CAPACITY);
        Ok(Self {
            pool,
            event_tx,
            transcript_tx,
            orchestrator_feed_tx,
        })
    }

    /// Open an in-memory database (useful for tests).
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1) // shared in-memory DB — one connection
            .connect("sqlite::memory:")
            .await?;
        MIGRATOR.run(&pool).await?;
        let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let (transcript_tx, _) = broadcast::channel(TRANSCRIPT_BROADCAST_CAPACITY);
        let (orchestrator_feed_tx, _) = broadcast::channel(ORCHESTRATOR_FEED_CAPACITY);
        Ok(Self {
            pool,
            event_tx,
            transcript_tx,
            orchestrator_feed_tx,
        })
    }

    // --- projects ---

    pub async fn insert_project(&self, project: &Project) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO projects (id, repo_owner, repo_name, added_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(project.id.as_str())
        .bind(&project.repo_owner)
        .bind(&project.repo_name)
        .bind(project.added_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_project(&self, id: &ProjectId) -> Result<Option<Project>, StoreError> {
        let row =
            sqlx::query("SELECT id, repo_owner, repo_name, added_at FROM projects WHERE id = ?")
                .bind(id.as_str())
                .fetch_optional(&self.pool)
                .await?;
        row.map(project_from_row).transpose()
    }

    pub async fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, repo_owner, repo_name, added_at FROM projects ORDER BY added_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(project_from_row).collect()
    }

    // --- tasks ---

    pub async fn insert_task(&self, task: &Task) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO tasks (id, project_id, gh_issue_number, title, body, labels, \
             gh_state, state, priority, manual_rank, dispatch_attempts, ingested_at, \
             updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(task.id.as_str())
        .bind(task.project_id.as_str())
        .bind(task.gh_issue_number as i64)
        .bind(&task.title)
        .bind(&task.body)
        .bind(serde_json::to_string(&task.labels)?)
        .bind(task.gh_state.as_str())
        .bind(task.state.as_str())
        .bind(task.priority)
        .bind(task.manual_rank)
        .bind(task.dispatch_attempts as i64)
        .bind(task.ingested_at.to_rfc3339())
        .bind(task.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_task(&self, id: &TaskId) -> Result<Option<Task>, StoreError> {
        let row = sqlx::query(
            "SELECT id, project_id, gh_issue_number, title, body, labels, gh_state, \
             state, priority, manual_rank, dispatch_attempts, ingested_at, updated_at \
             FROM tasks WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(task_from_row).transpose()
    }

    /// List tasks in queue order: manual rank first (nulls last), then derived
    /// priority descending, then oldest first.
    pub async fn list_tasks(&self) -> Result<Vec<Task>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, project_id, gh_issue_number, title, body, labels, gh_state, \
             state, priority, manual_rank, dispatch_attempts, ingested_at, updated_at \
             FROM tasks ORDER BY manual_rank IS NULL, manual_rank, priority DESC, ingested_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(task_from_row).collect()
    }

    /// [`Self::list_tasks`] minus rows whose story is over: tasks whose issue
    /// is closed on GitHub and that are either untouched intake noise
    /// (`backlog`) or work already concluded (`done` / `rejected` — closure
    /// -derived retirement's output).
    ///
    /// Everything in between stays visible whatever GitHub thinks of the
    /// issue — in-flight work must not vanish from a client's list because
    /// someone closed the issue behind it; the poller will retire it properly.
    /// A terminal task whose issue is still *open* also stays visible: it is
    /// the "close the issue or re-queue?" decision surface. Full history is
    /// [`Self::list_tasks`] (`GET /tasks?all=true`). Ordering is identical.
    pub async fn list_active_tasks(&self) -> Result<Vec<Task>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, project_id, gh_issue_number, title, body, labels, gh_state, \
             state, priority, manual_rank, dispatch_attempts, ingested_at, updated_at \
             FROM tasks WHERE NOT (gh_state = ? AND state IN (?, ?, ?)) \
             ORDER BY manual_rank IS NULL, manual_rank, priority DESC, ingested_at",
        )
        .bind(GhState::Closed.as_str())
        .bind(TaskState::Backlog.as_str())
        .bind(TaskState::Done.as_str())
        .bind(TaskState::Rejected.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(task_from_row).collect()
    }

    /// Replace the manual queue order with `ids`, assigning ranks 1..N in the
    /// given order. Every task not listed is left unranked, so a reorder is
    /// always a complete statement of the curated queue.
    ///
    /// Appends [`EventPayload::QueueReordered`] on success.
    pub async fn set_queue_order(&self, ids: &[TaskId]) -> Result<(), StoreError> {
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            if !seen.insert(id.as_str()) {
                return Err(StoreError::Invalid(format!("duplicate task id: {id}")));
            }
        }

        let mut tx = self.pool.begin().await?;
        let mut missing = Vec::new();
        for id in ids {
            let found = sqlx::query("SELECT 1 FROM tasks WHERE id = ?")
                .bind(id.as_str())
                .fetch_optional(&mut *tx)
                .await?;
            if found.is_none() {
                missing.push(id.to_string());
            }
        }
        if !missing.is_empty() {
            return Err(StoreError::NotFound(format!(
                "tasks: {}",
                missing.join(", ")
            )));
        }

        sqlx::query("UPDATE tasks SET manual_rank = NULL WHERE manual_rank IS NOT NULL")
            .execute(&mut *tx)
            .await?;
        for (idx, id) in ids.iter().enumerate() {
            sqlx::query("UPDATE tasks SET manual_rank = ? WHERE id = ?")
                .bind(idx as i64 + 1)
                .bind(id.as_str())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;

        self.append_event(EventPayload::QueueReordered {
            task_ids: ids.to_vec(),
        })
        .await?;
        Ok(())
    }

    /// Pick a backlog task up into the scout queue, appending it at the end of
    /// the ranked order. The only door from `Backlog` into the pipeline.
    ///
    /// Emits the `TaskStateChanged` event itself so every caller gets the same
    /// audit trail. Returns the updated task.
    pub async fn queue_task(&self, id: &TaskId) -> Result<Task, StoreError> {
        let mut tx = self.pool.begin().await?;
        let task = self
            .require_task_state(&mut tx, id, TaskState::Backlog)
            .await?;
        let next_rank: i64 =
            sqlx::query("SELECT COALESCE(MAX(manual_rank), 0) + 1 AS r FROM tasks")
                .fetch_one(&mut *tx)
                .await?
                .try_get("r")?;
        sqlx::query("UPDATE tasks SET state = ?, manual_rank = ?, updated_at = ? WHERE id = ?")
            .bind(TaskState::Queued.as_str())
            .bind(next_rank)
            .bind(Utc::now().to_rfc3339())
            .bind(id.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        self.append_event(EventPayload::TaskStateChanged {
            task_id: id.clone(),
            from: task.state,
            to: TaskState::Queued,
        })
        .await?;
        self.get_task(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("task {id}")))
    }

    /// Put a queued (not yet running) task back in the backlog, clearing its
    /// rank. Work past `Queued` can't be un-picked — cancel or review it
    /// through the pipeline instead.
    pub async fn dequeue_task(&self, id: &TaskId) -> Result<Task, StoreError> {
        let mut tx = self.pool.begin().await?;
        let task = self
            .require_task_state(&mut tx, id, TaskState::Queued)
            .await?;
        sqlx::query("UPDATE tasks SET state = ?, manual_rank = NULL, updated_at = ? WHERE id = ?")
            .bind(TaskState::Backlog.as_str())
            .bind(Utc::now().to_rfc3339())
            .bind(id.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        self.append_event(EventPayload::TaskStateChanged {
            task_id: id.clone(),
            from: task.state,
            to: TaskState::Backlog,
        })
        .await?;
        self.get_task(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("task {id}")))
    }

    /// "Scout now": queue the task (from `Backlog` or already `Queued`) at the
    /// FRONT of the ranked order, shifting everything else down one. The
    /// dispatch loop picks it up on its next tick; the concurrency cap still
    /// applies — this jumps the queue, it does not bypass it.
    pub async fn push_task_to_front(&self, id: &TaskId) -> Result<Task, StoreError> {
        let mut tx = self.pool.begin().await?;
        let task = self
            .get_task_in_tx(&mut tx, id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("task {id}")))?;
        if !matches!(task.state, TaskState::Backlog | TaskState::Queued) {
            return Err(StoreError::Invalid(format!(
                "task {id} is {}, only backlog or queued tasks can be scouted now",
                task.state.as_str()
            )));
        }
        sqlx::query("UPDATE tasks SET manual_rank = manual_rank + 1 WHERE manual_rank IS NOT NULL")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE tasks SET state = ?, manual_rank = 1, updated_at = ? WHERE id = ?")
            .bind(TaskState::Queued.as_str())
            .bind(Utc::now().to_rfc3339())
            .bind(id.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        if task.state == TaskState::Backlog {
            self.append_event(EventPayload::TaskStateChanged {
                task_id: id.clone(),
                from: TaskState::Backlog,
                to: TaskState::Queued,
            })
            .await?;
        }
        self.get_task(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("task {id}")))
    }

    /// Fetch a task inside a transaction and require an exact state, mapping
    /// the mismatch to [`StoreError::Invalid`] (a 400 at the API).
    async fn require_task_state(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        id: &TaskId,
        expected: TaskState,
    ) -> Result<Task, StoreError> {
        let task = self
            .get_task_in_tx(tx, id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("task {id}")))?;
        if task.state != expected {
            return Err(StoreError::Invalid(format!(
                "task {id} is {}, expected {}",
                task.state.as_str(),
                expected.as_str()
            )));
        }
        Ok(task)
    }

    async fn get_task_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        id: &TaskId,
    ) -> Result<Option<Task>, StoreError> {
        let row = sqlx::query(
            "SELECT id, project_id, gh_issue_number, title, body, labels, gh_state, \
             state, priority, manual_rank, dispatch_attempts, ingested_at, updated_at \
             FROM tasks WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&mut **tx)
        .await?;
        row.map(task_from_row).transpose()
    }

    /// Upsert a task from a GitHub issue.
    ///
    /// On first sighting: insert with `state = new`, priority 0, `ingested_at = now`.
    /// On re-sighting: update title/body/labels/gh_state but keep our internal
    /// id, state, priority, manual_rank, ingested_at untouched. GitHub is never
    /// allowed to disturb the human-curated ordering.
    pub async fn upsert_gh_issue(
        &self,
        project_id: &ProjectId,
        issue: GhIssue,
    ) -> Result<UpsertOutcome<Task>, StoreError> {
        let existing = sqlx::query(
            "SELECT id, project_id, gh_issue_number, title, body, labels, gh_state, \
             state, priority, manual_rank, dispatch_attempts, ingested_at, updated_at \
             FROM tasks WHERE project_id = ? AND gh_issue_number = ?",
        )
        .bind(project_id.as_str())
        .bind(issue.number as i64)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = existing {
            let task = task_from_row(row)?;
            let labels_json = serde_json::to_string(&issue.labels)?;
            let now = Utc::now();
            sqlx::query(
                "UPDATE tasks SET title = ?, body = ?, labels = ?, gh_state = ?, \
                 updated_at = ? WHERE id = ?",
            )
            .bind(&issue.title)
            .bind(&issue.body)
            .bind(labels_json)
            .bind(issue.state.as_str())
            .bind(now.to_rfc3339())
            .bind(task.id.as_str())
            .execute(&self.pool)
            .await?;

            let updated = Task {
                title: issue.title,
                body: issue.body,
                labels: issue.labels,
                gh_state: issue.state,
                updated_at: now,
                ..task
            };
            return Ok(UpsertOutcome::Existing(updated));
        }

        let now = Utc::now();
        let task = Task {
            id: TaskId::new(),
            project_id: project_id.clone(),
            gh_issue_number: issue.number,
            title: issue.title,
            body: issue.body,
            labels: issue.labels,
            gh_state: issue.state,
            state: TaskState::Backlog,
            priority: 0,
            manual_rank: None,
            dispatch_attempts: 0,
            ingested_at: now,
            updated_at: now,
        };
        self.insert_task(&task).await?;
        Ok(UpsertOutcome::Inserted(task))
    }

    /// Mark every task of `project_id` whose issue has vanished from the
    /// repository's open set as closed. Returns the ids it changed.
    ///
    /// GitHub's open-issue query is the only intake we have, and a closed issue
    /// simply stops appearing in it — absence is the close notification. So the
    /// caller must pass a *complete* open set: `open_issue_numbers` is every
    /// open issue number a successful fetch returned. A partial or failed fetch
    /// would read as "everything was closed", which is why
    /// [`crate::run::poll_once`] skips reconciliation for a project it could not
    /// fetch.
    ///
    /// Only `gh_state` (and `updated_at`) is written. `state`, `manual_rank` and
    /// `dispatch_attempts` are Tasks-owned — a task already scouting or queued
    /// keeps flowing; `gh_state` gates *new* dispatch only. Reopening needs no
    /// counterpart: the issue reappears in the open set and
    /// [`Self::upsert_gh_issue`] refreshes `gh_state` from the snapshot.
    ///
    /// The row updates are one transaction; the matching
    /// [`EventPayload::TaskGhStateChanged`] events are the caller's to append,
    /// mirroring how the poller emits [`EventPayload::TaskIngested`].
    pub async fn reconcile_closed_issues(
        &self,
        project_id: &ProjectId,
        open_issue_numbers: &[u64],
    ) -> Result<Vec<TaskId>, StoreError> {
        let open: std::collections::HashSet<u64> = open_issue_numbers.iter().copied().collect();

        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(
            "SELECT id, gh_issue_number FROM tasks WHERE project_id = ? AND gh_state = ?",
        )
        .bind(project_id.as_str())
        .bind(GhState::Open.as_str())
        .fetch_all(&mut *tx)
        .await?;

        let mut closed = Vec::new();
        for row in rows {
            let number = row.try_get::<i64, _>("gh_issue_number")?.max(0) as u64;
            if open.contains(&number) {
                continue;
            }
            closed.push(TaskId::from_raw(row.try_get::<String, _>("id")?));
        }

        let now = Utc::now().to_rfc3339();
        for id in &closed {
            sqlx::query("UPDATE tasks SET gh_state = ?, updated_at = ? WHERE id = ?")
                .bind(GhState::Closed.as_str())
                .bind(&now)
                .bind(id.as_str())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;

        Ok(closed)
    }

    /// Tasks whose GitHub issue is closed but whose Tasks-owned state still
    /// says the work is picked up: `queued`, `in_review`, or `ready_to_build`.
    ///
    /// These are the closure-derived retirement candidates — issue closure IS
    /// the "done" signal, there is no manual mark-done. `scouting` is
    /// deliberately excluded: a scout in flight runs to completion, lands the
    /// task in `in_review`, and the next poll retires it from there. The list
    /// is not limited to issues closed *this* poll, so rows that predate this
    /// mechanism (or that a failed reason-lookup skipped) self-heal on any
    /// later pass.
    pub async fn list_retirable_tasks(
        &self,
        project_id: &ProjectId,
    ) -> Result<Vec<Task>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, project_id, gh_issue_number, title, body, labels, gh_state, \
             state, priority, manual_rank, dispatch_attempts, ingested_at, updated_at \
             FROM tasks WHERE project_id = ? AND gh_state = ? AND state IN (?, ?, ?) \
             ORDER BY ingested_at",
        )
        .bind(project_id.as_str())
        .bind(GhState::Closed.as_str())
        .bind(TaskState::Queued.as_str())
        .bind(TaskState::InReview.as_str())
        .bind(TaskState::ReadyToBuild.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(task_from_row).collect()
    }

    /// Retire a picked-up task because its GitHub issue was closed. `to` is
    /// `Done` (closed as completed) or `Rejected` (closed as not-planned /
    /// duplicate) — nothing else is a retirement.
    ///
    /// Re-checks inside the transaction that the task is still a retirement
    /// candidate ([`Self::list_retirable_tasks`]'s criteria); if the state
    /// moved or the issue reopened in the meantime, returns `Ok(None)` and
    /// writes nothing. Clears `manual_rank` so retired work frees its queue
    /// slot, and emits [`EventPayload::TaskStateChanged`].
    pub async fn retire_task(
        &self,
        id: &TaskId,
        to: TaskState,
    ) -> Result<Option<Task>, StoreError> {
        if !matches!(to, TaskState::Done | TaskState::Rejected) {
            return Err(StoreError::Invalid(format!(
                "{} is not a retirement state",
                to.as_str()
            )));
        }

        let mut tx = self.pool.begin().await?;
        let task = self
            .get_task_in_tx(&mut tx, id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("task {id}")))?;
        let retirable = task.gh_state == GhState::Closed
            && matches!(
                task.state,
                TaskState::Queued | TaskState::InReview | TaskState::ReadyToBuild
            );
        if !retirable {
            return Ok(None);
        }
        sqlx::query("UPDATE tasks SET state = ?, manual_rank = NULL, updated_at = ? WHERE id = ?")
            .bind(to.as_str())
            .bind(Utc::now().to_rfc3339())
            .bind(id.as_str())
            .execute(&mut *tx)
            .await?;

        // Retirement drains the task's unconsumed specs too: a closed issue's
        // approved-but-unbuilt spec must not linger in the queue where
        // `create_build` would happily build work nobody wants anymore.
        // Terminal entries (`built`, `rejected`) are history and stay put.
        let spec_rows = sqlx::query(
            "SELECT q.spec_id, q.status FROM spec_queue q \
             JOIN specs s ON s.id = q.spec_id \
             WHERE s.task_id = ? AND q.status IN (?, ?, ?, ?)",
        )
        .bind(id.as_str())
        .bind(SpecQueueStatus::PendingReview.as_str())
        .bind(SpecQueueStatus::Approved.as_str())
        .bind(SpecQueueStatus::NeedsRevision.as_str())
        .bind(SpecQueueStatus::Blocked.as_str())
        .fetch_all(&mut *tx)
        .await?;
        let mut drained = Vec::new();
        for row in spec_rows {
            let spec_id = SpecId::from_raw(row.try_get::<String, _>("spec_id")?);
            let from_raw: String = row.try_get("status")?;
            sqlx::query("UPDATE spec_queue SET status = ?, rank = NULL WHERE spec_id = ?")
                .bind(SpecQueueStatus::Rejected.as_str())
                .bind(spec_id.as_str())
                .execute(&mut *tx)
                .await?;
            drained.push((spec_id, SpecQueueStatus::from_str(&from_raw)));
        }
        tx.commit().await?;

        self.append_event(EventPayload::TaskStateChanged {
            task_id: id.clone(),
            from: task.state,
            to,
        })
        .await?;
        for (spec_id, from) in drained {
            self.append_event(EventPayload::SpecQueueStatusChanged {
                spec_id,
                from,
                to: SpecQueueStatus::Rejected,
            })
            .await?;
        }
        self.get_task(id).await
    }

    pub async fn update_task_state(&self, id: &TaskId, state: TaskState) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query("UPDATE tasks SET state = ?, updated_at = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(now)
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("task {id}")));
        }
        Ok(())
    }

    /// Count one more failed dispatch against a task and return the new total.
    ///
    /// The counter lives in the database precisely so a restart can't forgive a
    /// task its strikes — an in-memory tally would let a task that can never be
    /// scouted be retried forever, one restart at a time.
    pub async fn record_dispatch_failure(&self, id: &TaskId) -> Result<u32, StoreError> {
        let row = sqlx::query(
            "UPDATE tasks SET dispatch_attempts = dispatch_attempts + 1, updated_at = ? \
             WHERE id = ? RETURNING dispatch_attempts",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let row = row.ok_or_else(|| StoreError::NotFound(format!("task {id}")))?;
        Ok(row.try_get::<i64, _>("dispatch_attempts")?.max(0) as u32)
    }

    /// Clear a task's dispatch failures. Called when a scout produces a spec:
    /// the task has proven dispatchable, so a later re-scout (`needs_revision`)
    /// starts from a clean slate.
    pub async fn reset_dispatch_attempts(&self, id: &TaskId) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE tasks SET dispatch_attempts = 0, updated_at = ? \
             WHERE id = ? AND dispatch_attempts != 0",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(id.as_str())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            // Either the task is already at zero or it is gone; only the latter
            // is an error.
            let exists = sqlx::query("SELECT 1 FROM tasks WHERE id = ?")
                .bind(id.as_str())
                .fetch_optional(&self.pool)
                .await?;
            if exists.is_none() {
                return Err(StoreError::NotFound(format!("task {id}")));
            }
        }
        Ok(())
    }

    // --- sessions ---

    pub async fn insert_session(&self, session: &Session) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO sessions (id, task_id, vm_id, branch, status, started_at, \
             completed_at, exit_reason) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(session.id.as_str())
        .bind(session.task_id.as_str())
        .bind(&session.vm_id)
        .bind(&session.branch)
        .bind(session.status.as_str())
        .bind(session.started_at.to_rfc3339())
        .bind(session.completed_at.map(|t| t.to_rfc3339()))
        .bind(&session.exit_reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update_session_branch(
        &self,
        id: &SessionId,
        branch: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx::query("UPDATE sessions SET branch = ? WHERE id = ?")
            .bind(branch)
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("session {id}")));
        }
        Ok(())
    }

    pub async fn update_session_completion(
        &self,
        id: &SessionId,
        status: SessionStatus,
        completed_at: DateTime<Utc>,
        exit_reason: Option<String>,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE sessions SET status = ?, completed_at = ?, exit_reason = ? WHERE id = ?",
        )
        .bind(status.as_str())
        .bind(completed_at.to_rfc3339())
        .bind(exit_reason)
        .bind(id.as_str())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("session {id}")));
        }
        Ok(())
    }

    pub async fn get_session(&self, id: &SessionId) -> Result<Option<Session>, StoreError> {
        let row = sqlx::query(
            "SELECT id, task_id, vm_id, branch, status, started_at, completed_at, \
             exit_reason, agent_usage FROM sessions WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(session_from_row).transpose()
    }

    pub async fn list_sessions(&self) -> Result<Vec<Session>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, task_id, vm_id, branch, status, started_at, completed_at, \
             exit_reason, agent_usage FROM sessions ORDER BY started_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(session_from_row).collect()
    }

    pub async fn update_session_usage(
        &self,
        id: &SessionId,
        usage: &SessionUsage,
    ) -> Result<(), StoreError> {
        let result = sqlx::query("UPDATE sessions SET agent_usage = ? WHERE id = ?")
            .bind(serde_json::to_string(usage)?)
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("session {id}")));
        }
        Ok(())
    }

    // --- transcripts ---

    /// Append agent output lines for a session, assigning dense `seq` values
    /// from `MAX(seq)+1`. One transaction for the batch; subscribers are
    /// notified only after it commits, so a live tail can never announce a line
    /// a catch-up read would fail to return.
    ///
    /// Returns the persisted lines with their seq values filled in.
    pub async fn append_transcript_lines(
        &self,
        session_id: &SessionId,
        lines: &[(TranscriptStream, String)],
    ) -> Result<Vec<TranscriptLine>, StoreError> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;

        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM transcript_lines WHERE session_id = ?",
        )
        .bind(session_id.as_str())
        .fetch_one(&mut *tx)
        .await?;

        let mut persisted = Vec::with_capacity(lines.len());
        for (offset, (stream, line)) in lines.iter().enumerate() {
            let seq = next + offset as i64;
            sqlx::query(
                "INSERT INTO transcript_lines (session_id, seq, timestamp, stream, line) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(session_id.as_str())
            .bind(seq)
            .bind(now.to_rfc3339())
            .bind(stream.as_str())
            .bind(line)
            .execute(&mut *tx)
            .await?;
            persisted.push(TranscriptLine {
                session_id: session_id.clone(),
                seq,
                timestamp: now,
                stream: *stream,
                line: line.clone(),
            });
        }
        tx.commit().await?;

        for line in &persisted {
            let _ = self.transcript_tx.send(line.clone());
        }
        Ok(persisted)
    }

    /// Transcript lines for a session with `seq >= since`, oldest first.
    /// `since` is inclusive, matching `/events?since=`; a tailing client passes
    /// `last_seq + 1`.
    pub async fn transcript_since(
        &self,
        session_id: &SessionId,
        since: i64,
        limit: i64,
    ) -> Result<Vec<TranscriptLine>, StoreError> {
        let rows = sqlx::query(
            "SELECT session_id, seq, timestamp, stream, line FROM transcript_lines \
             WHERE session_id = ? AND seq >= ? ORDER BY seq LIMIT ?",
        )
        .bind(session_id.as_str())
        .bind(since)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(transcript_line_from_row).collect()
    }

    /// Live transcript lines for *every* session; subscribers filter by id.
    /// One channel rather than per-session ones because `SCOUT_MAX_CONCURRENT`
    /// is small and per-session channel lifetimes aren't worth managing.
    pub fn subscribe_transcript(&self) -> broadcast::Receiver<TranscriptLine> {
        self.transcript_tx.subscribe()
    }

    // --- reconciliation ---

    /// Fail every `running` session and requeue every `scouting` task.
    ///
    /// Meant to be called once at startup, before any loop runs. One process
    /// owns all dispatch, so at that moment a `running` session cannot be live:
    /// it belongs to a process that died mid-scout. Left alone those rows are
    /// phantom state — the session never completes, the task never leaves
    /// `Scouting`, and the dispatch loop counts the ghost against its capacity
    /// forever.
    ///
    /// Scouting tasks are requeued whether or not they have a session row, on
    /// the theory that a task stuck in a transient state is always worth more
    /// requeued than stranded. Attempt counts are deliberately untouched: a
    /// crashed server is not the task's fault.
    ///
    /// The row updates are one transaction; the matching
    /// [`EventPayload::SessionCompleted`] / [`EventPayload::TaskStateChanged`]
    /// events are appended after it commits, exactly as the live failure path
    /// writes them.
    pub async fn reconcile_orphaned_work(&self) -> Result<ReconcileReport, StoreError> {
        let now = Utc::now();

        let mut tx = self.pool.begin().await?;
        let session_rows = sqlx::query("SELECT id, task_id FROM sessions WHERE status = ?")
            .bind(SessionStatus::Running.as_str())
            .fetch_all(&mut *tx)
            .await?;
        let orphaned_sessions = session_rows
            .into_iter()
            .map(|row| {
                Ok::<_, StoreError>((
                    SessionId::from_raw(row.try_get::<String, _>("id")?),
                    TaskId::from_raw(row.try_get::<String, _>("task_id")?),
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let task_rows = sqlx::query("SELECT id FROM tasks WHERE state = ?")
            .bind(TaskState::Scouting.as_str())
            .fetch_all(&mut *tx)
            .await?;
        let orphaned_tasks = task_rows
            .into_iter()
            .map(|row| Ok::<_, StoreError>(TaskId::from_raw(row.try_get::<String, _>("id")?)))
            .collect::<Result<Vec<_>, _>>()?;

        // A `running` build belongs to a dead process; left alone it wedges
        // the serial build queue forever. `queued` builds are durable intent
        // and survive untouched. Tasks mid-`building` go back to
        // `ready_to_build` — their specs are still approved and good.
        let build_rows = sqlx::query("SELECT id FROM builds WHERE status = ?")
            .bind(BuildStatus::Running.as_str())
            .fetch_all(&mut *tx)
            .await?;
        let orphaned_builds = build_rows
            .into_iter()
            .map(|row| Ok::<_, StoreError>(BuildId::from_raw(row.try_get::<String, _>("id")?)))
            .collect::<Result<Vec<_>, _>>()?;
        let building_rows = sqlx::query("SELECT id FROM tasks WHERE state = ?")
            .bind(TaskState::Building.as_str())
            .fetch_all(&mut *tx)
            .await?;
        let orphaned_building = building_rows
            .into_iter()
            .map(|row| Ok::<_, StoreError>(TaskId::from_raw(row.try_get::<String, _>("id")?)))
            .collect::<Result<Vec<_>, _>>()?;

        if !orphaned_sessions.is_empty() {
            sqlx::query(
                "UPDATE sessions SET status = ?, completed_at = ?, exit_reason = ? \
                 WHERE status = ?",
            )
            .bind(SessionStatus::ScoutFailed.as_str())
            .bind(now.to_rfc3339())
            .bind(ORPHANED_EXIT_REASON)
            .bind(SessionStatus::Running.as_str())
            .execute(&mut *tx)
            .await?;
        }
        if !orphaned_tasks.is_empty() {
            // Back to `Queued`, not `Backlog`: a crash doesn't un-pick work a
            // human put in the queue.
            sqlx::query("UPDATE tasks SET state = ?, updated_at = ? WHERE state = ?")
                .bind(TaskState::Queued.as_str())
                .bind(now.to_rfc3339())
                .bind(TaskState::Scouting.as_str())
                .execute(&mut *tx)
                .await?;
        }
        if !orphaned_builds.is_empty() {
            sqlx::query(
                "UPDATE builds SET status = ?, completed_at = ?, exit_reason = ? \
                 WHERE status = ?",
            )
            .bind(BuildStatus::Failed.as_str())
            .bind(now.to_rfc3339())
            .bind(ORPHANED_EXIT_REASON)
            .bind(BuildStatus::Running.as_str())
            .execute(&mut *tx)
            .await?;
        }
        if !orphaned_building.is_empty() {
            sqlx::query("UPDATE tasks SET state = ?, updated_at = ? WHERE state = ?")
                .bind(TaskState::ReadyToBuild.as_str())
                .bind(now.to_rfc3339())
                .bind(TaskState::Building.as_str())
                .execute(&mut *tx)
                .await?;
        }

        // Invariant sweep: no live spec-queue entry may belong to a concluded
        // task. `retire_task` maintains this inline; rows retired before that
        // existed (or by any future gap) are healed here at startup, so a
        // stale approved spec can never sit where `create_build` would
        // consume it.
        let stale_spec_rows = sqlx::query(
            "SELECT q.spec_id, q.status FROM spec_queue q \
             JOIN specs s ON s.id = q.spec_id \
             JOIN tasks t ON t.id = s.task_id \
             WHERE t.state IN (?, ?) AND q.status IN (?, ?, ?, ?)",
        )
        .bind(TaskState::Done.as_str())
        .bind(TaskState::Rejected.as_str())
        .bind(SpecQueueStatus::PendingReview.as_str())
        .bind(SpecQueueStatus::Approved.as_str())
        .bind(SpecQueueStatus::NeedsRevision.as_str())
        .bind(SpecQueueStatus::Blocked.as_str())
        .fetch_all(&mut *tx)
        .await?;
        let mut drained_specs = Vec::new();
        for row in stale_spec_rows {
            let spec_id = SpecId::from_raw(row.try_get::<String, _>("spec_id")?);
            let from_raw: String = row.try_get("status")?;
            sqlx::query("UPDATE spec_queue SET status = ?, rank = NULL WHERE spec_id = ?")
                .bind(SpecQueueStatus::Rejected.as_str())
                .bind(spec_id.as_str())
                .execute(&mut *tx)
                .await?;
            drained_specs.push((spec_id, SpecQueueStatus::from_str(&from_raw)));
        }
        tx.commit().await?;

        let report = ReconcileReport {
            sessions: orphaned_sessions.len(),
            tasks: orphaned_tasks.len(),
            builds: orphaned_builds.len(),
        };

        for (session_id, task_id) in orphaned_sessions {
            self.append_event(EventPayload::SessionCompleted {
                session_id,
                task_id,
                status: SessionStatus::ScoutFailed,
            })
            .await?;
        }
        for task_id in orphaned_tasks {
            self.append_event(EventPayload::TaskStateChanged {
                task_id,
                from: TaskState::Scouting,
                to: TaskState::Queued,
            })
            .await?;
        }
        for build_id in orphaned_builds {
            self.append_event(EventPayload::BuildCompleted {
                build_id,
                status: BuildStatus::Failed,
            })
            .await?;
        }
        for task_id in orphaned_building {
            self.append_event(EventPayload::TaskStateChanged {
                task_id,
                from: TaskState::Building,
                to: TaskState::ReadyToBuild,
            })
            .await?;
        }
        for (spec_id, from) in drained_specs {
            self.append_event(EventPayload::SpecQueueStatusChanged {
                spec_id,
                from,
                to: SpecQueueStatus::Rejected,
            })
            .await?;
        }

        Ok(report)
    }

    // --- specs ---

    pub async fn insert_spec(&self, spec: &Spec) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO specs (id, session_id, task_id, content, complexity, \
             files_touched, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(spec.id.as_str())
        .bind(spec.session_id.as_str())
        .bind(spec.task_id.as_str())
        .bind(&spec.content)
        .bind(spec.complexity.as_str())
        .bind(serde_json::to_string(&spec.files_touched)?)
        .bind(spec.created_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_spec(&self, id: &SpecId) -> Result<Option<Spec>, StoreError> {
        let row = sqlx::query(
            "SELECT id, session_id, task_id, content, complexity, files_touched, \
             created_at FROM specs WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(spec_from_row).transpose()
    }

    pub async fn list_specs(&self) -> Result<Vec<Spec>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, session_id, task_id, content, complexity, files_touched, \
             created_at FROM specs ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(spec_from_row).collect()
    }

    /// The most recently created spec for `task` that a reviewer has actually
    /// ruled on, with its verdict and feedback.
    ///
    /// Only the three verdict statuses count. `PendingReview` and `Blocked` are
    /// server-assigned, so there is no reviewer opinion to replay. Ordering is
    /// `created_at DESC` with `rowid DESC` as the tiebreak: two specs for one
    /// task can share a timestamp to second resolution, and the query has to be
    /// deterministic. `created_at` is always written as UTC RFC-3339, so
    /// lexicographic ordering matches chronological ordering.
    pub async fn latest_reviewed_spec(
        &self,
        task_id: &TaskId,
    ) -> Result<Option<ReviewedSpec>, StoreError> {
        let row = sqlx::query(
            "SELECT s.id, s.session_id, s.task_id, s.content, s.complexity, \
             s.files_touched, s.created_at, q.status, q.feedback \
             FROM specs s JOIN spec_queue q ON q.spec_id = s.id \
             WHERE s.task_id = ? AND q.status IN (?, ?, ?) \
             ORDER BY s.created_at DESC, s.rowid DESC LIMIT 1",
        )
        .bind(task_id.as_str())
        .bind(SpecQueueStatus::Approved.as_str())
        .bind(SpecQueueStatus::NeedsRevision.as_str())
        .bind(SpecQueueStatus::Rejected.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(reviewed_spec_from_row).transpose()
    }

    // --- spec queue ---

    pub async fn upsert_spec_queue_entry(&self, entry: &SpecQueueEntry) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO spec_queue (spec_id, status, rank, approved_at, feedback, \
             blocking_dependencies) VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(spec_id) DO UPDATE SET \
               status = excluded.status, \
               rank = excluded.rank, \
               approved_at = excluded.approved_at, \
               feedback = excluded.feedback, \
               blocking_dependencies = excluded.blocking_dependencies",
        )
        .bind(entry.spec_id.as_str())
        .bind(entry.status.as_str())
        .bind(entry.rank)
        .bind(entry.approved_at.map(|t| t.to_rfc3339()))
        .bind(&entry.feedback)
        .bind(serde_json::to_string(&entry.blocking_dependencies)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_spec_queue_entry(
        &self,
        spec_id: &SpecId,
    ) -> Result<Option<SpecQueueEntry>, StoreError> {
        let row = sqlx::query(
            "SELECT spec_id, status, rank, approved_at, feedback, blocking_dependencies \
             FROM spec_queue WHERE spec_id = ?",
        )
        .bind(spec_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(spec_queue_entry_from_row).transpose()
    }

    /// List the spec queue in review order: manual rank first (nulls last),
    /// then oldest spec first. Each item carries the owning task id.
    pub async fn list_spec_queue(&self) -> Result<Vec<SpecQueueItem>, StoreError> {
        let rows = sqlx::query(
            "SELECT q.spec_id, q.status, q.rank, q.approved_at, q.feedback, \
             q.blocking_dependencies, s.task_id FROM spec_queue q \
             JOIN specs s ON s.id = q.spec_id \
             ORDER BY q.rank IS NULL, q.rank, s.created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(spec_queue_item_from_row).collect()
    }

    /// Replace the spec queue order with `ids`, assigning ranks 1..N. Entries
    /// not listed are left unranked. Same semantics as [`Self::set_queue_order`].
    ///
    /// Appends [`EventPayload::SpecQueueReordered`] on success.
    pub async fn set_spec_queue_order(&self, ids: &[SpecId]) -> Result<(), StoreError> {
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            if !seen.insert(id.as_str()) {
                return Err(StoreError::Invalid(format!("duplicate spec id: {id}")));
            }
        }

        let mut tx = self.pool.begin().await?;
        let mut missing = Vec::new();
        for id in ids {
            let found = sqlx::query("SELECT 1 FROM spec_queue WHERE spec_id = ?")
                .bind(id.as_str())
                .fetch_optional(&mut *tx)
                .await?;
            if found.is_none() {
                missing.push(id.to_string());
            }
        }
        if !missing.is_empty() {
            return Err(StoreError::NotFound(format!(
                "spec queue entries: {}",
                missing.join(", ")
            )));
        }

        sqlx::query("UPDATE spec_queue SET rank = NULL WHERE rank IS NOT NULL")
            .execute(&mut *tx)
            .await?;
        for (idx, id) in ids.iter().enumerate() {
            sqlx::query("UPDATE spec_queue SET rank = ? WHERE spec_id = ?")
                .bind(idx as i64 + 1)
                .bind(id.as_str())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;

        self.append_event(EventPayload::SpecQueueReordered {
            spec_ids: ids.to_vec(),
        })
        .await?;
        Ok(())
    }

    /// Record a review outcome for a queued spec and advance the owning task.
    ///
    /// `status` must be a review outcome — `Approved`, `NeedsRevision` or
    /// `Rejected`. `PendingReview` and `Blocked` are states the system assigns,
    /// not verdicts a reviewer can render, and are rejected as invalid.
    ///
    /// Task side effects: approved → `ReadyToBuild`, rejected → `Rejected`,
    /// needs revision → back to `Queued` so it re-scouts without losing its
    /// place as picked-up work.
    pub async fn review_spec(
        &self,
        spec_id: &SpecId,
        status: SpecQueueStatus,
        feedback: Option<String>,
    ) -> Result<SpecQueueEntry, StoreError> {
        let next_task_state = match status {
            SpecQueueStatus::Approved => TaskState::ReadyToBuild,
            SpecQueueStatus::Rejected => TaskState::Rejected,
            SpecQueueStatus::NeedsRevision => TaskState::Queued,
            // `Built` is how the approved queue drains — assigned by a
            // successful Builder run, never rendered by a reviewer.
            SpecQueueStatus::PendingReview | SpecQueueStatus::Blocked | SpecQueueStatus::Built => {
                return Err(StoreError::Invalid(format!(
                    "{} is not a review outcome",
                    status.as_str()
                )));
            }
        };

        let entry = self
            .get_spec_queue_entry(spec_id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("spec queue entry {spec_id}")))?;
        let spec = self
            .get_spec(spec_id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("spec {spec_id}")))?;
        let task = self
            .get_task(&spec.task_id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("task {}", spec.task_id)))?;

        let now = Utc::now();
        let approved_at = match status {
            SpecQueueStatus::Approved => Some(now),
            _ => None,
        };

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE spec_queue SET status = ?, approved_at = ?, feedback = ? \
             WHERE spec_id = ?",
        )
        .bind(status.as_str())
        .bind(approved_at.map(|t| t.to_rfc3339()))
        .bind(&feedback)
        .bind(spec_id.as_str())
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE tasks SET state = ?, updated_at = ? WHERE id = ?")
            .bind(next_task_state.as_str())
            .bind(now.to_rfc3339())
            .bind(task.id.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        self.append_event(EventPayload::SpecQueueStatusChanged {
            spec_id: spec_id.clone(),
            from: Some(entry.status),
            to: status,
        })
        .await?;
        if task.state != next_task_state {
            self.append_event(EventPayload::TaskStateChanged {
                task_id: task.id.clone(),
                from: task.state,
                to: next_task_state,
            })
            .await?;
        }

        Ok(SpecQueueEntry {
            status,
            approved_at,
            feedback,
            ..entry
        })
    }

    // --- builds ---

    /// Queue a Builder run over a set of approved specs. Returns the created
    /// build in `queued` status; the serial build loop picks it up.
    ///
    /// Validation (all → [`StoreError::Invalid`], a 400 at the API):
    /// - the set is non-empty and free of duplicates
    /// - every spec exists and its queue status is `approved`
    /// - all specs belong to one project (one build = one branch = one repo)
    /// - no spec is already part of a `queued`/`running` build
    ///
    /// The order the caller sends is irrelevant: the batch is re-sorted into
    /// spec-queue order (rank, then spec age), because the queue is
    /// human-authoritative and a build must not scramble it.
    pub async fn create_build(
        &self,
        spec_ids: &[SpecId],
        base_branch: &str,
    ) -> Result<Build, StoreError> {
        if spec_ids.is_empty() {
            return Err(StoreError::Invalid(
                "a build needs at least one spec".into(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for id in spec_ids {
            if !seen.insert(id.as_str()) {
                return Err(StoreError::Invalid(format!("duplicate spec id: {id}")));
            }
        }

        let mut tx = self.pool.begin().await?;

        // One query resolves existence, review status, project, and queue
        // order for the whole batch.
        let mut resolved: Vec<(SpecId, String, Option<i64>, i64)> = Vec::new();
        for id in spec_ids {
            let row = sqlx::query(
                "SELECT s.id, s.rowid AS spec_rowid, t.project_id, q.status, q.rank \
                 FROM specs s \
                 JOIN tasks t ON t.id = s.task_id \
                 LEFT JOIN spec_queue q ON q.spec_id = s.id \
                 WHERE s.id = ?",
            )
            .bind(id.as_str())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("spec {id}")))?;

            let status: Option<String> = row.try_get("status")?;
            match status.as_deref().and_then(SpecQueueStatus::from_str) {
                Some(SpecQueueStatus::Approved) => {}
                other => {
                    return Err(StoreError::Invalid(format!(
                        "spec {id} is {}, only approved specs can be built",
                        other.map(|s| s.as_str()).unwrap_or("not in the queue")
                    )));
                }
            }

            let in_flight = sqlx::query(
                "SELECT b.id FROM build_specs bs JOIN builds b ON b.id = bs.build_id \
                 WHERE bs.spec_id = ? AND b.status IN (?, ?)",
            )
            .bind(id.as_str())
            .bind(BuildStatus::Queued.as_str())
            .bind(BuildStatus::Running.as_str())
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(row) = in_flight {
                let build: String = row.try_get("id")?;
                return Err(StoreError::Invalid(format!(
                    "spec {id} is already part of build {build}"
                )));
            }

            resolved.push((
                id.clone(),
                row.try_get("project_id")?,
                row.try_get("rank")?,
                row.try_get("spec_rowid")?,
            ));
        }

        let project_id = resolved[0].1.clone();
        if resolved.iter().any(|(_, p, _, _)| *p != project_id) {
            return Err(StoreError::Invalid(
                "specs span multiple projects; one build builds one repo".into(),
            ));
        }

        // Spec-queue order: rank first (nulls last), then spec age.
        resolved.sort_by_key(|(_, _, rank, rowid)| (rank.is_none(), rank.unwrap_or(0), *rowid));

        let build = Build {
            id: BuildId::new(),
            project_id: ProjectId::from_raw(project_id),
            vm_id: None,
            branch: String::new(), // set below, needs the id
            base_branch: base_branch.to_string(),
            base_sha: None,
            head_sha: None,
            pr_number: None,
            status: BuildStatus::Queued,
            summary: None,
            files_touched: vec![],
            exit_reason: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
        };
        let branch = format!("build/{}", build.id);
        sqlx::query(
            "INSERT INTO builds (id, project_id, branch, base_branch, status, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(build.id.as_str())
        .bind(build.project_id.as_str())
        .bind(&branch)
        .bind(&build.base_branch)
        .bind(build.status.as_str())
        .bind(build.created_at.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        for (position, (spec_id, _, _, _)) in resolved.iter().enumerate() {
            sqlx::query("INSERT INTO build_specs (build_id, spec_id, position) VALUES (?, ?, ?)")
                .bind(build.id.as_str())
                .bind(spec_id.as_str())
                .bind(position as i64 + 1)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;

        self.append_event(EventPayload::BuildRequested {
            build_id: build.id.clone(),
            spec_ids: resolved.iter().map(|(id, _, _, _)| id.clone()).collect(),
        })
        .await?;

        Ok(Build { branch, ..build })
    }

    /// Claim the next build for the serial loop: if any build is `running`,
    /// returns `None`; otherwise the oldest `queued` build becomes `running`
    /// in the same transaction — the check and the claim cannot be split,
    /// which is what makes execution serial by construction.
    ///
    /// The specs' tasks move `ready_to_build → building` (tasks that left
    /// `ready_to_build` some other way — e.g. retired because their issue
    /// closed — are left alone). Emits `BuildStarted` plus the task events.
    pub async fn claim_next_queued_build(&self) -> Result<Option<Build>, StoreError> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;

        let running = sqlx::query("SELECT 1 FROM builds WHERE status = ? LIMIT 1")
            .bind(BuildStatus::Running.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        if running.is_some() {
            return Ok(None);
        }

        let Some(row) = sqlx::query(
            "SELECT id FROM builds WHERE status = ? ORDER BY created_at, rowid LIMIT 1",
        )
        .bind(BuildStatus::Queued.as_str())
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(None);
        };
        let build_id = BuildId::from_raw(row.try_get::<String, _>("id")?);

        sqlx::query("UPDATE builds SET status = ?, started_at = ? WHERE id = ?")
            .bind(BuildStatus::Running.as_str())
            .bind(now.to_rfc3339())
            .bind(build_id.as_str())
            .execute(&mut *tx)
            .await?;

        let task_rows = sqlx::query(
            "SELECT DISTINCT t.id FROM build_specs bs \
             JOIN specs s ON s.id = bs.spec_id \
             JOIN tasks t ON t.id = s.task_id \
             WHERE bs.build_id = ? AND t.state = ?",
        )
        .bind(build_id.as_str())
        .bind(TaskState::ReadyToBuild.as_str())
        .fetch_all(&mut *tx)
        .await?;
        let mut building_tasks = Vec::new();
        for row in task_rows {
            let id = TaskId::from_raw(row.try_get::<String, _>("id")?);
            sqlx::query("UPDATE tasks SET state = ?, updated_at = ? WHERE id = ?")
                .bind(TaskState::Building.as_str())
                .bind(now.to_rfc3339())
                .bind(id.as_str())
                .execute(&mut *tx)
                .await?;
            building_tasks.push(id);
        }
        tx.commit().await?;

        self.append_event(EventPayload::BuildStarted {
            build_id: build_id.clone(),
        })
        .await?;
        for task_id in building_tasks {
            self.append_event(EventPayload::TaskStateChanged {
                task_id,
                from: TaskState::ReadyToBuild,
                to: TaskState::Building,
            })
            .await?;
        }

        self.get_build(&build_id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("build {build_id}")))
            .map(Some)
    }

    pub async fn get_build(&self, id: &BuildId) -> Result<Option<Build>, StoreError> {
        let row = sqlx::query(
            "SELECT id, project_id, vm_id, branch, base_branch, base_sha, head_sha, \
             pr_number, status, summary, files_touched, exit_reason, created_at, \
             started_at, completed_at FROM builds WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(build_from_row).transpose()
    }

    /// Newest first.
    pub async fn list_builds(&self) -> Result<Vec<Build>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, project_id, vm_id, branch, base_branch, base_sha, head_sha, \
             pr_number, status, summary, files_touched, exit_reason, created_at, \
             started_at, completed_at FROM builds ORDER BY created_at DESC, rowid DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(build_from_row).collect()
    }

    /// The build's specs in batch order.
    pub async fn build_spec_ids(&self, id: &BuildId) -> Result<Vec<SpecId>, StoreError> {
        let rows =
            sqlx::query("SELECT spec_id FROM build_specs WHERE build_id = ? ORDER BY position")
                .bind(id.as_str())
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|row| Ok(SpecId::from_raw(row.try_get::<String, _>("spec_id")?)))
            .collect()
    }

    pub async fn set_build_vm(&self, id: &BuildId, vm_id: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE builds SET vm_id = ? WHERE id = ?")
            .bind(vm_id)
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_build_base_sha(&self, id: &BuildId, base_sha: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE builds SET base_sha = ? WHERE id = ?")
            .bind(base_sha)
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Terminal success: branch pushed, PR open. In one transaction the build
    /// row is completed, the batch's specs drain `approved → built` (a spec
    /// cannot be built twice), and their tasks conclude `building → done`.
    pub async fn finalize_build_succeeded(
        &self,
        id: &BuildId,
        head_sha: &str,
        pr_number: u64,
        summary: Option<&str>,
        files_touched: &[String],
    ) -> Result<Build, StoreError> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "UPDATE builds SET status = ?, head_sha = ?, pr_number = ?, summary = ?, \
             files_touched = ?, completed_at = ? WHERE id = ?",
        )
        .bind(BuildStatus::Succeeded.as_str())
        .bind(head_sha)
        .bind(pr_number as i64)
        .bind(summary)
        .bind(serde_json::to_string(files_touched)?)
        .bind(now.to_rfc3339())
        .bind(id.as_str())
        .execute(&mut *tx)
        .await?;

        let spec_rows = sqlx::query(
            "SELECT bs.spec_id, s.task_id, t.state FROM build_specs bs \
             JOIN specs s ON s.id = bs.spec_id \
             JOIN tasks t ON t.id = s.task_id \
             WHERE bs.build_id = ? ORDER BY bs.position",
        )
        .bind(id.as_str())
        .fetch_all(&mut *tx)
        .await?;
        let mut built_specs = Vec::new();
        let mut done_tasks = Vec::new();
        for row in spec_rows {
            let spec_id = SpecId::from_raw(row.try_get::<String, _>("spec_id")?);
            sqlx::query("UPDATE spec_queue SET status = ? WHERE spec_id = ?")
                .bind(SpecQueueStatus::Built.as_str())
                .bind(spec_id.as_str())
                .execute(&mut *tx)
                .await?;
            built_specs.push(spec_id);

            let task_id = TaskId::from_raw(row.try_get::<String, _>("task_id")?);
            let state: String = row.try_get("state")?;
            if state == TaskState::Building.as_str() && !done_tasks.contains(&task_id) {
                sqlx::query("UPDATE tasks SET state = ?, updated_at = ? WHERE id = ?")
                    .bind(TaskState::Done.as_str())
                    .bind(now.to_rfc3339())
                    .bind(task_id.as_str())
                    .execute(&mut *tx)
                    .await?;
                done_tasks.push(task_id);
            }
        }
        tx.commit().await?;

        for spec_id in built_specs {
            self.append_event(EventPayload::SpecQueueStatusChanged {
                spec_id,
                from: Some(SpecQueueStatus::Approved),
                to: SpecQueueStatus::Built,
            })
            .await?;
        }
        for task_id in done_tasks {
            self.append_event(EventPayload::TaskStateChanged {
                task_id,
                from: TaskState::Building,
                to: TaskState::Done,
            })
            .await?;
        }
        self.append_event(EventPayload::PullRequestOpened {
            build_id: id.clone(),
            pr_number,
        })
        .await?;
        self.append_event(EventPayload::BuildCompleted {
            build_id: id.clone(),
            status: BuildStatus::Succeeded,
        })
        .await?;

        self.get_build(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("build {id}")))
    }

    /// Terminal failure, any stage. The batch's specs stay `approved` and
    /// their tasks return `building → ready_to_build` — never further back: a
    /// failed build must not re-scout work that already has a good spec.
    pub async fn finalize_build_failed(
        &self,
        id: &BuildId,
        reason: &str,
    ) -> Result<Build, StoreError> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;

        sqlx::query("UPDATE builds SET status = ?, exit_reason = ?, completed_at = ? WHERE id = ?")
            .bind(BuildStatus::Failed.as_str())
            .bind(reason)
            .bind(now.to_rfc3339())
            .bind(id.as_str())
            .execute(&mut *tx)
            .await?;

        let task_rows = sqlx::query(
            "SELECT DISTINCT t.id FROM build_specs bs \
             JOIN specs s ON s.id = bs.spec_id \
             JOIN tasks t ON t.id = s.task_id \
             WHERE bs.build_id = ? AND t.state = ?",
        )
        .bind(id.as_str())
        .bind(TaskState::Building.as_str())
        .fetch_all(&mut *tx)
        .await?;
        let mut returned = Vec::new();
        for row in task_rows {
            let task_id = TaskId::from_raw(row.try_get::<String, _>("id")?);
            sqlx::query("UPDATE tasks SET state = ?, updated_at = ? WHERE id = ?")
                .bind(TaskState::ReadyToBuild.as_str())
                .bind(now.to_rfc3339())
                .bind(task_id.as_str())
                .execute(&mut *tx)
                .await?;
            returned.push(task_id);
        }
        tx.commit().await?;

        for task_id in returned {
            self.append_event(EventPayload::TaskStateChanged {
                task_id,
                from: TaskState::Building,
                to: TaskState::ReadyToBuild,
            })
            .await?;
        }
        self.append_event(EventPayload::BuildCompleted {
            build_id: id.clone(),
            status: BuildStatus::Failed,
        })
        .await?;

        self.get_build(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("build {id}")))
    }

    // --- orchestrator ---

    /// Append one conversation turn and emit [`EventPayload::OrchestratorMessage`].
    pub async fn append_orchestrator_message(
        &self,
        role: ChatRole,
        content: &str,
    ) -> Result<OrchestratorMessage, StoreError> {
        let now = Utc::now();
        let result = sqlx::query(
            "INSERT INTO orchestrator_messages (role, content, created_at) VALUES (?, ?, ?)",
        )
        .bind(role.as_str())
        .bind(content)
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await?;
        let seq = result.last_insert_rowid();
        self.append_event(EventPayload::OrchestratorMessage { seq, role })
            .await?;
        Ok(OrchestratorMessage {
            seq,
            role,
            content: content.to_string(),
            created_at: now,
        })
    }

    /// Publish one moment of an in-flight tick to live-feed subscribers.
    /// Fire-and-forget: no subscribers, no problem — nothing is persisted.
    pub fn publish_orchestrator_feed(&self, event: OrchestratorFeedEvent) {
        let _ = self.orchestrator_feed_tx.send(event);
    }

    /// Live feed of the in-flight orchestrator tick (`/orchestrator/stream`).
    /// A lagged subscriber misses deltas, never the durable message.
    pub fn subscribe_orchestrator_feed(&self) -> broadcast::Receiver<OrchestratorFeedEvent> {
        self.orchestrator_feed_tx.subscribe()
    }

    /// Messages with `seq > since`, oldest first.
    pub async fn orchestrator_messages_since(
        &self,
        since: i64,
    ) -> Result<Vec<OrchestratorMessage>, StoreError> {
        let rows = sqlx::query(
            "SELECT seq, role, content, created_at FROM orchestrator_messages              WHERE seq > ? ORDER BY seq",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let role_raw: String = row.try_get("role")?;
                Ok(OrchestratorMessage {
                    seq: row.try_get("seq")?,
                    role: ChatRole::from_str(&role_raw).ok_or(StoreError::BadEnum {
                        column: "role",
                        value: role_raw,
                    })?,
                    content: row.try_get("content")?,
                    created_at: parse_ts(&row.try_get::<String, _>("created_at")?, "created_at")?,
                })
            })
            .collect()
    }

    /// The input turns (user + event) the orchestrator has not answered yet:
    /// everything above the `answered_through` watermark. Empty when the
    /// conversation is settled. This is the tick condition — DB-derived, so a
    /// crash between an input and its reply just means the next pass answers
    /// it again. The watermark (not "since the last assistant turn") is what
    /// keeps input appended *during* a multi-minute agent turn unanswered:
    /// it lands below the reply's seq but above the watermark.
    pub async fn unanswered_orchestrator_messages(
        &self,
    ) -> Result<Vec<OrchestratorMessage>, StoreError> {
        let watermark: i64 = sqlx::query("SELECT answered_through FROM orchestrator WHERE id = 1")
            .fetch_one(&self.pool)
            .await?
            .try_get("answered_through")?;
        Ok(self
            .orchestrator_messages_since(watermark)
            .await?
            .into_iter()
            .filter(|m| m.role.is_input())
            .collect())
    }

    /// Append the assistant's reply and advance the answered watermark to
    /// `answered_through` (the highest input seq the prompt covered) in one
    /// transaction, so a crash can't record the reply without settling the
    /// input it answered.
    pub async fn append_orchestrator_reply(
        &self,
        content: &str,
        answered_through: i64,
        cc_session_id: Option<&str>,
    ) -> Result<OrchestratorMessage, StoreError> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "INSERT INTO orchestrator_messages (role, content, created_at, cc_session_id)              VALUES (?, ?, ?, ?)",
        )
        .bind(ChatRole::Assistant.as_str())
        .bind(content)
        .bind(now.to_rfc3339())
        .bind(cc_session_id)
        .execute(&mut *tx)
        .await?;
        let seq = result.last_insert_rowid();
        sqlx::query(
            "UPDATE orchestrator SET answered_through = MAX(answered_through, ?) WHERE id = 1",
        )
        .bind(answered_through)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.append_event(EventPayload::OrchestratorMessage {
            seq,
            role: ChatRole::Assistant,
        })
        .await?;
        Ok(OrchestratorMessage {
            seq,
            role: ChatRole::Assistant,
            content: content.to_string(),
            created_at: now,
        })
    }

    pub async fn orchestrator_cc_session(&self) -> Result<Option<String>, StoreError> {
        let row = sqlx::query("SELECT cc_session_id FROM orchestrator WHERE id = 1")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get("cc_session_id")?)
    }

    // --- orchestrator session ledger ---

    /// Close out the live session and write the seam into the conversation,
    /// in one transaction.
    ///
    /// Called the moment the loss is *known* — a failed `--resume` — not when
    /// its replacement succeeds, because the context is already gone either
    /// way and the record should survive a fresh start that also fails. The
    /// seam is a [`ChatRole::System`] turn, so it is visible to the reader
    /// without becoming input the orchestrator owes a reply to.
    pub async fn end_orchestrator_session(
        &self,
        cc_session_id: &str,
        reason: SessionEndReason,
    ) -> Result<(), StoreError> {
        let now = Utc::now();
        let seam = match reason {
            SessionEndReason::ResumeFailed => {
                "(session restarted — resuming the previous one failed, so its accumulated \
                 context is gone. The conversation above is intact; the orchestrator's memory \
                 of it is not.)"
            }
            SessionEndReason::Rotated => {
                "(session rotated — the previous context was retired deliberately and seeded \
                 forward as a summary.)"
            }
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE orchestrator_sessions SET ended_at = ?, end_reason = ?              WHERE cc_session_id = ? AND ended_at IS NULL",
        )
        .bind(now.to_rfc3339())
        .bind(reason.as_str())
        .bind(cc_session_id)
        .execute(&mut *tx)
        .await?;
        let result = sqlx::query(
            "INSERT INTO orchestrator_messages (role, content, created_at, cc_session_id)              VALUES (?, ?, ?, ?)",
        )
        .bind(ChatRole::System.as_str())
        .bind(seam)
        .bind(now.to_rfc3339())
        .bind(cc_session_id)
        .execute(&mut *tx)
        .await?;
        let seq = result.last_insert_rowid();
        tx.commit().await?;
        self.append_event(EventPayload::OrchestratorMessage {
            seq,
            role: ChatRole::System,
        })
        .await?;
        Ok(())
    }

    /// Adopt a newly created Claude Code session as the live one: open its
    /// ledger row and point the singleton at it, in one transaction.
    ///
    /// Deliberately called only *after* the session's first turn succeeds, so
    /// a failed start leaves the previous session id in place and the next
    /// tick retries rather than stranding the conversation on a session
    /// Claude Code never created.
    pub async fn begin_orchestrator_session(
        &self,
        cc_session_id: &str,
        replacing: Option<&str>,
        reason: Option<SessionEndReason>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO orchestrator_sessions (cc_session_id, started_at) VALUES (?, ?)")
            .bind(cc_session_id)
            .bind(Utc::now().to_rfc3339())
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE orchestrator SET cc_session_id = ? WHERE id = 1")
            .bind(cc_session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.append_event(EventPayload::OrchestratorSessionStarted {
            session_id: cc_session_id.to_string(),
            replacing: replacing.map(str::to_string),
            reason,
        })
        .await?;
        Ok(())
    }

    /// Record the context size reported by a finished turn. This is the gauge
    /// a rotation threshold reads: an absolute measurement, not a running
    /// total, so it stays honest across turns the server never drove (an
    /// interactive checkout) and across an agent that isn't reporting usage
    /// at all (the column simply stops advancing).
    pub async fn record_orchestrator_context_tokens(
        &self,
        cc_session_id: &str,
        tokens: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE orchestrator_sessions SET last_context_tokens = ? WHERE cc_session_id = ?",
        )
        .bind(tokens)
        .bind(cc_session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The session ledger, newest first.
    pub async fn orchestrator_sessions(&self) -> Result<Vec<OrchestratorSession>, StoreError> {
        let rows = sqlx::query(
            "SELECT cc_session_id, started_at, ended_at, end_reason, last_context_tokens,                     summary, summary_generated_at              FROM orchestrator_sessions ORDER BY started_at DESC, rowid DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(orchestrator_session_row).collect()
    }

    /// Record the orchestrator agent's effective working directory. Written
    /// at startup (it's config, not state) so `GET /orchestrator/session`
    /// can tell clients where to `cd` before an interactive resume.
    pub async fn set_orchestrator_workdir(&self, workdir: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE orchestrator SET workdir = ? WHERE id = 1")
            .bind(workdir)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The orchestrator session as clients see it: the CC session id (if one
    /// exists yet), the agent's workdir, and whether a human currently has
    /// the session checked out for interactive use.
    pub async fn orchestrator_session_info(&self) -> Result<OrchestratorSessionInfo, StoreError> {
        let row = sqlx::query(
            "SELECT o.cc_session_id, o.workdir, o.checked_out_at, s.last_context_tokens              FROM orchestrator o              LEFT JOIN orchestrator_sessions s ON s.cc_session_id = o.cc_session_id              WHERE o.id = 1",
        )
        .fetch_one(&self.pool)
        .await?;
        let checked_out_at: Option<String> = row.try_get("checked_out_at")?;
        Ok(OrchestratorSessionInfo {
            cc_session_id: row.try_get("cc_session_id")?,
            workdir: row.try_get("workdir")?,
            checked_out: checked_out_at
                .as_deref()
                .is_some_and(checkout_heartbeat_fresh),
            context_tokens: row.try_get("last_context_tokens")?,
        })
    }

    /// Renew the interactive-checkout heartbeat. While it's fresh
    /// ([`ORCHESTRATOR_CHECKOUT_TTL`]) the headless tick must not run — CC
    /// sessions have no file locking, so a tick would interleave writes with
    /// the human's interactive client.
    pub async fn orchestrator_checkout(&self) -> Result<(), StoreError> {
        sqlx::query("UPDATE orchestrator SET checked_out_at = ? WHERE id = 1")
            .bind(Utc::now().to_rfc3339())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// End the interactive checkout; the next tick may resume the session.
    pub async fn orchestrator_release(&self) -> Result<(), StoreError> {
        sqlx::query("UPDATE orchestrator SET checked_out_at = NULL WHERE id = 1")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Whether a human currently has the orchestrator session checked out.
    pub async fn orchestrator_checked_out(&self) -> Result<bool, StoreError> {
        Ok(self.orchestrator_session_info().await?.checked_out)
    }

    // --- mode ---

    pub async fn get_mode(&self) -> Result<Mode, StoreError> {
        let row = sqlx::query("SELECT mode FROM mode WHERE id = 1")
            .fetch_one(&self.pool)
            .await?;
        let raw: String = row.try_get("mode")?;
        Mode::from_str(&raw).ok_or(StoreError::BadEnum {
            column: "mode",
            value: raw,
        })
    }

    pub async fn set_mode(&self, mode: Mode) -> Result<(), StoreError> {
        sqlx::query("UPDATE mode SET mode = ?, updated_at = ? WHERE id = 1")
            .bind(mode.as_str())
            .bind(Utc::now().to_rfc3339())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // --- events ---

    /// Append an event to the log and broadcast it to live subscribers.
    ///
    /// `seq` is assigned by SQLite's AUTOINCREMENT. If there are no
    /// subscribers or they've all fallen away, the broadcast silently drops.
    pub async fn append_event(&self, payload: EventPayload) -> Result<Event, StoreError> {
        let timestamp = Utc::now();
        let payload_json = serde_json::to_string(&payload)?;

        let row =
            sqlx::query("INSERT INTO events (timestamp, payload) VALUES (?, ?) RETURNING seq")
                .bind(timestamp.to_rfc3339())
                .bind(payload_json)
                .fetch_one(&self.pool)
                .await?;
        let seq: i64 = row.try_get("seq")?;

        let event = Event {
            seq,
            timestamp,
            payload,
        };
        let _ = self.event_tx.send(event.clone());
        Ok(event)
    }

    /// Subscribe to live events. Returns a receiver that will see every event
    /// appended from now on. Does not replay history — pair with `events_since`
    /// to catch up.
    pub fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.event_tx.subscribe()
    }

    /// Return up to `limit` events with seq >= `since`, ordered by seq
    /// ascending. The bound is in SQL so a client paging forward through the
    /// log reads one page per request rather than the whole log each time.
    pub async fn events_since(&self, since: i64, limit: i64) -> Result<Vec<Event>, StoreError> {
        let rows = sqlx::query(
            "SELECT seq, timestamp, payload FROM events WHERE seq >= ? ORDER BY seq LIMIT ?",
        )
        .bind(since)
        // SQLite reads a negative LIMIT as unbounded, so a caller-supplied
        // `-1` would mean "the whole log" — the opposite of what it looks
        // like. Clamp here so the bound doesn't depend on caller validation.
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(event_from_row).collect()
    }

    /// Return the entire event log, ordered by seq ascending. Callers that
    /// want a page want `events_since`; this one is unbounded on purpose.
    pub async fn all_events(&self) -> Result<Vec<Event>, StoreError> {
        self.events_since(0, i64::MAX).await
    }

    /// Return the last N events, ordered by seq ascending.
    pub async fn recent_events(&self, limit: i64) -> Result<Vec<Event>, StoreError> {
        let mut rows =
            sqlx::query("SELECT seq, timestamp, payload FROM events ORDER BY seq DESC LIMIT ?")
                .bind(limit)
                .fetch_all(&self.pool)
                .await?;
        rows.reverse();
        rows.into_iter().map(event_from_row).collect()
    }

    /// The newest event seq, 0 on an empty log. Recorded as a briefing's
    /// `event_high_water` so a later regen can ask "did anything move?".
    pub async fn latest_event_seq(&self) -> Result<i64, StoreError> {
        let row = sqlx::query("SELECT COALESCE(MAX(seq), 0) AS seq FROM events")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get("seq")?)
    }

    // --- briefings ---

    /// Every stored briefing. Sections never generated simply have no row —
    /// the API layer fills in the gaps so clients always see all three.
    pub async fn list_briefings(&self) -> Result<Vec<Briefing>, StoreError> {
        let rows =
            sqlx::query("SELECT section, content, generated_at, event_high_water FROM briefings")
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|row| {
                let section_raw: String = row.try_get("section")?;
                Ok(Briefing {
                    section: BriefingSection::from_str(&section_raw).ok_or(
                        StoreError::BadEnum {
                            column: "section",
                            value: section_raw,
                        },
                    )?,
                    content: row.try_get("content")?,
                    generated_at: parse_ts(
                        &row.try_get::<String, _>("generated_at")?,
                        "generated_at",
                    )?,
                    event_high_water: row.try_get("event_high_water")?,
                })
            })
            .collect()
    }

    /// Replace a section's briefing wholesale. Last write wins — the content
    /// is a cache, not state, so there is nothing to merge.
    pub async fn upsert_briefing(&self, briefing: &Briefing) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO briefings (section, content, generated_at, event_high_water)              VALUES (?, ?, ?, ?)              ON CONFLICT(section) DO UPDATE SET                  content = excluded.content,                  generated_at = excluded.generated_at,                  event_high_water = excluded.event_high_water",
        )
        .bind(briefing.section.as_str())
        .bind(&briefing.content)
        .bind(briefing.generated_at.to_rfc3339())
        .bind(briefing.event_high_water)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

// --- row mappers ---

fn project_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Project, StoreError> {
    Ok(Project {
        id: ProjectId::from_raw(row.try_get::<String, _>("id")?),
        repo_owner: row.try_get("repo_owner")?,
        repo_name: row.try_get("repo_name")?,
        added_at: parse_ts(&row.try_get::<String, _>("added_at")?, "added_at")?,
    })
}

fn task_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Task, StoreError> {
    let gh_state_raw: String = row.try_get("gh_state")?;
    let state_raw: String = row.try_get("state")?;
    let labels_raw: String = row.try_get("labels")?;
    Ok(Task {
        id: TaskId::from_raw(row.try_get::<String, _>("id")?),
        project_id: ProjectId::from_raw(row.try_get::<String, _>("project_id")?),
        gh_issue_number: row.try_get::<i64, _>("gh_issue_number")? as u64,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        labels: serde_json::from_str(&labels_raw)?,
        gh_state: GhState::from_str(&gh_state_raw).ok_or(StoreError::BadEnum {
            column: "gh_state",
            value: gh_state_raw,
        })?,
        state: TaskState::from_str(&state_raw).ok_or(StoreError::BadEnum {
            column: "state",
            value: state_raw,
        })?,
        priority: row.try_get("priority")?,
        manual_rank: row.try_get("manual_rank")?,
        dispatch_attempts: row.try_get::<i64, _>("dispatch_attempts")?.max(0) as u32,
        ingested_at: parse_ts(&row.try_get::<String, _>("ingested_at")?, "ingested_at")?,
        updated_at: parse_ts(&row.try_get::<String, _>("updated_at")?, "updated_at")?,
    })
}

fn session_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Session, StoreError> {
    let status_raw: String = row.try_get("status")?;
    let completed_at: Option<String> = row.try_get("completed_at")?;
    Ok(Session {
        id: SessionId::from_raw(row.try_get::<String, _>("id")?),
        task_id: TaskId::from_raw(row.try_get::<String, _>("task_id")?),
        vm_id: row.try_get("vm_id")?,
        branch: row.try_get("branch")?,
        status: SessionStatus::from_str(&status_raw).ok_or(StoreError::BadEnum {
            column: "status",
            value: status_raw,
        })?,
        started_at: parse_ts(&row.try_get::<String, _>("started_at")?, "started_at")?,
        completed_at: completed_at
            .map(|s| parse_ts(&s, "completed_at"))
            .transpose()?,
        exit_reason: row.try_get("exit_reason")?,
        usage: row
            .try_get::<Option<String>, _>("agent_usage")?
            .and_then(|raw| serde_json::from_str(&raw).ok()),
    })
}

fn transcript_line_from_row(row: sqlx::sqlite::SqliteRow) -> Result<TranscriptLine, StoreError> {
    let stream_raw: String = row.try_get("stream")?;
    Ok(TranscriptLine {
        session_id: SessionId::from_raw(row.try_get::<String, _>("session_id")?),
        seq: row.try_get("seq")?,
        timestamp: parse_ts(&row.try_get::<String, _>("timestamp")?, "timestamp")?,
        stream: TranscriptStream::from_str(&stream_raw).ok_or(StoreError::BadEnum {
            column: "stream",
            value: stream_raw,
        })?,
        line: row.try_get("line")?,
    })
}

fn build_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Build, StoreError> {
    let status_raw: String = row.try_get("status")?;
    let files_raw: String = row.try_get("files_touched")?;
    let opt_ts = |col: &'static str| -> Result<Option<DateTime<Utc>>, StoreError> {
        row.try_get::<Option<String>, _>(col)?
            .map(|s| parse_ts(&s, col))
            .transpose()
    };
    Ok(Build {
        id: BuildId::from_raw(row.try_get::<String, _>("id")?),
        project_id: ProjectId::from_raw(row.try_get::<String, _>("project_id")?),
        vm_id: row.try_get("vm_id")?,
        branch: row.try_get("branch")?,
        base_branch: row.try_get("base_branch")?,
        base_sha: row.try_get("base_sha")?,
        head_sha: row.try_get("head_sha")?,
        pr_number: row
            .try_get::<Option<i64>, _>("pr_number")?
            .map(|n| n.max(0) as u64),
        status: BuildStatus::from_str(&status_raw).ok_or(StoreError::BadEnum {
            column: "status",
            value: status_raw,
        })?,
        summary: row.try_get("summary")?,
        files_touched: serde_json::from_str(&files_raw)?,
        exit_reason: row.try_get("exit_reason")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?, "created_at")?,
        started_at: opt_ts("started_at")?,
        completed_at: opt_ts("completed_at")?,
    })
}

fn spec_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Spec, StoreError> {
    let complexity_raw: String = row.try_get("complexity")?;
    let files_raw: String = row.try_get("files_touched")?;
    Ok(Spec {
        id: SpecId::from_raw(row.try_get::<String, _>("id")?),
        session_id: SessionId::from_raw(row.try_get::<String, _>("session_id")?),
        task_id: TaskId::from_raw(row.try_get::<String, _>("task_id")?),
        content: row.try_get("content")?,
        complexity: Complexity::from_str(&complexity_raw).ok_or(StoreError::BadEnum {
            column: "complexity",
            value: complexity_raw,
        })?,
        files_touched: serde_json::from_str(&files_raw)?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?, "created_at")?,
    })
}

/// Maps a `specs` ⋈ `spec_queue` row. Safe to reuse [`spec_from_row`] on the
/// joined row because `specs` has no `status` or `feedback` column of its own,
/// so the two projections can't collide.
fn reviewed_spec_from_row(row: sqlx::sqlite::SqliteRow) -> Result<ReviewedSpec, StoreError> {
    let status_raw: String = row.try_get("status")?;
    let feedback: Option<String> = row.try_get("feedback")?;
    Ok(ReviewedSpec {
        status: SpecQueueStatus::from_str(&status_raw).ok_or(StoreError::BadEnum {
            column: "status",
            value: status_raw,
        })?,
        feedback,
        spec: spec_from_row(row)?,
    })
}

fn spec_queue_entry_from_row(row: sqlx::sqlite::SqliteRow) -> Result<SpecQueueEntry, StoreError> {
    let status_raw: String = row.try_get("status")?;
    let approved_at: Option<String> = row.try_get("approved_at")?;
    let deps_raw: String = row.try_get("blocking_dependencies")?;
    Ok(SpecQueueEntry {
        spec_id: SpecId::from_raw(row.try_get::<String, _>("spec_id")?),
        status: SpecQueueStatus::from_str(&status_raw).ok_or(StoreError::BadEnum {
            column: "status",
            value: status_raw,
        })?,
        rank: row.try_get("rank")?,
        approved_at: approved_at
            .map(|s| parse_ts(&s, "approved_at"))
            .transpose()?,
        feedback: row.try_get("feedback")?,
        blocking_dependencies: serde_json::from_str(&deps_raw)?,
    })
}

fn spec_queue_item_from_row(row: sqlx::sqlite::SqliteRow) -> Result<SpecQueueItem, StoreError> {
    let task_id = TaskId::from_raw(row.try_get::<String, _>("task_id")?);
    Ok(SpecQueueItem {
        entry: spec_queue_entry_from_row(row)?,
        task_id,
    })
}

fn event_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Event, StoreError> {
    let payload_raw: String = row.try_get("payload")?;
    Ok(Event {
        seq: row.try_get("seq")?,
        timestamp: parse_ts(&row.try_get::<String, _>("timestamp")?, "timestamp")?,
        payload: serde_json::from_str(&payload_raw)?,
    })
}

fn orchestrator_session_row(
    row: sqlx::sqlite::SqliteRow,
) -> Result<OrchestratorSession, StoreError> {
    let end_reason_raw: Option<String> = row.try_get("end_reason")?;
    let end_reason = end_reason_raw
        .map(|raw| {
            SessionEndReason::from_str(&raw).ok_or(StoreError::BadEnum {
                column: "end_reason",
                value: raw,
            })
        })
        .transpose()?;
    let ended_at: Option<String> = row.try_get("ended_at")?;
    let summary_generated_at: Option<String> = row.try_get("summary_generated_at")?;
    Ok(OrchestratorSession {
        cc_session_id: row.try_get("cc_session_id")?,
        started_at: parse_ts(&row.try_get::<String, _>("started_at")?, "started_at")?,
        ended_at: ended_at
            .as_deref()
            .map(|s| parse_ts(s, "ended_at"))
            .transpose()?,
        end_reason,
        last_context_tokens: row.try_get("last_context_tokens")?,
        summary: row.try_get("summary")?,
        summary_generated_at: summary_generated_at
            .as_deref()
            .map(|s| parse_ts(s, "summary_generated_at"))
            .transpose()?,
    })
}

fn parse_ts(s: &str, column: &'static str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| StoreError::BadEnum {
            column,
            value: s.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Complexity, GhState, Project, Task, TaskState};

    fn sample_project() -> Project {
        Project {
            id: ProjectId::new(),
            repo_owner: "iamnbutler".into(),
            repo_name: "tasks".into(),
            added_at: Utc::now(),
        }
    }

    fn sample_task(project_id: &ProjectId) -> Task {
        let now = Utc::now();
        Task {
            id: TaskId::new(),
            project_id: project_id.clone(),
            gh_issue_number: 42,
            title: "Example task".into(),
            body: "Do the thing".into(),
            labels: vec!["bug".into(), "p0".into()],
            gh_state: GhState::Open,
            state: TaskState::Backlog,
            priority: 10,
            manual_rank: None,
            dispatch_attempts: 0,
            ingested_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn open_in_memory_runs_migrations() {
        let store = Store::open_in_memory().await.unwrap();
        assert_eq!(store.get_mode().await.unwrap(), Mode::Pause);
        assert!(store.list_projects().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn open_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db");

        {
            let store = Store::open(&path).await.unwrap();
            let project = sample_project();
            store.insert_project(&project).await.unwrap();
        }

        // Reopen, verify persistence
        let store = Store::open(&path).await.unwrap();
        let projects = store.list_projects().await.unwrap();
        assert_eq!(projects.len(), 1);
    }

    #[tokio::test]
    async fn project_insert_and_get() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let loaded = store.get_project(&project.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, project.id);
        assert_eq!(loaded.repo_owner, "iamnbutler");
        assert_eq!(loaded.repo_name, "tasks");
    }

    #[tokio::test]
    async fn task_insert_and_get_roundtrip() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let task = sample_task(&project.id);
        store.insert_task(&task).await.unwrap();

        let loaded = store.get_task(&task.id).await.unwrap().unwrap();
        assert_eq!(loaded, task);
    }

    #[tokio::test]
    async fn task_labels_json_roundtrip() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let mut task = sample_task(&project.id);
        task.labels = vec!["a".into(), "b".into(), "has spaces".into()];
        store.insert_task(&task).await.unwrap();

        let loaded = store.get_task(&task.id).await.unwrap().unwrap();
        assert_eq!(loaded.labels, vec!["a", "b", "has spaces"]);
    }

    #[tokio::test]
    async fn update_task_state() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let task = sample_task(&project.id);
        store.insert_task(&task).await.unwrap();

        store
            .update_task_state(&task.id, TaskState::Scouting)
            .await
            .unwrap();
        let loaded = store.get_task(&task.id).await.unwrap().unwrap();
        assert_eq!(loaded.state, TaskState::Scouting);
        assert!(loaded.updated_at >= task.updated_at);
    }

    #[tokio::test]
    async fn upsert_gh_issue_inserts_new_task() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let issue = GhIssue {
            number: 7,
            title: "Fix the thing".into(),
            body: "Details".into(),
            labels: vec!["bug".into()],
            state: GhState::Open,
            updated_at: Utc::now(),
        };
        let outcome = store
            .upsert_gh_issue(&project.id, issue.clone())
            .await
            .unwrap();
        assert!(outcome.is_new());
        let task = outcome.into_inner();
        assert_eq!(task.gh_issue_number, 7);
        assert_eq!(task.title, "Fix the thing");
        assert_eq!(task.state, TaskState::Backlog);
        assert_eq!(task.priority, 0);

        // Verify persistence
        let loaded = store.get_task(&task.id).await.unwrap().unwrap();
        assert_eq!(loaded.title, "Fix the thing");
    }

    #[tokio::test]
    async fn upsert_gh_issue_updates_existing_preserves_internal_fields() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let issue = GhIssue {
            number: 42,
            title: "First pass".into(),
            body: "Initial body".into(),
            labels: vec!["p1".into()],
            state: GhState::Open,
            updated_at: Utc::now(),
        };
        let first = store
            .upsert_gh_issue(&project.id, issue.clone())
            .await
            .unwrap();
        let first_task = first.clone().into_inner();

        // Advance our internal state to something non-default
        store
            .update_task_state(&first_task.id, TaskState::Scouting)
            .await
            .unwrap();

        // Re-observe with edits from GitHub
        let updated_issue = GhIssue {
            number: 42,
            title: "Revised title".into(),
            body: "Revised body".into(),
            labels: vec!["p0".into(), "bug".into()],
            state: GhState::Open,
            updated_at: Utc::now(),
        };
        let second = store
            .upsert_gh_issue(&project.id, updated_issue)
            .await
            .unwrap();
        assert!(!second.is_new(), "second upsert should be Existing");
        let second_task = second.into_inner();

        // Identity preserved
        assert_eq!(second_task.id, first_task.id);
        assert_eq!(second_task.ingested_at, first_task.ingested_at);
        assert_eq!(second_task.priority, first_task.priority);
        // Internal state not clobbered by GitHub re-observation
        assert_eq!(second_task.state, TaskState::Scouting);
        // External fields updated
        assert_eq!(second_task.title, "Revised title");
        assert_eq!(second_task.body, "Revised body");
        assert_eq!(second_task.labels, vec!["p0", "bug"]);
        assert!(second_task.updated_at > first_task.updated_at);
    }

    #[tokio::test]
    async fn update_task_state_not_found() {
        let store = Store::open_in_memory().await.unwrap();
        let err = store
            .update_task_state(&TaskId::new(), TaskState::Done)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn session_and_spec_chain() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let task = sample_task(&project.id);
        store.insert_task(&task).await.unwrap();

        let session = Session {
            id: SessionId::new(),
            task_id: task.id.clone(),
            vm_id: Some("vm-abc".into()),
            branch: "scout/42-xyz".into(),
            status: SessionStatus::Running,
            started_at: Utc::now(),
            completed_at: None,
            exit_reason: None,
            usage: None,
        };
        store.insert_session(&session).await.unwrap();

        let spec = Spec {
            id: SpecId::new(),
            session_id: session.id.clone(),
            task_id: task.id.clone(),
            content: "## Spec\nTODO".into(),
            complexity: Complexity::Medium,
            files_touched: vec!["src/lib.rs".into()],
            created_at: Utc::now(),
        };
        store.insert_spec(&spec).await.unwrap();

        let loaded_session = store.get_session(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded_session, session);

        let loaded_spec = store.get_spec(&spec.id).await.unwrap().unwrap();
        assert_eq!(loaded_spec, spec);
    }

    #[tokio::test]
    async fn spec_queue_upsert() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let task = sample_task(&project.id);
        store.insert_task(&task).await.unwrap();
        let session = Session {
            id: SessionId::new(),
            task_id: task.id.clone(),
            vm_id: None,
            branch: "scout/42-xyz".into(),
            status: SessionStatus::ScoutSucceeded,
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            exit_reason: None,
            usage: None,
        };
        store.insert_session(&session).await.unwrap();
        let spec = Spec {
            id: SpecId::new(),
            session_id: session.id.clone(),
            task_id: task.id.clone(),
            content: "## Spec".into(),
            complexity: Complexity::Simple,
            files_touched: vec![],
            created_at: Utc::now(),
        };
        store.insert_spec(&spec).await.unwrap();

        let entry = SpecQueueEntry {
            spec_id: spec.id.clone(),
            status: SpecQueueStatus::PendingReview,
            rank: None,
            approved_at: None,
            feedback: None,
            blocking_dependencies: vec![],
        };
        store.upsert_spec_queue_entry(&entry).await.unwrap();

        let loaded = store.get_spec_queue_entry(&spec.id).await.unwrap().unwrap();
        assert_eq!(loaded, entry);

        let updated = SpecQueueEntry {
            status: SpecQueueStatus::Approved,
            rank: Some(1),
            approved_at: Some(Utc::now()),
            feedback: Some("lgtm".into()),
            blocking_dependencies: vec![],
            ..entry
        };
        store.upsert_spec_queue_entry(&updated).await.unwrap();
        let loaded = store.get_spec_queue_entry(&spec.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, SpecQueueStatus::Approved);
        assert_eq!(loaded.rank, Some(1));
        assert_eq!(loaded.feedback, Some("lgtm".into()));
    }

    #[tokio::test]
    async fn mode_default_and_set() {
        let store = Store::open_in_memory().await.unwrap();
        assert_eq!(store.get_mode().await.unwrap(), Mode::Pause);

        store.set_mode(Mode::Play).await.unwrap();
        assert_eq!(store.get_mode().await.unwrap(), Mode::Play);

        store.set_mode(Mode::Stop).await.unwrap();
        assert_eq!(store.get_mode().await.unwrap(), Mode::Stop);
    }

    // --- queue ordering ---

    fn task_with(project_id: &ProjectId, number: u64, priority: i32) -> Task {
        Task {
            gh_issue_number: number,
            priority,
            ..sample_task(project_id)
        }
    }

    #[tokio::test]
    async fn list_tasks_manual_rank_beats_priority_and_nulls_sort_last() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let high = task_with(&project.id, 1, 100);
        let low = task_with(&project.id, 2, 1);
        let unranked = task_with(&project.id, 3, 50);
        for t in [&high, &low, &unranked] {
            store.insert_task(t).await.unwrap();
        }

        // Default order is priority desc
        let ids: Vec<_> = store
            .list_tasks()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(
            ids,
            vec![high.id.clone(), unranked.id.clone(), low.id.clone()]
        );

        store
            .set_queue_order(&[low.id.clone(), high.id.clone()])
            .await
            .unwrap();

        let tasks = store.list_tasks().await.unwrap();
        let ids: Vec<_> = tasks.iter().map(|t| t.id.clone()).collect();
        assert_eq!(
            ids,
            vec![low.id.clone(), high.id.clone(), unranked.id.clone()]
        );
        assert_eq!(tasks[0].manual_rank, Some(1));
        assert_eq!(tasks[1].manual_rank, Some(2));
        assert_eq!(tasks[2].manual_rank, None);
    }

    #[tokio::test]
    async fn set_queue_order_clears_ranks_of_omitted_tasks() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let a = task_with(&project.id, 1, 0);
        let b = task_with(&project.id, 2, 0);
        store.insert_task(&a).await.unwrap();
        store.insert_task(&b).await.unwrap();

        store
            .set_queue_order(&[a.id.clone(), b.id.clone()])
            .await
            .unwrap();
        store
            .set_queue_order(std::slice::from_ref(&b.id))
            .await
            .unwrap();

        assert_eq!(
            store.get_task(&a.id).await.unwrap().unwrap().manual_rank,
            None
        );
        assert_eq!(
            store.get_task(&b.id).await.unwrap().unwrap().manual_rank,
            Some(1)
        );
    }

    #[tokio::test]
    async fn set_queue_order_rejects_unknown_and_duplicate_ids() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let task = sample_task(&project.id);
        store.insert_task(&task).await.unwrap();

        let ghost = TaskId::new();
        let err = store
            .set_queue_order(&[task.id.clone(), ghost.clone()])
            .await
            .unwrap_err();
        match err {
            StoreError::NotFound(msg) => assert!(msg.contains(ghost.as_str())),
            other => panic!("expected NotFound, got {other:?}"),
        }
        // Nothing was applied
        assert_eq!(
            store.get_task(&task.id).await.unwrap().unwrap().manual_rank,
            None
        );

        let err = store
            .set_queue_order(&[task.id.clone(), task.id.clone()])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)));
    }

    #[tokio::test]
    async fn upsert_gh_issue_never_touches_manual_rank() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let issue = GhIssue {
            number: 9,
            title: "First".into(),
            body: "b".into(),
            labels: vec![],
            state: GhState::Open,
            updated_at: Utc::now(),
        };
        let task = store
            .upsert_gh_issue(&project.id, issue.clone())
            .await
            .unwrap()
            .into_inner();
        store
            .set_queue_order(std::slice::from_ref(&task.id))
            .await
            .unwrap();

        let repoll = GhIssue {
            title: "Retitled".into(),
            labels: vec!["p0".into()],
            ..issue
        };
        let observed = store
            .upsert_gh_issue(&project.id, repoll)
            .await
            .unwrap()
            .into_inner();
        assert_eq!(observed.manual_rank, Some(1));
        assert_eq!(
            store.get_task(&task.id).await.unwrap().unwrap().manual_rank,
            Some(1)
        );
    }

    // --- gh_state reconciliation ---

    /// A second project, so the bystander assertions are about a real neighbour
    /// rather than a hypothetical one.
    async fn second_project(store: &Store) -> Project {
        let project = Project {
            id: ProjectId::new(),
            repo_owner: "iamnbutler".into(),
            repo_name: "other".into(),
            added_at: Utc::now(),
        };
        store.insert_project(&project).await.unwrap();
        project
    }

    #[tokio::test]
    async fn reconcile_closed_issues_closes_only_the_absent_open_tasks() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let other = second_project(&store).await;

        let still_open = task_with(&project.id, 1, 0);
        let vanished = task_with(&project.id, 2, 0);
        let also_vanished = task_with(&project.id, 3, 0);
        let already_closed = Task {
            gh_state: GhState::Closed,
            ..task_with(&project.id, 4, 0)
        };
        // Same issue number, different repo: absence over there says nothing
        // about this project.
        let bystander = task_with(&other.id, 2, 0);
        for t in [
            &still_open,
            &vanished,
            &also_vanished,
            &already_closed,
            &bystander,
        ] {
            store.insert_task(t).await.unwrap();
        }

        let mut closed = store
            .reconcile_closed_issues(&project.id, &[1])
            .await
            .unwrap();
        closed.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let mut expected = vec![vanished.id.clone(), also_vanished.id.clone()];
        expected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(closed, expected, "exactly the absent open tasks");

        for id in [&vanished.id, &also_vanished.id] {
            assert_eq!(
                store.get_task(id).await.unwrap().unwrap().gh_state,
                GhState::Closed,
                "task {id}"
            );
        }
        assert_eq!(
            store
                .get_task(&still_open.id)
                .await
                .unwrap()
                .unwrap()
                .gh_state,
            GhState::Open
        );
        assert_eq!(
            store
                .get_task(&bystander.id)
                .await
                .unwrap()
                .unwrap()
                .gh_state,
            GhState::Open,
            "another project's rows are none of this project's business"
        );

        // An already-closed row is not reported a second time, and nothing about
        // it moves.
        let stored = store.get_task(&already_closed.id).await.unwrap().unwrap();
        assert_eq!(stored, already_closed);

        // An empty open set closes everything still open in the project — a real
        // repository with no open issues left.
        let closed = store
            .reconcile_closed_issues(&project.id, &[])
            .await
            .unwrap();
        assert_eq!(closed, vec![still_open.id.clone()]);
        assert!(
            store
                .reconcile_closed_issues(&project.id, &[])
                .await
                .unwrap()
                .is_empty(),
            "reconciliation is idempotent"
        );
    }

    #[tokio::test]
    async fn reconcile_closed_issues_leaves_tasks_owned_state_alone() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let task = task_with(&project.id, 7, 0);
        store.insert_task(&task).await.unwrap();
        store
            .update_task_state(&task.id, TaskState::Scouting)
            .await
            .unwrap();
        store.record_dispatch_failure(&task.id).await.unwrap();
        store
            .set_queue_order(std::slice::from_ref(&task.id))
            .await
            .unwrap();
        let before = store.get_task(&task.id).await.unwrap().unwrap();

        let closed = store
            .reconcile_closed_issues(&project.id, &[])
            .await
            .unwrap();
        assert_eq!(closed, vec![task.id.clone()]);

        let after = store.get_task(&task.id).await.unwrap().unwrap();
        assert_eq!(after.gh_state, GhState::Closed);
        assert!(after.updated_at >= before.updated_at, "timestamp refreshed");
        // Everything Tasks owns is untouched: a task mid-pipeline keeps flowing.
        assert_eq!(
            after,
            Task {
                gh_state: GhState::Closed,
                updated_at: after.updated_at,
                ..before
            }
        );
    }

    /// Reopening needs no code of its own: the issue is back in the open set, so
    /// the ordinary upsert path refreshes the snapshot.
    #[tokio::test]
    async fn a_reopened_issue_goes_back_to_open_through_upsert() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let issue = GhIssue {
            number: 3,
            title: "Round trip".into(),
            body: "b".into(),
            labels: vec![],
            state: GhState::Open,
            updated_at: Utc::now(),
        };
        let task = store
            .upsert_gh_issue(&project.id, issue.clone())
            .await
            .unwrap()
            .into_inner();

        assert_eq!(
            store
                .reconcile_closed_issues(&project.id, &[])
                .await
                .unwrap(),
            vec![task.id.clone()]
        );
        assert_eq!(
            store.get_task(&task.id).await.unwrap().unwrap().gh_state,
            GhState::Closed
        );

        let reopened = store
            .upsert_gh_issue(&project.id, issue)
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reopened.id, task.id, "same row, not a re-ingest");
        assert_eq!(reopened.gh_state, GhState::Open);
        assert_eq!(
            store.get_task(&task.id).await.unwrap().unwrap().gh_state,
            GhState::Open
        );
    }

    #[tokio::test]
    async fn list_active_tasks_hides_only_closed_intake() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let open_new = task_with(&project.id, 1, 0);
        let closed_new = Task {
            gh_state: GhState::Closed,
            ..task_with(&project.id, 2, 0)
        };
        let closed_in_flight = Task {
            gh_state: GhState::Closed,
            ..task_with(&project.id, 3, 0)
        };
        for t in [&open_new, &closed_new, &closed_in_flight] {
            store.insert_task(t).await.unwrap();
        }
        store
            .update_task_state(&closed_in_flight.id, TaskState::InReview)
            .await
            .unwrap();

        let visible: Vec<_> = store
            .list_active_tasks()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(visible.contains(&open_new.id));
        assert!(
            visible.contains(&closed_in_flight.id),
            "work already underway must not disappear"
        );
        assert!(!visible.contains(&closed_new.id));
        assert_eq!(
            store.list_tasks().await.unwrap().len(),
            3,
            "no rows deleted"
        );
    }

    // --- dispatch attempts ---

    #[tokio::test]
    async fn dispatch_failures_accumulate_and_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db");
        let task_id;

        {
            let store = Store::open(&path).await.unwrap();
            let project = sample_project();
            store.insert_project(&project).await.unwrap();
            let task = sample_task(&project.id);
            store.insert_task(&task).await.unwrap();
            task_id = task.id.clone();
            assert_eq!(
                store
                    .get_task(&task_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .dispatch_attempts,
                0
            );

            for expected in 1..=2 {
                assert_eq!(
                    store.record_dispatch_failure(&task_id).await.unwrap(),
                    expected
                );
            }
        }

        // A restart must not forgive the strikes — that is the whole point.
        let store = Store::open(&path).await.unwrap();
        assert_eq!(
            store
                .get_task(&task_id)
                .await
                .unwrap()
                .unwrap()
                .dispatch_attempts,
            2
        );
        assert_eq!(store.record_dispatch_failure(&task_id).await.unwrap(), 3);

        store.reset_dispatch_attempts(&task_id).await.unwrap();
        assert_eq!(
            store
                .get_task(&task_id)
                .await
                .unwrap()
                .unwrap()
                .dispatch_attempts,
            0
        );
        // Resetting an already-clean task is a no-op, not an error.
        store.reset_dispatch_attempts(&task_id).await.unwrap();
    }

    #[tokio::test]
    async fn dispatch_attempt_writes_reject_unknown_tasks() {
        let store = Store::open_in_memory().await.unwrap();
        let ghost = TaskId::new();
        assert!(matches!(
            store.record_dispatch_failure(&ghost).await.unwrap_err(),
            StoreError::NotFound(_)
        ));
        assert!(matches!(
            store.reset_dispatch_attempts(&ghost).await.unwrap_err(),
            StoreError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn upsert_gh_issue_never_touches_dispatch_attempts() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let issue = GhIssue {
            number: 11,
            title: "First".into(),
            body: "b".into(),
            labels: vec![],
            state: GhState::Open,
            updated_at: Utc::now(),
        };
        let task = store
            .upsert_gh_issue(&project.id, issue.clone())
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            task.dispatch_attempts, 0,
            "a fresh issue starts unpenalized"
        );
        store.record_dispatch_failure(&task.id).await.unwrap();
        store.record_dispatch_failure(&task.id).await.unwrap();

        let repoll = GhIssue {
            title: "Retitled".into(),
            body: "edited".into(),
            ..issue
        };
        let observed = store
            .upsert_gh_issue(&project.id, repoll)
            .await
            .unwrap()
            .into_inner();
        assert_eq!(observed.dispatch_attempts, 2);
        assert_eq!(
            store
                .get_task(&task.id)
                .await
                .unwrap()
                .unwrap()
                .dispatch_attempts,
            2
        );
    }

    // --- reconciliation ---

    #[tokio::test]
    async fn reconcile_orphaned_work_fails_sessions_and_requeues_tasks() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        // Orphan 1: a scouting task with the running session it died under.
        let orphaned = task_with(&project.id, 1, 0);
        store.insert_task(&orphaned).await.unwrap();
        store
            .update_task_state(&orphaned.id, TaskState::Scouting)
            .await
            .unwrap();
        let running = Session {
            id: SessionId::new(),
            task_id: orphaned.id.clone(),
            vm_id: Some("vm-gone".into()),
            branch: String::new(),
            status: SessionStatus::Running,
            started_at: Utc::now(),
            completed_at: None,
            exit_reason: None,
            usage: None,
        };
        store.insert_session(&running).await.unwrap();

        // Orphan 2: a scouting task with no session row at all.
        let sessionless = task_with(&project.id, 2, 0);
        store.insert_task(&sessionless).await.unwrap();
        store
            .update_task_state(&sessionless.id, TaskState::Scouting)
            .await
            .unwrap();

        // Bystanders: a finished session and a task that never left New.
        let (settled_task, _) = seed_spec(&store, 3).await;
        let untouched = task_with(&project.id, 4, 0);
        store.insert_task(&untouched).await.unwrap();

        // Seqs are a 1-based AUTOINCREMENT, so the next one is count + 1.
        let next_seq = store.all_events().await.unwrap().len() as i64 + 1;
        let report = store.reconcile_orphaned_work().await.unwrap();
        assert_eq!(
            report,
            ReconcileReport {
                sessions: 1,
                tasks: 2,
                builds: 0
            }
        );

        let session = store.get_session(&running.id).await.unwrap().unwrap();
        assert_eq!(session.status, SessionStatus::ScoutFailed);
        assert!(session.completed_at.is_some());
        assert_eq!(
            session.exit_reason.as_deref(),
            Some("orphaned by server restart")
        );

        for id in [&orphaned.id, &sessionless.id] {
            assert_eq!(
                store.get_task(id).await.unwrap().unwrap().state,
                TaskState::Queued,
                "task {id}"
            );
        }
        assert_eq!(
            store.get_task(&untouched.id).await.unwrap().unwrap().state,
            TaskState::Backlog
        );
        // A completed session and its task are none of reconciliation's business.
        assert_eq!(
            store
                .get_task(&settled_task.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            TaskState::Backlog
        );
        assert_eq!(
            store
                .list_sessions()
                .await
                .unwrap()
                .iter()
                .filter(|s| s.status == SessionStatus::ScoutSucceeded)
                .count(),
            1
        );

        let payloads: Vec<_> = store
            .events_since(next_seq, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.payload)
            .collect();
        assert!(payloads.contains(&EventPayload::SessionCompleted {
            session_id: running.id.clone(),
            task_id: orphaned.id.clone(),
            status: SessionStatus::ScoutFailed,
        }));
        for id in [&orphaned.id, &sessionless.id] {
            assert!(
                payloads.contains(&EventPayload::TaskStateChanged {
                    task_id: id.clone(),
                    from: TaskState::Scouting,
                    to: TaskState::Queued,
                }),
                "no TaskStateChanged for {id}"
            );
        }
        assert_eq!(payloads.len(), 3, "no events beyond the affected rows");
    }

    #[tokio::test]
    async fn reconcile_orphaned_work_is_a_no_op_on_a_clean_store() {
        let store = Store::open_in_memory().await.unwrap();
        seed_spec(&store, 1).await;
        let before = store.all_events().await.unwrap().len();

        let report = store.reconcile_orphaned_work().await.unwrap();
        assert!(report.is_empty());
        assert_eq!(report, ReconcileReport::default());
        assert_eq!(store.all_events().await.unwrap().len(), before);
    }

    #[tokio::test]
    async fn reconcile_orphaned_work_leaves_attempt_counts_alone() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let task = sample_task(&project.id);
        store.insert_task(&task).await.unwrap();
        store.record_dispatch_failure(&task.id).await.unwrap();
        store
            .update_task_state(&task.id, TaskState::Scouting)
            .await
            .unwrap();

        store.reconcile_orphaned_work().await.unwrap();

        // A crashed server is not the task's fault.
        assert_eq!(
            store
                .get_task(&task.id)
                .await
                .unwrap()
                .unwrap()
                .dispatch_attempts,
            1
        );
    }

    // --- spec review ---

    /// Insert a project + task + session + spec + pending queue entry.
    async fn seed_spec(store: &Store, number: u64) -> (Task, Spec) {
        let project = match store.list_projects().await.unwrap().into_iter().next() {
            Some(p) => p,
            None => {
                let p = sample_project();
                store.insert_project(&p).await.unwrap();
                p
            }
        };
        let task = task_with(&project.id, number, 0);
        store.insert_task(&task).await.unwrap();
        let session = Session {
            id: SessionId::new(),
            task_id: task.id.clone(),
            vm_id: None,
            branch: format!("scout/{number}"),
            status: SessionStatus::ScoutSucceeded,
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            exit_reason: None,
            usage: None,
        };
        store.insert_session(&session).await.unwrap();
        let spec = Spec {
            id: SpecId::new(),
            session_id: session.id.clone(),
            task_id: task.id.clone(),
            content: "## Spec".into(),
            complexity: Complexity::Simple,
            files_touched: vec![],
            created_at: Utc::now(),
        };
        store.insert_spec(&spec).await.unwrap();
        store
            .upsert_spec_queue_entry(&SpecQueueEntry {
                spec_id: spec.id.clone(),
                status: SpecQueueStatus::PendingReview,
                rank: None,
                approved_at: None,
                feedback: None,
                blocking_dependencies: vec![],
            })
            .await
            .unwrap();
        (task, spec)
    }

    #[tokio::test]
    async fn review_spec_approve_readies_task_for_build() {
        let store = Store::open_in_memory().await.unwrap();
        let (task, spec) = seed_spec(&store, 1).await;

        let entry = store
            .review_spec(&spec.id, SpecQueueStatus::Approved, Some("ship it".into()))
            .await
            .unwrap();
        assert_eq!(entry.status, SpecQueueStatus::Approved);
        assert!(entry.approved_at.is_some());
        assert_eq!(entry.feedback, Some("ship it".into()));

        let loaded = store.get_spec_queue_entry(&spec.id).await.unwrap().unwrap();
        assert_eq!(loaded, entry);
        assert_eq!(
            store.get_task(&task.id).await.unwrap().unwrap().state,
            TaskState::ReadyToBuild
        );
    }

    #[tokio::test]
    async fn review_spec_outcomes_drive_task_state() {
        let store = Store::open_in_memory().await.unwrap();

        let (rejected_task, rejected_spec) = seed_spec(&store, 1).await;
        store
            .review_spec(&rejected_spec.id, SpecQueueStatus::Rejected, None)
            .await
            .unwrap();
        assert_eq!(
            store
                .get_task(&rejected_task.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            TaskState::Rejected
        );

        let (revise_task, revise_spec) = seed_spec(&store, 2).await;
        store
            .update_task_state(&revise_task.id, TaskState::InReview)
            .await
            .unwrap();
        store
            .review_spec(
                &revise_spec.id,
                SpecQueueStatus::NeedsRevision,
                Some("more detail".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .get_task(&revise_task.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            TaskState::Queued
        );
    }

    #[tokio::test]
    async fn review_spec_rejects_non_outcome_status_and_unknown_spec() {
        let store = Store::open_in_memory().await.unwrap();
        let (_, spec) = seed_spec(&store, 1).await;

        for bad in [SpecQueueStatus::PendingReview, SpecQueueStatus::Blocked] {
            let err = store.review_spec(&spec.id, bad, None).await.unwrap_err();
            assert!(matches!(err, StoreError::Invalid(_)), "{bad:?}");
        }

        let err = store
            .review_spec(&SpecId::new(), SpecQueueStatus::Approved, None)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn review_spec_emits_events() {
        let store = Store::open_in_memory().await.unwrap();
        let (task, spec) = seed_spec(&store, 1).await;
        store
            .update_task_state(&task.id, TaskState::InReview)
            .await
            .unwrap();

        store
            .review_spec(&spec.id, SpecQueueStatus::Approved, None)
            .await
            .unwrap();

        let payloads: Vec<_> = store
            .all_events()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.payload)
            .collect();
        assert!(payloads.contains(&EventPayload::SpecQueueStatusChanged {
            spec_id: spec.id.clone(),
            from: Some(SpecQueueStatus::PendingReview),
            to: SpecQueueStatus::Approved,
        }));
        assert!(payloads.contains(&EventPayload::TaskStateChanged {
            task_id: task.id.clone(),
            from: TaskState::InReview,
            to: TaskState::ReadyToBuild,
        }));
    }

    #[tokio::test]
    async fn spec_queue_listing_and_reorder() {
        let store = Store::open_in_memory().await.unwrap();
        let (task_a, spec_a) = seed_spec(&store, 1).await;
        let (_, spec_b) = seed_spec(&store, 2).await;

        let queue = store.list_spec_queue().await.unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].entry.spec_id, spec_a.id);
        assert_eq!(queue[0].task_id, task_a.id);

        store
            .set_spec_queue_order(&[spec_b.id.clone(), spec_a.id.clone()])
            .await
            .unwrap();
        let queue = store.list_spec_queue().await.unwrap();
        assert_eq!(queue[0].entry.spec_id, spec_b.id);
        assert_eq!(queue[0].entry.rank, Some(1));
        assert_eq!(queue[1].entry.rank, Some(2));

        let err = store
            .set_spec_queue_order(&[SpecId::new()])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_sessions_and_specs() {
        let store = Store::open_in_memory().await.unwrap();
        seed_spec(&store, 1).await;
        seed_spec(&store, 2).await;

        assert_eq!(store.list_sessions().await.unwrap().len(), 2);
        assert_eq!(store.list_specs().await.unwrap().len(), 2);
    }

    // --- events ---

    #[tokio::test]
    async fn append_event_assigns_monotonic_seq() {
        let store = Store::open_in_memory().await.unwrap();
        let project_id = ProjectId::new();
        let task_id = TaskId::new();

        let e1 = store
            .append_event(EventPayload::ProjectAdded {
                project_id: project_id.clone(),
            })
            .await
            .unwrap();
        let e2 = store
            .append_event(EventPayload::TaskIngested {
                task_id: task_id.clone(),
                project_id: project_id.clone(),
            })
            .await
            .unwrap();

        assert!(e2.seq > e1.seq);
    }

    #[tokio::test]
    async fn append_event_payload_roundtrip() {
        let store = Store::open_in_memory().await.unwrap();
        let task_id = TaskId::new();
        let session_id = SessionId::new();

        let payload = EventPayload::SessionCompleted {
            session_id: session_id.clone(),
            task_id: task_id.clone(),
            status: SessionStatus::ScoutSucceeded,
        };
        let appended = store.append_event(payload.clone()).await.unwrap();

        let history = store.all_events().await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].seq, appended.seq);
        assert_eq!(history[0].payload, payload);
    }

    #[tokio::test]
    async fn subscribe_receives_live_events() {
        let store = Store::open_in_memory().await.unwrap();
        let mut rx = store.subscribe_events();

        let task_id = TaskId::new();
        store
            .append_event(EventPayload::TaskStateChanged {
                task_id: task_id.clone(),
                from: TaskState::Queued,
                to: TaskState::Scouting,
            })
            .await
            .unwrap();

        let event = rx.recv().await.unwrap();
        match event.payload {
            EventPayload::TaskStateChanged { to, .. } => {
                assert_eq!(to, TaskState::Scouting);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[tokio::test]
    async fn events_since_filters_by_seq() {
        let store = Store::open_in_memory().await.unwrap();

        let mut seqs = Vec::new();
        for _ in 0..5 {
            let e = store
                .append_event(EventPayload::Note {
                    source: "test".into(),
                    message: "hello".into(),
                })
                .await
                .unwrap();
            seqs.push(e.seq);
        }

        let mid = seqs[2];
        let tail = store.events_since(mid, i64::MAX).await.unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail.first().unwrap().seq, mid);
    }

    #[tokio::test]
    async fn events_since_bounds_the_read_by_limit() {
        let store = Store::open_in_memory().await.unwrap();
        for _ in 0..50 {
            store
                .append_event(EventPayload::Note {
                    source: "test".into(),
                    message: "hello".into(),
                })
                .await
                .unwrap();
        }

        // A page is exactly `limit` long and starts at `since`, inclusive.
        let page = store.events_since(1, 5).await.unwrap();
        assert_eq!(
            page.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );

        // Paging forward from the last seq + 1 continues where it left off.
        let next = store
            .events_since(page.last().unwrap().seq + 1, 5)
            .await
            .unwrap();
        assert_eq!(next.first().unwrap().seq, 6);

        // Asking for more than remains returns what's left.
        let tail = store.events_since(48, 100).await.unwrap();
        assert_eq!(
            tail.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![48, 49, 50]
        );

        // SQLite treats a negative LIMIT as unbounded; the store clamps so
        // neither 0 nor -1 can turn a page into a full-log read.
        assert!(store.events_since(1, 0).await.unwrap().is_empty());
        assert!(store.events_since(1, -1).await.unwrap().is_empty());

        // The unbounded read has its own name.
        assert_eq!(store.all_events().await.unwrap().len(), 50);
    }

    #[tokio::test]
    async fn recent_events_returns_last_n_in_order() {
        let store = Store::open_in_memory().await.unwrap();
        for i in 0..10 {
            store
                .append_event(EventPayload::Note {
                    source: "test".into(),
                    message: format!("{i}"),
                })
                .await
                .unwrap();
        }

        let recent = store.recent_events(3).await.unwrap();
        assert_eq!(recent.len(), 3);
        // Ordered ascending: the last 3 appended
        assert!(recent[0].seq < recent[1].seq);
        assert!(recent[1].seq < recent[2].seq);
        match &recent[2].payload {
            EventPayload::Note { message, .. } => assert_eq!(message, "9"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn events_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db");

        {
            let store = Store::open(&path).await.unwrap();
            store
                .append_event(EventPayload::Note {
                    source: "boot".into(),
                    message: "first".into(),
                })
                .await
                .unwrap();
        }

        let store = Store::open(&path).await.unwrap();
        let history = store.all_events().await.unwrap();
        assert_eq!(history.len(), 1);
        match &history[0].payload {
            EventPayload::Note { message, .. } => assert_eq!(message, "first"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // --- previous-attempt lookup (#760) ---

    #[tokio::test]
    async fn latest_reviewed_spec_returns_the_newest_verdict_for_that_task() {
        let store = Store::open_in_memory().await.unwrap();
        let (task, first) = seed_spec(&store, 1).await;

        // Pending review is not a verdict — nothing to replay yet.
        assert!(
            store
                .latest_reviewed_spec(&task.id)
                .await
                .unwrap()
                .is_none(),
            "a pending spec is not a reviewed one"
        );

        store
            .review_spec(
                &first.id,
                SpecQueueStatus::NeedsRevision,
                Some("fix it".to_string()),
            )
            .await
            .unwrap();
        let found = store.latest_reviewed_spec(&task.id).await.unwrap().unwrap();
        assert_eq!(found.spec.id, first.id);
        assert_eq!(found.status, SpecQueueStatus::NeedsRevision);
        assert_eq!(found.feedback.as_deref(), Some("fix it"));

        // A newer *unreviewed* spec must not displace the reviewed one.
        let session = Session {
            id: SessionId::new(),
            task_id: task.id.clone(),
            vm_id: None,
            branch: "scout/2".into(),
            status: SessionStatus::ScoutSucceeded,
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            exit_reason: None,
            usage: None,
        };
        store.insert_session(&session).await.unwrap();
        let second = Spec {
            id: SpecId::new(),
            session_id: session.id.clone(),
            task_id: task.id.clone(),
            content: "## Spec two".into(),
            complexity: Complexity::Simple,
            files_touched: vec![],
            created_at: Utc::now(),
        };
        store.insert_spec(&second).await.unwrap();
        store
            .upsert_spec_queue_entry(&SpecQueueEntry {
                spec_id: second.id.clone(),
                status: SpecQueueStatus::PendingReview,
                rank: None,
                approved_at: None,
                feedback: None,
                blocking_dependencies: vec![],
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .latest_reviewed_spec(&task.id)
                .await
                .unwrap()
                .unwrap()
                .spec
                .id,
            first.id,
            "an unreviewed newer spec must not displace the reviewed one"
        );

        // Once reviewed, the newer one wins.
        store
            .review_spec(
                &second.id,
                SpecQueueStatus::Rejected,
                Some("no".to_string()),
            )
            .await
            .unwrap();
        let found = store.latest_reviewed_spec(&task.id).await.unwrap().unwrap();
        assert_eq!(found.spec.id, second.id);
        assert_eq!(found.status, SpecQueueStatus::Rejected);
    }

    #[tokio::test]
    async fn latest_reviewed_spec_does_not_leak_across_tasks() {
        let store = Store::open_in_memory().await.unwrap();
        let (_task_a, spec_a) = seed_spec(&store, 1).await;
        let (task_b, _spec_b) = seed_spec(&store, 2).await;
        store
            .review_spec(
                &spec_a.id,
                SpecQueueStatus::NeedsRevision,
                Some("a only".to_string()),
            )
            .await
            .unwrap();
        assert!(
            store
                .latest_reviewed_spec(&task_b.id)
                .await
                .unwrap()
                .is_none(),
            "another task's review must not surface here"
        );
    }

    // --- transcripts (#759) ---

    #[tokio::test]
    async fn transcript_lines_get_dense_seqs_and_read_back_from_since() {
        let store = Store::open_in_memory().await.unwrap();
        let (task, _) = seed_spec(&store, 1).await;
        let session = Session {
            id: SessionId::new(),
            task_id: task.id.clone(),
            vm_id: None,
            branch: String::new(),
            status: SessionStatus::Running,
            started_at: Utc::now(),
            completed_at: None,
            exit_reason: None,
            usage: None,
        };
        store.insert_session(&session).await.unwrap();

        let first = store
            .append_transcript_lines(
                &session.id,
                &[
                    (TranscriptStream::Stdout, "one".into()),
                    (TranscriptStream::Stderr, "two".into()),
                ],
            )
            .await
            .unwrap();
        assert_eq!(first.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![1, 2]);

        // A second batch continues the sequence rather than restarting it.
        let second = store
            .append_transcript_lines(&session.id, &[(TranscriptStream::Stdout, "three".into())])
            .await
            .unwrap();
        assert_eq!(second[0].seq, 3);

        let all = store.transcript_since(&session.id, 0, 100).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[1].stream, TranscriptStream::Stderr);

        // `since` is inclusive, matching /events?since=.
        let tail = store.transcript_since(&session.id, 2, 100).await.unwrap();
        assert_eq!(tail.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![2, 3]);

        let capped = store.transcript_since(&session.id, 0, 2).await.unwrap();
        assert_eq!(capped.len(), 2);
    }

    #[tokio::test]
    async fn appending_transcript_lines_notifies_subscribers_after_commit() {
        let store = Store::open_in_memory().await.unwrap();
        let (task, _) = seed_spec(&store, 1).await;
        let session = Session {
            id: SessionId::new(),
            task_id: task.id.clone(),
            vm_id: None,
            branch: String::new(),
            status: SessionStatus::Running,
            started_at: Utc::now(),
            completed_at: None,
            exit_reason: None,
            usage: None,
        };
        store.insert_session(&session).await.unwrap();

        let mut rx = store.subscribe_transcript();
        store
            .append_transcript_lines(&session.id, &[(TranscriptStream::Stdout, "hello".into())])
            .await
            .unwrap();

        let line = rx.try_recv().expect("broadcast after commit");
        assert_eq!(line.line, "hello");
        assert_eq!(line.seq, 1);
        // Anything announced on the channel must already be readable.
        assert_eq!(
            store
                .transcript_since(&session.id, line.seq, 10)
                .await
                .unwrap()[0]
                .line,
            "hello"
        );
    }

    #[tokio::test]
    async fn session_usage_round_trips() {
        let store = Store::open_in_memory().await.unwrap();
        let (task, _) = seed_spec(&store, 1).await;
        let session = Session {
            id: SessionId::new(),
            task_id: task.id.clone(),
            vm_id: None,
            branch: String::new(),
            status: SessionStatus::Running,
            started_at: Utc::now(),
            completed_at: None,
            exit_reason: None,
            usage: None,
        };
        store.insert_session(&session).await.unwrap();
        assert!(
            store
                .get_session(&session.id)
                .await
                .unwrap()
                .unwrap()
                .usage
                .is_none()
        );

        let usage = SessionUsage {
            input_tokens: Some(1200),
            output_tokens: Some(340),
            total_cost_usd: Some(0.0421),
            ..Default::default()
        };
        store
            .update_session_usage(&session.id, &usage)
            .await
            .unwrap();
        let back = store.get_session(&session.id).await.unwrap().unwrap();
        assert_eq!(back.usage, Some(usage));
    }

    #[tokio::test]
    async fn retirable_listing_is_closed_and_picked_up_only() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let mut wanted = Vec::new();
        for (number, state, gh_state) in [
            (1, TaskState::Queued, GhState::Closed),
            (2, TaskState::InReview, GhState::Closed),
            (3, TaskState::ReadyToBuild, GhState::Closed),
            // Not candidates: still open, never picked up, mid-scout, or done.
            (4, TaskState::Queued, GhState::Open),
            (5, TaskState::Backlog, GhState::Closed),
            (6, TaskState::Scouting, GhState::Closed),
            (7, TaskState::Done, GhState::Closed),
        ] {
            let mut task = sample_task(&project.id);
            task.gh_issue_number = number;
            task.state = state;
            task.gh_state = gh_state;
            store.insert_task(&task).await.unwrap();
            if number <= 3 {
                wanted.push(task.id);
            }
        }

        let mut got: Vec<TaskId> = store
            .list_retirable_tasks(&project.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        got.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        wanted.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(got, wanted);
    }

    #[tokio::test]
    async fn retire_task_concludes_the_work_and_frees_its_queue_slot() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let mut task = sample_task(&project.id);
        task.state = TaskState::Queued;
        task.gh_state = GhState::Closed;
        task.manual_rank = Some(3);
        store.insert_task(&task).await.unwrap();

        let retired = store
            .retire_task(&task.id, TaskState::Done)
            .await
            .unwrap()
            .expect("task was retirable");
        assert_eq!(retired.state, TaskState::Done);
        assert_eq!(retired.manual_rank, None);

        let payloads: Vec<EventPayload> = store
            .all_events()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.payload)
            .collect();
        assert!(payloads.contains(&EventPayload::TaskStateChanged {
            task_id: task.id.clone(),
            from: TaskState::Queued,
            to: TaskState::Done,
        }));
    }

    /// Retirement is a no-op (not an error) when the decision-time re-check
    /// fails: the issue reopened, or the state moved on.
    #[tokio::test]
    async fn retire_task_declines_when_no_longer_a_candidate() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();

        let mut reopened = sample_task(&project.id);
        reopened.state = TaskState::InReview;
        reopened.gh_state = GhState::Open;
        store.insert_task(&reopened).await.unwrap();
        assert!(
            store
                .retire_task(&reopened.id, TaskState::Done)
                .await
                .unwrap()
                .is_none()
        );

        let mut scouting = sample_task(&project.id);
        scouting.gh_issue_number = 43;
        scouting.state = TaskState::Scouting;
        scouting.gh_state = GhState::Closed;
        store.insert_task(&scouting).await.unwrap();
        assert!(
            store
                .retire_task(&scouting.id, TaskState::Rejected)
                .await
                .unwrap()
                .is_none()
        );

        // Neither attempt left a trace.
        assert!(store.all_events().await.unwrap().is_empty());

        let err = store
            .retire_task(&reopened.id, TaskState::Backlog)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)));
    }

    /// Insert the full chain a build consumes: a ready_to_build task, its
    /// session, its spec, and an approved queue entry.
    async fn approved_spec(store: &Store, project: &Project, issue: u64) -> (Task, Spec) {
        let mut task = sample_task(&project.id);
        task.gh_issue_number = issue;
        task.state = TaskState::ReadyToBuild;
        store.insert_task(&task).await.unwrap();

        let session = Session {
            id: SessionId::new(),
            task_id: task.id.clone(),
            vm_id: None,
            branch: format!("scout/{}", task.id),
            status: SessionStatus::ScoutSucceeded,
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            exit_reason: None,
            usage: None,
        };
        store.insert_session(&session).await.unwrap();

        let spec = Spec {
            id: SpecId::new(),
            session_id: session.id,
            task_id: task.id.clone(),
            content: format!("## Spec: issue {issue}"),
            complexity: Complexity::Simple,
            files_touched: vec![],
            created_at: Utc::now(),
        };
        store.insert_spec(&spec).await.unwrap();
        store
            .upsert_spec_queue_entry(&SpecQueueEntry {
                spec_id: spec.id.clone(),
                status: SpecQueueStatus::Approved,
                rank: None,
                approved_at: Some(Utc::now()),
                feedback: None,
                blocking_dependencies: vec![],
            })
            .await
            .unwrap();
        (task, spec)
    }

    #[tokio::test]
    async fn create_build_validates_the_batch() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let (_task, spec) = approved_spec(&store, &project, 1).await;

        // Empty and duplicated sets.
        assert!(matches!(
            store.create_build(&[], "main").await.unwrap_err(),
            StoreError::Invalid(_)
        ));
        assert!(matches!(
            store
                .create_build(&[spec.id.clone(), spec.id.clone()], "main")
                .await
                .unwrap_err(),
            StoreError::Invalid(_)
        ));

        // A non-approved spec.
        let (_t2, pending) = approved_spec(&store, &project, 2).await;
        store
            .review_spec(&pending.id, SpecQueueStatus::Rejected, None)
            .await
            .unwrap();
        let err = store
            .create_build(std::slice::from_ref(&pending.id), "main")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("rejected"), "{err}");

        // A spec already in an active build.
        let build = store
            .create_build(std::slice::from_ref(&spec.id), "main")
            .await
            .unwrap();
        assert_eq!(build.status, BuildStatus::Queued);
        assert_eq!(build.branch, format!("build/{}", build.id));
        let err = store
            .create_build(std::slice::from_ref(&spec.id), "main")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("already part of"), "{err}");
    }

    #[tokio::test]
    async fn create_build_resorts_the_batch_into_spec_queue_order() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let (_ta, spec_a) = approved_spec(&store, &project, 1).await;
        let (_tb, spec_b) = approved_spec(&store, &project, 2).await;
        // Rank b above a; the human's order wins over the caller's.
        store
            .set_spec_queue_order(&[spec_b.id.clone(), spec_a.id.clone()])
            .await
            .unwrap();

        let build = store
            .create_build(&[spec_a.id.clone(), spec_b.id.clone()], "main")
            .await
            .unwrap();
        assert_eq!(
            store.build_spec_ids(&build.id).await.unwrap(),
            vec![spec_b.id, spec_a.id]
        );
    }

    #[tokio::test]
    async fn claiming_is_serial_by_construction() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let (task_a, spec_a) = approved_spec(&store, &project, 1).await;
        let (_tb, spec_b) = approved_spec(&store, &project, 2).await;

        let first = store
            .create_build(std::slice::from_ref(&spec_a.id), "main")
            .await
            .unwrap();
        let _second = store
            .create_build(std::slice::from_ref(&spec_b.id), "main")
            .await
            .unwrap();

        let claimed = store.claim_next_queued_build().await.unwrap().unwrap();
        assert_eq!(claimed.id, first.id, "oldest first");
        assert_eq!(claimed.status, BuildStatus::Running);
        // Its task went building; the second build cannot be claimed.
        assert_eq!(
            store.get_task(&task_a.id).await.unwrap().unwrap().state,
            TaskState::Building
        );
        assert!(
            store.claim_next_queued_build().await.unwrap().is_none(),
            "one at a time"
        );

        // Failure returns the task to ready_to_build, spec stays approved,
        // and the queue unblocks.
        store
            .finalize_build_failed(&claimed.id, "agent produced no commits")
            .await
            .unwrap();
        assert_eq!(
            store.get_task(&task_a.id).await.unwrap().unwrap().state,
            TaskState::ReadyToBuild
        );
        assert_eq!(
            store
                .get_spec_queue_entry(&spec_a.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SpecQueueStatus::Approved
        );
        let next = store.claim_next_queued_build().await.unwrap().unwrap();
        assert_eq!(next.status, BuildStatus::Running);
    }

    #[tokio::test]
    async fn a_successful_build_drains_the_batch() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let (task, spec) = approved_spec(&store, &project, 1).await;
        let build = store
            .create_build(std::slice::from_ref(&spec.id), "main")
            .await
            .unwrap();
        store.claim_next_queued_build().await.unwrap().unwrap();

        let done = store
            .finalize_build_succeeded(
                &build.id,
                "headsha123",
                77,
                Some("Did the thing."),
                &["src/lib.rs".to_string()],
            )
            .await
            .unwrap();
        assert_eq!(done.status, BuildStatus::Succeeded);
        assert_eq!(done.pr_number, Some(77));
        assert_eq!(done.head_sha.as_deref(), Some("headsha123"));

        // Spec drained, task concluded, and neither can be re-consumed.
        assert_eq!(
            store
                .get_spec_queue_entry(&spec.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SpecQueueStatus::Built
        );
        assert_eq!(
            store.get_task(&task.id).await.unwrap().unwrap().state,
            TaskState::Done
        );
        let err = store
            .create_build(std::slice::from_ref(&spec.id), "main")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("built"), "{err}");

        // Built is not a verdict a reviewer can render.
        let err = store
            .review_spec(&spec.id, SpecQueueStatus::Built, None)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)));

        let payloads: Vec<EventPayload> = store
            .all_events()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.payload)
            .collect();
        assert!(payloads.contains(&EventPayload::PullRequestOpened {
            build_id: build.id.clone(),
            pr_number: 77,
        }));
        assert!(payloads.contains(&EventPayload::BuildCompleted {
            build_id: build.id,
            status: BuildStatus::Succeeded,
        }));
    }

    #[tokio::test]
    async fn reconcile_fails_orphaned_running_builds_but_keeps_queued_ones() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let (task_a, spec_a) = approved_spec(&store, &project, 1).await;
        let (_tb, spec_b) = approved_spec(&store, &project, 2).await;
        let running = store.create_build(&[spec_a.id], "main").await.unwrap();
        let queued = store.create_build(&[spec_b.id], "main").await.unwrap();
        store.claim_next_queued_build().await.unwrap().unwrap();

        let report = store.reconcile_orphaned_work().await.unwrap();
        assert_eq!(report.builds, 1);

        let after = store.get_build(&running.id).await.unwrap().unwrap();
        assert_eq!(after.status, BuildStatus::Failed);
        assert_eq!(
            after.exit_reason.as_deref(),
            Some("orphaned by server restart")
        );
        assert_eq!(
            store.get_task(&task_a.id).await.unwrap().unwrap().state,
            TaskState::ReadyToBuild
        );
        // Queued builds are durable intent: untouched, claimable now.
        assert_eq!(
            store.get_build(&queued.id).await.unwrap().unwrap().status,
            BuildStatus::Queued
        );
        assert_eq!(
            store.claim_next_queued_build().await.unwrap().unwrap().id,
            queued.id
        );
    }

    /// A closed issue's approved-but-unbuilt spec must not linger where
    /// `create_build` would consume it.
    #[tokio::test]
    async fn retiring_a_task_drains_its_unconsumed_specs() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let (task, spec) = approved_spec(&store, &project, 1).await;
        store
            .reconcile_closed_issues(&project.id, &[])
            .await
            .unwrap();

        let retired = store
            .retire_task(&task.id, TaskState::Done)
            .await
            .unwrap()
            .expect("retirable");
        assert_eq!(retired.state, TaskState::Done);
        assert_eq!(
            store
                .get_spec_queue_entry(&spec.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SpecQueueStatus::Rejected
        );
        let err = store.create_build(&[spec.id], "main").await.unwrap_err();
        assert!(format!("{err}").contains("rejected"), "{err}");
    }

    /// Rows retired before the inline drain existed (or via any future gap)
    /// are healed at startup: no live queue entry may belong to a concluded
    /// task.
    #[tokio::test]
    async fn startup_reconcile_drains_specs_of_concluded_tasks() {
        let store = Store::open_in_memory().await.unwrap();
        let project = sample_project();
        store.insert_project(&project).await.unwrap();
        let (task, spec) = approved_spec(&store, &project, 1).await;
        // Conclude the task behind the queue's back (as pre-drain retirement did).
        store
            .update_task_state(&task.id, TaskState::Done)
            .await
            .unwrap();

        store.reconcile_orphaned_work().await.unwrap();
        assert_eq!(
            store
                .get_spec_queue_entry(&spec.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SpecQueueStatus::Rejected
        );

        // A live task's approved spec is untouched.
        let (_t2, live) = approved_spec(&store, &project, 2).await;
        store.reconcile_orphaned_work().await.unwrap();
        assert_eq!(
            store
                .get_spec_queue_entry(&live.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SpecQueueStatus::Approved
        );
    }
}
