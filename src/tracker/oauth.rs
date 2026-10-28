//! Infrastructure-owned Linear app installation and token refresh.

use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};

use crate::isolation::private_directory;
use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    routing::get,
};
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc, oneshot},
    time::timeout,
};

use super::{TrackerError, automation::LinearAutomation};
use crate::config::LinearSettings;

const TOKEN_URL: &str = "https://api.linear.app/oauth/token";

#[derive(Deserialize, Serialize)]
pub struct AppCredentials {
    access_token: String,
    refresh_token: String,
    expires_at_ms: i64,
    pub workspace_id: String,
    /// What the workspace was called when it was installed, so a machine can say
    /// what it holds without asking Linear again.
    #[serde(default)]
    pub workspace_name: String,
    pub app_user_id: String,
}

/// What a machine already has installed, read from the file it saved.
pub struct Installed {
    pub credentials: AppCredentials,
    pub installed_at: Option<SystemTime>,
}

pub fn installed(path: &Path) -> Option<Installed> {
    let credentials = fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())?;
    let installed_at = fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok());
    Some(Installed {
        credentials,
        installed_at,
    })
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

impl AppCredentials {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|error| {
            format!("Linear app is not connected: {error}. Run altitude serve connect linear on this host.")
        })?;
        serde_json::from_str(&text).map_err(|_| "the saved Linear app login is invalid".into())
    }

    pub async fn client(&self) -> Result<LinearAutomation, TrackerError> {
        let client =
            LinearAutomation::new(&self.access_token, &self.workspace_id, &self.app_user_id)?;
        client.verify().await?;
        Ok(client)
    }

    pub async fn refresh_if_needed(
        &mut self,
        settings: &LinearSettings,
        path: &Path,
    ) -> Result<bool, String> {
        if self.expires_at_ms > chrono::Utc::now().timestamp_millis() + 300_000 {
            return Ok(false);
        }
        let secret = settings.client_secret()?;
        let response = exchange(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &self.refresh_token),
            ("client_id", &settings.client_id),
            ("client_secret", &secret),
        ])
        .await?;
        let candidate = Self {
            access_token: response.access_token,
            refresh_token: response
                .refresh_token
                .unwrap_or_else(|| self.refresh_token.clone()),
            expires_at_ms: expiry(response.expires_in)?,
            workspace_id: self.workspace_id.clone(),
            workspace_name: self.workspace_name.clone(),
            app_user_id: self.app_user_id.clone(),
        };
        candidate
            .client()
            .await
            .map_err(|error| error.to_string())?;
        candidate.save(path)?;
        *self = candidate;
        Ok(true)
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        let parent = path
            .parent()
            .ok_or("Linear login has no parent directory")?;
        private_directory(parent).map_err(|error| error.to_string())?;
        let temporary = parent.join(format!(".oauth-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            let bytes =
                serde_json::to_vec(self).map_err(|_| "could not encode the Linear login")?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|_| "could not save the Linear login")?;
            file.write_all(&bytes)
                .map_err(|_| "could not save the Linear login")?;
            file.sync_all()
                .map_err(|_| "could not save the Linear login")?;
            fs::rename(&temporary, path).map_err(|_| "could not save the Linear login")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

fn expiry(seconds: u64) -> Result<i64, String> {
    let duration = i64::try_from(seconds)
        .ok()
        .and_then(|value| value.checked_mul(1_000))
        .ok_or("Linear returned an invalid token lifetime")?;
    chrono::Utc::now()
        .timestamp_millis()
        .checked_add(duration)
        .ok_or_else(|| "Linear returned an invalid token lifetime".into())
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| "could not initialize Linear sign-in".into())
}

async fn exchange(fields: &[(&str, &str)]) -> Result<TokenResponse, String> {
    let response = http_client()?
        .post(TOKEN_URL)
        .form(fields)
        .send()
        .await
        .map_err(|_| "Linear did not answer the sign-in request")?;
    if !response.status().is_success() {
        return Err(
            "Linear rejected the sign-in request; check the app settings and try again".into(),
        );
    }
    let token: TokenResponse = response
        .json()
        .await
        .map_err(|_| "Linear returned an invalid sign-in response")?;
    if token.access_token.is_empty() || token.expires_in == 0 {
        return Err("Linear returned an incomplete sign-in response".into());
    }
    Ok(token)
}

struct Installation {
    settings: LinearSettings,
    state: String,
    result: Mutex<Option<mpsc::Sender<Result<AppCredentials, String>>>>,
}

#[derive(Deserialize)]
struct Callback {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

async fn callback(
    State(installation): State<Arc<Installation>>,
    Query(query): Query<Callback>,
) -> (StatusCode, &'static str) {
    if query.state.as_deref() != Some(&installation.state) {
        return (
            StatusCode::BAD_REQUEST,
            "This sign-in does not belong to this Altitude installation.",
        );
    }
    // Consume once before exchanging: a repeated callback cannot replace the installed workspace.
    let Some(result) = installation.result.lock().await.take() else {
        return (
            StatusCode::CONFLICT,
            "This sign-in has already been handled.",
        );
    };
    let credentials = if query.error.is_some() {
        Err("Linear sign-in was declined".into())
    } else if let Some(code) = query.code {
        install(&installation.settings, &code).await
    } else {
        Err("Linear did not return an authorization code".into())
    };
    let is_ok = credentials.is_ok();
    let _ = result.send(credentials).await;
    if is_ok {
        (
            StatusCode::OK,
            "Altitude is connected. You can close this page and return to the remote host.",
        )
    } else {
        (
            StatusCode::BAD_REQUEST,
            "Altitude could not connect. Check the remote host for setup guidance.",
        )
    }
}

async fn install(settings: &LinearSettings, code: &str) -> Result<AppCredentials, String> {
    let secret = settings.client_secret()?;
    let redirect = settings
        .endpoint("/altitude/linear/oauth/callback")?
        .to_string();
    let token = exchange(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", &redirect),
        ("client_id", &settings.client_id),
        ("client_secret", &secret),
    ])
    .await?;
    let response = http_client()?
        .post("https://api.linear.app/graphql")
        .bearer_auth(&token.access_token)
        .json(&serde_json::json!({
            "query": "query { viewer { id app organization { id name } } }"
        }))
        .send()
        .await
        .map_err(|_| "Linear did not identify the installed app")?;
    if !response.status().is_success() {
        return Err("Linear did not identify the installed app".into());
    }
    let response: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "Linear returned an invalid app identity")?;
    if response
        .get("errors")
        .is_some_and(|errors| !errors.as_array().is_some_and(Vec::is_empty))
    {
        return Err("Linear could not verify the installed app".into());
    }
    let viewer = response
        .pointer("/data/viewer")
        .ok_or("Linear returned no app identity")?;
    if viewer.get("app").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err("Linear installed a personal login instead of the Altitude app".into());
    }
    let app_user_id = viewer
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or("Linear returned no app user")?
        .to_owned();
    let workspace_id = viewer
        .pointer("/organization/id")
        .and_then(serde_json::Value::as_str)
        .ok_or("Linear returned no workspace")?
        .to_owned();
    let workspace_name = viewer
        .pointer("/organization/name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(AppCredentials {
        access_token: token.access_token,
        refresh_token: token
            .refresh_token
            .ok_or("Linear returned no refresh token")?,
        expires_at_ms: expiry(token.expires_in)?,
        workspace_id,
        workspace_name,
        app_user_id,
    })
}

pub async fn connect(settings: LinearSettings, path: &Path) -> Result<AppCredentials, String> {
    settings.client_secret()?;
    settings.webhook_secret()?;
    let redirect = settings.endpoint("/altitude/linear/oauth/callback")?;
    let listener = TcpListener::bind(&settings.listen)
        .await
        .map_err(|error| format!("could not listen for Linear sign-in: {error}"))?;
    let state = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let mut authorize =
        url::Url::parse("https://linear.app/oauth/authorize").expect("constant OAuth URL");
    authorize
        .query_pairs_mut()
        .append_pair("client_id", &settings.client_id)
        .append_pair("redirect_uri", redirect.as_str())
        .append_pair("response_type", "code")
        .append_pair("actor", "app")
        .append_pair("scope", "read,write,app:assignable,app:mentionable")
        .append_pair("state", &state);
    let (result, mut received) = mpsc::channel(1);
    let installation = Arc::new(Installation {
        settings,
        state,
        result: Mutex::new(Some(result)),
    });
    let router = Router::new()
        .route("/altitude/linear/oauth/callback", get(callback))
        .with_state(installation);
    println!("Open this as a workspace administrator:\n\n{authorize}\n");
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    let credentials = timeout(Duration::from_secs(900), received.recv()).await;
    let _ = stop.send(());
    let server_result = server
        .await
        .map_err(|_| "Linear sign-in listener stopped unexpectedly")?;
    server_result.map_err(|_| "Linear sign-in listener failed")?;
    let credentials = credentials
        .map_err(|_| "Linear sign-in timed out; run `altitude serve connect linear` again")?
        .ok_or("Linear sign-in listener stopped before completion")??;
    credentials.save(path)?;
    Ok(credentials)
}
