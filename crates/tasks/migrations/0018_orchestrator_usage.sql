-- Two numbers wearing one column's name.
--
-- `last_context_tokens` was written from the stream-json `result` record's
-- usage, which aggregates every internal turn of a single `claude --print`
-- invocation -- and each of those turns re-reads the cached prefix. So it
-- measured what one tick *spent*, not what the session is *holding*: on a
-- live server it read 2.7M, against a context window a fraction of that.
--
-- The old values keep their true meaning under an honest name, and the gauge
-- starts over. `last_context_tokens` is now read from the last main-chain
-- assistant record's usage -- the prompt behind one model call, which is a
-- genuine absolute reading. It starts NULL rather than inheriting the tick
-- numbers: seeding a rotation threshold with nothing is safe, seeding it with
-- reinterpreted garbage is not.
--
-- 0012's comment on the original column is dated by this, and stays that way:
-- sqlx checksums applied migrations, so editing history breaks every existing
-- database. The correction lives here.
ALTER TABLE orchestrator_sessions RENAME COLUMN last_context_tokens TO last_tick_tokens;

ALTER TABLE orchestrator_sessions ADD COLUMN last_context_tokens INTEGER;
