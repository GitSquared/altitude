-- The agent can say something on a thread without asking a new question, and a
-- close records who closed it. Neither changes what a thread is: the latest
-- raise still decides what is being asked and whether the operator is the one
-- being waited on, while a message is a new thing to read.
--
-- Still derived from the event log. A thread's identity, kind and title come
-- from its latest raise; its latest message can be a raise or a plain agent
-- message.
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
agent AS (
    SELECT sequence, task_id, recorded_at_ms, json_extract(payload, '$.data.signal.id') AS id
    FROM task_event_logs
    WHERE event_type = 'operator_signal_raised'
    UNION ALL
    SELECT sequence, task_id, recorded_at_ms, json_extract(payload, '$.data.signal_id')
    FROM task_event_logs
    WHERE event_type = 'operator_signal_message'
),
latest_raise AS (
    SELECT id, task_id, MAX(sequence) AS sequence
    FROM raised
    GROUP BY id, task_id
),
latest_agent AS (
    SELECT id, task_id, MAX(sequence) AS sequence
    FROM agent
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
        latest_agent.sequence AS latest_agent_sequence,
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
              AND seen.sequence > latest_agent.sequence
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
        -- A message is not a new question, so only an answer after the latest
        -- raise takes the thread off the operator's desk.
        NOT EXISTS (
            SELECT 1 FROM task_event_logs AS answered
            WHERE answered.task_id = raised.task_id
              AND answered.sequence > raised.sequence
              AND answered.event_type = 'operator_signal_answered'
              AND json_extract(answered.payload, '$.data.signal_id') = raised.id
        ) AS awaiting
    FROM latest_raise
    JOIN raised
        ON raised.id = latest_raise.id AND raised.sequence = latest_raise.sequence
    JOIN latest_agent
        ON latest_agent.id = latest_raise.id AND latest_agent.task_id = latest_raise.task_id
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
    (NOT settled AND awaiting) AS awaiting
FROM state;
