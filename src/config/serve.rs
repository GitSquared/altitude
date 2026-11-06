use std::{
    env, fmt, fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use serde::{Deserialize, Serialize};

use super::SettingsError;

pub const DEFAULT_PORT: u16 = 25848;

pub fn default_listen() -> String {
    format!("0.0.0.0:{DEFAULT_PORT}")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServeSettings {
    pub listen: String,
    pub linear: Option<LinearSettings>,
    #[serde(default)]
    pub pause_delegated_work: bool,
}

impl Default for ServeSettings {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            linear: None,
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
    format!(
        "# Where clients reach this remote: discovery over TCP, tasks over UDP.\n\
         listen = \"{}\"\n\
         \n\
         # Stop starting work delegated from a task tracker, without taking the\n\
         # tracker away. An operator consents to that per project; this is the brake.\n\
         # pause_delegated_work = true\n\
         \n\
         # A shared app for work delegated from a task tracker. Install it with\n\
         # `altitude serve connect linear`, which prints the exact addresses to register.\n\
         # public_url is the HTTPS address that reaches this machine, and the secrets belong\n\
         # here, in a file only this machine's user can read.\n\
         # [linear]\n\
         # client_id = \"\"\n\
         # public_url = \"\"\n\
         # client_secret = \"\"\n\
         # webhook_secret = \"\"\n",
        default_listen()
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

#[derive(Clone, Deserialize, Serialize)]
pub struct LinearSettings {
    pub client_id: String,
    pub public_url: String,
    #[serde(default = "default_linear_listen")]
    pub listen: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secret: Option<String>,
    #[serde(default = "default_client_secret_env")]
    pub client_secret_env: String,
    #[serde(default = "default_webhook_secret_env")]
    pub webhook_secret_env: String,
}

impl fmt::Debug for LinearSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn redacted(secret: &Option<String>) -> Option<&'static str> {
            secret.as_ref().map(|_| "redacted")
        }
        formatter
            .debug_struct("LinearSettings")
            .field("client_id", &self.client_id)
            .field("public_url", &self.public_url)
            .field("listen", &self.listen)
            .field("client_secret", &redacted(&self.client_secret))
            .field("webhook_secret", &redacted(&self.webhook_secret))
            .field("client_secret_env", &self.client_secret_env)
            .field("webhook_secret_env", &self.webhook_secret_env)
            .finish()
    }
}

fn default_linear_listen() -> String {
    "127.0.0.1:25849".into()
}

fn default_client_secret_env() -> String {
    "ALTITUDE_LINEAR_CLIENT_SECRET".into()
}

fn default_webhook_secret_env() -> String {
    "ALTITUDE_LINEAR_WEBHOOK_SECRET".into()
}

impl LinearSettings {
    pub fn endpoint(&self, path: &str) -> Result<url::Url, String> {
        let mut url = url::Url::parse(&self.public_url)
            .map_err(|_| "Linear public_url must be an HTTPS URL")?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "Linear public_url must be an HTTPS URL without credentials, query, or fragment"
                    .into(),
            );
        }
        url.set_path(path);
        Ok(url)
    }

    pub fn client_secret(&self) -> Result<String, String> {
        secret(self.client_secret.as_deref(), &self.client_secret_env)
    }

    pub fn webhook_secret(&self) -> Result<String, String> {
        secret(self.webhook_secret.as_deref(), &self.webhook_secret_env)
    }
}

fn secret(inline: Option<&str>, environment: &str) -> Result<String, String> {
    if let Some(secret) = inline.map(str::trim).filter(|secret| !secret.is_empty()) {
        return Ok(secret.to_owned());
    }
    secret_from_environment(environment)
}

fn secret_from_environment(name: &str) -> Result<String, String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "set {name} on the remote host, or write the value into this file's [linear] section"
            )
        })
}
