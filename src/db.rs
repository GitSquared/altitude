use std::{error::Error, fmt, fs, io, time::Duration};

use sqlx::{
    SqlitePool,
    migrate::MigrateError,
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
};

use crate::config::Config;

pub struct Database {
    pool: SqlitePool,
}

impl Database {
    pub async fn open(config: &Config) -> Result<Self, DatabaseError> {
        tracing::info!(path = %config.database_path().display(), "opening database");
        fs::create_dir_all(config.data_dir()).map_err(DatabaseError::CreateDirectory)?;

        let options = SqliteConnectOptions::new()
            .filename(config.database_path())
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePool::connect_with(options)
            .await
            .map_err(DatabaseError::Open)?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(DatabaseError::Migrate)?;

        tracing::info!("database migrations complete");

        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[derive(Debug)]
pub enum DatabaseError {
    CreateDirectory(io::Error),
    Open(sqlx::Error),
    Migrate(MigrateError),
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateDirectory(error) => write!(
                formatter,
                "could not create altitude data directory: {error}"
            ),
            Self::Open(error) => write!(formatter, "could not open altitude database: {error}"),
            Self::Migrate(error) => {
                write!(formatter, "could not migrate altitude database: {error}")
            }
        }
    }
}

impl Error for DatabaseError {}
