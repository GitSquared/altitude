//! Finds Altitude remotes on the machine's own tailnet.
//!
//! Uses the authenticated local Tailscale client to obtain probe candidates.

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
    remote::{announce, protocol::LINK_ALPN},
};

const STATUS_LIMIT: Duration = Duration::from_secs(5);
const PROBE_LIMIT: Duration = Duration::from_secs(3);
const MAX_CONCURRENT_PROBES: usize = 4;

/// The app bundle ships a CLI the PATH often does not include, so on macOS it
/// is the last place to look rather than a hard-coded first choice.
#[cfg(target_os = "macos")]
const MACOS_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";

#[derive(Deserialize)]
struct Peer {
    #[serde(rename = "Online", default)]
    is_online: bool,
    #[serde(rename = "TailscaleIPs", default)]
    addresses: Vec<IpAddr>,
    /// The name this peer is known by on the tailnet, e.g. `studio.tailnet.ts.net.`
    ///
    /// Preferred over its address: a MagicDNS name keeps working when the address
    /// does not, and the client's own resolver is what turns it back into one.
    #[serde(rename = "DNSName", default)]
    name: Option<String>,
}

/// One peer worth probing, and what to write down if it turns out to be a remote.
struct Candidate {
    address: SocketAddr,
    /// What to reach it by once it is known to be one: the name where the tailnet
    /// gave one, and its address otherwise.
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
    let Some(addresses) = peer_addresses(&executable).await else {
        return;
    };
    let remotes = stream::iter(addresses)
        .map(|candidate| discover_remote(candidate, own_id))
        .buffer_unordered(MAX_CONCURRENT_PROBES)
        .filter_map(future::ready);
    tokio::pin!(remotes);
    while let Some(remote) = remotes.next().await {
        let event = SourceEvent {
            source: Source::Tailscale,
            event: DiscoveryEvent::Found(remote),
        };
        if events.send(event).await.is_err() {
            return;
        }
    }
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

/// The name a tailnet peer is known by, without the trailing dot a DNS name
/// carries and without anything that could not be typed into a settings file.
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
    let announcement = time::timeout(PROBE_LIMIT, announce::ask(&host, address.port()))
        .await
        .ok()?
        .ok()?;
    if announcement.protocol != String::from_utf8_lossy(LINK_ALPN) {
        return None;
    }
    let id: EndpointId = announcement.id.parse().ok()?;
    if id == own_id {
        return None;
    }
    Some(DiscoveredRemote {
        id,
        label: clean(&announcement.label, 64),
        version: clean(&announcement.version, 32),
        address: candidate.wrote,
    })
}
