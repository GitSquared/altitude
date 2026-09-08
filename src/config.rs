use std::{
    io,
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
            data_dir: directories.data_dir().to_owned(),
            cache_dir: directories.cache_dir().to_owned(),
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

    pub fn microsandbox_socket_dir(&self) -> PathBuf {
        self.cache_dir.join("msb")
    }
}
