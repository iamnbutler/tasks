//! Persistent storage for the Tasks platform (spec §3.5).
//!
//! Wraps SQLite via rusqlite. Exposes typed CRUD methods — no SQL
//! leaks into other crates. The implementation can be swapped without
//! affecting consumers.

mod accounting;
mod schema;

pub use accounting::{AccountingSummary, TaskAccounting};

use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
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

    /// Get the last polled timestamp for a project (poller high-water mark).
    ///
    /// Returns `None` if the project doesn't exist or hasn't been polled yet.
    /// Used to initialize the poller after server restarts (spec github.md §5.3).
    pub fn get_last_polled_at(&self, id: &str) -> Result<Option<DateTime<Utc>>, StoreError> {
        let mut stmt = self
            .conn
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
        self.conn.execute(
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
                    conflict_info: None, // Not persisted to DB yet
                    changes_requested_feedback: None, // TODO: persist to DB
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
                conflict_info: None, // Not persisted to DB yet
                changes_requested_feedback: None, // TODO: persist to DB
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
        let last_failure_json = task
            .last_failure
            .as_ref()
            .map(|f| serde_json::to_string(f))
            .transpose()?;
        let source_created_at = task.source_created_at.map(|dt| dt.to_rfc3339());
        let last_activity_at = task.last_activity_at.map(|dt| dt.to_rfc3339());
        let created_at = task.created_at.to_rfc3339();
        let updated_at = task.updated_at.to_rfc3339();

        self.conn.execute(
            "INSERT OR REPLACE INTO tasks (
                id, source_json, title, description, state,
                parent_id, blocked_by_json, project, labels_json, priority,
                session_id, workspace_id, retry_count, last_failure_at,
                last_failure_json, source_created_at, source_number, last_activity_at, created_at, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14,
                ?15, ?16, ?17, ?18, ?19, ?20
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
                    last_failure_json, source_created_at, source_number, last_activity_at, created_at, updated_at
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
                    last_failure_json, source_created_at, source_number, last_activity_at, created_at, updated_at
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
                    last_failure_json, source_created_at, source_number, last_activity_at, created_at, updated_at
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
                    last_failure_json, source_created_at, source_number, last_activity_at, created_at, updated_at
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

    /// Delete all tasks from the database (for rebuild command, issue #256).
    /// Returns the number of rows deleted.
    pub fn clear_tasks(&self) -> Result<usize, StoreError> {
        let affected = self.conn.execute("DELETE FROM tasks", [])?;
        Ok(affected)
    }

    /// Delete all merge queue entries from the database (for rebuild command, issue #256).
    /// Returns the number of rows deleted.
    pub fn clear_merge_queue(&self) -> Result<usize, StoreError> {
        let affected = self.conn.execute("DELETE FROM merge_queue", [])?;
        Ok(affected)
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
        let mut stmt = self.conn.prepare(
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
        self.conn.execute(
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
        let mut stmt = self.conn.prepare(
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
        let mut stmt = self.conn.prepare(
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
            .conn
            .execute("DELETE FROM task_accounting WHERE task_id = ?1", params![task_id])?;
        Ok(affected > 0)
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
}
