//! Persistent storage for the Tasks platform (spec §3.5).
//!
//! Wraps SQLite via rusqlite. Exposes typed CRUD methods — no SQL
//! leaks into other crates. The implementation can be swapped without
//! affecting consumers.

mod schema;

use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use models::merge_queue::{MergeQueueEntry, MergeStatus};
use models::project::Project;
use models::task::{Task, TaskSource, TaskState};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Persistent storage backed by SQLite (spec §3.5).
///
/// Stores projects, tasks, and merge queue entries. The event log
/// is stored separately as JSONL files (handled by the events crate).
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open or create a store at the given path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        schema::initialize(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory store (for testing).
    pub fn open_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        schema::initialize(&conn)?;
        Ok(Self { conn })
    }

    /// Insert or replace a project.
    pub fn save_project(&self, project: &Project) -> Result<(), StoreError> {
        let config = serde_json::to_string(&project.config)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO projects (id, repo, default_branch, config) VALUES (?1, ?2, ?3, ?4)",
            params![project.id, project.repo, project.default_branch, config],
        )?;
        Ok(())
    }

    /// Get a project by ID.
    pub fn get_project(&self, id: &str) -> Result<Option<Project>, StoreError> {
        let mut stmt = self
            .conn
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
        let mut stmt = self
            .conn
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
            .conn
            .execute("DELETE FROM projects WHERE id = ?1", params![id])?;
        Ok(affected > 0)
    }

    // ── Merge queue ──────────────────────────────────────────────

    /// Insert or replace a merge queue entry.
    pub fn save_merge_entry(&self, entry: &MergeQueueEntry) -> Result<(), StoreError> {
        let status = serde_json::to_value(&entry.status)?
            .as_str()
            .unwrap()
            .to_string();
        let queued_at = entry.queued_at.to_rfc3339();
        self.conn.execute(
            "INSERT OR REPLACE INTO merge_queue (id, task_id, pr_url, status, queued_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![entry.id, entry.task_id, entry.pr_url, status, queued_at],
        )?;
        Ok(())
    }

    /// Get a merge queue entry by ID.
    pub fn get_merge_entry(&self, id: &str) -> Result<Option<MergeQueueEntry>, StoreError> {
        let mut stmt = self.conn.prepare(
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
                }))
            }
            None => Ok(None),
        }
    }

    /// List all merge queue entries.
    pub fn list_merge_entries(&self) -> Result<Vec<MergeQueueEntry>, StoreError> {
        let mut stmt = self
            .conn
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
            });
        }
        Ok(entries)
    }

    /// Delete a merge queue entry by ID. Returns true if a row was deleted.
    pub fn delete_merge_entry(&self, id: &str) -> Result<bool, StoreError> {
        let affected = self
            .conn
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
        let created_at = task.created_at.to_rfc3339();
        let updated_at = task.updated_at.to_rfc3339();

        self.conn.execute(
            "INSERT OR REPLACE INTO tasks (
                id, source_json, title, description, state,
                parent_id, blocked_by_json, project, labels_json, priority,
                session_id, workspace_id, retry_count, last_failure_at,
                created_at, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14,
                ?15, ?16
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
                created_at,
                updated_at,
            ],
        )?;
        Ok(())
    }

    /// Get a task by ID.
    pub fn get_task(&self, id: &str) -> Result<Option<Task>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source_json, title, description, state,
                    parent_id, blocked_by_json, project, labels_json, priority,
                    session_id, workspace_id, retry_count, last_failure_at,
                    created_at, updated_at
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
        let mut stmt = self.conn.prepare(
            "SELECT id, source_json, title, description, state,
                    parent_id, blocked_by_json, project, labels_json, priority,
                    session_id, workspace_id, retry_count, last_failure_at,
                    created_at, updated_at
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
        let mut stmt = self.conn.prepare(
            "SELECT id, source_json, title, description, state,
                    parent_id, blocked_by_json, project, labels_json, priority,
                    session_id, workspace_id, retry_count, last_failure_at,
                    created_at, updated_at
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
        let mut stmt = self.conn.prepare(
            "SELECT id, source_json, title, description, state,
                    parent_id, blocked_by_json, project, labels_json, priority,
                    session_id, workspace_id, retry_count, last_failure_at,
                    created_at, updated_at
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
            .conn
            .execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        Ok(affected > 0)
    }

    // ── Accounting (spec §16.4) ───────────────────────────────────

    /// Get or create accounting summary for a task.
    pub fn get_task_accounting(
        &self,
        task_id: &str,
    ) -> Result<models::accounting::TaskAccountingSummary, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id, input_tokens, output_tokens, total_duration_seconds, session_count
             FROM task_accounting WHERE task_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![task_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u32>(4)?,
            ))
        })?;

        match rows.next() {
            Some(row) => {
                let (task_id, input, output, duration, sessions) = row?;
                Ok(models::accounting::TaskAccountingSummary {
                    task_id,
                    tokens: models::accounting::TokenUsage::new(input, output),
                    total_duration_seconds: duration,
                    session_count: sessions,
                })
            }
            None => Ok(models::accounting::TaskAccountingSummary::new(task_id)),
        }
    }

    /// Update accounting summary for a task (upsert).
    pub fn save_task_accounting(
        &self,
        summary: &models::accounting::TaskAccountingSummary,
    ) -> Result<(), StoreError> {
        let updated_at = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO task_accounting (task_id, input_tokens, output_tokens, total_duration_seconds, session_count, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(task_id) DO UPDATE SET
                 input_tokens = excluded.input_tokens,
                 output_tokens = excluded.output_tokens,
                 total_duration_seconds = excluded.total_duration_seconds,
                 session_count = excluded.session_count,
                 updated_at = excluded.updated_at",
            params![
                summary.task_id,
                summary.tokens.input_tokens,
                summary.tokens.output_tokens,
                summary.total_duration_seconds,
                summary.session_count,
                updated_at,
            ],
        )?;
        Ok(())
    }

    /// Add token usage to a task's accounting.
    pub fn add_task_tokens(
        &self,
        task_id: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<(), StoreError> {
        let updated_at = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO task_accounting (task_id, input_tokens, output_tokens, total_duration_seconds, session_count, updated_at)
             VALUES (?1, ?2, ?3, 0, 0, ?4)
             ON CONFLICT(task_id) DO UPDATE SET
                 input_tokens = input_tokens + excluded.input_tokens,
                 output_tokens = output_tokens + excluded.output_tokens,
                 updated_at = excluded.updated_at",
            params![task_id, input_tokens, output_tokens, updated_at],
        )?;
        Ok(())
    }

    /// Add a session to a task's accounting.
    pub fn add_task_session(
        &self,
        task_id: &str,
        duration_seconds: u64,
    ) -> Result<(), StoreError> {
        let updated_at = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO task_accounting (task_id, input_tokens, output_tokens, total_duration_seconds, session_count, updated_at)
             VALUES (?1, 0, 0, ?2, 1, ?3)
             ON CONFLICT(task_id) DO UPDATE SET
                 total_duration_seconds = total_duration_seconds + excluded.total_duration_seconds,
                 session_count = session_count + 1,
                 updated_at = excluded.updated_at",
            params![task_id, duration_seconds, updated_at],
        )?;
        Ok(())
    }

    /// Get global accounting summary across all tasks.
    pub fn get_global_accounting(&self) -> Result<models::accounting::GlobalAccountingSummary, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(total_duration_seconds), 0), COALESCE(SUM(session_count), 0)
             FROM task_accounting",
        )?;
        let mut rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u32>(3)?,
            ))
        })?;

        match rows.next() {
            Some(row) => {
                let (input, output, duration, sessions) = row?;
                Ok(models::accounting::GlobalAccountingSummary {
                    tokens: models::accounting::TokenUsage::new(input, output),
                    total_duration_seconds: duration,
                    session_count: sessions,
                    api_call_count: 0, // API calls tracked via events only
                })
            }
            None => Ok(models::accounting::GlobalAccountingSummary::default()),
        }
    }

    /// List all task accounting summaries.
    pub fn list_task_accounting(&self) -> Result<Vec<models::accounting::TaskAccountingSummary>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id, input_tokens, output_tokens, total_duration_seconds, session_count
             FROM task_accounting",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u32>(4)?,
            ))
        })?;

        let mut summaries = Vec::new();
        for row in rows {
            let (task_id, input, output, duration, sessions) = row?;
            summaries.push(models::accounting::TaskAccountingSummary {
                task_id,
                tokens: models::accounting::TokenUsage::new(input, output),
                total_duration_seconds: duration,
                session_count: sessions,
            });
        }
        Ok(summaries)
    }

    /// Delete accounting data for a task.
    pub fn delete_task_accounting(&self, task_id: &str) -> Result<bool, StoreError> {
        let affected = self
            .conn
            .execute("DELETE FROM task_accounting WHERE task_id = ?1", params![task_id])?;
        Ok(affected > 0)
    }
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
    let created_at_str: String = row.get(14)?;
    let updated_at_str: String = row.get(15)?;

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
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                14,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;
    let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                15,
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
        created_at,
        updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_memory_creates_tables() {
        let store = Store::open_memory().unwrap();
        // Verify tables exist by querying them
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM merge_queue", [], |row| row.get(0))
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

        for (id, status) in [
            ("m1", MergeStatus::Pending),
            ("m2", MergeStatus::Approved),
            ("m3", MergeStatus::Rejected),
            ("m4", MergeStatus::Merged),
            ("m5", MergeStatus::Conflict),
        ] {
            let mut entry = MergeQueueEntry::new(id, "t1", "https://github.com/test/repo/pull/1");
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
    fn get_task_accounting_returns_default_for_new() {
        let store = Store::open_memory().unwrap();
        let summary = store.get_task_accounting("task-1").unwrap();
        assert_eq!(summary.task_id, "task-1");
        assert_eq!(summary.tokens.input_tokens, 0);
        assert_eq!(summary.tokens.output_tokens, 0);
        assert_eq!(summary.total_duration_seconds, 0);
        assert_eq!(summary.session_count, 0);
    }

    #[test]
    fn save_and_get_task_accounting() {
        let store = Store::open_memory().unwrap();
        let mut summary = models::accounting::TaskAccountingSummary::new("task-1");
        summary.tokens = models::accounting::TokenUsage::new(1000, 500);
        summary.total_duration_seconds = 3600;
        summary.session_count = 1;

        store.save_task_accounting(&summary).unwrap();

        let loaded = store.get_task_accounting("task-1").unwrap();
        assert_eq!(loaded.tokens.input_tokens, 1000);
        assert_eq!(loaded.tokens.output_tokens, 500);
        assert_eq!(loaded.total_duration_seconds, 3600);
        assert_eq!(loaded.session_count, 1);
    }

    #[test]
    fn add_task_tokens_creates_and_increments() {
        let store = Store::open_memory().unwrap();

        // First addition creates the record
        store.add_task_tokens("task-1", 1000, 500).unwrap();
        let summary = store.get_task_accounting("task-1").unwrap();
        assert_eq!(summary.tokens.input_tokens, 1000);
        assert_eq!(summary.tokens.output_tokens, 500);

        // Second addition increments
        store.add_task_tokens("task-1", 500, 250).unwrap();
        let summary = store.get_task_accounting("task-1").unwrap();
        assert_eq!(summary.tokens.input_tokens, 1500);
        assert_eq!(summary.tokens.output_tokens, 750);
    }

    #[test]
    fn add_task_session_creates_and_increments() {
        let store = Store::open_memory().unwrap();

        // First session
        store.add_task_session("task-1", 3600).unwrap();
        let summary = store.get_task_accounting("task-1").unwrap();
        assert_eq!(summary.total_duration_seconds, 3600);
        assert_eq!(summary.session_count, 1);

        // Second session
        store.add_task_session("task-1", 1800).unwrap();
        let summary = store.get_task_accounting("task-1").unwrap();
        assert_eq!(summary.total_duration_seconds, 5400);
        assert_eq!(summary.session_count, 2);
    }

    #[test]
    fn get_global_accounting() {
        let store = Store::open_memory().unwrap();

        // Add accounting for multiple tasks
        store.add_task_tokens("task-1", 1000, 500).unwrap();
        store.add_task_session("task-1", 3600).unwrap();

        store.add_task_tokens("task-2", 2000, 1000).unwrap();
        store.add_task_session("task-2", 1800).unwrap();

        let global = store.get_global_accounting().unwrap();
        assert_eq!(global.tokens.input_tokens, 3000);
        assert_eq!(global.tokens.output_tokens, 1500);
        assert_eq!(global.total_duration_seconds, 5400);
        assert_eq!(global.session_count, 2);
    }

    #[test]
    fn list_task_accounting() {
        let store = Store::open_memory().unwrap();

        store.add_task_tokens("task-1", 1000, 500).unwrap();
        store.add_task_tokens("task-2", 2000, 1000).unwrap();

        let summaries = store.list_task_accounting().unwrap();
        assert_eq!(summaries.len(), 2);
    }

    #[test]
    fn delete_task_accounting() {
        let store = Store::open_memory().unwrap();

        store.add_task_tokens("task-1", 1000, 500).unwrap();
        assert!(store.delete_task_accounting("task-1").unwrap());

        let summary = store.get_task_accounting("task-1").unwrap();
        assert_eq!(summary.tokens.input_tokens, 0);
    }

    #[test]
    fn global_accounting_empty_returns_defaults() {
        let store = Store::open_memory().unwrap();
        let global = store.get_global_accounting().unwrap();
        assert_eq!(global.tokens.input_tokens, 0);
        assert_eq!(global.tokens.output_tokens, 0);
        assert_eq!(global.total_duration_seconds, 0);
        assert_eq!(global.session_count, 0);
    }
}
