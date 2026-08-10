-- 0005 renamed the task-state vocabulary but only remapped tasks.state; the
-- event log still holds task_state_changed payloads speaking the old one, and
-- the server refuses to deserialize its own history (GET /events -> 500).
--
-- Rewriting the log is safe here: this is a vocabulary rename of our own
-- data, not a change to what happened. json_set targets exactly the affected
-- keys of exactly the affected kind, so free-text fields (notes) are never
-- touched. Old `queued` maps to `ready_to_build` per 0005's table.
UPDATE events SET payload = json_set(payload, '$.from', 'backlog')
    WHERE json_extract(payload, '$.kind') = 'task_state_changed'
      AND json_extract(payload, '$.from') = 'new';
UPDATE events SET payload = json_set(payload, '$.to', 'backlog')
    WHERE json_extract(payload, '$.kind') = 'task_state_changed'
      AND json_extract(payload, '$.to') = 'new';
UPDATE events SET payload = json_set(payload, '$.from', 'in_review')
    WHERE json_extract(payload, '$.kind') = 'task_state_changed'
      AND json_extract(payload, '$.from') = 'spec_ready';
UPDATE events SET payload = json_set(payload, '$.to', 'in_review')
    WHERE json_extract(payload, '$.kind') = 'task_state_changed'
      AND json_extract(payload, '$.to') = 'spec_ready';
UPDATE events SET payload = json_set(payload, '$.from', 'ready_to_build')
    WHERE json_extract(payload, '$.kind') = 'task_state_changed'
      AND json_extract(payload, '$.from') = 'queued';
UPDATE events SET payload = json_set(payload, '$.to', 'ready_to_build')
    WHERE json_extract(payload, '$.kind') = 'task_state_changed'
      AND json_extract(payload, '$.to') = 'queued';
