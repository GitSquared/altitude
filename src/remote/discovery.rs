//! Combines network discovery sources into remote suggestions for the TUI.
//! Discovering a remote never adopts it or sends configuration and credentials.

mod local;
mod tailscale;

use iroh::{Endpoint, EndpointId};
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    time::Duration,
};
use tokio::{
    sync::mpsc,
    task::JoinSet,
    time::{Instant, timeout_at},
};

use crate::{config::DEFAULT_PORT, remote::LinkResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredRemote {
    pub id: EndpointId,
    pub label: String,
    pub version: String,
    pub address: String,
}

#[derive(Clone, Debug)]
pub enum DiscoveryEvent {
    Found(DiscoveredRemote),
    Removed(EndpointId),
}

/// A VPN address wins when both sources see a remote: it remains reachable
/// when the client moves away from the remote's LAN.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Tailscale,
    Local,
}

struct SourceEvent {
    source: Source,
    event: DiscoveryEvent,
}

pub struct Scan {
    events: mpsc::Receiver<SourceEvent>,
    sightings: BTreeMap<Source, HashMap<EndpointId, DiscoveredRemote>>,
    tasks: JoinSet<()>,
    deadline: Instant,
}

pub(crate) async fn scan(endpoint: &Endpoint) -> LinkResult<Scan> {
    let (send, events) = mpsc::channel(64);
    let mut tasks = JoinSet::new();
    match local::browse(endpoint).await {
        Ok(mut browser) => {
            let send = send.clone();
            tasks.spawn(async move {
                while let Some(event) = browser.recv().await {
                    let event = SourceEvent {
                        source: Source::Local,
                        event,
                    };
                    if send.send(event).await.is_err() {
                        break;
                    }
                }
            });
        }
        Err(error) => tracing::warn!(%error, "Local network discovery unavailable"),
    }
    if let Some(executable) = tailscale::executable() {
        tasks.spawn(tailscale::scan(executable, endpoint.id(), send.clone()));
    }
    if tasks.is_empty() {
        return Err("No network discovery source is available; add remotes by hostname".into());
    }
    Ok(Scan {
        events,
        sightings: BTreeMap::new(),
        tasks,
        deadline: Instant::now() + Duration::from_secs(5),
    })
}

pub(crate) fn advertise(endpoint: &Endpoint, label: &str) -> LinkResult<()> {
    local::advertise(endpoint, label)
}

impl Scan {
    pub async fn recv(&mut self) -> Option<DiscoveryEvent> {
        loop {
            let SourceEvent { source, event } =
                match timeout_at(self.deadline, self.events.recv()).await {
                    Ok(Some(event)) => event,
                    _ => {
                        self.tasks.abort_all();
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
                    self.sightings.entry(source).or_default().insert(id, remote);
                }
                DiscoveryEvent::Removed(id) => {
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

/// Drops control characters and caps the length, so a remote cannot put
/// terminal escape sequences or an unbounded string in front of the operator.
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
