use std::{env, fmt};

use serde::{Deserialize, Serialize};

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
