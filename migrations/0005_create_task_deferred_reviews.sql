-- The wrap-up requests the host is still holding.
--
-- A request is held from the moment it is recorded until it is either raised for
-- the operator or cleared for the agent, which are the two ways a hold ends. Both
-- readers of that fact -- the task card's own facts, and the review scheduler
-- asking which reviews to observe -- ask it here, so "still held" is written once.
--
-- The resolution is matched on the signal, not on the task: a task can raise
-- other signals while a review is held, and those must not end the hold.
CREATE VIEW task_deferred_reviews AS
SELECT deferred.task_id, deferred.sequence, deferred.payload
FROM task_event_logs AS deferred
WHERE deferred.event_type = 'review_deferred'
  AND NOT EXISTS (
    SELECT 1 FROM task_event_logs AS resolved
    WHERE resolved.task_id = deferred.task_id
      AND resolved.sequence > deferred.sequence
      AND resolved.event_type IN ('operator_signal_raised', 'review_deferral_cleared')
      AND COALESCE(
            json_extract(resolved.payload, '$.data.signal_id'),
            json_extract(resolved.payload, '$.data.signal.id')) =
          json_extract(deferred.payload, '$.data.signal.id')
  );

-- The view's first question is which events are holds, and the scheduler asks it
-- on a clock. The existing indexes lead with task_id, which a question about a
-- kind of event cannot use.
CREATE INDEX task_event_logs_type_sequence ON task_event_logs(event_type, sequence);
