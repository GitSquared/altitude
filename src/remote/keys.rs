//! The key each remote address answered with, and what it called itself.
//!
//! The protection is not that the first answer was trustworthy — a key learned
//! from an announcement is unauthenticated by definition — but that a *different*
//! answer later is noticed. Remembering is what makes noticing possible, and it
//! is the operator's decision, because the first sighting is the part that
//! cannot be authenticated.
//!
//! Kept in the database rather than a file because it is machine state that
//! changes on its own, not a preference anyone edits. The link itself keeps
//! nothing: it can say how a key compares to one you remember, and storing that
//! is the caller's business, which is why a machine with no database can still
//! use the link.

use sqlx::{FromRow, SqlitePool};
use tracing::info;

#[derive(Clone)]
pub struct RemoteKeys {
    pool: SqlitePool,
}

/// What a remote address last answered with: the key it proved, and the name it
/// gave itself. The name is only ever for reading — the key is what is checked.
#[derive(Clone, Debug, FromRow)]
pub struct RemoteKey {
    pub address: String,
    pub key: String,
    pub label: String,
    pub version: String,
    pub seen_at_ms: i64,
}

impl RemoteKeys {
    pub fn new(pool: &SqlitePool) -> Self {
        Self { pool: pool.clone() }
    }

    /// The key this address answered with last time, if it is remembered at all.
    ///
    /// This is what a caller compares a fresh sighting against.
    pub async fn remembered(&self, address: &str) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT key FROM remote_keys WHERE address = ?")
                .bind(address)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(id,)| id))
    }

    /// Writes down what a remote said.
    ///
    /// This is also how a change is accepted: remembering a new key replaces the
    /// old one, so accepting is the same act as meeting a remote for the first
    /// time.
    pub async fn remember(
        &self,
        address: &str,
        key: &str,
        label: &str,
        version: &str,
    ) -> Result<(), sqlx::Error> {
        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO remote_keys (address, key, label, version, seen_at_ms)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(address) DO UPDATE SET key = excluded.key,
                                                label = excluded.label,
                                                version = excluded.version,
                                                seen_at_ms = excluded.seen_at_ms",
        )
        .bind(address)
        .bind(key)
        .bind(label)
        .bind(version)
        .bind(now)
        .execute(&self.pool)
        .await?;
        info!(address, label, "remembering a remote");
        Ok(())
    }

    /// Forgets a remote. `true` if it was remembered.
    ///
    /// The next meeting is a first sighting again, which is what forgetting
    /// means.
    pub async fn forget(&self, address: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM remote_keys WHERE address = ?")
            .bind(address)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn list(&self) -> Result<Vec<RemoteKey>, sqlx::Error> {
        sqlx::query_as::<_, RemoteKey>(
            "SELECT address, key, label, version, seen_at_ms FROM remote_keys ORDER BY address",
        )
        .fetch_all(&self.pool)
        .await
    }
}
