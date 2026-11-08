-- A task's history is read back as the events were written, so rewriting only the
-- view that lists asks leaves every task founded before the vocabulary changed
-- unreadable: a machine decoding `decision` cannot report on work it holds.
--
-- The four old kinds become the two that exist. A `blocked` was the question that
-- stopped the work and a `decision` was one the work went past, so both are the
-- shallower of the two; `in_review` is what everyone already called a review. An
-- `update` asked nobody anything, and as a question nobody answered it stays in the
-- history with nothing to act on.
UPDATE task_event_logs
SET payload = json_set(
    payload,
    '$.data.signal.kind',
    CASE json_extract(payload, '$.data.signal.kind')
        WHEN 'in_review' THEN 'review'
        ELSE 'steer'
    END
)
WHERE json_extract(payload, '$.data.signal.kind') IN (
    'blocked',
    'decision',
    'update',
    'in_review'
);
