//! Database schema initialization.

use rusqlite::Connection;

/// Current data schema version. Bump this when making incompatible schema changes
/// that require a full data reset rather than an incremental migration.
pub const DATA_VERSION: u32 = 1;

/// Read the stored data version from SQLite's `user_version` pragma.
/// Returns 0 if no version has been set (fresh database).
pub fn read_version(conn: &Connection) -> Result<u32, rusqlite::Error> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Write the data version to SQLite's `user_version` pragma.
pub fn write_version(conn: &Connection, version: u32) -> Result<(), rusqlite::Error> {
    conn.pragma_update(None, "user_version", version)?;
    Ok(())
}

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
            source_number INTEGER,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS merge_queue (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL,
            pr_url TEXT UNIQUE,
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

        -- Automations: reusable workflows triggered by schedule, events, or manual action
        CREATE TABLE IF NOT EXISTS automations (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL REFERENCES projects(id),
            name TEXT NOT NULL,
            prompt TEXT NOT NULL,
            compiled_workflow TEXT,
            trigger_type TEXT NOT NULL,
            trigger_config TEXT NOT NULL DEFAULT '{}',
            state TEXT NOT NULL DEFAULT 'active',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        -- Automation runs: history of automation executions
        CREATE TABLE IF NOT EXISTS automation_runs (
            id TEXT PRIMARY KEY,
            automation_id TEXT NOT NULL REFERENCES automations(id),
            status TEXT NOT NULL,
            started_at TEXT NOT NULL,
            completed_at TEXT,
            output TEXT,
            error TEXT
        );

        -- Indexes for common query patterns (issue #465)
        -- tasks: lookup by project, filter by state, find by source
        CREATE INDEX IF NOT EXISTS idx_tasks_project ON tasks(project);
        CREATE INDEX IF NOT EXISTS idx_tasks_state ON tasks(state);
        CREATE INDEX IF NOT EXISTS idx_tasks_source ON tasks(source_json);

        -- merge_queue: lookup by task_id (pr_url already has a UNIQUE index)
        CREATE INDEX IF NOT EXISTS idx_merge_queue_task_id ON merge_queue(task_id);

        -- automations: lookup by project
        CREATE INDEX IF NOT EXISTS idx_automations_project_id ON automations(project_id);

        -- automation_runs: lookup by automation_id
        CREATE INDEX IF NOT EXISTS idx_automation_runs_automation_id ON automation_runs(automation_id);
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

    // Migration: add last_polled_at column to projects table (spec github.md §5.3)
    // This persists the poller high-water mark to survive server restarts.
    match conn.execute(
        "ALTER TABLE projects ADD COLUMN last_polled_at TEXT",
        [],
    ) {
        Ok(_) => {
            tracing::info!("added last_polled_at column to projects table");
        }
        Err(rusqlite::Error::SqliteFailure(e, Some(ref msg)))
            if e.extended_code == rusqlite::ffi::SQLITE_ERROR
                && msg.contains("duplicate column name") =>
        {
            // Column already exists — this is expected for existing databases
            tracing::debug!("last_polled_at column already exists");
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to add last_polled_at column");
            return Err(e);
        }
    }

    // Migration: add source_number column if it doesn't exist (issue #327)
    // This stores the GitHub issue/PR number for deterministic dispatch ordering.
    match conn.execute(
        "ALTER TABLE tasks ADD COLUMN source_number INTEGER",
        [],
    ) {
        Ok(_) => {
            tracing::info!("added source_number column to tasks table");
        }
        Err(rusqlite::Error::SqliteFailure(e, Some(ref msg)))
            if e.extended_code == rusqlite::ffi::SQLITE_ERROR
                && msg.contains("duplicate column name") =>
        {
            // Column already exists — this is expected for existing databases
            tracing::debug!("source_number column already exists");
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to add source_number column");
            return Err(e);
        }
    }

    // Migration: add rejection_feedback column if it doesn't exist (issue #423)
    // This stores orchestrator feedback from PR rejections for delivery to re-dispatched agents.
    match conn.execute(
        "ALTER TABLE tasks ADD COLUMN rejection_feedback TEXT",
        [],
    ) {
        Ok(_) => {
            tracing::info!("added rejection_feedback column to tasks table");
        }
        Err(rusqlite::Error::SqliteFailure(e, Some(ref msg)))
            if e.extended_code == rusqlite::ffi::SQLITE_ERROR
                && msg.contains("duplicate column name") =>
        {
            // Column already exists — this is expected for existing databases
            tracing::debug!("rejection_feedback column already exists");
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to add rejection_feedback column");
            return Err(e);
        }
    }

    // Migration: add pr_title and pr_number columns to merge_queue (issue #589)
    // These store PR metadata directly so the UI can display titles even without a linked task.
    for (col, col_type) in &[("pr_title", "TEXT"), ("pr_number", "INTEGER")] {
        match conn.execute(
            &format!("ALTER TABLE merge_queue ADD COLUMN {} {}", col, col_type),
            [],
        ) {
            Ok(_) => {
                tracing::info!("added {} column to merge_queue table", col);
            }
            Err(rusqlite::Error::SqliteFailure(e, Some(ref msg)))
                if e.extended_code == rusqlite::ffi::SQLITE_ERROR
                    && msg.contains("duplicate column name") =>
            {
                tracing::debug!("{} column already exists in merge_queue", col);
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to add {} column to merge_queue", col);
                return Err(e);
            }
        }
    }

    // Migration: add UNIQUE constraint on merge_queue.pr_url (issue #464)
    // SQLite cannot add constraints to existing columns, so we create the unique index
    // if it doesn't already exist. The column-level UNIQUE in CREATE TABLE handles new DBs;
    // this handles existing DBs that already have the table.
    match conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_merge_queue_pr_url_unique ON merge_queue(pr_url)",
        [],
    ) {
        Ok(_) => {
            // Also drop the old non-unique index if it exists
            let _ = conn.execute("DROP INDEX IF EXISTS idx_merge_queue_pr_url", []);
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not create unique index on merge_queue.pr_url (duplicates may exist)");
        }
    }

    // Stamp the current data version so future runs can detect mismatches.
    write_version(conn, DATA_VERSION)?;

    Ok(())
}
