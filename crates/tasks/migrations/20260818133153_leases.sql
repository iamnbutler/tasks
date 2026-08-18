-- Short-lived credential leases (docs/plans/2026-08-18-credential-custody.md).
--
-- What a VM receives at dispatch is not a key but a lease: a random bearer
-- token the broker exchanges, per request, for the real ANTHROPIC_API_KEY /
-- GITHUB_TOKEN it never hands out. Rows rather than process state because the
-- process that mints one need not be the one serving it: a reattach after a
-- restart extends the same lease by subject.
--
-- The token itself is never stored — token_hash is its SHA-256, so a copied
-- database yields no live credential. Expiry is the backstop revocation can
-- forget: every read re-checks expires_at, and rows long past it are pruned
-- at boot rather than kept as history (a lease is operational state, not a
-- decision).
CREATE TABLE leases (
    id TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    -- Space-separated scope names: `anthropic`, `git-read`, `git-write`.
    scopes TEXT NOT NULL,
    -- `owner/name` the git scopes are bound to; NULL for a lease with no git
    -- scope.
    repo TEXT,
    -- What this lease was minted for: `scout` (subject = session id),
    -- `build` (subject = build id), `land` (the server's own push window,
    -- subject = build id).
    subject_kind TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    revoked_at TEXT
);

CREATE INDEX idx_leases_subject ON leases(subject_kind, subject_id);
