CREATE TABLE projects (
    id          TEXT PRIMARY KEY,
    repo_owner  TEXT NOT NULL,
    repo_name   TEXT NOT NULL,
    added_at    TEXT NOT NULL,
    UNIQUE(repo_owner, repo_name)
);

CREATE TABLE tasks (
    id                TEXT PRIMARY KEY,
    project_id        TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    gh_issue_number   INTEGER NOT NULL,
    title             TEXT NOT NULL,
    body              TEXT NOT NULL,
    labels            TEXT NOT NULL,              -- JSON array
    gh_state          TEXT NOT NULL,              -- open / closed
    state             TEXT NOT NULL,              -- new / scouting / spec_ready / queued / done / rejected
    priority          INTEGER NOT NULL DEFAULT 0,
    ingested_at       TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    UNIQUE(project_id, gh_issue_number)
);

CREATE INDEX tasks_state_idx ON tasks(state);
CREATE INDEX tasks_project_idx ON tasks(project_id);

CREATE TABLE sessions (
    id            TEXT PRIMARY KEY,
    task_id       TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    vm_id         TEXT,
    branch        TEXT NOT NULL,
    status        TEXT NOT NULL,                  -- running / scout_succeeded / scout_failed / cancelled
    started_at    TEXT NOT NULL,
    completed_at  TEXT,
    exit_reason   TEXT
);

CREATE INDEX sessions_task_idx ON sessions(task_id);
CREATE INDEX sessions_status_idx ON sessions(status);

CREATE TABLE specs (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    task_id         TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    content         TEXT NOT NULL,
    complexity      TEXT NOT NULL,                -- simple / medium / complex
    files_touched   TEXT NOT NULL,                -- JSON array
    created_at      TEXT NOT NULL
);

CREATE INDEX specs_task_idx ON specs(task_id);

CREATE TABLE spec_queue (
    spec_id                  TEXT PRIMARY KEY REFERENCES specs(id) ON DELETE CASCADE,
    status                   TEXT NOT NULL,       -- pending_review / approved / needs_revision / blocked / rejected
    rank                     INTEGER,
    approved_at              TEXT,
    feedback                 TEXT,
    blocking_dependencies    TEXT NOT NULL         -- JSON array of task IDs
);

CREATE INDEX spec_queue_status_idx ON spec_queue(status);

CREATE TABLE events (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp   TEXT NOT NULL,
    payload     TEXT NOT NULL                     -- JSON
);

CREATE TABLE mode (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    mode        TEXT NOT NULL,                    -- play / pause / stop
    updated_at  TEXT NOT NULL
);

INSERT INTO mode (id, mode, updated_at) VALUES (1, 'pause', datetime('now'));
