-- Two names this store can be holding because they were recorded before the turn
-- that raised a signal replaced the report: the report's execution mode and the
-- alarm signal kind. Both are decoded by type when a task's events are read, so an
-- event still naming one is a task that cannot be opened at all.
UPDATE task_event_logs
SET payload = json_set(payload, '$.data.execution_mode', 'autopilot')
WHERE json_extract(payload, '$.data.execution_mode') = 'autopilot_report';

UPDATE task_event_logs
SET payload = json_set(payload, '$.data.signal.kind', 'update')
WHERE json_extract(payload, '$.data.signal.kind') = 'alarm';

-- A report turn left a row of its own, holding the summary the operator read and
-- what the report asked for next. There is no event to rewrite it into: the turn is
-- gone. Its rows are moved out of the log rather than deleted, because they are the
-- record of what those turns did, and reading a task's history can only decode what
-- still exists.
CREATE TABLE retired_task_events (
    sequence INTEGER PRIMARY KEY,
    id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    payload TEXT NOT NULL,
    retired_because TEXT NOT NULL
);

INSERT INTO retired_task_events (sequence, id, task_id, recorded_at_ms, payload, retired_because)
SELECT sequence, id, task_id, recorded_at_ms, payload,
    'the report turn it recorded no longer exists'
FROM task_event_logs
WHERE event_type = 'agent_turn_reported';

DELETE FROM task_event_logs WHERE event_type = 'agent_turn_reported';
