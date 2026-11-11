-- Work that was taken or completed is never run again, and it accumulates: a machine
-- that has been in service for a year has thousands of finished tasks behind it. Both
-- readings of a task ask whether it is finished -- what the screens list, and what the
-- queue may start -- so the answer is stated once, here, rather than in each query that
-- needs it.
CREATE VIEW finished_tasks AS
    SELECT task_id FROM task_event_logs
    WHERE event_type IN ('operator_signal_confirmed', 'task_completed');
