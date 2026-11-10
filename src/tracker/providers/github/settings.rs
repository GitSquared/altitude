use std::{
    env, fmt, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

// A GitHub App's settings on the remote that runs it. The key stays a path: the
// settings file can be shown without showing the key, and the key is read where the
// app runs and nowhere else.
#[derive(Clone, Deserialize, Serialize)]
pub struct GitHubSettings {
    pub app_id: String,
    pub public_url: String,
    pub private_key_path: String,
    #[serde(default = "default_github_listen")]
    pub listen: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secret: Option<String>,
    #[serde(default = "default_webhook_secret_env")]
    pub webhook_secret_env: String,
}

impl fmt::Debug for GitHubSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubSettings")
            .field("app_id", &self.app_id)
            .field("public_url", &self.public_url)
            .field("private_key_path", &self.private_key_path)
            .field("listen", &self.listen)
            .field(
                "webhook_secret",
                &self.webhook_secret.as_ref().map(|_| "redacted"),
            )
            .field("webhook_secret_env", &self.webhook_secret_env)
            .finish()
    }
}

fn default_github_listen() -> String {
    "127.0.0.1:25850".into()
}

fn default_webhook_secret_env() -> String {
    "ALTITUDE_GITHUB_WEBHOOK_SECRET".into()
}

impl GitHubSettings {
    pub fn webhook_secret(&self) -> Result<String, String> {
        if let Some(secret) = self
            .webhook_secret
            .as_deref()
            .map(str::trim)
            .filter(|secret| !secret.is_empty())
        {
            return Ok(secret.to_owned());
        }
        env::var(&self.webhook_secret_env)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!(
                    "set {} on the remote host, or write the value into this file's [github] \
                     section",
                    self.webhook_secret_env
                )
            })
    }

    pub fn private_key(&self) -> Result<Vec<u8>, String> {
        let path = home_expanded(&self.private_key_path);
        fs::read(&path).map_err(|error| {
            format!(
                "could not read the GitHub App private key at {}: {error}",
                path.display()
            )
        })
    }
}

fn home_expanded(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => env::var_os("HOME")
            .map(|home| Path::new(&home).join(rest))
            .unwrap_or_else(|| Path::new(path).to_owned()),
        None => Path::new(path).to_owned(),
    }
}
