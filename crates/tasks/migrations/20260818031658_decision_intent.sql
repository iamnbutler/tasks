-- Intent-then-confirm on the decisions ledger (#964).
--
-- #957 closed the half of the attribution gap that can be *refused*: every
-- charter-gated handler applies `require_rationale` inside `authorize`, before
-- it touches GitHub, so a 4xx is a genuine no-op. This closes the half that
-- cannot be. Every write that lands in somebody else's system ran the effect
-- and *then* the `record_decision` explaining it, so a SQLite error, a panic
-- or a SIGKILL in between left a real artifact upstream that nothing in the
-- ledger accounts for.
--
-- Recording first stays refused: a row claiming an effect a failed call never
-- had makes every row suspect, where a missing row leaves one artifact
-- unexplained. So the window is *represented* instead.
--
-- One row with a state column, and deliberately not an intent row plus a
-- confirmation row. Every existing aggregate over this table would
-- double-count under two rows -- `orchestrator_actions_today` (the daily cap),
-- `has_decision`, and the `NOT EXISTS (SELECT 1 FROM decisions ...)` behind
-- the `ReviewSpec` obligation -- and each would need an "and not the intent
-- one" clause the next query written against this table would forget. One row
-- keeps every reader correct without being taught anything.
--
-- `DEFAULT 'applied'` is load-bearing twice. It leaves every historical row
-- untouched, and it leaves every *store-only* decision (a review verdict, a
-- queueing, a cancel, a build request) alone by construction: those are
-- written in the same transaction as the state change they authorize, so
-- there is no window to represent. Only the ten sites whose effect lands in
-- someone else's system ever write 'pending'.
--
-- Append-only survives where it matters. Actor, action, rationale, evidence
-- and subject are never rewritten. `state` moves once, guarded by
-- `WHERE state = 'pending'`, so a settled row cannot be re-settled and two
-- reconciliations cannot silently disagree.
ALTER TABLE decisions ADD COLUMN state TEXT NOT NULL DEFAULT 'applied';

-- What the effect produced or refused with -- an issue number, a merge SHA,
-- GitHub's own error message. Merged with `json_patch` rather than replaced
-- when a row settles, so the error a refused call wrote here survives the
-- reconciliation that later finds the artifact. Also carries the *intent*
-- (what was about to be sent) from the moment the row is written, because
-- that is what a reconciler needs in order to look the artifact up.
ALTER TABLE decisions ADD COLUMN outcome TEXT;

-- When the row left 'pending'. NULL while it is still open, and NULL on every
-- row that was never pending at all -- including all of history, which is why
-- this is not `NOT NULL DEFAULT ...`.
ALTER TABLE decisions ADD COLUMN settled_at TEXT;

-- Partial, because the interesting set is tiny and the table is not: the
-- obligation pass and `GET /decisions?pending=true` both ask only this.
CREATE INDEX decisions_pending_idx ON decisions(seq) WHERE state = 'pending';
