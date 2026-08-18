-- A gauge needs a scale, and the agent already reports one.
--
-- 0018 gave us an honest absolute reading (`last_context_tokens`: the prompt
-- behind the last main-chain model call) but nothing to read it *against*, so
-- a client showing it could only print a number. The context window is not
-- ours to guess -- a model->window table in our code is a fact owned
-- elsewhere, and it goes stale the next time a model ships. Claude Code
-- reports it: the stream-json `result` record carries `modelUsage`, keyed by
-- model id, and every entry states its own `contextWindow`. So these columns
-- are transcription, not inference.
--
-- `model_id` is the wire id (`claude-opus-5[1m]`) rather than the canonical
-- name, because the suffix is the part that distinguishes a 1M session from a
-- 200k one, and dropping it would leave the id unable to explain its own
-- window.
ALTER TABLE orchestrator_sessions ADD COLUMN model_id TEXT;
ALTER TABLE orchestrator_sessions ADD COLUMN context_window INTEGER;

-- The composition of `last_context_tokens`, off the same record, so the three
-- sum to it exactly. Cached and fresh are the same tokens to a context window
-- and very different tokens to a bill, which is the whole reason to split
-- them: on a long resumed session the cache is nearly all of it, and a gauge
-- that showed only `input_tokens` would read near zero on a session holding
-- 400k.
ALTER TABLE orchestrator_sessions ADD COLUMN last_input_tokens INTEGER;
ALTER TABLE orchestrator_sessions ADD COLUMN last_cache_read_tokens INTEGER;
ALTER TABLE orchestrator_sessions ADD COLUMN last_cache_creation_tokens INTEGER;

-- Compaction, counted rather than described. Claude Code compacts a session
-- in place -- same session id, and our own gauge simply reads lower on the
-- next turn -- so without this a compaction is indistinguishable from a
-- shorter turn, and "is compaction working?" can only be answered by diffing
-- a log. The stream says so directly: a `system`/`status` record carries
-- `compact_result`, and only an `ok` is counted here.
--
-- A count and a timestamp, not a history: what a human needs is whether the
-- mechanism has fired and how recently, and the per-turn readings are already
-- in the log for anyone who wants the curve.
ALTER TABLE orchestrator_sessions ADD COLUMN compactions INTEGER NOT NULL DEFAULT 0;
ALTER TABLE orchestrator_sessions ADD COLUMN last_compacted_at TEXT;
