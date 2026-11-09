use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::{net::TcpListener, time};

use crate::config::Config;
use crate::tracker::IssueReader;
use crate::tracker::host::{DelegatedWork, HostTask, TrackerIntegration, TrackerReadiness};
use crate::tracker::providers::linear::automation::LinearAutomation;
use crate::tracker::providers::linear::oauth;
use crate::tracker::providers::linear::settings::LinearSettings;

use super::{LinearIntake, webhook_router};
use crate::tracker::host::automation::TrackerAutomation;
use crate::tracker::host::remote::{RemoteTracker, RemoteTrackerConfig};

pub struct LinearRuntime {
    automation: Arc<LinearAutomation>,
    tracker: Arc<RemoteTracker>,
    router: Mutex<Option<axum::Router>>,
    listener: Mutex<Option<TcpListener>>,
    webhook_address: String,
    workspace_name: String,
}

// Six bytes of the digest: enough to tell two secrets apart, far too little to be
// a secret itself.
fn fingerprint(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(secret.as_bytes())[..6])
}

pub fn credentials_path(config: &Config) -> PathBuf {
    config.data_dir().join("trackers/linear/oauth.json")
}

pub fn readiness(credentials_path: &Path) -> TrackerReadiness {
    if oauth::installed(credentials_path).is_some() {
        TrackerReadiness::Unavailable
    } else {
        TrackerReadiness::NotInstalled
    }
}

pub async fn connect(
    config: &Config,
    settings: &LinearSettings,
    credentials_path: &Path,
) -> Result<Arc<LinearRuntime>, String> {
    let mut credentials = oauth::AppCredentials::load(credentials_path)?;
    credentials
        .refresh_if_needed(settings, credentials_path)
        .await?;
    let workspace_name = credentials.workspace_name.clone();
    let automation = Arc::new(
        credentials
            .client()
            .await
            .map_err(|error| error.to_string())?,
    );
    let listener = TcpListener::bind(&settings.listen)
        .await
        .map_err(|error| format!("could not listen for Linear webhooks: {error}"))?;
    let webhook_address = listener
        .local_addr()
        .map(|address| address.to_string())
        .unwrap_or_default();
    let webhook_secret = settings.webhook_secret()?;
    // A fingerprint rather than the secret: when Linear rejects a delivery the
    // question an operator has is which value this machine is holding, and twelve
    // hex characters answer it against `printf '%s' '<the app secret>' | sha256sum`
    // without a secret reaching a log.
    tracing::info!(
        fingerprint = %fingerprint(&webhook_secret),
        "Linear webhook secret loaded"
    );
    let tracker = RemoteTracker::new(RemoteTrackerConfig {
        automation: Arc::clone(&automation) as Arc<dyn TrackerAutomation>,
        storage_path: config.data_dir().join("trackers/linear/deliveries.json"),
        attachments_dir: config.attachments_dir(),
        operators_dir: config.data_dir().join("operators"),
        host_label: whoami::hostname().unwrap_or_else(|_| "remote".to_owned()),
    })?;
    let router = webhook_router(LinearIntake {
        tracker: Arc::clone(&tracker),
        automation: Arc::clone(&automation),
        webhook_secret: webhook_secret.into_bytes(),
        oauth_client_id: settings.client_id.clone(),
        delivered: Default::default(),
    });
    let settings = settings.clone();
    let path = credentials_path.to_owned();
    let refreshed_automation = Arc::clone(&automation);
    tokio::spawn(async move {
        // The token says when it expires, so renewing it is a moment to wait for
        // rather than a question to ask every minute.
        loop {
            time::sleep(credentials.refresh_in()).await;
            match credentials.refresh_if_needed(&settings, &path).await {
                Ok(true) => {
                    if let Err(error) =
                        refreshed_automation.replace_token(credentials.access_token().to_owned())
                    {
                        tracing::warn!(%error, "Linear login needs attention");
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, "Linear login refresh remains queued");
                    // A refresh that failed did not move the expiry, so the next wait
                    // would be no wait at all.
                    time::sleep(time::Duration::from_secs(60)).await;
                }
            }
        }
    });
    Ok(Arc::new(LinearRuntime {
        automation,
        tracker,
        router: Mutex::new(Some(router)),
        listener: Mutex::new(Some(listener)),
        webhook_address,
        workspace_name,
    }))
}

impl TrackerIntegration for LinearRuntime {
    fn workspace_label(&self) -> &str {
        &self.workspace_name
    }

    fn webhook_address(&self) -> &str {
        &self.webhook_address
    }

    fn issue_reader(&self) -> Option<Arc<dyn IssueReader>> {
        Some(Arc::clone(&self.automation) as Arc<dyn IssueReader>)
    }

    fn wake(&self) {
        self.tracker.wake();
    }

    fn forget_task<'a>(&'a self, task_id: &'a str) -> HostTask<'a> {
        Box::pin(async move { self.tracker.forget_task(task_id).await })
    }

    fn announce_shutdown<'a>(&'a self, work: &'a dyn DelegatedWork) -> HostTask<'a> {
        Box::pin(self.tracker.announce_shutdown(work))
    }

    fn run(self: Arc<Self>, work: Arc<dyn DelegatedWork>) {
        let router = self.router.lock().ok().and_then(|mut router| router.take());
        let listener = self
            .listener
            .lock()
            .ok()
            .and_then(|mut listener| listener.take());
        if let (Some(listener), Some(router)) = (listener, router) {
            tokio::spawn(async move {
                if let Err(error) = axum::serve(listener, router).await {
                    tracing::error!(%error, "Linear webhook listener stopped");
                }
            });
        } else {
            tracing::warn!("Linear webhook listener was started twice");
        }
        tokio::spawn(Arc::clone(&self.tracker).run(work));
    }
}
