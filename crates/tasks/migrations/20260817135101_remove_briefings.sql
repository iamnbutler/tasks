-- remove_briefings
--
-- The Home section and the whole briefing subsystem are gone (#918), so the
-- cache table goes with them. It was only ever a cache with a visible date —
-- nothing here was read back as truth — so there is nothing to preserve.
DROP TABLE IF EXISTS briefings;

-- The load-bearing half. `EventPayload` is a strict internally-tagged enum and
-- `Store::event_from_row` deserializes it with a hard `?`, so a
-- `briefing_updated` row left behind after the variant is deleted makes
-- `GET /events` and `/events/stream` fail forever — the Activity feed dies
-- permanently on every database that ever generated a briefing.
--
-- Editing the log to retire a vocabulary has a precedent in-tree:
-- `0006_event_vocabulary.sql` rewrites `events.payload` with `json_extract`.
-- The `seq` gaps this leaves are harmless: every reader compares seq
-- (`WHERE seq >= ?`, `ORDER BY seq`, the orchestrator's `> answered_seq`
-- watermark) and none of them counts on contiguity.
DELETE FROM events WHERE json_extract(payload, '$.kind') = 'briefing_updated';
