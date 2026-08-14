-- The charter: what the orchestrator may do, as rows rather than as prose.
--
-- Authority stated in a prompt is authority that a failed `--resume` can
-- silently reset, and that a long conversation can talk itself out of. So the
-- statement lives here, the system prompt's authority section is *generated*
-- from these rows every turn, and the server enforces them on the mutating
-- endpoints. There is exactly one place that says what is allowed, and it is
-- not in the context window.
--
-- Three levels, and `shadow` is the interesting one. It does not mean silent:
-- the orchestrator calls the endpoint exactly as it would when live, the
-- server records the decision it *would* have made, and then does not apply
-- it. That is deliberately not "the prompt tells it to narrate instead of
-- acting" -- prompt compliance is the thing that degrades, and shadow exists
-- precisely to gather evidence about a capability nobody trusts yet. Making
-- it a server behaviour means the calibration data accrues whether the agent
-- cooperates or not.
--
-- The seeded levels are chosen so that merging this changes what the
-- orchestrator may do as little as possible, and only in the safe direction:
--
--   * `queue_tasks` and `dispatch_builds` are `live` because they already
--     worked. The charter exists to govern new autonomy, not to quietly
--     remove function that was there yesterday.
--   * `capture_work` is `live` with a cap, which is *stricter* than the
--     status quo rather than looser: the orchestrator can already file issues
--     with its own `gh` credential, entirely outside this system, which is
--     the side channel that produced the `Closes #N` incident. Routing it
--     here adds a ledger row, an event, and a daily bound.
--   * `auto_review_specs` starts in `shadow`. Today the only thing stopping
--     an autonomous verdict is a sentence in the prompt, and this replaces
--     that sentence with a row -- so `live` would grant more than existed
--     before. Shadow keeps the verdict the human's while the ledger fills up
--     with the verdicts it *would* have rendered, which is the evidence the
--     eventual flip should rest on.
--   * `retire_work` starts in `shadow`: it is genuinely new, and "no longer
--     relevant" is the one custodial judgment with no cheap evidence
--     standard.
--
-- Flipping is a human write, one capability at a time, on evidence rather
-- than nerve.
CREATE TABLE orchestrator_charter (
    capability TEXT PRIMARY KEY,       -- capture_work | retire_work | queue_tasks | dispatch_builds | auto_review_specs
    level      TEXT NOT NULL,          -- off | shadow | live
    -- JSON. Today: {"daily_limit": N} -- a mechanical floor, not a judgment.
    -- Policy contributes caps and budgets; it never contributes verdicts.
    params     TEXT,
    updated_at TEXT NOT NULL
);

INSERT INTO orchestrator_charter (capability, level, params, updated_at) VALUES
    ('capture_work',      'live',   '{"daily_limit": 5}',  '1970-01-01T00:00:00Z'),
    ('queue_tasks',       'live',   '{"daily_limit": 10}', '1970-01-01T00:00:00Z'),
    ('dispatch_builds',   'live',   NULL,                  '1970-01-01T00:00:00Z'),
    ('auto_review_specs', 'shadow', NULL,                  '1970-01-01T00:00:00Z'),
    ('retire_work',       'shadow', NULL,                  '1970-01-01T00:00:00Z');

-- Whether this decision was actually applied. A shadow decision is a real
-- entry in the ledger -- it is the record of what the orchestrator judged --
-- but the state it describes never changed, and reading the two as the same
-- thing would make an evaluation look like a history.
--
-- Existing rows are all enforced: nothing could shadow before this migration.
ALTER TABLE decisions ADD COLUMN enforced INTEGER NOT NULL DEFAULT 1;
