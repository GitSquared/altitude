use std::{fs, io, path::Path};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};

use crate::config::Settings;
use crate::project::Project;
use crate::task::memory;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigBundle {
    pub settings: String,
    pub profiles: Vec<ProfileBundle>,
    // A project's memory, carried so a machine that has not been handed one of the
    // project's tasks yet still starts from what earlier tasks elsewhere found.
    #[serde(default)]
    pub memory: Vec<MemoryBundle>,
    #[serde(default)]
    pub remove_logins: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryBundle {
    pub project_id: String,
    #[serde(default)]
    pub files: Vec<MemoryFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryFile {
    pub path: String,
    pub contents: Option<String>,
    pub at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileBundle {
    pub profile: String,
    pub digest: String,
    pub files: Vec<ProfileFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileFile {
    pub path: String,
    pub contents: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileDigest {
    pub profile: String,
    pub digest: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HeldConfig {
    pub profiles: Vec<ProfileDigest>,
}

impl HeldConfig {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let incoming = path.with_extension("json.incoming");
        fs::write(&incoming, serde_json::to_vec(self)?)?;
        fs::rename(&incoming, path)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Logins {
    Always,
    WithWork,
}

impl ConfigBundle {
    pub fn digest(&self) -> io::Result<String> {
        let profiles = self
            .profiles
            .iter()
            .map(|profile| (&profile.profile, &profile.digest))
            .collect::<Vec<_>>();
        Ok(blake3::hash(&serde_json::to_vec(&(
            &self.settings,
            profiles,
            &self.memory,
        ))?)
        .to_hex()
        .to_string())
    }

    pub fn retain_changed_profiles(&mut self, held: &[ProfileDigest]) {
        self.profiles.retain(|profile| {
            !held
                .iter()
                .any(|stamp| stamp.profile == profile.profile && stamp.digest == profile.digest)
        });
    }

    pub fn of(
        settings: &str,
        data_dir: &Path,
        held: &[ProfileDigest],
        host: Option<&str>,
        logins: Logins,
    ) -> Result<Self, io::Error> {
        let mut parsed: Settings = toml::from_str(settings).map_err(io::Error::other)?;
        // Filtered before `shared()`, which clears the preference: the machine a
        // project runs on is how this decides what to send, then it is no longer
        // the remote's business.
        if let Some(host) = host {
            parsed.retain_projects_for(host);
        }
        let holds_work = !parsed.projects().is_empty();
        let memory = if holds_work {
            project_memory(data_dir, parsed.projects())
        } else {
            Vec::new()
        };
        let settings = toml::to_string(&parsed.shared()).map_err(io::Error::other)?;
        let profiles = match logins {
            Logins::Always => true,
            Logins::WithWork => holds_work,
        };
        let profiles = if profiles {
            parsed
                .used_credential_ids()
                .iter()
                .map(|profile| ProfileBundle::of(data_dir, profile))
                .collect::<io::Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        let remove_logins = logins == Logins::WithWork && !holds_work;
        let mut bundle = Self {
            settings,
            profiles,
            memory,
            remove_logins,
        };
        bundle.retain_changed_profiles(held);
        Ok(bundle)
    }
}

fn project_memory(data_dir: &Path, projects: &[Project]) -> Vec<MemoryBundle> {
    projects
        .iter()
        .filter_map(|project| {
            let directory = data_dir.join("projects").join(&project.id).join("memory");
            let files = memory::read(&directory).ok()?;
            if files.is_empty() {
                return None;
            }
            Some(MemoryBundle {
                project_id: project.id.clone(),
                files: files
                    .into_iter()
                    .map(|file| MemoryFile {
                        path: file.path,
                        contents: file.contents,
                        at_ms: file.at_ms,
                    })
                    .collect(),
            })
        })
        .collect()
}

impl ProfileBundle {
    pub fn of(data_dir: &Path, profile: &str) -> Result<Self, io::Error> {
        let root = data_dir.join(profile).join("profile");
        let mut files = Vec::new();
        if root.is_dir() {
            collect(&root, &root, &mut files)?;
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let digest = blake3::hash(&serde_json::to_vec(&files)?)
            .to_hex()
            .to_string();
        Ok(Self {
            profile: profile.to_owned(),
            digest,
            files,
        })
    }
}

fn collect(root: &Path, directory: &Path, files: &mut Vec<ProfileFile>) -> Result<(), io::Error> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            collect(root, &path, files)?;
            continue;
        }
        // A symlink inside a profile points at something on the machine that wrote
        // it, which means nothing here. Only the files themselves travel.
        if meta.file_type().is_symlink() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        files.push(ProfileFile {
            path: relative,
            contents: BASE64.encode(fs::read(&path)?),
        });
    }
    Ok(())
}
