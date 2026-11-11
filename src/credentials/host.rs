use std::{
    env, fs, io,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};

use crate::{agent::AgentKind, config::Config, vcs::VcsProviderKind};

use super::{Credential, Service, files};

// A login the operator already has on this machine, at the place its tool keeps
// it, which altitude can copy into a profile of its own.
pub struct HostLogin {
    pub service: Service,
    pub source: PathBuf,
    relative: &'static str,
}

// The logins this machine has that altitude does not: at most one per service,
// and only while none of that service's profiles is signed in. Nothing is read
// here — a file's size says whether it holds a login — so browsing the settings
// list never opens a secret.
pub fn logins(config: &Config, known: &[Credential]) -> Vec<HostLogin> {
    Service::ALL
        .into_iter()
        .filter(|service| service.is_mounted())
        .filter(|service| !is_signed_in(config, known, *service))
        .filter_map(host_login)
        .collect()
}

// Copying is the whole of it: the file stays where the tool wrote it, and the
// profile gets a private copy the task mounts.
pub fn import(config: &Config, login: &HostLogin, id: &str) -> io::Result<()> {
    let destination = config.profile_dir(id).join(login.relative);
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("an imported login has a file name"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    // The source may be world-readable; the copy is the operator's secret and is
    // kept to them.
    fs::copy(&login.source, &destination)?;
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o600))
}

fn is_signed_in(config: &Config, known: &[Credential], service: Service) -> bool {
    known.iter().any(|credential| {
        credential.service() == Some(service)
            && files::has_saved_login(&config.profile_dir(&credential.id), &credential.id)
    })
}

fn host_login(service: Service) -> Option<HostLogin> {
    let (source, relative) = match service {
        Service::Agent(AgentKind::Claude) => {
            let directory = tool_dir("CLAUDE_CONFIG_DIR", ".claude")?;
            (directory.join(".credentials.json"), ".credentials.json")
        }
        Service::Agent(AgentKind::Pi) => {
            let directory = tool_dir("PI_CODING_AGENT_DIR", ".pi/agent")?;
            (directory.join("auth.json"), "auth.json")
        }
        Service::VersionControl(VcsProviderKind::GitHub) => {
            let directory = dir_from_env("GH_CONFIG_DIR")
                .or_else(|| config_home().map(|home| home.join("gh")))?;
            (directory.join("hosts.yml"), "gh/hosts.yml")
        }
        // A tracker's session is altitude's own, not a file the tool left here.
        Service::Tracker(_) => return None,
    };
    holds_login(&source).then_some(HostLogin {
        service,
        source,
        relative,
    })
}

fn holds_login(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

// The directory a tool keeps its login in: the variable it honours, else the
// place under the operator's home it falls back to.
fn tool_dir(variable: &str, fallback: &str) -> Option<PathBuf> {
    dir_from_env(variable).or_else(|| home_dir().map(|home| home.join(fallback)))
}

fn config_home() -> Option<PathBuf> {
    dir_from_env("XDG_CONFIG_HOME").or_else(|| home_dir().map(|home| home.join(".config")))
}

fn dir_from_env(variable: &str) -> Option<PathBuf> {
    env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> Option<PathBuf> {
    dir_from_env("HOME")
}
