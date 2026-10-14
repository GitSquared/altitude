-- Read facts as recorded. Absence leaves the previous value; JSON null is an explicit value.
SELECT tasks.id, tasks.agent_kind, tasks.host,
    EXISTS (SELECT 1 FROM task_event_logs WHERE task_id = tasks.id AND event_type IN ('operator_signal_confirmed', 'task_completed')) AS accepted,
    (SELECT MAX(sequence) FROM task_event_logs WHERE task_id = tasks.id) AS last_event_sequence,
    (SELECT payload ->> '$.data.intent'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.intent') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS intent,
    (SELECT payload ->> '$.data.title'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.title') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS title,
    (SELECT payload ->> '$.data.identity_id'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND event_type = 'intent_expressed'
     ORDER BY sequence ASC LIMIT 1) AS operator_id,
    (SELECT payload ->> '$.data.identity_label'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND event_type = 'intent_expressed'
     ORDER BY sequence ASC LIMIT 1) AS operator_label,
    (SELECT payload ->> '$.data.environment.name'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.environment.name') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS environment_name,
    (SELECT payload ->> '$.data.environment.setup_command'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.environment.setup_command') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS environment_setup_command,
    (SELECT payload ->> '$.data.agent_conversation_id'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.agent_conversation_id') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS agent_conversation_id,
    CASE WHEN EXISTS (SELECT 1 FROM task_event_logs WHERE task_id = tasks.id AND event_type IN ('operator_signal_confirmed', 'task_completed'))
         THEN 'completed'
         ELSE (SELECT payload ->> '$.data.execution_status'
               FROM task_event_logs
               WHERE task_id = tasks.id AND json_type(payload, '$.data.execution_status') IS NOT NULL
               ORDER BY sequence DESC LIMIT 1)
    END AS execution_status,
    (SELECT payload ->> '$.data.execution_pause_reason'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.execution_pause_reason') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS execution_pause_reason,
    (SELECT payload ->> '$.data.execution_mode'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.execution_mode') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS execution_mode,
    (SELECT payload ->> '$.data.operator_attention_level'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.operator_attention_level') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS operator_attention_level,
    (SELECT payload ->> '$.data.sandbox_id'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.sandbox_id') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS sandbox_id,
    (SELECT payload ->> '$.data.sandbox_available'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.sandbox_available') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS sandbox_available,
    (SELECT payload ->> '$.data.snapshot_path'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.snapshot_path') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS snapshot_path,
    (SELECT payload ->> '$.data.snapshot_available'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.snapshot_available') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS snapshot_available,
    (SELECT payload ->> '$.data.error'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.error') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS last_error,
    (SELECT payload ->> '$.data.summary'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.summary') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS latest_turn_summary,
    (SELECT payload -> '$.data'
     FROM task_event_logs
     WHERE task_id = tasks.id AND event_type = 'operator_signal_answered'
     ORDER BY sequence DESC LIMIT 1) AS latest_operator_answer,
    (SELECT payload -> '$.data.issue.issue.reference'
     FROM task_event_logs
     WHERE task_id = tasks.id AND event_type = 'tracker_issue_bound'
     ORDER BY sequence DESC LIMIT 1) AS tracker_issue,
    (SELECT kind
     FROM task_operator_signals
     WHERE task_id = tasks.id
       AND resolved = 0
       AND kind IN ('decision', 'in_review')
     ORDER BY raised_at_ms DESC LIMIT 1) AS awaiting_signal_kind,
    (SELECT count(*) FROM task_operator_signals
     WHERE task_id = tasks.id AND read = 0 AND resolved = 0) AS unread_operator_signal_count,
    (SELECT json_group_array(json(contribution))
     FROM task_code_contributions WHERE task_id = tasks.id) AS contributions,
    (SELECT json_group_array(json(observation))
     FROM task_code_contribution_observations
     WHERE task_id = tasks.id AND sequence IN (
         SELECT MAX(sequence) FROM task_code_contribution_observations
         WHERE task_id = tasks.id
         GROUP BY json_extract(observation, '$.contribution_id')
     )) AS observations
FROM tasks
