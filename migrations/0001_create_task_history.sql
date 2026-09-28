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
            AND read.event_type IN ('operator_signal_read', 'operator_signal_acknowledged', 'operator_signal_answered')
            AND json_extract(read.payload, '$.data.signal_id') = json_extract(raised.payload, '$.data.signal.id')
    ) AS read,
    EXISTS (
        SELECT 1 FROM task_event_logs AS resolved
        WHERE resolved.task_id = raised.task_id
            AND resolved.event_type IN ('operator_signal_acknowledged', 'operator_signal_answered')
            AND json_extract(resolved.payload, '$.data.signal_id') = json_extract(raised.payload, '$.data.signal.id')
    ) AS resolved
FROM task_event_logs AS raised
WHERE raised.event_type = 'operator_signal_raised';

-- Each event changes only these properties. An explicit null clears a property;
-- an absent property leaves its previous value intact.
CREATE VIEW task_statuses AS
WITH projected_tasks AS (
    SELECT tasks.id AS task_id, (
        WITH event_changes AS (
            SELECT task_id, sequence,
                CASE event_type
                    WHEN 'intent_expressed' THEN json_object(
                        'intent', payload ->> '$.data.intent',
                        'agent_conversation_id', payload ->> '$.data.agent_conversation_id',
                        'execution_status', 'pending',
                        'execution_pause_reason', NULL,
                        'execution_mode', 'autopilot',
                        'operator_attention_level', 'nominal',
                        'sandbox_id', NULL,
                        'snapshot_path', NULL,
                        'last_error', NULL,
                        'latest_turn_summary', NULL,
                        'unread_operator_signal_count', 0
                    )
                    WHEN 'sandbox_provisioned' THEN json_object(
                        'sandbox_id', payload ->> '$.data.sandbox_id'
                    )
                    WHEN 'agent_run_started' THEN json_object(
                        'execution_status', 'running',
                        'execution_pause_reason', NULL,
                        'execution_mode', payload ->> '$.data.mode',
                        'sandbox_id', payload ->> '$.data.sandbox_id',
                        'last_error', NULL
                    )
                    WHEN 'agent_turn_reported' THEN
                        json_object(
                            'latest_turn_summary', payload ->> '$.data.summary',
                            'operator_attention_level', payload ->> '$.data.operator_attention_level'
                        )
                    WHEN 'operator_signal_answered' THEN json_object(
                        'operator_attention_level', 'nominal'
                    )
                    WHEN 'execution_paused' THEN json_object(
                        'execution_status', 'paused',
                        'execution_pause_reason', payload ->> '$.data.reason',
                        'sandbox_id', NULL,
                        'snapshot_path', payload ->> '$.data.snapshot_path',
                        'last_error', NULL
                    )
                    WHEN 'execution_recovered' THEN json_object(
                        'execution_status', 'pending',
                        'execution_pause_reason', NULL,
                        'sandbox_id', NULL,
                        'snapshot_path', payload ->> '$.data.snapshot_path',
                        'last_error', NULL
                    )
                    WHEN 'task_snapshot_saved' THEN json_object(
                        'sandbox_id', NULL,
                        'snapshot_path', payload ->> '$.data.snapshot_path'
                    )
                    WHEN 'execution_failed' THEN json_object(
                        'execution_status', 'failed',
                        'execution_pause_reason', NULL,
                        'last_error', payload ->> '$.data.error'
                    )
                    WHEN 'task_completed' THEN json_object(
                        'execution_status', 'completed',
                        'execution_pause_reason', NULL,
                        'sandbox_id', NULL,
                        'snapshot_path', NULL,
                        'last_error', NULL
                    )
                    ELSE '{}'
                END AS properties
            FROM task_event_logs
            WHERE task_id = tasks.id
        ), ranked_properties AS (
            SELECT task_id, property.key, property.value,
                row_number() OVER (
                    PARTITION BY task_id, property.key ORDER BY sequence DESC
                ) AS recency
            FROM event_changes, json_each(event_changes.properties) AS property
        )
        SELECT json_group_object(key, value)
        FROM ranked_properties
        WHERE recency = 1
    ) AS projected_state,
    (SELECT max(sequence) FROM task_event_logs WHERE task_id = tasks.id) AS last_event_sequence
    FROM tasks
)
SELECT task_id,
    json_set(
        projected_state,
        '$.unread_operator_signal_count',
        (SELECT count(*) FROM task_operator_signals WHERE task_id = projected_tasks.task_id AND read = 0 AND resolved = 0)
    ) AS state,
    projected_state ->> '$.intent' AS intent,
    projected_state ->> '$.agent_conversation_id' AS agent_conversation_id,
    projected_state ->> '$.execution_status' AS execution_status,
    projected_state ->> '$.execution_pause_reason' AS execution_pause_reason,
    projected_state ->> '$.execution_mode' AS execution_mode,
    projected_state ->> '$.operator_attention_level' AS operator_attention_level,
    projected_state ->> '$.sandbox_id' AS sandbox_id,
    projected_state ->> '$.snapshot_path' AS snapshot_path,
    projected_state ->> '$.last_error' AS last_error,
    projected_state ->> '$.latest_turn_summary' AS latest_turn_summary,
    last_event_sequence
FROM projected_tasks;
