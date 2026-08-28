import type { Database } from "bun:sqlite";

export const CURRENT_SCHEMA_VERSION = 2;

type Migration = Readonly<{
  version: number;
  sql: string;
}>;

const MIGRATIONS: readonly Migration[] = [
  {
    version: 1,
    sql: `
      CREATE TABLE tasks (
        id TEXT PRIMARY KEY,
        status TEXT NOT NULL,
        intent_level TEXT NOT NULL,
        revision INTEGER NOT NULL CHECK (revision > 0),
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        state_json TEXT NOT NULL
      ) STRICT;

      CREATE INDEX tasks_by_status_updated
        ON tasks(status, updated_at DESC);

      CREATE TABLE events (
        id TEXT PRIMARY KEY,
        task_id TEXT NOT NULL REFERENCES tasks(id) DEFERRABLE INITIALLY DEFERRED,
        sequence INTEGER NOT NULL CHECK (sequence > 0),
        event_type TEXT NOT NULL,
        schema_version INTEGER NOT NULL CHECK (schema_version > 0),
        occurred_at INTEGER NOT NULL,
        idempotency_key TEXT,
        attention_id TEXT,
        event_json TEXT NOT NULL,
        UNIQUE(task_id, sequence)
      ) STRICT;

      CREATE UNIQUE INDEX events_by_task_idempotency
        ON events(task_id, idempotency_key)
        WHERE idempotency_key IS NOT NULL;

      CREATE UNIQUE INDEX events_unique_attention_raise
        ON events(task_id, attention_id)
        WHERE event_type = 'attention.raised';

      CREATE INDEX events_by_task_time
        ON events(task_id, occurred_at, sequence);

      CREATE TABLE outbox (
        id TEXT PRIMARY KEY,
        task_id TEXT NOT NULL REFERENCES tasks(id),
        event_id TEXT NOT NULL REFERENCES events(id),
        topic TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        state TEXT NOT NULL DEFAULT 'pending'
          CHECK (state IN ('pending', 'processing', 'delivered', 'dead')),
        available_at INTEGER NOT NULL,
        attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
        lease_owner TEXT,
        lease_expires_at INTEGER,
        last_error TEXT,
        dedupe_key TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
      ) STRICT;

      CREATE UNIQUE INDEX outbox_by_topic_dedupe
        ON outbox(topic, dedupe_key)
        WHERE dedupe_key IS NOT NULL;

      CREATE INDEX outbox_due
        ON outbox(state, available_at, lease_expires_at);

      CREATE TABLE scheduled_actions (
        id TEXT PRIMARY KEY,
        task_id TEXT NOT NULL REFERENCES tasks(id),
        kind TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        state TEXT NOT NULL DEFAULT 'scheduled'
          CHECK (state IN ('scheduled', 'claimed', 'completed', 'canceled', 'dead')),
        run_at INTEGER NOT NULL,
        attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
        lease_owner TEXT,
        lease_expires_at INTEGER,
        last_error TEXT,
        dedupe_key TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
      ) STRICT;

      CREATE UNIQUE INDEX scheduled_actions_by_task_dedupe
        ON scheduled_actions(task_id, dedupe_key)
        WHERE dedupe_key IS NOT NULL;

      CREATE INDEX scheduled_actions_due
        ON scheduled_actions(state, run_at, lease_expires_at);
    `,
  },
  {
    version: 2,
    sql: `
      CREATE TABLE agent_writer_leases (
        task_id TEXT PRIMARY KEY REFERENCES tasks(id),
        session_id TEXT,
        holder_id TEXT,
        mode TEXT CHECK (mode IN ('headless', 'interactive')),
        acquired_at INTEGER,
        revision INTEGER NOT NULL CHECK (revision > 0),
        CHECK (
          (
            session_id IS NULL
            AND holder_id IS NULL
            AND mode IS NULL
            AND acquired_at IS NULL
          )
          OR
          (
            session_id IS NOT NULL AND length(trim(session_id)) > 0
            AND holder_id IS NOT NULL AND length(trim(holder_id)) > 0
            AND mode IS NOT NULL
            AND acquired_at IS NOT NULL AND acquired_at >= 0
          )
        )
      ) STRICT;
    `,
  },
];

export function migrate(database: Database, now: number): void {
  database.exec(`
    CREATE TABLE IF NOT EXISTS schema_migrations (
      version INTEGER PRIMARY KEY,
      applied_at INTEGER NOT NULL
    ) STRICT;
  `);

  const applied = new Set(
    database
      .query<{ version: number }, []>("SELECT version FROM schema_migrations ORDER BY version")
      .all()
      .map((row) => row.version),
  );

  const unknown = [...applied].find((version) => version > CURRENT_SCHEMA_VERSION);
  if (unknown !== undefined) {
    throw new Error(`Altitude database schema ${unknown} is newer than supported schema ${CURRENT_SCHEMA_VERSION}`);
  }

  for (const migration of MIGRATIONS) {
    if (applied.has(migration.version)) continue;
    database.transaction(() => {
      database.exec(migration.sql);
      database
        .query<unknown, [number, number]>("INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)")
        .run(migration.version, now);
    }).immediate();
  }
}
