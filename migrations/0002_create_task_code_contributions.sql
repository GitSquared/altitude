CREATE VIEW task_code_contributions AS
SELECT task_id, sequence,
    payload -> '$.data.contribution' AS contribution
FROM task_event_logs
WHERE event_type = 'code_contribution_linked';
