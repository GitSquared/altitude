//! One driver per task database.
//!
//! A task's record has one driver: the process holding its capsule, its agent
//! channel and its run. A second client reading the same database sees the same
//! pending tasks and provisions them again — and because a setup sandbox is named
//! after the environment it builds, the second build removes the first one's
//! sandbox as a leftover, failing a task that had done nothing wrong. So the
//! client claims the database for as long as it runs, and a second client says so
//! rather than racing.

use std::{
    error::Error,
    ffi::OsStr,
    fmt, fs, io,
    path::{Path, PathBuf},
};

pub struct Instance {
    _file: fs::File,
}

pub enum ClaimError {
    InUse { database: PathBuf },
    Io(io::Error),
}

impl Instance {
    pub fn claim(database: &Path) -> Result<Self, ClaimError> {
        // The database is created when it is opened, which is after this on a first
        // start, and the lock lives beside it.
        if let Some(directory) = database.parent() {
            fs::create_dir_all(directory)?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path(database))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs::TryLockError::WouldBlock) => Err(ClaimError::InUse {
                database: database.to_owned(),
            }),
            Err(fs::TryLockError::Error(error)) => Err(ClaimError::Io(error)),
        }
    }
}

impl From<io::Error> for ClaimError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl fmt::Display for ClaimError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InUse { database } => write!(
                formatter,
                "Altitude is already running with {}. Quit it there first, or start this one \
                 with another ALTITUDE_DATA_DIR.",
                database.display()
            ),
            Self::Io(error) => write!(formatter, "could not claim the task database: {error}"),
        }
    }
}

impl fmt::Debug for ClaimError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Error for ClaimError {}

fn lock_path(database: &Path) -> PathBuf {
    let mut name = database
        .file_name()
        .unwrap_or_else(|| OsStr::new("altitude.db"))
        .to_os_string();
    name.push(".lock");
    database.with_file_name(name)
}
