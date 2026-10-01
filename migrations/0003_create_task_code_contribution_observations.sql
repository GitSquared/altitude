CREATE VIEW task_code_contribution_observations AS
SELECT task_id, sequence,
    payload -> '$.data.observation' AS observation
FROM task_event_logs
WHERE event_type = 'code_contribution_observed';
