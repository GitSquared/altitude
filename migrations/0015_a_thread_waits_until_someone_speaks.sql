-- A thread wants a person until someone speaks in it after it was asked, so the wait is
-- the raise and not whatever was said last. Reading the last author as the wait made
-- every reply a hand-back: an agent that said what it was doing in a thread the operator
-- was already answering stopped the task by doing so, and a turn that ended with work
-- left on it read as a turn that had handed the work over.

DROP VIEW task_operator_signals;

CREATE VIEW task_operator_signals AS
WITH raised AS (
    SELECT
        sequence,
        task_id,
        recorded_at_ms,
        json_extract(payload, '$.data.signal.id') AS id,
        CASE json_extract(payload, '$.data.signal.kind')
            WHEN 'in_review' THEN 'ready_for_review'
            WHEN 'review' THEN 'ready_for_review'
            WHEN 'ready_for_review' THEN 'ready_for_review'
            ELSE 'blocking_question'
        END AS kind,
        CASE json_extract(payload, '$.data.signal.kind')
            WHEN 'update' THEN 1
            WHEN 'decision' THEN 1
            ELSE 0
        END AS asked_nobody,
        json_extract(payload, '$.data.signal.title') AS title,
        json_extract(payload, '$.data.signal.body') AS body
    FROM task_event_logs
    WHERE event_type = 'operator_signal_raised'
),
latest_raise AS (
    SELECT id, task_id, MAX(sequence) AS sequence
    FROM raised
    GROUP BY id, task_id
),
opened AS (
    SELECT id, MIN(recorded_at_ms) AS at_ms
    FROM raised
    GROUP BY id
),
activity AS (
    SELECT id, task_id, MAX(sequence) AS sequence
    FROM (
        SELECT id, task_id, sequence FROM raised
        UNION ALL
        SELECT
            json_extract(payload, '$.data.signal_id') AS id,
            task_id,
            sequence
        FROM task_event_logs
        WHERE event_type = 'operator_signal_message'
    )
    GROUP BY id, task_id
),
resolution AS (
    SELECT
        json_extract(payload, '$.data.signal_id') AS id,
        task_id,
        sequence,
        json_extract(payload, '$.data.resolved') AS resolved,
        json_extract(payload, '$.data.by') AS by
    FROM task_event_logs
    WHERE event_type = 'operator_signal_resolved'
    UNION ALL
    SELECT
        json_extract(payload, '$.data.signal_id') AS id,
        task_id,
        sequence,
        1 AS resolved,
        'operator' AS by
    FROM task_event_logs
    WHERE event_type = 'operator_signal_confirmed'
),
latest_resolution AS (
    SELECT id, task_id, MAX(sequence) AS sequence
    FROM resolution
    GROUP BY id, task_id
),
state AS (
    SELECT
        raised.task_id,
        raised.id,
        raised.sequence,
        raised.recorded_at_ms AS raised_at_ms,
        opened.at_ms AS opened_at_ms,
        raised.kind,
        raised.asked_nobody,
        raised.title,
        raised.body,
        CASE
            WHEN latest_resolution.sequence > COALESCE(activity.sequence, 0)
            THEN COALESCE((
                SELECT resolution.resolved
                FROM resolution
                WHERE resolution.id = raised.id
                  AND resolution.task_id = raised.task_id
                  AND resolution.sequence = latest_resolution.sequence
            ), 0)
            ELSE 0
        END AS resolved,
        (
            SELECT resolution.by
            FROM resolution
            WHERE resolution.id = raised.id
              AND resolution.task_id = raised.task_id
              AND resolution.sequence = latest_resolution.sequence
              AND latest_resolution.sequence > COALESCE(activity.sequence, 0)
        ) AS resolved_by,
        (
            SELECT task_event_logs.recorded_at_ms
            FROM task_event_logs
            WHERE task_event_logs.sequence = latest_resolution.sequence
              AND latest_resolution.sequence > COALESCE(activity.sequence, 0)
        ) AS resolved_at_ms,
        -- Raised, and nobody has spoken in it since: the raise is the newest thing in
        -- the thread. A thread answered in is one the operator is already in, so what
        -- the agent says there is conversation and not a second time of asking, and
        -- asking again is raising it again.
        CASE
            WHEN raised.sequence = COALESCE(activity.sequence, 0) THEN 1
            ELSE 0
        END AS wants_a_person
    FROM latest_raise
    JOIN raised
        ON raised.id = latest_raise.id AND raised.sequence = latest_raise.sequence
    JOIN opened ON opened.id = raised.id
    LEFT JOIN activity
        ON activity.id = raised.id AND activity.task_id = raised.task_id
    LEFT JOIN latest_resolution
        ON latest_resolution.id = raised.id
        AND latest_resolution.task_id = raised.task_id
)
SELECT
    task_id,
    id,
    sequence,
    raised_at_ms,
    opened_at_ms,
    kind,
    title,
    body,
    CASE WHEN asked_nobody THEN 1 ELSE resolved END AS resolved,
    CASE WHEN asked_nobody THEN 'agent' ELSE resolved_by END AS resolved_by,
    CASE WHEN asked_nobody THEN raised_at_ms ELSE resolved_at_ms END AS resolved_at_ms,
    wants_a_person
FROM state;
