-- Agent enrollments: the device-code flow that gives an external agent a
-- voice in the orchestrator conversation.
--
-- Before this, `POST /orchestrator/messages` had exactly two speakers: the
-- human (any request without a verified `X-Tasks-Actor`) and the pipeline
-- (`[pipeline]` event turns the server writes itself). An external agent —
-- a Claude Code session the human points at a task — had no honest way in:
-- posting to the messages route landed as the human, which is impersonation
-- the orchestrator has no way to see through, and the human relaying by hand
-- is the telephone game this exists to end.
--
-- The shape is the broker lease's, one level up: a mint returns a random
-- 256-bit bearer code exactly once, the row keeps only its SHA-256, and the
-- code is bound to a *name* and an expiry rather than to a VM. Presenting a
-- valid code turns a message into an `event` turn headed `[agent <name>]`;
-- presenting an invalid, expired, or revoked one is a 403 and the message is
-- discarded — a failed claim is never demoted to "the human", the same rule
-- `X-Tasks-Actor` follows. No header at all stays the human's path, because
-- the human is never gated.
--
-- What an enrollment conveys is a voice, not authority: an agent turn is
-- input the orchestrator weighs (and is told to weigh as a peer's unverified
-- claims), never a charter-gated write. That is why `enroll_agents` can ship
-- `live` like the other nine — the mint is ledgered with a rationale, the
-- feed announces it, and the recourse is `POST /agents/{id}/revoke` under
-- the same capability.
--
-- Rows are never deleted. A revoked or expired enrollment is the audit trail
-- for turns that already happened under its name; `revoked_at` and
-- `expires_at` are what end its life, and `token_hash` stays UNIQUE so a
-- code can never mean two names.

CREATE TABLE agent_enrollments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    minted_by TEXT NOT NULL,             -- 'human' | 'orchestrator'
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    revoked_at TEXT,
    last_used_at TEXT
);

INSERT INTO orchestrator_charter (capability, level, params, updated_at) VALUES
    ('enroll_agents', 'live', NULL, '1970-01-01T00:00:00Z');
