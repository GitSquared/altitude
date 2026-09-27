use std::{
    env, io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;

#[derive(Clone)]
pub struct Config {
    data_dir: PathBuf,
    cache_dir: PathBuf,
}

impl Config {
    pub fn load() -> io::Result<Self> {
        let directories = ProjectDirs::from("", "", "altitude")
            .ok_or_else(|| io::Error::other("could not determine altitude's data directory"))?;

        Ok(Self {
            data_dir: env::var_os("ALTITUDE_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| directories.data_dir().to_owned()),
            cache_dir: env::var_os("ALTITUDE_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| directories.cache_dir().to_owned()),
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("altitude.db")
    }

    pub fn isolation_data_dir(&self) -> PathBuf {
        self.data_dir.join("isolation")
    }

    pub fn isolation_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("isolation")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.cache_dir.join("logs")
    }

    pub fn microsandbox_socket_dir(&self) -> PathBuf {
        self.cache_dir.join("msb")
    }

    pub fn task_snapshots_dir(&self) -> PathBuf {
        self.data_dir.join("snapshots")
    }

    pub fn profile_dir(&self, profile: &str) -> PathBuf {
        self.data_dir.join(profile).join("profile")
    }

    pub fn environment_snapshots_dir(&self, name: &str) -> PathBuf {
        self.data_dir
            .join("environments")
            .join(name)
            .join("snapshots")
    }
}
