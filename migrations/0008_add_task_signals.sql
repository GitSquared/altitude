ALTER TABLE tasks ADD COLUMN operator_attention TEXT NOT NULL DEFAULT 'nominal'
    CHECK (operator_attention IN ('nominal', 'attention', 'alarm'));

CREATE TABLE task_signals (
    id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    run_id TEXT NOT NULL REFERENCES task_runs(id) ON DELETE CASCADE,
    level TEXT NOT NULL CHECK (level IN ('info', 'attention', 'alarm')),
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    acknowledged_at_ms INTEGER
);

CREATE INDEX task_signals_task_id_created_at
    ON task_signals(task_id, created_at_ms DESC);
