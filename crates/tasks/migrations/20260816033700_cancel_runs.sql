-- Cancelling a scout or a build that is already in flight (#876).
--
-- The problem was never that a VM could not be destroyed — `deallocate` has
-- always been one call away, and killing the container by hand was the
-- recourse people actually reached for. It is that destroying the VM answers
-- the wrong question. The dispatcher following the run is parked on a vm-pool
-- event stream that, once the VM is gone, will never produce another event: the
-- session stays `running`, the serial build lane stays occupied, and nothing in
-- the event log says the cancel took. So a cancel has to reach the *drain*, and
-- the drain is inside a process that may not be the one taking the request.
--
-- Hence a durable row rather than an in-memory channel. Whoever is following
-- the run reads this table — on a broadcast wake-up in the common case, and on
-- a slow poll for the two cases a broadcast cannot cover: a subscriber that
-- lagged, and a request made while nothing was watching, which is exactly what
-- a run picked back up by `resume_in_flight` after a restart is.
--
-- Keyed `(kind, id)` and inserted with `INSERT OR IGNORE`, so a double-click is
-- idempotent and the *first* request is the one on record — the actor and
-- rationale that end up in the run's `exit_reason` are the ones that stopped
-- it, not whichever arrived last.
--
-- The row is never deleted. Run ids are never reused, and a cancel that arrived
-- a moment too late (the run concluded on its own in the same breath) is worth
-- keeping: it is a decision somebody made, and the ledger row beside it points
-- at the reasoning.
CREATE TABLE cancellations (
    kind         TEXT NOT NULL,             -- session | build
    id           TEXT NOT NULL,             -- session id or build id
    actor        TEXT NOT NULL,             -- human | orchestrator
    rationale    TEXT,
    -- The `decisions` row carrying the reasoning. Nullable only because the
    -- ledger write and this one are separate statements; the handler writes the
    -- ledger first, deliberately, so an orchestrator cancel with no rationale is
    -- refused before any work is destroyed.
    decision_seq INTEGER,
    requested_at TEXT NOT NULL,
    PRIMARY KEY (kind, id)
);

-- Without this row the capability grants nothing: `Store::charter_entry` reads
-- a missing row as `off`, so the enum variant alone would produce a capability
-- that is silently refused, and the failure would look like a bug in
-- `authorize` rather than a missing migration.
--
-- `live` and uncapped, like the other eight (see 0016). The charter is a kill
-- switch, not a promotion ladder, and a cancel that waits for a human arrives
-- after the run it was meant to stop has finished — which is not a safer
-- version of this feature, it is no version of it. What makes it safe is the
-- same thing that makes the rest safe: a mandatory rationale, a `decisions` row
-- naming the actor, and a cancelled run that costs the work nothing — no
-- dispatch attempt, no build strike, the specs back to `approved` and the task
-- back in the pipeline.
--
-- Its own switch rather than part of `dispatch_builds`, because starting work
-- and stopping it have unrelated failure modes: one spends a VM hour, the other
-- throws one away, and a human who trusts the orchestrator with one and not the
-- other has to be able to say so.
INSERT INTO orchestrator_charter (capability, level, params, updated_at) VALUES
    ('cancel_runs', 'live', NULL, '1970-01-01T00:00:00Z');
