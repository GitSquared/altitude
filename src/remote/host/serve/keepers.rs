use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use tokio::time::{self, Instant};

use crate::config::Config;
use crate::credentials::keeper::Keeper;
use crate::isolation::IsolationEngine;
use crate::task::TaskService;

// How often the set of operators is looked at, so an operator whose configuration
// arrived while this machine was running gets a keeper without a restart. A keeper
// holds its own profile's retry clock, so a short discovery tick is not a short
// retry.
const DISCOVER: Duration = Duration::from_secs(60);

// A remote holds each operator's credentials, and a client that has gone away cannot
// renew them: the machine that is still up does. One keeper per operator, scoped to
// that operator's profiles, so a renewal writes where the operator's next task reads.
pub(super) async fn watch<E>(service: TaskService<E>, config: Config)
where
    E: IsolationEngine + Clone + 'static,
{
    let mut keepers: HashMap<String, Keeper<E>> = HashMap::new();
    loop {
        for owner in operators(&config) {
            if keepers.contains_key(&owner) {
                continue;
            }
            match service.credentials_for_operator(&owner) {
                Ok(credentials) => {
                    keepers.insert(owner, Keeper::new(credentials));
                }
                Err(error) => {
                    tracing::warn!(%owner, %error, "could not scope a credential keeper to an operator")
                }
            }
        }
        let mut wake = Instant::now() + DISCOVER;
        for keeper in keepers.values_mut() {
            wake = wake.min(keeper.poll().await);
        }
        time::sleep_until(wake.max(Instant::now())).await;
    }
}

// The operators whose configuration this remote holds: one directory per operator,
// named by the peer that pushed it.
fn operators(config: &Config) -> Vec<String> {
    let directory = config.scoped_data_dir().join("operators");
    let Ok(entries) = fs::read_dir(&directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}
