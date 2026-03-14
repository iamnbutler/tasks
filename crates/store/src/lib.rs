//! Persistent storage for the Tasks platform (spec §3.5).
//!
//! Wraps SQLite via rusqlite. Exposes typed CRUD methods — no SQL
//! leaks into other crates. The implementation can be swapped without
//! affecting consumers.

mod schema;

use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use server::model::merge_queue::{MergeQueueEntry, MergeStatus};
use server::model::project::Project;
use server::model::task::{Task, TaskSource, TaskState};
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
                row.get::<_, Option<String>>(2)?,
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
                row.get::<_, Option<String>>(2)?,
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
        let entry = MergeQueueEntry::new("m1", "t1");
        store.save_merge_entry(&entry).unwrap();

        let loaded = store.get_merge_entry("m1").unwrap().unwrap();
        assert_eq!(loaded.id, "m1");
        assert_eq!(loaded.task_id, "t1");
        assert_eq!(loaded.status, MergeStatus::Pending);
        assert!(loaded.pr_url.is_none());
    }

    #[test]
    fn list_merge_entries() {
        let store = Store::open_memory().unwrap();
        store
            .save_merge_entry(&MergeQueueEntry::new("m1", "t1"))
            .unwrap();
        store
            .save_merge_entry(&MergeQueueEntry::new("m2", "t2"))
            .unwrap();

        let entries = store.list_merge_entries().unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn delete_merge_entry() {
        let store = Store::open_memory().unwrap();
        store
            .save_merge_entry(&MergeQueueEntry::new("m1", "t1"))
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
            let mut entry = MergeQueueEntry::new(id, "t1");
            entry.status = status;
            store.save_merge_entry(&entry).unwrap();

            let loaded = store.get_merge_entry(id).unwrap().unwrap();
            assert_eq!(loaded.status, status, "failed for {id}");
        }
    }

    #[test]
    fn merge_entry_with_pr_url() {
        let store = Store::open_memory().unwrap();
        let mut entry = MergeQueueEntry::new("m1", "t1");
        entry.pr_url = Some("https://github.com/owner/repo/pull/1".to_string());
        store.save_merge_entry(&entry).unwrap();

        let loaded = store.get_merge_entry("m1").unwrap().unwrap();
        assert_eq!(
            loaded.pr_url.as_deref(),
            Some("https://github.com/owner/repo/pull/1")
        );
    }
}
