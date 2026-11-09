pub use connect::connect;

use std::sync::Arc;

use axum::{Router, extract::DefaultBodyLimit, routing::post};

use crate::tracker::host::remote::RemoteTracker;

mod connect;
mod intake;

const WEBHOOK_ROUTE: &str = "/altitude/github/webhook";

const MAX_WEBHOOK_BYTES: usize = 1024 * 1024;

// What GitHub's webhook needs to tell a delivery for this app from anything else
// that finds the address: the secret the app signs with, and the user the app
// writes as, so its own comments coming back are not read as anyone speaking.
pub(super) struct GitHubIntake {
    tracker: Arc<RemoteTracker>,
    webhook_secret: Vec<u8>,
    app_user_id: String,
}

fn webhook_router(intake: GitHubIntake) -> Router {
    Router::new()
        .route(WEBHOOK_ROUTE, post(intake::webhook))
        .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BYTES))
        .with_state(Arc::new(intake))
}
