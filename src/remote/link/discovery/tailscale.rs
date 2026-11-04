use std::{
    collections::HashMap,
    env,
    net::{IpAddr, SocketAddr},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use futures_util::{StreamExt, future, stream};
use iroh::EndpointId;
use serde::Deserialize;
use tokio::{process::Command, sync::mpsc, time};

use super::{DiscoveredRemote, DiscoveryEvent, Source, SourceEvent, clean, format_address};
use crate::{
    config::DEFAULT_PORT,
    remote::{announce, compat::Compatibility, protocol::LINK_ALPN},
};

const STATUS_LIMIT: Duration = Duration::from_secs(5);
const PROBE_LIMIT: Duration = Duration::from_secs(3);
// A probe is one small request, so this is set to finish an ordinary tailnet in a
// single round: the scan has no window of its own any more, and a peer that waits
// its turn behind a queue of unresponsive ones is a peer that may not be reported.
const MAX_CONCURRENT_PROBES: usize = 16;

#[cfg(target_os = "macos")]
const MACOS_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";

#[derive(Deserialize)]
struct Peer {
    #[serde(rename = "Online", default)]
    is_online: bool,
    #[serde(rename = "TailscaleIPs", default)]
    addresses: Vec<IpAddr>,
    #[serde(rename = "DNSName", default)]
    name: Option<String>,
}

struct Candidate {
    address: SocketAddr,
    wrote: String,
}

#[derive(Deserialize)]
struct Status {
    #[serde(rename = "BackendState")]
    backend_state: String,
    #[serde(rename = "Peer", default)]
    peers: Option<HashMap<String, Peer>>,
}

pub(super) fn executable() -> Option<PathBuf> {
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            let candidate = directory.join("tailscale");
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let bundled = Path::new(MACOS_CLI);
        if is_executable(bundled) {
            return Some(bundled.to_owned());
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

pub(super) async fn scan(
    executable: PathBuf,
    own_id: EndpointId,
    events: mpsc::Sender<SourceEvent>,
) {
    let Some(candidates) = peer_addresses(&executable).await else {
        return;
    };
    let online = candidates.len();
    let mut answered = 0;
    tracing::debug!(online, "the tailnet scan is probing peers");
    let remotes = stream::iter(candidates)
        .map(|candidate| discover_remote(candidate, own_id))
        .buffer_unordered(MAX_CONCURRENT_PROBES)
        .filter_map(future::ready);
    tokio::pin!(remotes);
    while let Some(remote) = remotes.next().await {
        answered += 1;
        let event = SourceEvent {
            source: Source::Tailscale,
            event: DiscoveryEvent::Found(remote),
        };
        if events.send(event).await.is_err() {
            return;
        }
    }
    tracing::debug!(online, answered, "the tailnet scan ended");
}

async fn peer_addresses(executable: &Path) -> Option<Vec<Candidate>> {
    let mut command = Command::new(executable);
    command
        .arg("status")
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = match time::timeout(STATUS_LIMIT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            tracing::debug!(%error, "could not run the tailscale CLI");
            return None;
        }
        Err(_) => {
            tracing::debug!("the tailscale CLI did not answer in time");
            return None;
        }
    };
    if !output.status.success() {
        tracing::debug!(status = %output.status, "the tailscale CLI reported a failure");
        return None;
    }
    let status: Status = match serde_json::from_slice(&output.stdout) {
        Ok(status) => status,
        Err(error) => {
            tracing::debug!(%error, "the tailscale status was not readable");
            return None;
        }
    };
    if status.backend_state != "Running" {
        tracing::debug!(state = %status.backend_state, "tailscale is not connected");
        return None;
    }
    Some(
        status
            .peers
            .unwrap_or_default()
            .into_values()
            .filter(|peer| peer.is_online)
            .filter_map(|peer| {
                let address = SocketAddr::new(preferred_address(&peer.addresses)?, DEFAULT_PORT);
                Some(match tailnet_name(peer.name.as_deref()) {
                    Some(name) => Candidate {
                        address,
                        wrote: if address.port() == DEFAULT_PORT {
                            name
                        } else {
                            format!("{name}:{}", address.port())
                        },
                    },
                    None => Candidate {
                        address,
                        wrote: format_address(address),
                    },
                })
            })
            .collect(),
    )
}

fn tailnet_name(name: Option<&str>) -> Option<String> {
    let name = name?.trim().trim_end_matches('.');
    let name = clean(name, 253);
    (!name.is_empty()).then_some(name)
}

fn preferred_address(addresses: &[IpAddr]) -> Option<IpAddr> {
    addresses
        .iter()
        .copied()
        .find(IpAddr::is_ipv4)
        .or_else(|| addresses.first().copied())
}

async fn discover_remote(candidate: Candidate, own_id: EndpointId) -> Option<DiscoveredRemote> {
    // Probed at its address, which is what is certainly reachable from here; the
    // name is what gets written down, and the resolver takes it from there.
    let address = candidate.address;
    let host = address.ip().to_string();
    // The announce exchange has its own limit; this one caps the whole probe,
    // including the connect, so a stalled address cannot hold a slot.
    let announcement = match time::timeout(PROBE_LIMIT, announce::ask(&host, address.port())).await
    {
        Ok(Ok(announcement)) => announcement,
        Ok(Err(error)) => {
            tracing::debug!(%host, %error, "a tailnet peer did not answer");
            return None;
        }
        Err(_) => {
            tracing::debug!(%host, "a tailnet peer did not answer in time");
            return None;
        }
    };
    if announcement.protocol != String::from_utf8_lossy(LINK_ALPN) {
        tracing::debug!(
            %host,
            protocol = %announcement.protocol,
            "a tailnet peer answers another protocol"
        );
        return None;
    }
    let id: EndpointId = match announcement.id.parse() {
        Ok(id) => id,
        Err(_) => {
            tracing::debug!(%host, id = %announcement.id, "a tailnet peer has no id");
            return None;
        }
    };
    if id == own_id {
        return None;
    }
    tracing::debug!(%host, label = %announcement.label, "a tailnet peer is a remote");
    Some(DiscoveredRemote {
        id,
        label: clean(&announcement.label, 64),
        version: clean(&announcement.version, 32),
        address: candidate.wrote,
        compatibility: Compatibility::with_local_version(&announcement.version),
    })
}
