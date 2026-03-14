//! Persistent storage for the Tasks platform (spec §3.5).
//!
//! Wraps SQLite via rusqlite. Exposes typed CRUD methods — no SQL
//! leaks into other crates. The implementation can be swapped without
//! affecting consumers.

mod schema;

use std::path::Path;

use rusqlite::{params, Connection};
use server::model::project::Project;
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
}
