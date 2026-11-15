use std::fs;

use iroh::EndpointAddr;

use crate::{
    agent::AgentEngines,
    config::Config,
    credentials::{keeper, profile_recipe},
};

use super::LinkClient;

// Syncing a configuration is one mechanism in both directions: what this machine has that
// the other needs goes out, and what the other kept that outlasts this machine's comes
// back. A client that has been away is holding a spent session, and sending it would undo
// what the remote refreshed while nobody was looking, so this happens before the push.
pub(crate) async fn reconcile_sessions(client: &LinkClient, peer: EndpointAddr, config: &Config) {
    let agents = AgentEngines::new();
    for profile in keeper::profiles(config) {
        let Ok(recipe) = profile_recipe(&profile, &agents) else {
            continue;
        };
        let (Some(file), Some(deadline)) = (recipe.session_file, recipe.session_deadline) else {
            continue;
        };
        let path = config.profile_dir(&profile).join(file);
        let here = fs::read_to_string(&path)
            .ok()
            .and_then(|contents| deadline(&contents));
        let Ok(Some(contents)) = client.read_session(peer.clone(), &profile).await else {
            continue;
        };
        let Some(theirs) = deadline(&contents) else {
            continue;
        };
        if here.is_some_and(|here| here >= theirs) {
            continue;
        }
        if let Some(parent) = path.parent()
            && fs::create_dir_all(parent).is_err()
        {
            continue;
        }
        let incoming = path.with_extension("incoming");
        if fs::write(&incoming, &contents).is_ok() && fs::rename(&incoming, &path).is_ok() {
            tracing::info!(%profile, "took the session this remote kept; it outlasts this one");
        }
    }
}
