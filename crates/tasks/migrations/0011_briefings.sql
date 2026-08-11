-- Home briefings: three LLM-written narrative slots (state_of_project /
-- changes / issues), generated on demand by one-shot read-only agents.
--
-- The persisted text is a CACHE WITH A VISIBLE DATE, never state: GitHub
-- facts inside it were queried at generation time and are labeled with
-- `generated_at` — this table does not violate the "never persist a
-- GitHub-owned fact" rule because nothing here is ever read back as truth.
-- `event_high_water` records the newest event seq at generation start, for
-- later regeneration gating (skip when nothing moved).
CREATE TABLE briefings (
    section TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    generated_at TEXT NOT NULL,
    event_high_water INTEGER NOT NULL DEFAULT 0
);
