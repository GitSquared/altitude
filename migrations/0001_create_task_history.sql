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
                        'environment_name', payload ->> '$.data.environment.name',
                        'environment_setup_command', payload ->> '$.data.environment.setup_command',
                        'agent_conversation_id', payload ->> '$.data.agent_conversation_id',
                        'execution_status', 'pending',
                        'execution_pause_reason', NULL,
                        'execution_mode', 'autopilot',
                        'operator_attention_level', 'nominal',
                        'sandbox_id', NULL,
                        'snapshot_path', NULL,
                        'last_error', NULL,
                        'latest_turn_summary', NULL
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
                        CASE WHEN payload ->> '$.data.operator_attention_level' IS NULL
                            THEN json_object('latest_turn_summary', payload ->> '$.data.summary')
                            ELSE json_object(
                                'latest_turn_summary', payload ->> '$.data.summary',
                                'operator_attention_level', payload ->> '$.data.operator_attention_level'
                            )
                        END
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
    ) AS state,
    (SELECT max(sequence) FROM task_event_logs WHERE task_id = tasks.id) AS last_event_sequence
    FROM tasks
)
SELECT task_id, state,
    state ->> '$.intent' AS intent,
    state ->> '$.environment_name' AS environment_name,
    state ->> '$.environment_setup_command' AS environment_setup_command,
    state ->> '$.agent_conversation_id' AS agent_conversation_id,
    state ->> '$.execution_status' AS execution_status,
    state ->> '$.execution_pause_reason' AS execution_pause_reason,
    state ->> '$.execution_mode' AS execution_mode,
    state ->> '$.operator_attention_level' AS operator_attention_level,
    state ->> '$.sandbox_id' AS sandbox_id,
    state ->> '$.snapshot_path' AS snapshot_path,
    state ->> '$.last_error' AS last_error,
    state ->> '$.latest_turn_summary' AS latest_turn_summary,
    last_event_sequence
FROM projected_tasks;

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
