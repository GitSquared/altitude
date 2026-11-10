use std::{
    fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::config::{DEFAULT_PORT, SettingsError};
use crate::tracker::TrackerKind;
use crate::tracker::providers::{
    github::settings::GitHubSettings, linear::settings::LinearSettings,
};

fn default_listen() -> String {
    format!("0.0.0.0:{DEFAULT_PORT}")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServeSettings {
    pub listen: String,
    #[serde(default, rename = "linear", skip_serializing_if = "Option::is_none")]
    pub tracker: Option<LinearSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<GitHubSettings>,
    #[serde(default)]
    pub pause_delegated_work: bool,
}

impl Default for ServeSettings {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            tracker: None,
            github: None,
            pause_delegated_work: false,
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
    let kind = TrackerKind::Linear;
    format!(
        "# Where clients reach this remote: discovery over TCP, tasks over UDP.\n\
         listen = \"{listen}\"\n\
         \n\
         # Stop starting work delegated from a task tracker, without taking the\n\
         # tracker away. An operator consents to that per project; this is the brake.\n\
         # pause_delegated_work = true\n\
         \n\
         # A shared {name} app for work delegated from a task tracker. Install it with\n\
         # `altitude serve connect {slug}`, which prints the exact addresses to register.\n\
         # public_url is the HTTPS address that reaches this machine, and the secrets belong\n\
         # here, in a file only this machine's user can read.\n\
         # [{slug}]\n\
         # client_id = \"\"\n\
         # public_url = \"\"\n\
         # client_secret = \"\"\n\
         # webhook_secret = \"\"\n\
         \n\
         # Or a GitHub App, for work delegated by labelling an issue agent:altitude.\n\
         # A remote reports into one tracker; with both set it uses [{slug}].\n\
         # [github]\n\
         # app_id = \"\"\n\
         # public_url = \"\"\n\
         # private_key_path = \"\"\n\
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
