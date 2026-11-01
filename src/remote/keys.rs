use sqlx::{FromRow, SqlitePool};
use tracing::info;

#[derive(Clone)]
pub struct RemoteKeys {
    pool: SqlitePool,
}

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

    pub async fn remembered(&self, address: &str) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT key FROM remote_keys WHERE address = ?")
                .bind(address)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(id,)| id))
    }

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
