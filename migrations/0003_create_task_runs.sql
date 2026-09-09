CREATE TABLE task_runs (
    id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    started_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER,
    outcome TEXT,
    exit_code INTEGER
);

CREATE INDEX task_runs_task_id_started_at ON task_runs (task_id, started_at_ms DESC);

CREATE TABLE run_output (
    run_id TEXT NOT NULL REFERENCES task_runs(id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    stream TEXT NOT NULL,
    bytes BLOB NOT NULL,
    PRIMARY KEY (run_id, sequence)
);
