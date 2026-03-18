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
            source_created_at TEXT,
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

        -- Token and cost accounting (spec §16.4)
        CREATE TABLE IF NOT EXISTS task_accounting (
            task_id TEXT PRIMARY KEY,
            total_input_tokens INTEGER NOT NULL DEFAULT 0,
            total_output_tokens INTEGER NOT NULL DEFAULT 0,
            session_count INTEGER NOT NULL DEFAULT 0,
            total_duration_seconds INTEGER NOT NULL DEFAULT 0,
            last_updated TEXT NOT NULL
        );
        ",
    )?;

    // Migration: add last_failure_json column if it doesn't exist (spec §13.4)
    // This handles existing databases that were created before this column was added.
    match conn.execute(
        "ALTER TABLE tasks ADD COLUMN last_failure_json TEXT",
        [],
    ) {
        Ok(_) => {
            tracing::info!("added last_failure_json column to tasks table");
        }
        Err(rusqlite::Error::SqliteFailure(e, Some(ref msg)))
            if e.extended_code == rusqlite::ffi::SQLITE_ERROR
                && msg.contains("duplicate column name") =>
        {
            // Column already exists — this is expected for existing databases
            tracing::debug!("last_failure_json column already exists");
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to add last_failure_json column");
            return Err(e);
        }
    }

    // Migration: add last_activity_at column if it doesn't exist (spec §10.3)
    // This is used for stale workspace detection.
    match conn.execute(
        "ALTER TABLE tasks ADD COLUMN last_activity_at TEXT",
        [],
    ) {
        Ok(_) => {
            tracing::info!("added last_activity_at column to tasks table");
        }
        Err(rusqlite::Error::SqliteFailure(e, Some(ref msg)))
            if e.extended_code == rusqlite::ffi::SQLITE_ERROR
                && msg.contains("duplicate column name") =>
        {
            // Column already exists — this is expected for existing databases
            tracing::debug!("last_activity_at column already exists");
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to add last_activity_at column");
            return Err(e);
        }
    }

    Ok(())
}
