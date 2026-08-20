-- Has this install's owner ever been shown what unattended operation means?
-- (#993)
--
-- **Not a gate.** Nothing in the server reads this row to decide whether an
-- action may happen: no handler refuses because of it, no dispatcher consults
-- it, and the pipeline behaves identically whether it is set or not. It exists
-- because a client needs to tell "explain this once" from "do not explain it
-- again", and the alternative triggers are all wrong — `TASKS_DEFAULT_MODE`
-- overwrites the stored mode on every boot, so "the mode became play" fires on
-- every restart, and a per-client file would re-explain on every new machine
-- pointed at the same server while staying silent on the one that was told.
--
-- One row, pinned by `CHECK (id = 1)`: this is a property of the install, not
-- of a user, a session or a client. There is no user model here.
--
-- `acknowledged_at` is written once and never overwritten — the store uses
-- INSERT OR IGNORE, so the **first** acknowledgement stands. A second click
-- from another surface must not rewrite "when was this person told" into a
-- later, wronger answer. There is deliberately no un-acknowledge: it is a fact
-- about something that happened, and facts of that shape do not un-happen.
CREATE TABLE autonomy_notice (
    id             INTEGER PRIMARY KEY CHECK (id = 1),
    acknowledged_at TEXT NOT NULL
);
