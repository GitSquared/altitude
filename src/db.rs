use std::{fmt, fs, io};

use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};

use crate::config::Config;

pub struct Database {
    pool: SqlitePool,
}

impl Database {
    pub async fn open(config: &Config) -> Result<Self, DatabaseError> {
        fs::create_dir_all(config.data_dir()).map_err(DatabaseError::CreateDirectory)?;

        let options = SqliteConnectOptions::new()
            .filename(config.database_path())
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePool::connect_with(options)
            .await
            .map_err(DatabaseError::Open)?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(DatabaseError::Migrate)?;

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
    Migrate(sqlx::migrate::MigrateError),
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

impl std::error::Error for DatabaseError {}
