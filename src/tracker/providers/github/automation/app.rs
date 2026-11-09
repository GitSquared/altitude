use std::{
    collections::HashMap,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::Method;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::tracker::TrackerError;

use super::super::api::GitHubApi;

// A GitHub App proves who it is with a short-lived JWT, and acts on a repository
// with an installation token that lasts an hour. Both are derived from the private
// key on demand, so a token that expired is a token asked for again rather than a
// refresh loop to keep running.
const JWT_LIFETIME: Duration = Duration::from_secs(9 * 60);
const JWT_CLOCK_SKEW: Duration = Duration::from_secs(60);
const TOKEN_MARGIN: Duration = Duration::from_secs(5 * 60);

pub(super) struct AppCredentials {
    app_id: String,
    key: EncodingKey,
    installations: Mutex<HashMap<String, u64>>,
    tokens: Mutex<HashMap<u64, InstallationToken>>,
}

struct InstallationToken {
    token: String,
    usable_until: Instant,
}

#[derive(Serialize)]
struct Claims {
    iat: u64,
    exp: u64,
    iss: String,
}

impl AppCredentials {
    pub(super) fn new(app_id: String, private_key_pem: &[u8]) -> Result<Self, TrackerError> {
        let key = EncodingKey::from_rsa_pem(private_key_pem).map_err(|_| {
            TrackerError::new("the GitHub App private key is not an RSA key in PEM form")
        })?;
        Ok(Self {
            app_id,
            key,
            installations: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
        })
    }

    pub(super) fn jwt(&self) -> Result<String, TrackerError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| TrackerError::new("this machine's clock is before 1970"))?;
        let claims = Claims {
            iat: (now - JWT_CLOCK_SKEW).as_secs(),
            exp: (now + JWT_LIFETIME).as_secs(),
            iss: self.app_id.clone(),
        };
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key)
            .map_err(|_| TrackerError::new("could not sign the GitHub App's token"))
    }

    pub(super) async fn token_for_repository(
        &self,
        api: &GitHubApi,
        repository: &str,
    ) -> Result<String, TrackerError> {
        let installation = self.installation_for(api, repository).await?;
        self.token_for_installation(api, installation).await
    }

    pub(super) async fn token_for_installation(
        &self,
        api: &GitHubApi,
        installation: u64,
    ) -> Result<String, TrackerError> {
        let mut tokens = self.tokens.lock().await;
        if let Some(held) = tokens.get(&installation)
            && held.usable_until > Instant::now()
        {
            return Ok(held.token.clone());
        }
        let granted = api
            .send(
                &self.jwt()?,
                Method::POST,
                &format!("/app/installations/{installation}/access_tokens"),
                None,
            )
            .await?;
        let token = granted
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| TrackerError::new("GitHub granted no installation token"))?
            .to_owned();
        let lifetime = granted
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .and_then(|at| {
                (at.with_timezone(&chrono::Utc) - chrono::Utc::now())
                    .to_std()
                    .ok()
            })
            .unwrap_or(Duration::from_secs(60 * 60));
        tokens.insert(
            installation,
            InstallationToken {
                token: token.clone(),
                usable_until: Instant::now() + lifetime.saturating_sub(TOKEN_MARGIN),
            },
        );
        Ok(token)
    }

    // A user is the same person on every installation, so looking one up can use
    // whichever installation answers.
    pub(super) async fn any_installation_token(
        &self,
        api: &GitHubApi,
    ) -> Result<String, TrackerError> {
        let held = self.tokens.lock().await.keys().next().copied();
        let installation = match held {
            Some(installation) => installation,
            None => *self.installations(api).await?.first().ok_or_else(|| {
                TrackerError::new(
                    "the GitHub App is not installed anywhere yet; install it on a repository",
                )
            })?,
        };
        self.token_for_installation(api, installation).await
    }

    pub(super) async fn installations(&self, api: &GitHubApi) -> Result<Vec<u64>, TrackerError> {
        Ok(api
            .pages(&self.jwt()?, "/app/installations?per_page=100")
            .await?
            .iter()
            .filter_map(|installation| installation.get("id").and_then(Value::as_u64))
            .collect())
    }

    async fn installation_for(
        &self,
        api: &GitHubApi,
        repository: &str,
    ) -> Result<u64, TrackerError> {
        let key = repository.to_ascii_lowercase();
        if let Some(installation) = self.installations.lock().await.get(&key) {
            return Ok(*installation);
        }
        let installation = api
            .get(&self.jwt()?, &format!("/repos/{repository}/installation"))
            .await
            .map_err(|_| {
                TrackerError::new(format!(
                    "the GitHub App is not installed on {repository}; install it from the app's page"
                ))
            })?
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| TrackerError::new("GitHub named no installation for that repository"))?;
        self.installations.lock().await.insert(key, installation);
        Ok(installation)
    }
}
