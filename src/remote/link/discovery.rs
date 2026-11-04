mod local;
mod tailscale;

use iroh::{Endpoint, EndpointId};
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinSet, time::timeout};

use crate::{
    config::DEFAULT_PORT,
    remote::{LinkResult, compat::Compatibility},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredRemote {
    pub id: EndpointId,
    pub label: String,
    pub version: String,
    pub address: String,
    pub compatibility: Compatibility,
}

#[derive(Clone, Debug)]
pub enum DiscoveryEvent {
    Found(DiscoveredRemote),
    Removed(EndpointId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Tailscale,
    Local,
}

struct SourceEvent {
    source: Source,
    event: DiscoveryEvent,
}

// How long the local network is listened to. Browsing is a subscription that never
// ends by itself, so it is the one source that needs a window; a tailnet scan
// bounds itself by the peers it has to probe.
const LOCAL_WINDOW: Duration = Duration::from_secs(3);

pub struct Scan {
    events: mpsc::Receiver<SourceEvent>,
    sightings: BTreeMap<Source, HashMap<EndpointId, DiscoveredRemote>>,
    tasks: JoinSet<()>,
}

pub(crate) async fn scan(endpoint: &Endpoint) -> LinkResult<Scan> {
    let (send, events) = mpsc::channel(64);
    let mut tasks = JoinSet::new();
    match local::browse(endpoint).await {
        Ok(mut browser) => {
            let send = send.clone();
            tasks.spawn(async move {
                let browsing = async move {
                    while let Some(event) = browser.recv().await {
                        let event = SourceEvent {
                            source: Source::Local,
                            event,
                        };
                        if send.send(event).await.is_err() {
                            break;
                        }
                    }
                };
                let _ = timeout(LOCAL_WINDOW, browsing).await;
            });
        }
        Err(error) => tracing::warn!(%error, "Local network discovery unavailable"),
    }
    match tailscale::executable() {
        Some(executable) => {
            tasks.spawn(tailscale::scan(executable, endpoint.id(), send.clone()));
        }
        None => tracing::debug!("the tailscale CLI was not found; the tailnet was not scanned"),
    }
    if tasks.is_empty() {
        return Err("No network discovery source is available; add remotes by hostname".into());
    }
    tracing::debug!("a scan is looking for remotes");
    Ok(Scan {
        events,
        sightings: BTreeMap::new(),
        tasks,
    })
}

pub(crate) fn advertise(endpoint: &Endpoint, label: &str) -> LinkResult<()> {
    local::advertise(endpoint, label)
}

impl Scan {
    pub async fn recv(&mut self) -> Option<DiscoveryEvent> {
        loop {
            // A scan ends when its sources do: each one bounds itself, so nothing
            // can be cut off mid-probe the way a window over the whole scan did.
            let SourceEvent { source, event } = match self.events.recv().await {
                Some(event) => event,
                None => {
                    self.tasks.abort_all();
                    tracing::debug!("the scan ended");
                    return None;
                }
            };
            let id = match &event {
                DiscoveryEvent::Found(remote) => remote.id,
                DiscoveryEvent::Removed(id) => *id,
            };
            let previous = self.preferred_remote(id).cloned();
            match event {
                DiscoveryEvent::Found(remote) => {
                    tracing::debug!(
                        ?source,
                        id = %remote.id,
                        address = %remote.address,
                        label = %remote.label,
                        "a remote answered"
                    );
                    self.sightings.entry(source).or_default().insert(id, remote);
                }
                DiscoveryEvent::Removed(id) => {
                    tracing::debug!(?source, id = %id, "a remote went away");
                    if let Some(remotes) = self.sightings.get_mut(&source) {
                        remotes.remove(&id);
                    }
                }
            }
            match (previous, self.preferred_remote(id)) {
                (None, Some(remote)) => return Some(DiscoveryEvent::Found(remote.clone())),
                (Some(previous), Some(remote)) if previous != *remote => {
                    return Some(DiscoveryEvent::Found(remote.clone()));
                }
                (Some(_), None) => return Some(DiscoveryEvent::Removed(id)),
                _ => {}
            }
        }
    }

    fn preferred_remote(&self, id: EndpointId) -> Option<&DiscoveredRemote> {
        self.sightings.values().find_map(|remotes| remotes.get(&id))
    }
}

fn format_address(address: SocketAddr) -> String {
    // Bracketed IPv6 needs an explicit port in the settings parser.
    if address.is_ipv4() && address.port() == DEFAULT_PORT {
        address.ip().to_string()
    } else {
        address.to_string()
    }
}

fn clean(value: &str, limit: usize) -> String {
    let mut bytes = 0;
    value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take_while(|character| {
            bytes += character.len_utf8();
            bytes <= limit
        })
        .collect()
}
