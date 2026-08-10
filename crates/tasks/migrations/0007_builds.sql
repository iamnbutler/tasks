-- Builder v0: one serial run over a set of approved specs -> one branch, one
-- PR. `build_specs` (not a builds.spec_id column) is the point: the model is
-- set-shaped even though v0 usually carries one spec. `position` is the
-- spec-queue order at request time.
--
-- Everything stored is Tasks-owned or immutable. `pr_number` is an
-- identifier, never a state -- PR mergeability / checks / open-closed are
-- GitHub's, queried at decision time.
CREATE TABLE builds (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    vm_id TEXT,
    branch TEXT NOT NULL,
    base_branch TEXT NOT NULL,
    base_sha TEXT,
    head_sha TEXT,
    pr_number INTEGER,
    status TEXT NOT NULL DEFAULT 'queued',
    summary TEXT,
    files_touched TEXT NOT NULL DEFAULT '[]',
    exit_reason TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    completed_at TEXT
);

CREATE TABLE build_specs (
    build_id TEXT NOT NULL REFERENCES builds(id) ON DELETE CASCADE,
    spec_id TEXT NOT NULL REFERENCES specs(id),
    position INTEGER NOT NULL,
    PRIMARY KEY (build_id, spec_id)
);

CREATE INDEX idx_builds_status ON builds(status);
CREATE INDEX idx_build_specs_spec ON build_specs(spec_id);
