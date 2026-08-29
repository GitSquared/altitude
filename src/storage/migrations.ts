import type { Database } from "bun:sqlite";

export const CURRENT_SCHEMA_VERSION = 7;

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
  {
    version: 3,
    sql: `
      CREATE TABLE task_mcp_grants (
        id TEXT PRIMARY KEY,
        token_hash TEXT NOT NULL UNIQUE,
        task_id TEXT NOT NULL REFERENCES tasks(id),
        identity_id TEXT NOT NULL,
        agent_session_id TEXT,
        allowed_tools_json TEXT NOT NULL,
        issued_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL CHECK (expires_at > issued_at),
        revoked_at INTEGER
      ) STRICT;

      CREATE INDEX task_mcp_grants_active
        ON task_mcp_grants(token_hash, expires_at, revoked_at);
    `,
  },
  {
    version: 4,
    sql: `
      CREATE TABLE task_agent_sessions (
        task_id TEXT PRIMARY KEY REFERENCES tasks(id),
        adapter TEXT NOT NULL,
        session_id TEXT NOT NULL UNIQUE,
        identity_id TEXT NOT NULL,
        project_directory_name TEXT NOT NULL,
        control_directory TEXT,
        mcp_config_path TEXT,
        grant_id TEXT,
        run_count INTEGER NOT NULL DEFAULT 0 CHECK (run_count >= 0),
        last_outcome TEXT CHECK (
          last_outcome IS NULL OR last_outcome IN (
            'success', 'error', 'yielded', 'stopped', 'rate_limited', 'protocol_error'
          )
        ),
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        CHECK (
          (control_directory IS NULL AND mcp_config_path IS NULL AND grant_id IS NULL)
          OR
          (control_directory IS NOT NULL AND mcp_config_path IS NOT NULL AND grant_id IS NOT NULL)
        )
      ) STRICT;

      CREATE INDEX task_agent_sessions_by_identity
        ON task_agent_sessions(identity_id, updated_at DESC);
    `,
  },
  {
    version: 5,
    sql: `
      ALTER TABLE task_agent_sessions
        ADD COLUMN consecutive_rate_limits INTEGER NOT NULL DEFAULT 0
        CHECK (consecutive_rate_limits >= 0);
    `,
  },
  {
    version: 6,
    sql: `
      CREATE TABLE intent_submission_grants (
        session_id TEXT PRIMARY KEY,
        task_id TEXT NOT NULL REFERENCES tasks(id),
        capsule_id TEXT NOT NULL,
        token_hash TEXT NOT NULL UNIQUE,
        base_url TEXT NOT NULL,
        head_sha TEXT NOT NULL,
        checkpoint_digest TEXT NOT NULL,
        checkpoint_revision INTEGER NOT NULL CHECK (checkpoint_revision > 0),
        created_at INTEGER NOT NULL CHECK (created_at >= 0),
        expires_at INTEGER NOT NULL CHECK (expires_at > created_at),
        consumed_at INTEGER,
        revoked_at INTEGER,
        CHECK (consumed_at IS NULL OR consumed_at >= created_at),
        CHECK (revoked_at IS NULL OR revoked_at >= created_at),
        CHECK (consumed_at IS NULL OR revoked_at IS NULL)
      ) STRICT;

      CREATE INDEX intent_submission_grants_active
        ON intent_submission_grants(session_id, expires_at, consumed_at, revoked_at);

      CREATE TABLE intent_artifacts (
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL UNIQUE REFERENCES intent_submission_grants(session_id),
        task_id TEXT NOT NULL REFERENCES tasks(id),
        capsule_id TEXT NOT NULL,
        tool TEXT NOT NULL CHECK (tool = 'redmarker'),
        payload_schema TEXT NOT NULL,
        base_url TEXT NOT NULL,
        head_sha TEXT NOT NULL,
        checkpoint_digest TEXT NOT NULL,
        checkpoint_revision INTEGER NOT NULL CHECK (checkpoint_revision > 0),
        created_at INTEGER NOT NULL CHECK (created_at >= 0),
        artifact_json TEXT NOT NULL
      ) STRICT;

      CREATE INDEX intent_artifacts_by_task_time
        ON intent_artifacts(task_id, created_at, id);
    `,
  },
  {
    version: 7,
    sql: `
      DROP INDEX intent_artifacts_by_task_time;
      ALTER TABLE intent_artifacts RENAME TO intent_artifacts_v6;

      CREATE TABLE intent_artifacts (
        id TEXT PRIMARY KEY,
        session_id TEXT UNIQUE REFERENCES intent_submission_grants(session_id),
        task_id TEXT NOT NULL REFERENCES tasks(id),
        capsule_id TEXT NOT NULL,
        tool TEXT NOT NULL CHECK (tool = 'redmarker'),
        payload_schema TEXT NOT NULL,
        base_url TEXT NOT NULL,
        head_sha TEXT NOT NULL,
        checkpoint_digest TEXT NOT NULL,
        checkpoint_revision INTEGER NOT NULL CHECK (checkpoint_revision > 0),
        created_at INTEGER NOT NULL CHECK (created_at >= 0),
        artifact_json TEXT NOT NULL
      ) STRICT;

      INSERT INTO intent_artifacts(
        id, session_id, task_id, capsule_id, tool, payload_schema,
        base_url, head_sha, checkpoint_digest, checkpoint_revision,
        created_at, artifact_json
      )
      SELECT
        id, session_id, task_id, capsule_id, tool, payload_schema,
        base_url, head_sha, checkpoint_digest, checkpoint_revision,
        created_at, artifact_json
      FROM intent_artifacts_v6;

      DROP TABLE intent_artifacts_v6;

      CREATE INDEX intent_artifacts_by_task_time
        ON intent_artifacts(task_id, created_at, id);
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
