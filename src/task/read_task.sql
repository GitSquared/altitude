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
    (SELECT payload -> '$.data.project'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.project') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS project,
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
    (SELECT json_object(
         'id', 'm' || (
             SELECT count(*)
             FROM task_event_logs AS earlier
             WHERE earlier.task_id = tasks.id
               AND earlier.sequence <= e.sequence
               AND (
                   (earlier.event_type = 'operator_signal_raised'
                    AND json_extract(earlier.payload, '$.data.signal.id')
                        = json_extract(e.payload, '$.data.signal_id'))
                   OR (earlier.event_type = 'operator_signal_message'
                    AND json_extract(earlier.payload, '$.data.signal_id')
                        = json_extract(e.payload, '$.data.signal_id'))
               )
         ),
         'author', 'operator',
         'body', json_extract(e.payload, '$.data.body'),
         'at_ms', e.recorded_at_ms)
     FROM task_event_logs AS e
     WHERE e.task_id = tasks.id
       AND e.event_type = 'operator_signal_message'
       AND json_extract(e.payload, '$.data.author') = 'operator'
     ORDER BY e.sequence DESC LIMIT 1) AS latest_operator_message,
    (SELECT payload -> '$.data.issue.issue.reference'
     FROM task_event_logs
     WHERE task_id = tasks.id AND event_type = 'tracker_issue_bound'
     ORDER BY sequence DESC LIMIT 1) AS tracker_issue,
    (SELECT kind
     FROM task_operator_signals
     WHERE task_id = tasks.id
       AND resolved = 0
       AND kind IN ('blocked', 'in_review')
     ORDER BY raised_at_ms DESC LIMIT 1) AS blocking_signal_kind,
    (SELECT count(*) FROM task_operator_signals
     WHERE task_id = tasks.id AND resolved = 0) AS unresolved_signal_count,
    (SELECT json_object(
         'signal', json(payload -> '$.data.signal'),
         'agent_run_id', payload -> '$.data.agent_run_id')
     FROM task_deferred_reviews
     WHERE task_id = tasks.id
     ORDER BY sequence DESC LIMIT 1) AS deferred_review,
    (SELECT json_extract(cleared.payload, '$.data.reason')
     FROM task_event_logs AS cleared
     WHERE cleared.task_id = tasks.id
       AND cleared.event_type = 'review_deferral_cleared'
       AND cleared.sequence > COALESCE(
         (SELECT MAX(sequence) FROM task_event_logs
          WHERE task_id = tasks.id AND event_type = 'agent_run_started'), 0)
     ORDER BY cleared.sequence DESC LIMIT 1) AS review_wake_reason,
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
