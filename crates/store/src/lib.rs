//! Persistent storage for the Tasks platform (spec §3.5).
//!
//! Wraps SQLite via rusqlite. Exposes typed CRUD methods — no SQL
//! leaks into other crates. The implementation can be swapped without
//! affecting consumers.

mod accounting;
mod schema;

pub use accounting::{AccountingSummary, TaskAccounting};
pub use schema::DATA_VERSION;

use std::path::Path;

use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use models::automation::{Automation, AutomationRun, AutomationState, RunStatus, TriggerType};
use models::merge_queue::{MergeQueueEntry, MergeStatus};
use models::project::Project;
use models::task::{FailureInfo, Task, TaskSource, TaskState};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("pool error: {0}")]
    Pool(#[from] r2d2::Error),
}

/// Persistent storage backed by SQLite (spec §3.5).
///
/// Uses a connection pool (r2d2) with WAL mode so reads don't block
/// each other and aren't blocked by writes. Thread-safe without
/// external synchronization.
pub struct Store {
    pool: Pool<SqliteConnectionManager>,
}

impl Store {
    /// Check the stored data version without fully initializing the schema.
    ///
    /// Returns `Ok(None)` if the database is new (version 0).
    /// Returns `Ok(Some((stored, expected)))` if there is a mismatch.
    /// Returns `Ok(None)` if versions match.
    pub fn check_version(path: impl AsRef<Path>) -> Result<Option<(u32, u32)>, StoreError> {
        let conn = rusqlite::Connection::open(path)?;
        let stored = schema::read_version(&conn)?;
        if stored == 0 || stored == schema::DATA_VERSION {
            Ok(None)
        } else {
            Ok(Some((stored, schema::DATA_VERSION)))
        }
    }

    /// Delete the database file and event log directory, then open a fresh store.
    ///
    /// Returns an error if the primary database file cannot be removed.
    /// WAL/SHM files and the events directory are removed on a best-effort basis.
    pub fn clear_and_reopen(db_path: impl AsRef<Path>, data_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let db = db_path.as_ref();
        let data = data_dir.as_ref();

        // Remove the primary SQLite database file (propagate errors)
        if db.exists() {
            std::fs::remove_file(db)?;
        }

        // Remove WAL/SHM files on best-effort basis (auxiliary files)
        for ext in &["-wal", "-shm"] {
            let mut p = db.as_os_str().to_owned();
            p.push(ext);
            let path = std::path::Path::new(&p);
            if path.exists() {
                if let Err(e) = std::fs::remove_file(path) {
                    tracing::warn!(path = ?path, error = %e, "failed to remove auxiliary db file");
                }
            }
        }

        // Remove the event log directory on best-effort basis
        let events_dir = data.join("events");
        if events_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&events_dir) {
                tracing::warn!(path = ?events_dir, error = %e, "failed to remove events directory");
            }
        }

        tracing::info!("cleared local data for version upgrade");

        Self::open(db)
    }

    /// Build a connection pool for the given SQLiteConnectionManager.
    fn build_pool(manager: SqliteConnectionManager) -> Result<Pool<SqliteConnectionManager>, StoreError> {
        let pool = Pool::builder()
            .max_size(8)
            .build(manager)?;
        // Initialize schema using a single connection from the pool.
        let conn = pool.get()?;
        schema::initialize(&conn)?;
        Ok(pool)
    }

    /// Open or create a store at the given path.
    ///
    /// Version checking should be done before calling this method using
    /// [`Store::check_version`] in `main.rs`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let manager = SqliteConnectionManager::file(path.as_ref())
            .with_init(|conn| {
                conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
            });
        let pool = Self::build_pool(manager)?;
        Ok(Self { pool })
    }

    /// Open an in-memory store (for testing).
    pub fn open_memory() -> Result<Self, StoreError> {
        let manager = SqliteConnectionManager::memory()
            .with_init(|conn| {
                conn.execute_batch("PRAGMA foreign_keys=ON;")
            });
        // In-memory databases are per-connection, so use a single connection
        // to ensure all operations share the same database.
        let pool = Pool::builder()
            .max_size(1)
            .build(manager)?;
        let conn = pool.get()?;
        schema::initialize(&conn)?;
        Ok(Self { pool })
    }

    /// Get a connection from the pool.
    fn conn(&self) -> Result<r2d2::PooledConnection<SqliteConnectionManager>, StoreError> {
        Ok(self.pool.get()?)
    }

    /// Insert or replace a project.
    pub fn save_project(&self, project: &Project) -> Result<(), StoreError> {
        let config = serde_json::to_string(&project.config)?;
        self.conn()?.execute(
            "INSERT OR REPLACE INTO projects (id, repo, default_branch, config) VALUES (?1, ?2, ?3, ?4)",
            params![project.id, project.repo, project.default_branch, config],
        )?;
        Ok(())
    }

    /// Get a project by ID.
    pub fn get_project(&self, id: &str) -> Result<Option<Project>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare("SELECT id, repo, default_branch, config FROM projects WHERE id = ?1")?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        match rows.next() {
            Some(row) => {
                let (id, repo, default_branch, config_str) = row?;
                let config: serde_json::Value = serde_json::from_str(&config_str)?;
                Ok(Some(Project {
                    id,
                    repo,
                    default_branch,
                    config,
                }))
            }
            None => Ok(None),
        }
    }

    /// List all projects.
    pub fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare("SELECT id, repo, default_branch, config FROM projects")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut projects = Vec::new();
        for row in rows {
            let (id, repo, default_branch, config_str) = row?;
            let config: serde_json::Value = serde_json::from_str(&config_str)?;
            projects.push(Project {
                id,
                repo,
                default_branch,
                config,
            });
        }
        Ok(projects)
    }

    /// Delete a project by ID. Returns true if a row was deleted.
    pub fn delete_project(&self, id: &str) -> Result<bool, StoreError> {
        let affected = self
            .conn()?
            .execute("DELETE FROM projects WHERE id = ?1", params![id])?;
        Ok(affected > 0)
    }

    /// Get the last polled timestamp for a project (poller high-water mark).
    ///
    /// Returns `None` if the project doesn't exist or hasn't been polled yet.
    /// Used to initialize the poller after server restarts (spec github.md §5.3).
    pub fn get_last_polled_at(&self, id: &str) -> Result<Option<DateTime<Utc>>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare("SELECT last_polled_at FROM projects WHERE id = ?1")?;
        let mut rows = stmt.query_map(params![id], |row| row.get::<_, Option<String>>(0))?;
        match rows.next() {
            Some(row) => {
                let ts_str: Option<String> = row?;
                match ts_str {
                    Some(s) => {
                        let ts: DateTime<Utc> = s.parse().map_err(|e: chrono::ParseError| {
                            serde_json::from_str::<()>(&e.to_string()).unwrap_err()
                        })?;
                        Ok(Some(ts))
                    }
                    None => Ok(None),
                }
            }
            None => Ok(None),
        }
    }

    /// Set the last polled timestamp for a project (poller high-water mark).
    ///
    /// Called after each successful poll to persist the high-water mark.
    /// On restart, this value is used to avoid re-fetching all open items.
    pub fn set_last_polled_at(&self, id: &str, timestamp: DateTime<Utc>) -> Result<(), StoreError> {
        let ts_str = timestamp.to_rfc3339();
        self.conn()?.execute(
            "UPDATE projects SET last_polled_at = ?1 WHERE id = ?2",
            params![ts_str, id],
        )?;
        Ok(())
    }

    // ── Merge queue ──────────────────────────────────────────────

    /// Insert or replace a merge queue entry.
    pub fn save_merge_entry(&self, entry: &MergeQueueEntry) -> Result<(), StoreError> {
        let status = serde_json::to_value(entry.status)?
            .as_str()
            .unwrap()
            .to_string();
        let queued_at = entry.queued_at.to_rfc3339();
        self.conn()?.execute(
            "INSERT INTO merge_queue (id, task_id, pr_url, status, queued_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET task_id=excluded.task_id, pr_url=excluded.pr_url, status=excluded.status, queued_at=excluded.queued_at",
            params![entry.id, entry.task_id, entry.pr_url, status, queued_at],
        )?;
        Ok(())
    }

    /// Get a merge queue entry by ID.
    pub fn get_merge_entry(&self, id: &str) -> Result<Option<MergeQueueEntry>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, task_id, pr_url, status, queued_at FROM merge_queue WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        match rows.next() {
            Some(row) => {
                let (id, task_id, pr_url, status_str, queued_at_str) = row?;
                let status: MergeStatus = serde_json::from_str(&format!("\"{status_str}\""))?;
                let queued_at: DateTime<Utc> = queued_at_str
                    .parse()
                    .map_err(|e: chrono::ParseError| {
                        serde_json::from_str::<()>(&e.to_string()).unwrap_err()
                    })?;
                Ok(Some(MergeQueueEntry {
                    id,
                    task_id,
                    pr_url,
                    status,
                    queued_at,
                    conflict_info: None, // Not persisted to DB yet
                    changes_requested_feedback: None, // TODO: persist to DB
                    head_sha: None,      // Updated from GitHub on reconciliation
                    queue_position: None, // Computed lazily on API read
                    completed_at: None,  // Not persisted to DB yet
                    mergeable_unknown: false, // Transient, updated from GitHub on reconciliation
                }))
            }
            None => Ok(None),
        }
    }

    /// List all merge queue entries.
    pub fn list_merge_entries(&self) -> Result<Vec<MergeQueueEntry>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare("SELECT id, task_id, pr_url, status, queued_at FROM merge_queue")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (id, task_id, pr_url, status_str, queued_at_str) = row?;
            let status: MergeStatus = serde_json::from_str(&format!("\"{status_str}\""))?;
            let queued_at: DateTime<Utc> = queued_at_str
                .parse()
                .map_err(|e: chrono::ParseError| {
                    serde_json::from_str::<()>(&e.to_string()).unwrap_err()
                })?;
            entries.push(MergeQueueEntry {
                id,
                task_id,
                pr_url,
                status,
                queued_at,
                conflict_info: None, // Not persisted to DB yet
                changes_requested_feedback: None, // TODO: persist to DB
                head_sha: None,      // Updated from GitHub on reconciliation
                queue_position: None, // Computed lazily on API read
                completed_at: None,  // Not persisted to DB yet
                mergeable_unknown: false, // Transient, updated from GitHub on reconciliation
            });
        }
        Ok(entries)
    }

    /// Delete a merge queue entry by ID. Returns true if a row was deleted.
    pub fn delete_merge_entry(&self, id: &str) -> Result<bool, StoreError> {
        let affected = self
            .conn()?
            .execute("DELETE FROM merge_queue WHERE id = ?1", params![id])?;
        Ok(affected > 0)
    }

    // ── Tasks ──────────────────────────────────────────────────────

    /// Insert or replace a task.
    pub fn save_task(&self, task: &Task) -> Result<(), StoreError> {
        let source_json = serde_json::to_string(&task.source)?;
        let state_json = serde_json::to_string(&task.state)?;
        let blocked_by_json = serde_json::to_string(&task.blocked_by)?;
        let labels_json = serde_json::to_string(&task.labels)?;
        let last_failure_at = task.last_failure_at.map(|dt| dt.to_rfc3339());
        let last_failure_json = task
            .last_failure
            .as_ref()
            .map(|f| serde_json::to_string(f))
            .transpose()?;
        let source_created_at = task.source_created_at.map(|dt| dt.to_rfc3339());
        let last_activity_at = task.last_activity_at.map(|dt| dt.to_rfc3339());
        let created_at = task.created_at.to_rfc3339();
        let updated_at = task.updated_at.to_rfc3339();

        self.conn()?.execute(
            "INSERT OR REPLACE INTO tasks (
                id, source_json, title, description, state,
                parent_id, blocked_by_json, project, labels_json, priority,
                session_id, workspace_id, retry_count, last_failure_at,
                last_failure_json, source_created_at, source_number, last_activity_at, created_at, updated_at,
                rejection_feedback
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14,
                ?15, ?16, ?17, ?18, ?19, ?20,
                ?21
            )",
            params![
                task.id,
                source_json,
                task.title,
                task.description,
                state_json,
                task.parent_id,
                blocked_by_json,
                task.project,
                labels_json,
                task.priority,
                task.session_id,
                task.workspace_id,
                task.retry_count,
                last_failure_at,
                last_failure_json,
                source_created_at,
                task.source_number,
                last_activity_at,
                created_at,
                updated_at,
                task.rejection_feedback,
            ],
        )?;
        Ok(())
    }

    /// Get a task by ID.
    pub fn get_task(&self, id: &str) -> Result<Option<Task>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, source_json, title, description, state,
                    parent_id, blocked_by_json, project, labels_json, priority,
                    session_id, workspace_id, retry_count, last_failure_at,
                    last_failure_json, source_created_at, source_number, last_activity_at, created_at, updated_at,
                    rejection_feedback
             FROM tasks WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], row_to_task)?;
        match rows.next() {
            Some(row) => {
                let task = row?;
                Ok(Some(task))
            }
            None => Ok(None),
        }
    }

    /// List all tasks.
    pub fn list_tasks(&self) -> Result<Vec<Task>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, source_json, title, description, state,
                    parent_id, blocked_by_json, project, labels_json, priority,
                    session_id, workspace_id, retry_count, last_failure_at,
                    last_failure_json, source_created_at, source_number, last_activity_at, created_at, updated_at,
                    rejection_feedback
             FROM tasks",
        )?;
        let rows = stmt.query_map([], row_to_task)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    }

    /// List tasks for a specific project.
    pub fn list_tasks_by_project(&self, project: &str) -> Result<Vec<Task>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, source_json, title, description, state,
                    parent_id, blocked_by_json, project, labels_json, priority,
                    session_id, workspace_id, retry_count, last_failure_at,
                    last_failure_json, source_created_at, source_number, last_activity_at, created_at, updated_at,
                    rejection_feedback
             FROM tasks WHERE project = ?1",
        )?;
        let rows = stmt.query_map(params![project], row_to_task)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    }

    /// List tasks in a specific state.
    pub fn list_tasks_by_state(&self, state: TaskState) -> Result<Vec<Task>, StoreError> {
        let state_json = serde_json::to_string(&state).map_err(StoreError::Json)?;
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, source_json, title, description, state,
                    parent_id, blocked_by_json, project, labels_json, priority,
                    session_id, workspace_id, retry_count, last_failure_at,
                    last_failure_json, source_created_at, source_number, last_activity_at, created_at, updated_at,
                    rejection_feedback
             FROM tasks WHERE state = ?1",
        )?;
        let rows = stmt.query_map(params![state_json], row_to_task)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    }

    /// Delete a task by ID. Returns true if a row was deleted.
    pub fn delete_task(&self, id: &str) -> Result<bool, StoreError> {
        let affected = self
            .conn()?
            .execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        Ok(affected > 0)
    }

    /// Delete all tasks from the database (for rebuild command, issue #256).
    /// Returns the number of rows deleted.
    pub fn clear_tasks(&self) -> Result<usize, StoreError> {
        let affected = self.conn()?.execute("DELETE FROM tasks", [])?;
        Ok(affected)
    }

    /// Delete all merge queue entries from the database (for rebuild command, issue #256).
    /// Returns the number of rows deleted.
    pub fn clear_merge_queue(&self) -> Result<usize, StoreError> {
        let affected = self.conn()?.execute("DELETE FROM merge_queue", [])?;
        Ok(affected)
    }

    /// Cascade-delete all data for a project's tasks: merge queue entries,
    /// accounting, then tasks. Runs in a transaction so partial failures
    /// don't leave the store inconsistent.
    pub fn delete_project_data(&self, project: &str, task_ids: &[String]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        let tx = conn.unchecked_transaction()?;

        // Delete merge queue entries and accounting for each task
        for task_id in task_ids {
            tx.execute("DELETE FROM merge_queue WHERE task_id = ?1", params![task_id])?;
            tx.execute("DELETE FROM task_accounting WHERE task_id = ?1", params![task_id])?;
        }

        // Delete all tasks for the project
        tx.execute("DELETE FROM tasks WHERE project = ?1", params![project])?;

        tx.commit()?;
        Ok(())
    }

    // ── Accounting (spec §16.4) ───────────────────────────────────

    /// Get or create accounting data for a task.
    pub fn get_or_create_accounting(&self, task_id: &str) -> Result<TaskAccounting, StoreError> {
        if let Some(acc) = self.get_accounting(task_id)? {
            return Ok(acc);
        }
        let acc = TaskAccounting::new(task_id);
        self.save_accounting(&acc)?;
        Ok(acc)
    }

    /// Get accounting data for a task, if it exists.
    pub fn get_accounting(&self, task_id: &str) -> Result<Option<TaskAccounting>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT task_id, total_input_tokens, total_output_tokens, session_count,
                    total_duration_seconds, last_updated
             FROM task_accounting WHERE task_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![task_id], row_to_accounting)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Save accounting data for a task.
    pub fn save_accounting(&self, accounting: &TaskAccounting) -> Result<(), StoreError> {
        let last_updated = accounting.last_updated.to_rfc3339();
        self.conn()?.execute(
            "INSERT OR REPLACE INTO task_accounting
             (task_id, total_input_tokens, total_output_tokens, session_count,
              total_duration_seconds, last_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                accounting.task_id,
                accounting.total_input_tokens as i64,
                accounting.total_output_tokens as i64,
                accounting.session_count,
                accounting.total_duration_seconds as i64,
                last_updated,
            ],
        )?;
        Ok(())
    }

    /// Add token usage to a task's accounting.
    pub fn add_token_usage(
        &self,
        task_id: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<TaskAccounting, StoreError> {
        let mut acc = self.get_or_create_accounting(task_id)?;
        acc.add_tokens(input_tokens, output_tokens);
        self.save_accounting(&acc)?;
        Ok(acc)
    }

    /// Record a session completion for a task.
    pub fn record_session_end(
        &self,
        task_id: &str,
        duration_seconds: u64,
    ) -> Result<TaskAccounting, StoreError> {
        let mut acc = self.get_or_create_accounting(task_id)?;
        acc.record_session(duration_seconds);
        self.save_accounting(&acc)?;
        Ok(acc)
    }

    /// List all accounting records.
    pub fn list_accounting(&self) -> Result<Vec<TaskAccounting>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT task_id, total_input_tokens, total_output_tokens, session_count,
                    total_duration_seconds, last_updated
             FROM task_accounting",
        )?;
        let rows = stmt.query_map([], row_to_accounting)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Get global accounting summary across all tasks.
    pub fn get_accounting_summary(&self) -> Result<AccountingSummary, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(SUM(total_input_tokens), 0),
                    COALESCE(SUM(total_output_tokens), 0),
                    COALESCE(SUM(session_count), 0),
                    COALESCE(SUM(total_duration_seconds), 0),
                    COUNT(*)
             FROM task_accounting",
        )?;
        let summary = stmt.query_row([], |row| {
            Ok(AccountingSummary {
                total_input_tokens: row.get::<_, i64>(0)? as u64,
                total_output_tokens: row.get::<_, i64>(1)? as u64,
                total_sessions: row.get::<_, i64>(2)? as u32,
                total_duration_seconds: row.get::<_, i64>(3)? as u64,
                task_count: row.get::<_, i64>(4)? as u32,
            })
        })?;
        Ok(summary)
    }

    /// Delete accounting data for a task.
    pub fn delete_accounting(&self, task_id: &str) -> Result<bool, StoreError> {
        let affected = self
            .conn()?
            .execute("DELETE FROM task_accounting WHERE task_id = ?1", params![task_id])?;
        Ok(affected > 0)
    }

    // ── Automations ───────────────────────────────────────────────

    /// Insert or replace an automation.
    pub fn save_automation(&self, automation: &Automation) -> Result<(), StoreError> {
        // Extract trigger type and config
        let (trigger_type, trigger_config) = match &automation.trigger {
            TriggerType::Schedule { cron } => {
                ("schedule", serde_json::json!({ "cron": cron }).to_string())
            }
            TriggerType::Event { event_type } => {
                ("event", serde_json::json!({ "event_type": event_type }).to_string())
            }
            TriggerType::Manual => ("manual", "{}".to_string()),
        };
        let state = serde_json::to_value(&automation.state)?
            .as_str()
            .unwrap()
            .to_string();
        let created_at = automation.created_at.to_rfc3339();
        let updated_at = automation.updated_at.to_rfc3339();

        self.conn()?.execute(
            "INSERT OR REPLACE INTO automations (
                id, project_id, name, prompt, compiled_workflow,
                trigger_type, trigger_config, state, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                automation.id,
                automation.project_id,
                automation.name,
                automation.prompt,
                automation.compiled_workflow,
                trigger_type,
                trigger_config,
                state,
                created_at,
                updated_at,
            ],
        )?;
        Ok(())
    }

    /// Get an automation by ID.
    pub fn get_automation(&self, id: &str) -> Result<Option<Automation>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, project_id, name, prompt, compiled_workflow,
                    trigger_type, trigger_config, state, created_at, updated_at
             FROM automations WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], row_to_automation)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// List all automations for a project.
    pub fn list_automations_for_project(&self, project_id: &str) -> Result<Vec<Automation>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, project_id, name, prompt, compiled_workflow,
                    trigger_type, trigger_config, state, created_at, updated_at
             FROM automations WHERE project_id = ?1",
        )?;
        let rows = stmt.query_map(params![project_id], row_to_automation)?;
        let mut automations = Vec::new();
        for row in rows {
            automations.push(row?);
        }
        Ok(automations)
    }

    /// Delete an automation and its runs by ID. Returns true if a row was deleted.
    /// Runs in a transaction so partial failures don't leave the store inconsistent.
    pub fn delete_automation(&self, id: &str) -> Result<bool, StoreError> {
        let conn = self.conn()?;
        let tx = conn.unchecked_transaction()?;
        // First delete associated runs
        tx.execute("DELETE FROM automation_runs WHERE automation_id = ?1", params![id])?;
        let affected = tx.execute("DELETE FROM automations WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(affected > 0)
    }

    // ── Automation Runs ───────────────────────────────────────────

    /// Insert or replace an automation run.
    pub fn save_automation_run(&self, run: &AutomationRun) -> Result<(), StoreError> {
        let status = serde_json::to_value(&run.status)?
            .as_str()
            .unwrap()
            .to_string();
        let started_at = run.started_at.to_rfc3339();
        let completed_at = run.completed_at.map(|dt| dt.to_rfc3339());

        self.conn()?.execute(
            "INSERT OR REPLACE INTO automation_runs (
                id, automation_id, status, started_at, completed_at, output, error
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run.id,
                run.automation_id,
                status,
                started_at,
                completed_at,
                run.output,
                run.error,
            ],
        )?;
        Ok(())
    }

    /// List all automations across all projects.
    pub fn list_automations(&self) -> Result<Vec<Automation>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, project_id, name, prompt, compiled_workflow,
                    trigger_type, trigger_config, state, created_at, updated_at
             FROM automations",
        )?;
        let rows = stmt.query_map([], row_to_automation)?;
        let mut automations = Vec::new();
        for row in rows {
            automations.push(row?);
        }
        Ok(automations)
    }

    /// Delete all automations (and their runs) for a project.
    pub fn delete_automations_for_project(&self, project_id: &str) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM automation_runs WHERE automation_id IN (SELECT id FROM automations WHERE project_id = ?1)",
            params![project_id],
        )?;
        let affected = tx.execute("DELETE FROM automations WHERE project_id = ?1", params![project_id])?;
        tx.commit()?;
        Ok(affected)
    }

    /// Alias: list runs for an automation (matches server API naming).
    pub fn list_automation_runs(&self, automation_id: &str) -> Result<Vec<AutomationRun>, StoreError> {
        self.list_runs_for_automation(automation_id)
    }

    /// Get a single automation run by ID.
    pub fn get_automation_run(&self, run_id: &str) -> Result<Option<AutomationRun>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, automation_id, status, started_at, completed_at, output, error
             FROM automation_runs WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![run_id], row_to_automation_run)?;
        match rows.next() {
            Some(Ok(run)) => Ok(Some(run)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// List all runs for an automation.
    pub fn list_runs_for_automation(&self, automation_id: &str) -> Result<Vec<AutomationRun>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, automation_id, status, started_at, completed_at, output, error
             FROM automation_runs WHERE automation_id = ?1 ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map(params![automation_id], row_to_automation_run)?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(row?);
        }
        Ok(runs)
    }

}

/// Map a rusqlite Row to a TaskAccounting.
fn row_to_accounting(row: &rusqlite::Row) -> Result<TaskAccounting, rusqlite::Error> {
    let task_id: String = row.get(0)?;
    let total_input_tokens: i64 = row.get(1)?;
    let total_output_tokens: i64 = row.get(2)?;
    let session_count: u32 = row.get(3)?;
    let total_duration_seconds: i64 = row.get(4)?;
    let last_updated_str: String = row.get(5)?;

    let last_updated = DateTime::parse_from_rfc3339(&last_updated_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;

    Ok(TaskAccounting {
        task_id,
        total_input_tokens: total_input_tokens as u64,
        total_output_tokens: total_output_tokens as u64,
        session_count,
        total_duration_seconds: total_duration_seconds as u64,
        last_updated,
    })
}

/// Map a rusqlite Row to a Task.
fn row_to_task(row: &rusqlite::Row) -> Result<Task, rusqlite::Error> {
    let id: String = row.get(0)?;
    let source_json: String = row.get(1)?;
    let title: String = row.get(2)?;
    let description: Option<String> = row.get(3)?;
    let state_json: String = row.get(4)?;
    let parent_id: Option<String> = row.get(5)?;
    let blocked_by_json: String = row.get(6)?;
    let project: String = row.get(7)?;
    let labels_json: String = row.get(8)?;
    let priority: Option<i32> = row.get(9)?;
    let session_id: Option<String> = row.get(10)?;
    let workspace_id: Option<String> = row.get(11)?;
    let retry_count: u32 = row.get(12)?;
    let last_failure_at_str: Option<String> = row.get(13)?;
    let last_failure_json: Option<String> = row.get(14)?;
    let source_created_at_str: Option<String> = row.get(15)?;
    let source_number: Option<u64> = row.get(16)?;
    let last_activity_at_str: Option<String> = row.get(17)?;
    let created_at_str: String = row.get(18)?;
    let updated_at_str: String = row.get(19)?;
    let rejection_feedback: Option<String> = row.get(20)?;

    let source: TaskSource = serde_json::from_str(&source_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let state: TaskState = serde_json::from_str(&state_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let blocked_by: Vec<String> = serde_json::from_str(&blocked_by_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let labels: Vec<String> = serde_json::from_str(&labels_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
    })?;

    let last_failure_at = last_failure_at_str
        .map(|s| {
            DateTime::parse_from_rfc3339(&s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        13,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
        })
        .transpose()?;
    let last_failure: Option<FailureInfo> = last_failure_json
        .map(|s| {
            serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()?;
    let source_created_at = source_created_at_str
        .map(|s| {
            DateTime::parse_from_rfc3339(&s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        15,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
        })
        .transpose()?;
    let last_activity_at = last_activity_at_str
        .map(|s| {
            DateTime::parse_from_rfc3339(&s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        17,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
        })
        .transpose()?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                18,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;
    let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                19,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;

    Ok(Task {
        id,
        source,
        title,
        description,
        state,
        parent_id,
        blocked_by,
        project,
        labels,
        priority,
        session_id,
        workspace_id,
        retry_count,
        last_failure_at,
        last_failure,
        source_created_at,
        source_number,
        last_activity_at,
        created_at,
        updated_at,
        rejection_feedback,
    })
}

/// Map a rusqlite Row to an Automation.
fn row_to_automation(row: &rusqlite::Row) -> Result<Automation, rusqlite::Error> {
    let id: String = row.get(0)?;
    let project_id: String = row.get(1)?;
    let name: String = row.get(2)?;
    let prompt: String = row.get(3)?;
    let compiled_workflow: Option<String> = row.get(4)?;
    let trigger_type: String = row.get(5)?;
    let trigger_config: String = row.get(6)?;
    let state_str: String = row.get(7)?;
    let created_at_str: String = row.get(8)?;
    let updated_at_str: String = row.get(9)?;

    // Parse trigger
    let trigger = match trigger_type.as_str() {
        "schedule" => {
            let config: serde_json::Value = serde_json::from_str(&trigger_config).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
            })?;
            let cron = config["cron"].as_str().ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    format!("missing or non-string 'cron' in trigger_config: {trigger_config}").into(),
                )
            })?;
            TriggerType::Schedule { cron: cron.to_string() }
        }
        "event" => {
            let config: serde_json::Value = serde_json::from_str(&trigger_config).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
            })?;
            let event_type = config["event_type"].as_str().ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    format!("missing or non-string 'event_type' in trigger_config: {trigger_config}").into(),
                )
            })?;
            TriggerType::Event { event_type: event_type.to_string() }
        }
        "manual" => TriggerType::Manual,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                format!("unrecognized trigger_type: {other}").into(),
            ));
        }
    };

    // Parse state
    let state: AutomationState = serde_json::from_str(&format!("\"{state_str}\"")).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
    })?;

    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(e))
        })?;

    Ok(Automation {
        id,
        project_id,
        name,
        prompt,
        compiled_workflow,
        trigger,
        state,
        created_at,
        updated_at,
    })
}

/// Map a rusqlite Row to an AutomationRun.
fn row_to_automation_run(row: &rusqlite::Row) -> Result<AutomationRun, rusqlite::Error> {
    let id: String = row.get(0)?;
    let automation_id: String = row.get(1)?;
    let status_str: String = row.get(2)?;
    let started_at_str: String = row.get(3)?;
    let completed_at_str: Option<String> = row.get(4)?;
    let output: Option<String> = row.get(5)?;
    let error: Option<String> = row.get(6)?;

    let status: RunStatus = serde_json::from_str(&format!("\"{status_str}\"")).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;

    let started_at = DateTime::parse_from_rfc3339(&started_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
        })?;

    let completed_at = completed_at_str
        .map(|s| {
            DateTime::parse_from_rfc3339(&s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
                })
        })
        .transpose()?;

    Ok(AutomationRun {
        id,
        automation_id,
        status,
        started_at,
        completed_at,
        output,
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_memory_creates_tables() {
        let store = Store::open_memory().unwrap();
        // Verify tables exist by querying them
        let conn = store.conn().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM projects", [], |row: &rusqlite::Row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |row: &rusqlite::Row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM merge_queue", [], |row: &rusqlite::Row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn save_and_get_project() {
        let store = Store::open_memory().unwrap();
        let project = Project::new("p1", "owner/repo");
        store.save_project(&project).unwrap();

        let loaded = store.get_project("p1").unwrap().unwrap();
        assert_eq!(loaded.id, "p1");
        assert_eq!(loaded.repo, "owner/repo");
        assert_eq!(loaded.default_branch, "main");
    }

    #[test]
    fn list_projects() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "a/b")).unwrap();
        store.save_project(&Project::new("p2", "c/d")).unwrap();

        let projects = store.list_projects().unwrap();
        assert_eq!(projects.len(), 2);
    }

    #[test]
    fn delete_project() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "a/b")).unwrap();
        assert!(store.delete_project("p1").unwrap());
        assert!(store.get_project("p1").unwrap().is_none());
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let store = Store::open_memory().unwrap();
        assert!(store.get_project("nope").unwrap().is_none());
    }

    #[test]
    fn save_project_overwrites() {
        let store = Store::open_memory().unwrap();
        let mut p = Project::new("p1", "a/b");
        store.save_project(&p).unwrap();

        p.default_branch = "develop".to_string();
        store.save_project(&p).unwrap();

        let loaded = store.get_project("p1").unwrap().unwrap();
        assert_eq!(loaded.default_branch, "develop");
    }

    // ── Merge queue tests ────────────────────────────────────────

    #[test]
    fn save_and_get_merge_entry() {
        let store = Store::open_memory().unwrap();
        let entry = MergeQueueEntry::new("m1", "t1", "https://github.com/test/repo/pull/1");
        store.save_merge_entry(&entry).unwrap();

        let loaded = store.get_merge_entry("m1").unwrap().unwrap();
        assert_eq!(loaded.id, "m1");
        assert_eq!(loaded.task_id, "t1");
        assert_eq!(loaded.status, MergeStatus::Pending);
        assert_eq!(loaded.pr_url, "https://github.com/test/repo/pull/1");
    }

    #[test]
    fn list_merge_entries() {
        let store = Store::open_memory().unwrap();
        store
            .save_merge_entry(&MergeQueueEntry::new("m1", "t1", "https://github.com/test/repo/pull/1"))
            .unwrap();
        store
            .save_merge_entry(&MergeQueueEntry::new("m2", "t2", "https://github.com/test/repo/pull/2"))
            .unwrap();

        let entries = store.list_merge_entries().unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn delete_merge_entry() {
        let store = Store::open_memory().unwrap();
        store
            .save_merge_entry(&MergeQueueEntry::new("m1", "t1", "https://github.com/test/repo/pull/1"))
            .unwrap();
        assert!(store.delete_merge_entry("m1").unwrap());
        assert!(store.get_merge_entry("m1").unwrap().is_none());
    }

    #[test]
    fn merge_status_roundtrip() {
        let store = Store::open_memory().unwrap();

        for (id, pr_num, status) in [
            ("m1", 1, MergeStatus::Pending),
            ("m2", 2, MergeStatus::Approved),
            ("m3", 3, MergeStatus::Merging),
            ("m4", 4, MergeStatus::Rejected),
            ("m5", 5, MergeStatus::Merged),
            ("m6", 6, MergeStatus::Conflict),
        ] {
            let mut entry = MergeQueueEntry::new(id, "t1", &format!("https://github.com/test/repo/pull/{pr_num}"));
            entry.status = status;
            store.save_merge_entry(&entry).unwrap();

            let loaded = store.get_merge_entry(id).unwrap().unwrap();
            assert_eq!(loaded.status, status, "failed for {id}");
        }
    }

    #[test]
    fn merge_entry_with_pr_url() {
        let store = Store::open_memory().unwrap();
        let entry = MergeQueueEntry::new("m1", "t1", "https://github.com/owner/repo/pull/1");
        store.save_merge_entry(&entry).unwrap();

        let loaded = store.get_merge_entry("m1").unwrap().unwrap();
        assert_eq!(loaded.pr_url, "https://github.com/owner/repo/pull/1");
    }

    #[test]
    fn merge_entry_duplicate_pr_url_rejected() {
        let store = Store::open_memory().unwrap();
        let entry1 = MergeQueueEntry::new("m1", "t1", "https://github.com/owner/repo/pull/1");
        store.save_merge_entry(&entry1).unwrap();

        // Different id, same pr_url — should fail with UNIQUE constraint violation
        let entry2 = MergeQueueEntry::new("m2", "t2", "https://github.com/owner/repo/pull/1");
        assert!(store.save_merge_entry(&entry2).is_err());

        // Only one entry should exist
        let entries = store.list_merge_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "m1");
    }

    #[test]
    fn merge_entry_same_id_update_allowed() {
        let store = Store::open_memory().unwrap();
        let mut entry = MergeQueueEntry::new("m1", "t1", "https://github.com/owner/repo/pull/1");
        store.save_merge_entry(&entry).unwrap();

        // Same id, updated status — should succeed (upsert)
        entry.status = MergeStatus::Approved;
        store.save_merge_entry(&entry).unwrap();

        let loaded = store.get_merge_entry("m1").unwrap().unwrap();
        assert_eq!(loaded.status, MergeStatus::Approved);
    }

    // ── Task tests ───────────────────────────────────────────────

    #[test]
    fn save_and_get_task() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "o/r")).unwrap();

        let mut task = Task::new("t1", TaskSource::Internal, "Test task", "p1");
        task.description = Some("A description".to_string());
        task.labels = vec!["bug".to_string(), "urgent".to_string()];
        task.priority = Some(1);
        store.save_task(&task).unwrap();

        let loaded = store.get_task("t1").unwrap().unwrap();
        assert_eq!(loaded.id, "t1");
        assert_eq!(loaded.title, "Test task");
        assert_eq!(loaded.description.as_deref(), Some("A description"));
        assert_eq!(loaded.state, TaskState::Waiting);
        assert_eq!(loaded.labels, vec!["bug", "urgent"]);
        assert_eq!(loaded.priority, Some(1));
        assert_eq!(loaded.project, "p1");
    }

    #[test]
    fn task_source_github_roundtrip() {
        let store = Store::open_memory().unwrap();
        store
            .save_project(&Project::new("p1", "owner/repo"))
            .unwrap();

        let source = TaskSource::GithubIssue {
            owner: "owner".into(),
            repo: "repo".into(),
            number: 42,
        };
        let task = Task::new("t1", source, "Issue #42", "p1");
        store.save_task(&task).unwrap();

        let loaded = store.get_task("t1").unwrap().unwrap();
        match &loaded.source {
            TaskSource::GithubIssue {
                owner,
                repo,
                number,
            } => {
                assert_eq!(owner, "owner");
                assert_eq!(repo, "repo");
                assert_eq!(*number, 42);
            }
            _ => panic!("wrong source type"),
        }
    }

    #[test]
    fn task_source_pr_roundtrip() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "o/r")).unwrap();

        let source = TaskSource::GithubPr {
            owner: "o".into(),
            repo: "r".into(),
            number: 10,
        };
        let task = Task::new("t1", source, "PR #10", "p1");
        store.save_task(&task).unwrap();

        let loaded = store.get_task("t1").unwrap().unwrap();
        assert!(matches!(
            loaded.source,
            TaskSource::GithubPr { number: 10, .. }
        ));
    }

    #[test]
    fn list_tasks_by_project() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "a/b")).unwrap();
        store.save_project(&Project::new("p2", "c/d")).unwrap();

        store
            .save_task(&Task::new("t1", TaskSource::Internal, "Task 1", "p1"))
            .unwrap();
        store
            .save_task(&Task::new("t2", TaskSource::Internal, "Task 2", "p1"))
            .unwrap();
        store
            .save_task(&Task::new("t3", TaskSource::Internal, "Task 3", "p2"))
            .unwrap();

        let p1_tasks = store.list_tasks_by_project("p1").unwrap();
        assert_eq!(p1_tasks.len(), 2);
    }

    #[test]
    fn list_tasks_by_state() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "a/b")).unwrap();

        let t1 = Task::new("t1", TaskSource::Internal, "Task 1", "p1");
        let mut t2 = Task::new("t2", TaskSource::Internal, "Task 2", "p1");
        t2.set_state(TaskState::Running);

        store.save_task(&t1).unwrap();
        store.save_task(&t2).unwrap();

        let waiting = store.list_tasks_by_state(TaskState::Waiting).unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].id, "t1");

        let running = store.list_tasks_by_state(TaskState::Running).unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, "t2");
    }

    #[test]
    fn delete_task() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "a/b")).unwrap();
        store
            .save_task(&Task::new("t1", TaskSource::Internal, "Task", "p1"))
            .unwrap();

        assert!(store.delete_task("t1").unwrap());
        assert!(store.get_task("t1").unwrap().is_none());
    }

    #[test]
    fn save_task_with_blocked_by() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "a/b")).unwrap();

        let mut task = Task::new("t2", TaskSource::Internal, "Task 2", "p1");
        task.blocked_by = vec!["t1".to_string()];
        task.state = TaskState::Blocked;
        store.save_task(&task).unwrap();

        let loaded = store.get_task("t2").unwrap().unwrap();
        assert_eq!(loaded.blocked_by, vec!["t1"]);
        assert_eq!(loaded.state, TaskState::Blocked);
    }

    // ── Accounting tests ──────────────────────────────────────────

    #[test]
    fn get_or_create_accounting() {
        let store = Store::open_memory().unwrap();
        let acc = store.get_or_create_accounting("task-1").unwrap();
        assert_eq!(acc.task_id, "task-1");
        assert_eq!(acc.total_input_tokens, 0);
        assert_eq!(acc.total_output_tokens, 0);
        assert_eq!(acc.session_count, 0);
    }

    #[test]
    fn add_token_usage() {
        let store = Store::open_memory().unwrap();

        // Add first batch
        let acc = store.add_token_usage("task-1", 100, 50).unwrap();
        assert_eq!(acc.total_input_tokens, 100);
        assert_eq!(acc.total_output_tokens, 50);

        // Add second batch
        let acc = store.add_token_usage("task-1", 200, 100).unwrap();
        assert_eq!(acc.total_input_tokens, 300);
        assert_eq!(acc.total_output_tokens, 150);

        // Verify persistence
        let loaded = store.get_accounting("task-1").unwrap().unwrap();
        assert_eq!(loaded.total_input_tokens, 300);
        assert_eq!(loaded.total_output_tokens, 150);
    }

    #[test]
    fn record_session_end() {
        let store = Store::open_memory().unwrap();

        let acc = store.record_session_end("task-1", 3600).unwrap();
        assert_eq!(acc.session_count, 1);
        assert_eq!(acc.total_duration_seconds, 3600);

        let acc = store.record_session_end("task-1", 1800).unwrap();
        assert_eq!(acc.session_count, 2);
        assert_eq!(acc.total_duration_seconds, 5400);
    }

    #[test]
    fn list_accounting() {
        let store = Store::open_memory().unwrap();

        store.add_token_usage("task-1", 100, 50).unwrap();
        store.add_token_usage("task-2", 200, 100).unwrap();

        let all = store.list_accounting().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn get_accounting_summary() {
        let store = Store::open_memory().unwrap();

        store.add_token_usage("task-1", 100, 50).unwrap();
        store.record_session_end("task-1", 3600).unwrap();

        store.add_token_usage("task-2", 200, 100).unwrap();
        store.record_session_end("task-2", 1800).unwrap();

        let summary = store.get_accounting_summary().unwrap();
        assert_eq!(summary.total_input_tokens, 300);
        assert_eq!(summary.total_output_tokens, 150);
        assert_eq!(summary.total_tokens(), 450);
        assert_eq!(summary.total_sessions, 2);
        assert_eq!(summary.total_duration_seconds, 5400);
        assert_eq!(summary.task_count, 2);
    }

    #[test]
    fn delete_accounting() {
        let store = Store::open_memory().unwrap();

        store.add_token_usage("task-1", 100, 50).unwrap();
        assert!(store.get_accounting("task-1").unwrap().is_some());

        assert!(store.delete_accounting("task-1").unwrap());
        assert!(store.get_accounting("task-1").unwrap().is_none());
    }

    #[test]
    fn accounting_summary_empty() {
        let store = Store::open_memory().unwrap();
        let summary = store.get_accounting_summary().unwrap();
        assert_eq!(summary.total_input_tokens, 0);
        assert_eq!(summary.total_output_tokens, 0);
        assert_eq!(summary.total_sessions, 0);
        assert_eq!(summary.task_count, 0);
    }

    // ── Cascade delete tests ──────────────────────────────────────────
    // NOTE: These tests reference methods that were never implemented.
    // Commented out until the batch delete methods are added.
    // See: delete_tasks_by_project, delete_merge_entries_by_task_ids,
    //      delete_accounting_by_task_ids

    // ── Automation tests ──────────────────────────────────────────

    // Tests for unimplemented batch methods (delete_tasks_by_project,
    // delete_merge_entries_by_task_ids, delete_accounting_by_task_ids) removed.

    #[test]
    fn save_and_get_automation() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "o/r")).unwrap();

        let automation = Automation::new(
            "a1",
            "p1",
            "Daily cleanup",
            "Clean up stale branches",
            TriggerType::Schedule { cron: "0 0 * * *".to_string() },
        );
        store.save_automation(&automation).unwrap();

        let loaded = store.get_automation("a1").unwrap().unwrap();
        assert_eq!(loaded.id, "a1");
        assert_eq!(loaded.project_id, "p1");
        assert_eq!(loaded.name, "Daily cleanup");
        assert_eq!(loaded.prompt, "Clean up stale branches");
        assert!(matches!(loaded.trigger, TriggerType::Schedule { cron } if cron == "0 0 * * *"));
        assert_eq!(loaded.state, AutomationState::Active);
    }

    #[test]
    fn save_automation_with_event_trigger() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "o/r")).unwrap();

        let automation = Automation::new(
            "a1",
            "p1",
            "On PR merged",
            "Notify team",
            TriggerType::Event { event_type: "pr:merged".to_string() },
        );
        store.save_automation(&automation).unwrap();

        let loaded = store.get_automation("a1").unwrap().unwrap();
        assert!(matches!(loaded.trigger, TriggerType::Event { event_type } if event_type == "pr:merged"));
    }

    #[test]
    fn save_automation_with_manual_trigger() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "o/r")).unwrap();

        let automation = Automation::new(
            "a1",
            "p1",
            "Manual deploy",
            "Deploy to production",
            TriggerType::Manual,
        );
        store.save_automation(&automation).unwrap();

        let loaded = store.get_automation("a1").unwrap().unwrap();
        assert!(matches!(loaded.trigger, TriggerType::Manual));
    }

    #[test]
    fn list_automations_for_project() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "o/r")).unwrap();
        store.save_project(&Project::new("p2", "a/b")).unwrap();

        store.save_automation(&Automation::new("a1", "p1", "Auto 1", "Prompt 1", TriggerType::Manual)).unwrap();
        store.save_automation(&Automation::new("a2", "p1", "Auto 2", "Prompt 2", TriggerType::Manual)).unwrap();
        store.save_automation(&Automation::new("a3", "p2", "Auto 3", "Prompt 3", TriggerType::Manual)).unwrap();

        let p1_automations = store.list_automations_for_project("p1").unwrap();
        assert_eq!(p1_automations.len(), 2);

        let p2_automations = store.list_automations_for_project("p2").unwrap();
        assert_eq!(p2_automations.len(), 1);
    }

    #[test]
    fn delete_automation() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "o/r")).unwrap();
        store.save_automation(&Automation::new("a1", "p1", "Auto", "Prompt", TriggerType::Manual)).unwrap();

        assert!(store.delete_automation("a1").unwrap());
        assert!(store.get_automation("a1").unwrap().is_none());
    }

    #[test]
    fn delete_automation_cascades_runs() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "o/r")).unwrap();
        store.save_automation(&Automation::new("a1", "p1", "Auto", "Prompt", TriggerType::Manual)).unwrap();

        // Create some runs
        store.save_automation_run(&AutomationRun::new("r1", "a1")).unwrap();
        store.save_automation_run(&AutomationRun::new("r2", "a1")).unwrap();

        // Verify runs exist
        let runs = store.list_runs_for_automation("a1").unwrap();
        assert_eq!(runs.len(), 2);

        // Delete automation should cascade to runs
        store.delete_automation("a1").unwrap();
        let runs = store.list_runs_for_automation("a1").unwrap();
        assert_eq!(runs.len(), 0);
    }

    #[test]
    fn automation_state_roundtrip() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "o/r")).unwrap();

        for (id, state) in [
            ("a1", AutomationState::Active),
            ("a2", AutomationState::Paused),
            ("a3", AutomationState::Disabled),
        ] {
            let mut automation = Automation::new(id, "p1", "Auto", "Prompt", TriggerType::Manual);
            automation.state = state;
            store.save_automation(&automation).unwrap();

            let loaded = store.get_automation(id).unwrap().unwrap();
            assert_eq!(loaded.state, state, "failed for {id}");
        }
    }

    // ── Automation run tests ──────────────────────────────────────

    #[test]
    fn save_and_list_automation_runs() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "o/r")).unwrap();
        store.save_automation(&Automation::new("a1", "p1", "Auto", "Prompt", TriggerType::Manual)).unwrap();

        let run1 = AutomationRun::new("r1", "a1");
        let mut run2 = AutomationRun::new("r2", "a1");
        run2.complete(Some("Success".to_string()));

        store.save_automation_run(&run1).unwrap();
        store.save_automation_run(&run2).unwrap();

        let runs = store.list_runs_for_automation("a1").unwrap();
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn automation_run_status_roundtrip() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "o/r")).unwrap();
        store.save_automation(&Automation::new("a1", "p1", "Auto", "Prompt", TriggerType::Manual)).unwrap();

        for (id, status) in [
            ("r1", RunStatus::Pending),
            ("r2", RunStatus::Running),
            ("r3", RunStatus::Completed),
            ("r4", RunStatus::Failed),
        ] {
            let mut run = AutomationRun::new(id, "a1");
            run.status = status;
            store.save_automation_run(&run).unwrap();

            let runs = store.list_runs_for_automation("a1").unwrap();
            let loaded = runs.iter().find(|r| r.id == id).unwrap();
            assert_eq!(loaded.status, status, "failed for {id}");
        }
    }

    #[test]
    fn automation_run_with_output_and_error() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "o/r")).unwrap();
        store.save_automation(&Automation::new("a1", "p1", "Auto", "Prompt", TriggerType::Manual)).unwrap();

        // Test completed run with output
        let mut run1 = AutomationRun::new("r1", "a1");
        run1.complete(Some("All done!".to_string()));
        store.save_automation_run(&run1).unwrap();

        let runs = store.list_runs_for_automation("a1").unwrap();
        let loaded1 = runs.iter().find(|r| r.id == "r1").unwrap();
        assert_eq!(loaded1.output, Some("All done!".to_string()));
        assert!(loaded1.completed_at.is_some());

        // Test failed run with error
        let mut run2 = AutomationRun::new("r2", "a1");
        run2.fail("Something went wrong");
        store.save_automation_run(&run2).unwrap();

        let runs = store.list_runs_for_automation("a1").unwrap();
        let loaded2 = runs.iter().find(|r| r.id == "r2").unwrap();
        assert_eq!(loaded2.error, Some("Something went wrong".to_string()));
        assert!(loaded2.completed_at.is_some());
    }

    #[test]
    fn get_automation_run_by_id() {
        let store = Store::open_memory().unwrap();
        store.save_project(&Project::new("p1", "o/r")).unwrap();
        store.save_automation(&Automation::new("a1", "p1", "Auto", "Prompt", TriggerType::Manual)).unwrap();

        // Non-existent run returns None
        assert!(store.get_automation_run("nonexistent").unwrap().is_none());

        // Create and save a run
        let mut run = AutomationRun::new("r1", "a1");
        run.start();
        store.save_automation_run(&run).unwrap();

        // Get by ID works
        let loaded = store.get_automation_run("r1").unwrap().unwrap();
        assert_eq!(loaded.id, "r1");
        assert_eq!(loaded.automation_id, "a1");
        assert_eq!(loaded.status, RunStatus::Running);

        // Update the run
        let mut run2 = loaded;
        run2.complete(Some("Output text".to_string()));
        store.save_automation_run(&run2).unwrap();

        // Get updated run
        let updated = store.get_automation_run("r1").unwrap().unwrap();
        assert_eq!(updated.status, RunStatus::Completed);
        assert_eq!(updated.output, Some("Output text".to_string()));
    }

    #[test]
    fn test_data_version_stamped_on_init() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        // Before init, version is 0
        assert_eq!(schema::read_version(&conn).unwrap(), 0);
        schema::initialize(&conn).unwrap();
        // After init, version matches DATA_VERSION
        assert_eq!(schema::read_version(&conn).unwrap(), schema::DATA_VERSION);
    }

    #[test]
    fn test_check_version_fresh_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.sqlite");
        // Fresh DB (doesn't exist yet) — no mismatch
        assert!(Store::check_version(&db).unwrap().is_none());
    }

    #[test]
    fn test_check_version_matching() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.sqlite");
        // Create and initialize
        Store::open(&db).unwrap();
        // Version should match
        assert!(Store::check_version(&db).unwrap().is_none());
    }

    #[test]
    fn test_check_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.sqlite");
        // Create DB with a different version
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.pragma_update(None, "user_version", 999u32).unwrap();
        }
        let result = Store::check_version(&db).unwrap();
        assert_eq!(result, Some((999, schema::DATA_VERSION)));
    }

    #[test]
    fn test_clear_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.sqlite");

        // Create a store with some data
        {
            let store = Store::open(&db).unwrap();
            let project = models::project::Project::new("test/repo", "test/repo");
            store.save_project(&project).unwrap();
            assert_eq!(store.list_projects().unwrap().len(), 1);
        }

        // Create events dir to verify it gets cleaned up
        let events = dir.path().join("events");
        std::fs::create_dir_all(&events).unwrap();
        std::fs::write(events.join("dummy.jsonl"), "{}").unwrap();

        // Clear and reopen
        let store = Store::clear_and_reopen(&db, dir.path()).unwrap();
        // Data should be gone
        assert_eq!(store.list_projects().unwrap().len(), 0);
        // Events dir should be gone
        assert!(!events.exists());
    }
}
