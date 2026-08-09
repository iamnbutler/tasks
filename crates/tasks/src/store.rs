//! SQLite-backed persistence.

use std::path::Path;

use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use thiserror::Error;
use tokio::sync::broadcast;

use crate::events::{Event, EventPayload};
use crate::github::GhIssue;
use crate::models::{
    Complexity, GhState, Mode, Project, ProjectId, Session, SessionId, SessionStatus, Spec, SpecId,
    SpecQueueEntry, SpecQueueItem, SpecQueueStatus, Task, TaskId, TaskState,
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
}

impl Store {
    /// Open (creating if necessary) a SQLite database at the given path and run migrations.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect(&url)
            .await?;
        MIGRATOR.run(&pool).await?;
        let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        Ok(Self { pool, event_tx })
    }

    /// Open an in-memory database (useful for tests).
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1) // shared in-memory DB — one connection
            .connect("sqlite::memory:")
            .await?;
        MIGRATOR.run(&pool).await?;
        let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        Ok(Self { pool, event_tx })
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
             gh_state, state, priority, manual_rank, ingested_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(task.ingested_at.to_rfc3339())
        .bind(task.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_task(&self, id: &TaskId) -> Result<Option<Task>, StoreError> {
        let row = sqlx::query(
            "SELECT id, project_id, gh_issue_number, title, body, labels, gh_state, \
             state, priority, manual_rank, ingested_at, updated_at FROM tasks WHERE id = ?",
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
             state, priority, manual_rank, ingested_at, updated_at FROM tasks \
             ORDER BY manual_rank IS NULL, manual_rank, priority DESC, ingested_at",
        )
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
             state, priority, manual_rank, ingested_at, updated_at FROM tasks \
             WHERE project_id = ? AND gh_issue_number = ?",
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
            state: TaskState::New,
            priority: 0,
            manual_rank: None,
            ingested_at: now,
            updated_at: now,
        };
        self.insert_task(&task).await?;
        Ok(UpsertOutcome::Inserted(task))
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
             exit_reason FROM sessions WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(session_from_row).transpose()
    }

    pub async fn list_sessions(&self) -> Result<Vec<Session>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, task_id, vm_id, branch, status, started_at, completed_at, \
             exit_reason FROM sessions ORDER BY started_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(session_from_row).collect()
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
    /// Task side effects: approved → `Queued`, rejected → `Rejected`,
    /// needs revision → back to `New` so it can be scouted again.
    pub async fn review_spec(
        &self,
        spec_id: &SpecId,
        status: SpecQueueStatus,
        feedback: Option<String>,
    ) -> Result<SpecQueueEntry, StoreError> {
        let next_task_state = match status {
            SpecQueueStatus::Approved => TaskState::Queued,
            SpecQueueStatus::Rejected => TaskState::Rejected,
            SpecQueueStatus::NeedsRevision => TaskState::New,
            SpecQueueStatus::PendingReview | SpecQueueStatus::Blocked => {
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

    /// Return all events with seq >= `since`, ordered by seq ascending.
    pub async fn events_since(&self, since: i64) -> Result<Vec<Event>, StoreError> {
        let rows =
            sqlx::query("SELECT seq, timestamp, payload FROM events WHERE seq >= ? ORDER BY seq")
                .bind(since)
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter().map(event_from_row).collect()
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
            state: TaskState::New,
            priority: 10,
            manual_rank: None,
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
        assert_eq!(task.state, TaskState::New);
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
    async fn review_spec_approve_queues_task() {
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
            TaskState::Queued
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
            .update_task_state(&revise_task.id, TaskState::SpecReady)
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
            TaskState::New
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
            .review_spec(&spec.id, SpecQueueStatus::Approved, None)
            .await
            .unwrap();

        let payloads: Vec<_> = store
            .events_since(0)
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
            from: TaskState::New,
            to: TaskState::Queued,
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

        let history = store.events_since(0).await.unwrap();
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
                from: TaskState::New,
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
        let tail = store.events_since(mid).await.unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail.first().unwrap().seq, mid);
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
        let history = store.events_since(0).await.unwrap();
        assert_eq!(history.len(), 1);
        match &history[0].payload {
            EventPayload::Note { message, .. } => assert_eq!(message, "first"),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
