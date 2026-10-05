CREATE TABLE tasks (
    id TEXT PRIMARY KEY NOT NULL,
    agent_kind TEXT CHECK (agent_kind IS NULL OR agent_kind IN ('claude', 'pi')),
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE task_event_logs (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT UNIQUE NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    recorded_at_ms INTEGER NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    event_type TEXT GENERATED ALWAYS AS (json_extract(payload, '$.type')) STORED NOT NULL
);

CREATE INDEX task_event_logs_task_sequence ON task_event_logs(task_id, sequence);
CREATE INDEX task_event_logs_task_time ON task_event_logs(task_id, recorded_at_ms);
CREATE INDEX task_event_logs_task_type ON task_event_logs(task_id, event_type, sequence);

CREATE VIEW task_operator_signals AS
SELECT raised.sequence, raised.task_id, raised.recorded_at_ms AS raised_at_ms,
    json_extract(raised.payload, '$.data.signal.id') AS id,
    json_extract(raised.payload, '$.data.signal.kind') AS kind,
    json_extract(raised.payload, '$.data.signal.priority') AS priority,
    json_extract(raised.payload, '$.data.signal.title') AS title,
    json_extract(raised.payload, '$.data.signal.body') AS body,
    EXISTS (
        SELECT 1 FROM task_event_logs AS read
        WHERE read.task_id = raised.task_id
            AND read.event_type IN ('operator_signal_read', 'operator_signal_acknowledged', 'operator_signal_answered', 'operator_signal_confirmed')
            AND json_extract(read.payload, '$.data.signal_id') = json_extract(raised.payload, '$.data.signal.id')
    ) AS read,
    EXISTS (
        SELECT 1 FROM task_event_logs AS resolved
        WHERE resolved.task_id = raised.task_id
            AND resolved.event_type IN ('operator_signal_acknowledged', 'operator_signal_answered', 'operator_signal_confirmed')
            AND json_extract(resolved.payload, '$.data.signal_id') = json_extract(raised.payload, '$.data.signal.id')
    ) AS resolved
FROM task_event_logs AS raised
WHERE raised.event_type = 'operator_signal_raised';
