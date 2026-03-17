//! Database schema initialization.

use rusqlite::Connection;

/// Create tables if they don't exist.
pub(crate) fn initialize(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS projects (
            id TEXT PRIMARY KEY,
            repo TEXT NOT NULL,
            default_branch TEXT NOT NULL DEFAULT 'main',
            config TEXT NOT NULL DEFAULT '{}'
        );

        CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY,
            source_json TEXT NOT NULL,
            title TEXT NOT NULL,
            description TEXT,
            state TEXT NOT NULL DEFAULT 'waiting',
            parent_id TEXT,
            blocked_by_json TEXT NOT NULL DEFAULT '[]',
            project TEXT NOT NULL REFERENCES projects(id),
            labels_json TEXT NOT NULL DEFAULT '[]',
            priority INTEGER,
            session_id TEXT,
            workspace_id TEXT,
            retry_count INTEGER NOT NULL DEFAULT 0,
            last_failure_at TEXT,
            last_failure_json TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS merge_queue (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL,
            pr_url TEXT,
            status TEXT NOT NULL DEFAULT 'pending',
            queued_at TEXT NOT NULL
        );
        ",
    )?;

    // Migration: add last_failure_json column if it doesn't exist (for existing databases)
    // This is safe to run multiple times - it will fail silently if the column exists
    let _ = conn.execute(
        "ALTER TABLE tasks ADD COLUMN last_failure_json TEXT",
        [],
    );

    Ok(())
}
