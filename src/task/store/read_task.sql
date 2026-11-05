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
    (SELECT payload ->> '$.data.model'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.model') IS NOT NULL
     ORDER BY sequence DESC LIMIT 1) AS model,
    (SELECT payload -> '$.data.steps'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND event_type = 'plan_updated'
     ORDER BY sequence DESC LIMIT 1) AS plan,
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
    -- Whether this task is waiting on the operator, read from the threads that are
    -- open rather than stored beside them: one fact, one place.
    CASE WHEN EXISTS (
        SELECT 1 FROM task_operator_signals
        WHERE task_id = tasks.id AND resolved = 0 AND kind IN ('blocked', 'in_review')
    ) THEN 'requested' ELSE 'nominal' END AS operator_attention_level,
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
    (SELECT payload ->> '$.data.clears_at_ms'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND event_type = 'limit_held'
     ORDER BY sequence DESC LIMIT 1) AS resumes_at_ms,
    (SELECT payload ->> '$.data.window'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND event_type = 'limit_held'
     ORDER BY sequence DESC LIMIT 1) AS limit_window,
    (SELECT payload -> '$.data.standing'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND event_type = 'agent_standing_reported'
     ORDER BY sequence DESC LIMIT 1) AS standing,
    (SELECT json_object(
         'cost_usd_micros', coalesce(SUM(json_extract(payload, '$.data.cost_usd_micros')), 0),
         -- The total is the provider's figure only if every part of it was: one run
         -- priced by the harness's own table makes the sum an estimate.
         -- A flag has to leave here as JSON true or false: a number reads as a number
         -- on the way back in, and the facts would not decode at all.
         'cost_from_provider', CASE WHEN coalesce(MIN(json_extract(payload, '$.data.cost_from_provider')), 0) = 1
                                    THEN json('true') ELSE json('false') END,
         -- And it is charged for only if every part of it was, for the same reason.
         'is_charged', CASE WHEN coalesce(MIN(json_extract(payload, '$.data.is_charged')), 0) = 1
                            THEN json('true') ELSE json('false') END)
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND event_type = 'agent_usage_reported') AS usage,
    -- The error of the failure the task is in, if it is in one: a run that started
    -- afterwards is the task going back to work, and the error it left behind is
    -- history rather than a state.
    (SELECT payload ->> '$.data.error'
     FROM task_event_logs
     WHERE task_id = tasks.id
       AND json_type(payload, '$.data.error') IS NOT NULL
       AND sequence > COALESCE(
         (SELECT MAX(sequence) FROM task_event_logs
          WHERE task_id = tasks.id AND event_type = 'agent_run_started'), 0)
     ORDER BY sequence DESC LIMIT 1) AS last_error,
    (SELECT json_object(
         'id', 'm' || e.sequence,
         'author', 'operator',
         'body', COALESCE(
             json_extract(e.payload, '$.data.input.body'),
             json_extract(e.payload, '$.data.body')),
         'at_ms', e.recorded_at_ms)
     FROM task_event_logs AS e
     WHERE e.task_id = tasks.id
       AND (
           e.event_type = 'task_input_expressed'
           OR (e.event_type = 'operator_signal_message'
               AND json_extract(e.payload, '$.data.author') = 'operator')
       )
     ORDER BY e.sequence DESC LIMIT 1) AS latest_operator_message,
    (SELECT payload -> '$.data.issue.issue.reference'
     FROM task_event_logs
     WHERE task_id = tasks.id AND event_type = 'tracker_issue_bound'
     ORDER BY sequence DESC LIMIT 1) AS tracker_issue,
    (SELECT payload ->> '$.data.delegation'
     FROM task_event_logs
     WHERE task_id = tasks.id AND event_type = 'tracker_issue_bound'
     ORDER BY sequence DESC LIMIT 1) AS tracker_delegation,
    (SELECT payload ->> '$.data.status'
     FROM task_event_logs
     WHERE task_id = tasks.id AND event_type = 'tracker_synced'
     ORDER BY sequence DESC LIMIT 1) AS tracker_status,
    (SELECT payload ->> '$.data.address'
     FROM task_event_logs
     WHERE task_id = tasks.id AND event_type = 'task_arrived_from'
     ORDER BY sequence DESC LIMIT 1) AS arrived_from,
    (SELECT kind
     FROM task_operator_signals
     WHERE task_id = tasks.id
       AND resolved = 0
       AND kind IN ('blocked', 'in_review')
     ORDER BY raised_at_ms DESC LIMIT 1) AS blocking_signal_kind,
    -- The kind a list shows the task under: the most urgent unresolved thread,
    -- which is not the same question as whether the task is blocked.
    (SELECT kind
     FROM task_operator_signals
     WHERE task_id = tasks.id
       AND resolved = 0
     ORDER BY CASE kind
         WHEN 'blocked' THEN 0
         WHEN 'in_review' THEN 1
         WHEN 'decision' THEN 2
         ELSE 3
     END, raised_at_ms DESC
     LIMIT 1) AS leading_signal_kind,
    -- And what that thread is about, because a row can say the question itself
    -- rather than only that there is one.
    (SELECT title
     FROM task_operator_signals
     WHERE task_id = tasks.id
       AND resolved = 0
     ORDER BY CASE kind
         WHEN 'blocked' THEN 0
         WHEN 'in_review' THEN 1
         WHEN 'decision' THEN 2
         ELSE 3
     END, raised_at_ms DESC
     LIMIT 1) AS leading_signal_title,
    (SELECT count(*) FROM task_operator_signals
     WHERE task_id = tasks.id AND resolved = 0) AS unresolved_signal_count,
    -- How many of each kind. One number says a row is noisy; the numbers by kind
    -- say whether it is asking something or only reporting.
    COALESCE((SELECT json_group_object(kind, total) FROM (
         SELECT kind, count(*) AS total
         FROM task_operator_signals
         WHERE task_id = tasks.id AND resolved = 0
         GROUP BY kind)), '{}') AS signal_counts,
    (SELECT json_object('signal', json(payload -> '$.data.signal'))
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
     ORDER BY cleared.sequence DESC LIMIT 1) AS review_wake_news,
    -- Everything the machine has said to the agent about its pull requests, most
    -- recently last. Saying it again when nothing has moved is what a wake loop is
    -- made of, so the last one is what this pass compares itself against.
    (SELECT json_extract(cleared.payload, '$.data.reason')
     FROM task_event_logs AS cleared
     WHERE cleared.task_id = tasks.id
       AND cleared.event_type = 'review_deferral_cleared'
     ORDER BY cleared.sequence DESC LIMIT 1) AS last_wake_news,
    -- What the operator said while the agent was working. Read by the next turn, so a
    -- message that arrives mid-run is answered rather than lost.
    COALESCE((SELECT json_group_array(json(input))
     FROM (
         SELECT json_extract(expressed.payload, '$.data.input') AS input
         FROM task_event_logs AS expressed
         WHERE expressed.task_id = tasks.id
           AND expressed.event_type = 'task_input_expressed'
           AND expressed.sequence > COALESCE(
             (SELECT MAX(sequence) FROM task_event_logs
              WHERE task_id = tasks.id AND event_type = 'agent_run_started'), 0)
         ORDER BY expressed.sequence
     )), '[]') AS pending_inputs,
    tasks.created_at_ms AS created_at_ms,
    -- The age a list shows: when one of the facts it prints last moved. The log
    -- has plenty of events that change nothing on a row — a plan refreshed every
    -- turn, a model observed, a tracker echoing a status back — and counting those
    -- would make every age mean "recently" and nothing else.
    COALESCE((SELECT MAX(recorded_at_ms) FROM task_event_logs
              WHERE task_id = tasks.id
                AND event_type IN (
                    'intent_expressed', 'task_title_set', 'task_arrived_from',
                    'agent_run_started', 'agent_run_ended',
                    'control_requested', 'execution_queued', 'execution_paused',
                    'execution_recovered', 'execution_failed', 'task_completed',
                    'task_handed_back', 'operator_signal_raised', 'operator_signal_resolved',
                    'operator_signal_confirmed', 'review_deferred', 'review_deferral_cleared',
                    'code_contribution_linked', 'code_contribution_observed',
                    'tracker_issue_bound', 'transfer_staged', 'transfer_landed',
                    'transfer_committed', 'transfer_abandoned',
                    'limit_held', 'agent_usage_reported'
                )), tasks.created_at_ms) AS shown_updated_at_ms,
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
