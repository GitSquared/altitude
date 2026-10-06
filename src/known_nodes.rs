//! The nodes this client has met, and the key each address answered with.
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
//! is the caller's business, which is why a node with no database can still use
//! the link.

use sqlx::{FromRow, SqlitePool};
use tracing::info;

use crate::net::KnownNode;

#[derive(Clone)]
pub struct KnownNodes {
    pool: SqlitePool,
}

/// One remembered node, as a list would show it.
#[derive(Clone, Debug, FromRow)]
pub struct KnownNodeRow {
    pub address: String,
    pub id: String,
    pub label: String,
    pub met_at_ms: i64,
}

impl From<KnownNodeRow> for KnownNode {
    fn from(row: KnownNodeRow) -> Self {
        Self {
            address: row.address,
            id: row.id,
            label: row.label,
        }
    }
}

impl KnownNodes {
    pub fn new(pool: &SqlitePool) -> Self {
        Self { pool: pool.clone() }
    }

    /// The key this address answered with last time, if it is remembered at all.
    ///
    /// This is what a caller compares a fresh sighting against.
    pub async fn remembered(&self, address: &str) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(String,)> = sqlx::query_as("SELECT id FROM known_nodes WHERE address = ?")
            .bind(address)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(id,)| id))
    }

    /// Writes down what a node said.
    ///
    /// This is also how a change is accepted: remembering a new key replaces the
    /// old one, so accepting is the same act as meeting a node for the first
    /// time.
    pub async fn remember(&self, address: &str, id: &str, label: &str) -> Result<(), sqlx::Error> {
        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO known_nodes (address, id, label, met_at_ms) VALUES (?, ?, ?, ?)
             ON CONFLICT(address) DO UPDATE SET id = excluded.id,
                                                label = excluded.label,
                                                met_at_ms = excluded.met_at_ms",
        )
        .bind(address)
        .bind(id)
        .bind(label)
        .bind(now)
        .execute(&self.pool)
        .await?;
        info!(address, label, "remembering a node");
        Ok(())
    }

    /// Forgets a node. `true` if it was remembered.
    ///
    /// The next meeting is a first sighting again, which is what forgetting
    /// means.
    pub async fn forget(&self, address: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM known_nodes WHERE address = ?")
            .bind(address)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Every node remembered, by address.
    pub async fn list(&self) -> Result<Vec<KnownNodeRow>, sqlx::Error> {
        sqlx::query_as::<_, KnownNodeRow>(
            "SELECT address, id, label, met_at_ms FROM known_nodes ORDER BY address",
        )
        .fetch_all(&self.pool)
        .await
    }
}
