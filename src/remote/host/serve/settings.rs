use std::{
    fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::config::{DEFAULT_PORT, SettingsError};
use crate::tracker::TrackerKind;
use crate::tracker::providers::linear::settings::LinearSettings;

fn default_listen() -> String {
    format!("0.0.0.0:{DEFAULT_PORT}")
}

fn default_worker_max() -> usize {
    8
}

fn default_provision_timeout_secs() -> u64 {
    10 * 60
}

fn default_deprovision_timeout_secs() -> u64 {
    5 * 60
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServeSettings {
    pub listen: String,
    #[serde(default, rename = "linear", skip_serializing_if = "Option::is_none")]
    pub tracker: Option<LinearSettings>,
    #[serde(default)]
    pub pause_delegated_work: bool,
    // Configured together: a fleet that can be grown but not shrunk would strand
    // machines, so the two scripts are one decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers: Option<WorkersSettings>,
}

// How a remote grows beyond this machine. The scripts are the whole infra
// contract: altitude names a worker and runs them, and it does not need to know
// what a worker actually is.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkersSettings {
    pub provision: String,
    pub deprovision: String,
    #[serde(default = "default_worker_max")]
    pub max: usize,
    #[serde(default = "default_provision_timeout_secs")]
    pub provision_timeout_secs: u64,
    #[serde(default = "default_deprovision_timeout_secs")]
    pub deprovision_timeout_secs: u64,
}

impl WorkersSettings {
    // The ceiling is `max`, the most hosts this remote will have running at
    // once. There is no floor: a host is spawned for a sandbox that needs one
    // and destroyed when it ends, so an idle remote runs none.
    pub fn ceiling(&self) -> usize {
        self.max
    }
}

impl Default for ServeSettings {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            tracker: None,
            pause_delegated_work: false,
            workers: None,
        }
    }
}

impl ServeSettings {
    pub fn load_or_create(path: &Path) -> Result<Self, SettingsError> {
        match fs::read_to_string(path) {
            Ok(source) => toml::from_str(&source).map_err(SettingsError::Parse),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                write_new(path, &template())?;
                Ok(Self::default())
            }
            Err(error) => Err(SettingsError::Io(error)),
        }
    }
}

fn template() -> String {
    let kind = TrackerKind::sole();
    format!(
        "# Where clients reach this remote: discovery over TCP, tasks over UDP.\n\
         listen = \"{listen}\"\n\
         \n\
         # Stop starting work delegated from a task tracker, without taking the\n\
         # tracker away. An operator consents to that per project; this is the brake.\n\
         # pause_delegated_work = true\n\
         \n\
         # Grow this remote beyond this host. Each script is handed\n\
         # ALTITUDE_MASTER_TICKET, ALTITUDE_WORKER_TOKEN and ALTITUDE_WORKER_NAME and\n\
         # must bring one worker up or take it down.\n\
         # [workers]\n\
         # provision = \"/etc/altitude/provision-worker.sh\"\n\
         # deprovision = \"/etc/altitude/deprovision-worker.sh\"\n\
         # max = 8\n\
         # provision_timeout_secs = 600\n\
         # deprovision_timeout_secs = 300\n\
         \n\
         # A shared {name} app for work delegated from a task tracker. Install it with\n\
         # `altitude serve connect {slug}`, which prints the exact addresses to register.\n\
         # public_url is the HTTPS address that reaches this machine, and the secrets belong\n\
         # here, in a file only this machine's user can read.\n\
         # [{slug}]\n\
         # client_id = \"\"\n\
         # public_url = \"\"\n\
         # client_secret = \"\"\n\
         # webhook_secret = \"\"\n",
        listen = default_listen(),
        name = kind.label(),
        slug = kind.slug()
    )
}

fn write_new(path: &Path, contents: &str) -> Result<(), SettingsError> {
    let directory = path.parent().expect("a settings path has a parent");
    fs::create_dir_all(directory).map_err(SettingsError::Io)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(SettingsError::Io)?;
    file.write_all(contents.as_bytes())
        .map_err(SettingsError::Io)?;
    file.sync_all().map_err(SettingsError::Io)?;
    Ok(())
}
