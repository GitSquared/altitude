CREATE TABLE tasks (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    command TEXT NOT NULL,
    status TEXT NOT NULL,
    sandbox_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX tasks_updated_at ON tasks (updated_at DESC);
