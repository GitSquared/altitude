use std::{
    env, fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use async_trait::async_trait;
use rmcp::transport::{
    auth::{
        AuthClient, AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession,
        CredentialStore, StoredCredentials,
    },
    streamable_http_client::{StreamableHttpClientTransport, StreamableHttpClientTransportConfig},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use crate::tracker::TrackerError;

const MCP_URL: &str = "https://mcp.linear.app/mcp";
const GRANT_FILE: &str = "grant.json";

struct Grant(PathBuf);

impl Grant {
    fn new(profile: &Path) -> Self {
        Self(profile.join(GRANT_FILE))
    }

    fn is_present(&self) -> bool {
        self.0.is_file()
    }

    fn changed_at(&self) -> Option<SystemTime> {
        fs::metadata(&self.0)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
    }
}

#[async_trait]
impl CredentialStore for Grant {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        match fs::read_to_string(&self.0) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|error| AuthError::CredentialStoreError(error.to_string())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(AuthError::CredentialStoreError(error.to_string())),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let text = serde_json::to_string_pretty(&credentials)
            .map_err(|error| AuthError::CredentialStoreError(error.to_string()))?;
        write_private(&self.0, &text).map_err(AuthError::CredentialStoreError)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        match fs::remove_file(&self.0) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AuthError::CredentialStoreError(error.to_string())),
        }
    }
}

pub(super) fn has_grant(profile: &Path) -> bool {
    Grant::new(profile).is_present()
}

pub(super) fn grant_changed_at(profile: &Path) -> Option<SystemTime> {
    Grant::new(profile).changed_at()
}

pub(super) async fn authorized_client(
    profile: &Path,
) -> Result<AuthClient<reqwest::Client>, TrackerError> {
    let mut authorization = authorization(profile).await?;
    let restored = authorization
        .initialize_from_store()
        .await
        .map_err(|error| {
            TrackerError::new(format!("could not read the Linear sign-in: {error}"))
        })?;
    if !restored {
        authorization = sign_in(authorization).await?;
    }
    Ok(AuthClient::new(reqwest::Client::new(), authorization))
}

pub(super) fn transport(
    client: AuthClient<reqwest::Client>,
) -> StreamableHttpClientTransport<AuthClient<reqwest::Client>> {
    StreamableHttpClientTransport::with_client(
        client,
        StreamableHttpClientTransportConfig::with_uri(MCP_URL),
    )
}

async fn authorization(profile: &Path) -> Result<AuthorizationManager, TrackerError> {
    let mut authorization = AuthorizationManager::new(MCP_URL).await.map_err(|error| {
        TrackerError::new(format!("could not prepare the Linear sign-in: {error}"))
    })?;
    authorization.set_credential_store(Grant::new(profile));
    Ok(authorization)
}

async fn sign_in(
    mut authorization: AuthorizationManager,
) -> Result<AuthorizationManager, TrackerError> {
    let resolution = authorization.resolve_metadata().await.map_err(|error| {
        TrackerError::new(format!("Linear did not describe how to sign in: {error}"))
    })?;
    authorization.set_metadata(resolution.metadata);

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|error| {
        TrackerError::new(format!("could not listen for the Linear sign-in: {error}"))
    })?;
    let port = listener
        .local_addr()
        .map_err(|error| {
            TrackerError::new(format!("could not listen for the Linear sign-in: {error}"))
        })?
        .port();
    let request = AuthorizationRequest::new(format!("http://127.0.0.1:{port}/callback"))
        .with_client_name("altitude");
    let session = AuthorizationSession::new(authorization, request)
        .await
        .map_err(|(_, error)| {
            TrackerError::new(format!("Linear refused the sign-in request: {error}"))
        })?;

    open_browser(session.get_authorization_url())?;
    let redirect = wait_for_redirect(listener, port).await?;
    session
        .handle_callback_url(&redirect)
        .await
        .map_err(|error| {
            TrackerError::new(format!("Linear's sign-in did not complete: {error}"))
        })?;
    Ok(session.auth_manager)
}

async fn wait_for_redirect(listener: TcpListener, port: u16) -> Result<String, TrackerError> {
    let (mut stream, _) = listener
        .accept()
        .await
        .map_err(|error| TrackerError::new(format!("the Linear sign-in stopped: {error}")))?;
    let mut buffer = [0u8; 4096];
    let read = stream
        .read(&mut buffer)
        .await
        .map_err(|error| TrackerError::new(format!("the Linear sign-in stopped: {error}")))?;
    let request = String::from_utf8_lossy(&buffer[..read]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| TrackerError::new("the Linear sign-in returned nothing readable"))?;
    let body =
        "<html><body>altitude is connected to Linear. You can close this page.</body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    Ok(format!("http://127.0.0.1:{port}{target}"))
}

fn open_browser(url: &str) -> Result<(), TrackerError> {
    let opener = match env::consts::OS {
        "macos" => "open",
        _ => "xdg-open",
    };
    let status = Command::new(opener)
        .arg(url)
        .status()
        .map_err(|error| TrackerError::new(format!("could not open a browser: {error}")))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| TrackerError::new(format!("{opener} exited with {status}")))
}

fn write_private(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = path.with_file_name(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(text.as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&temporary, path).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
