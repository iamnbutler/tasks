//! SQLite-backed persistence.

use std::path::Path;

use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use thiserror::Error;
use tokio::sync::broadcast;

use crate::events::{Event, EventPayload};
use crate::models::{
    Complexity, GhState, Mode, Project, ProjectId, Session, SessionId, SessionStatus, Spec,
    SpecId, SpecQueueEntry, SpecQueueStatus, Task, TaskId, TaskState,
};

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
        let pool = SqlitePoolOptions::new().max_connections(8).connect(&url).await?;
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
        let row = sqlx::query(
            "SELECT id, repo_owner, repo_name, added_at FROM projects WHERE id = ?",
        )
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
             gh_state, state, priority, ingested_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(task.ingested_at.to_rfc3339())
        .bind(task.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_task(&self, id: &TaskId) -> Result<Option<Task>, StoreError> {
        let row = sqlx::query(
            "SELECT id, project_id, gh_issue_number, title, body, labels, gh_state, \
             state, priority, ingested_at, updated_at FROM tasks WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(task_from_row).transpose()
    }

    pub async fn list_tasks(&self) -> Result<Vec<Task>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, project_id, gh_issue_number, title, body, labels, gh_state, \
             state, priority, ingested_at, updated_at FROM tasks ORDER BY ingested_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(task_from_row).collect()
    }

    pub async fn update_task_state(
        &self,
        id: &TaskId,
        state: TaskState,
    ) -> Result<(), StoreError> {
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

    // --- spec queue ---

    pub async fn upsert_spec_queue_entry(
        &self,
        entry: &SpecQueueEntry,
    ) -> Result<(), StoreError> {
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
    pub async fn append_event(
        &self,
        payload: EventPayload,
    ) -> Result<Event, StoreError> {
        let timestamp = Utc::now();
        let payload_json = serde_json::to_string(&payload)?;

        let row = sqlx::query(
            "INSERT INTO events (timestamp, payload) VALUES (?, ?) RETURNING seq",
        )
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
        let rows = sqlx::query(
            "SELECT seq, timestamp, payload FROM events WHERE seq >= ? ORDER BY seq",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(event_from_row).collect()
    }

    /// Return the last N events, ordered by seq ascending.
    pub async fn recent_events(&self, limit: i64) -> Result<Vec<Event>, StoreError> {
        let mut rows = sqlx::query(
            "SELECT seq, timestamp, payload FROM events ORDER BY seq DESC LIMIT ?",
        )
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

fn spec_queue_entry_from_row(
    row: sqlx::sqlite::SqliteRow,
) -> Result<SpecQueueEntry, StoreError> {
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
