-- A thread has one state: unresolved or resolved. Reading is not a separate
-- fact, and no kind gets its own resolution event. A message reopens a resolved
-- thread; the latest message or resolution decides.
--
-- Still derived from the event log. A thread is every event that names a signal
-- id; its kind and title come from the latest raise.

DROP VIEW task_operator_signals;

CREATE VIEW task_operator_signals AS
WITH raised AS (
    SELECT
        sequence,
        task_id,
        recorded_at_ms,
        json_extract(payload, '$.data.signal.id') AS id,
        json_extract(payload, '$.data.signal.kind') AS kind,
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
        ) AS resolved_at_ms
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
    resolved,
    resolved_by,
    resolved_at_ms
FROM state;
