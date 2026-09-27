CREATE TABLE tasks (
    id TEXT PRIMARY KEY NOT NULL,
    agent_kind TEXT CHECK (agent_kind IS NULL OR agent_kind = 'claude'),
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

-- Written only alongside appended events, through TaskCurrentState's reducer.
CREATE TABLE task_statuses (
    task_id TEXT PRIMARY KEY NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    intent TEXT NOT NULL,
    agent_conversation_id TEXT,
    execution_status TEXT NOT NULL CHECK (execution_status IN ('pending', 'running', 'paused', 'completed', 'failed')),
    execution_pause_reason TEXT CHECK (execution_pause_reason IN ('awaiting_operator', 'application_shutdown')),
    execution_controller TEXT NOT NULL CHECK (execution_controller IN ('altitude', 'operator')),
    operator_attention_level TEXT NOT NULL CHECK (operator_attention_level IN ('nominal', 'requested', 'alarm')),
    sandbox_id TEXT,
    snapshot_path TEXT,
    last_error TEXT,
    latest_turn_summary TEXT,
    last_event_sequence INTEGER NOT NULL REFERENCES task_event_logs(sequence),
    CHECK ((execution_status = 'paused') = (execution_pause_reason IS NOT NULL))
);

CREATE VIEW task_operator_notices AS
SELECT raised.sequence, raised.task_id,
    json_extract(raised.payload, '$.data.notice.id') AS id,
    json_extract(raised.payload, '$.data.notice.level') AS level,
    json_extract(raised.payload, '$.data.notice.title') AS title,
    json_extract(raised.payload, '$.data.notice.body') AS body,
    EXISTS (
        SELECT 1 FROM task_event_logs AS acknowledged
        WHERE acknowledged.task_id = raised.task_id
            AND acknowledged.event_type = 'operator_notice_acknowledged'
            AND json_extract(acknowledged.payload, '$.data.notice_id') = json_extract(raised.payload, '$.data.notice.id')
    ) AS acknowledged
FROM task_event_logs AS raised
WHERE raised.event_type = 'operator_notice_raised';
