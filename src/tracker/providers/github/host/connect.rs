use std::sync::{Arc, Mutex};

use axum::Router;
use tokio::net::TcpListener;

use crate::config::Config;
use crate::tracker::IssueReader;
use crate::tracker::host::automation::TrackerAutomation;
use crate::tracker::host::remote::{RemoteTracker, RemoteTrackerConfig};
use crate::tracker::host::{DelegatedWork, HostTask, TrackerIntegration};
use crate::tracker::providers::github::{automation::GitHubAutomation, settings::GitHubSettings};

use super::{GitHubIntake, webhook_router};

pub struct GitHubRuntime {
    automation: Arc<GitHubAutomation>,
    tracker: Arc<RemoteTracker>,
    router: Mutex<Option<Router>>,
    listener: Mutex<Option<TcpListener>>,
    webhook_address: String,
}

// Six bytes of the digest: enough to tell two secrets apart, far too little to be
// a secret itself.
fn fingerprint(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(secret.as_bytes())[..6])
}

// A GitHub App needs nothing refreshed: every token it uses is derived from its key
// when it is needed, so connecting is asking GitHub which app the key belongs to.
pub async fn connect(
    config: &Config,
    settings: &GitHubSettings,
) -> Result<Arc<GitHubRuntime>, String> {
    let automation = Arc::new(
        GitHubAutomation::connect(settings.app_id.clone(), &settings.private_key()?)
            .await
            .map_err(|error| error.to_string())?,
    );
    let listener = TcpListener::bind(&settings.listen)
        .await
        .map_err(|error| format!("could not listen for GitHub webhooks: {error}"))?;
    let webhook_address = listener
        .local_addr()
        .map(|address| address.to_string())
        .unwrap_or_default();
    let webhook_secret = settings.webhook_secret()?;
    tracing::info!(
        fingerprint = %fingerprint(&webhook_secret),
        "GitHub webhook secret loaded"
    );
    let tracker = RemoteTracker::new(RemoteTrackerConfig {
        automation: Arc::clone(&automation) as Arc<dyn TrackerAutomation>,
        storage_path: config.data_dir().join("trackers/github/deliveries.json"),
        attachments_dir: config.attachments_dir(),
        operators_dir: config.data_dir().join("operators"),
        host_label: whoami::hostname().unwrap_or_else(|_| "remote".to_owned()),
    })?;
    let router = webhook_router(GitHubIntake {
        tracker: Arc::clone(&tracker),
        webhook_secret: webhook_secret.into_bytes(),
        app_user_id: automation.app_user_id().to_owned(),
    });
    Ok(Arc::new(GitHubRuntime {
        automation,
        tracker,
        router: Mutex::new(Some(router)),
        listener: Mutex::new(Some(listener)),
        webhook_address,
    }))
}

impl TrackerIntegration for GitHubRuntime {
    fn workspace_label(&self) -> &str {
        self.automation.app_slug()
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
                    tracing::error!(%error, "GitHub webhook listener stopped");
                }
            });
        } else {
            tracing::warn!("GitHub webhook listener was started twice");
        }
        tokio::spawn(Arc::clone(&self.tracker).run(work));
    }
}
