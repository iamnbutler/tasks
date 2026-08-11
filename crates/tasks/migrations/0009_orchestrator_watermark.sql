-- Proactive orchestrator: pipeline events become 'event' turns in the
-- conversation, so "unanswered" can no longer mean "after the last assistant
-- turn" -- a message appended while the agent is mid-turn (minutes, now that
-- it's a full dev agent) would land below the reply's seq and be silently
-- skipped. `answered_through` is an explicit watermark: the tick records the
-- highest seq it actually put in the prompt, and everything above it is
-- unanswered regardless of arrival order.
ALTER TABLE orchestrator ADD COLUMN answered_through INTEGER NOT NULL DEFAULT 0;

-- Backfill with the old rule so existing conversations stay settled.
UPDATE orchestrator SET answered_through = COALESCE(
    (SELECT MAX(seq) FROM orchestrator_messages WHERE role = 'assistant'), 0);
