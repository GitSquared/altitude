-- A reply used to be written two ways: a task input that named the thread it answered,
-- and a message in that thread. Only the second is a thing of its own now, and the
-- history is read back as it was written, so the inputs that were really replies are
-- rewritten as the messages they meant.
--
-- Duplicates are possible where both were recorded for one answer: the message beside
-- an input with the same body and thread is dropped, and only the input is converted.
DELETE FROM task_event_logs
WHERE event_type = 'operator_signal_message'
  AND EXISTS (
      SELECT 1 FROM task_event_logs AS input
      WHERE input.event_type = 'task_input_expressed'
        AND input.task_id = task_event_logs.task_id
        AND json_extract(input.payload, '$.data.input.responding_to')
            = json_extract(task_event_logs.payload, '$.data.signal_id')
        AND json_extract(input.payload, '$.data.input.body')
            = json_extract(task_event_logs.payload, '$.data.body')
  );

UPDATE task_event_logs
SET payload = json_object(
        'type', 'operator_signal_message',
        'data', json_object(
            'signal_id', json_extract(payload, '$.data.input.responding_to'),
            'author', 'operator',
            'body', json_extract(payload, '$.data.input.body')
        )
    )
WHERE event_type = 'task_input_expressed'
  AND json_type(payload, '$.data.input.responding_to') = 'text';
