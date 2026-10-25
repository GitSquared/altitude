-- Signals are threads: the agent can speak again on one it already raised, the
-- operator can answer more than once, and a thread closes when it stops needing
-- anybody rather than when it is first answered.
--
-- Still derived from the event log. A thread is every event that names a signal
-- id. Its kind, priority and title are the latest raise's, its opened time is
-- the first raise's, and whether it waits is decided by which side spoke last.
DROP VIEW task_operator_signals;

CREATE VIEW task_operator_signals AS
WITH raised AS (
    SELECT
        sequence,
        task_id,
        recorded_at_ms,
        json_extract(payload, '$.data.signal.id') AS id,
        json_extract(payload, '$.data.signal.kind') AS kind,
        json_extract(payload, '$.data.signal.priority') AS priority,
        json_extract(payload, '$.data.signal.title') AS title,
        json_extract(payload, '$.data.signal.body') AS body
    FROM task_event_logs
    WHERE event_type = 'operator_signal_raised'
),
latest AS (
    SELECT id, task_id, MAX(sequence) AS sequence
    FROM raised
    GROUP BY id, task_id
),
opened AS (
    SELECT id, MIN(recorded_at_ms) AS at_ms
    FROM raised
    GROUP BY id
),
state AS (
    SELECT
        raised.task_id,
        raised.id,
        raised.sequence,
        raised.recorded_at_ms AS raised_at_ms,
        opened.at_ms AS opened_at_ms,
        raised.kind,
        raised.priority,
        raised.title,
        raised.body,
        -- The latest message has been seen when anything about it was recorded
        -- after it: reading, answering or closing all mean the operator saw it.
        EXISTS (
            SELECT 1 FROM task_event_logs AS seen
            WHERE seen.task_id = raised.task_id
              AND seen.sequence > raised.sequence
              AND seen.event_type IN (
                  'operator_signal_read',
                  'operator_signal_answered',
                  'operator_signal_acknowledged',
                  'operator_signal_confirmed',
                  'operator_signal_settled'
              )
              AND json_extract(seen.payload, '$.data.signal_id') = raised.id
        ) AS read,
        EXISTS (
            SELECT 1 FROM task_event_logs AS settled
            WHERE settled.task_id = raised.task_id
              AND settled.event_type IN (
                  'operator_signal_acknowledged',
                  'operator_signal_confirmed',
                  'operator_signal_settled'
              )
              AND json_extract(settled.payload, '$.data.signal_id') = raised.id
        ) AS settled,
        NOT EXISTS (
            SELECT 1 FROM task_event_logs AS answered
            WHERE answered.task_id = raised.task_id
              AND answered.sequence > raised.sequence
              AND answered.event_type = 'operator_signal_answered'
              AND json_extract(answered.payload, '$.data.signal_id') = raised.id
        ) AS agent_spoke_last
    FROM latest
    JOIN raised ON raised.id = latest.id AND raised.sequence = latest.sequence
    JOIN opened ON opened.id = raised.id
)
SELECT
    task_id,
    id,
    sequence,
    raised_at_ms,
    opened_at_ms,
    kind,
    priority,
    title,
    body,
    read,
    settled,
    (NOT settled AND agent_spoke_last) AS awaiting
FROM state;
