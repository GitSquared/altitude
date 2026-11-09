pub use connect::{connect, credentials_path, readiness};
pub use setup::connect_remote;

use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use axum::{Router, extract::DefaultBodyLimit, routing::post};

use crate::tracker::host::remote::RemoteTracker;
use crate::tracker::providers::linear::automation::LinearAutomation;

mod connect;
mod intake;
mod setup;
mod webhook;

const WEBHOOK_ROUTE: &str = "/altitude/linear/webhook";

const MAX_WEBHOOK_BYTES: usize = 1024 * 1024;

const SIGNATURE_HEADER: &str = "linear-signature";

const WEBHOOK_MAX_AGE: Duration = Duration::from_secs(300);

// What Linear's webhook needs to tell a delivery for this app from anything else that
// finds the address: the shared secret, the app's own OAuth client, and the workspace
// and app user the automation signed in as. What a delivery becomes is the loop's.
pub(super) struct LinearIntake {
    tracker: Arc<RemoteTracker>,
    automation: Arc<LinearAutomation>,
    webhook_secret: Vec<u8>,
    oauth_client_id: String,
    delivered: AtomicBool,
}

fn webhook_router(intake: LinearIntake) -> Router {
    Router::new()
        .route(WEBHOOK_ROUTE, post(intake::webhook))
        .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BYTES))
        .with_state(Arc::new(intake))
}
