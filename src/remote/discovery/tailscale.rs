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
        .map(|address| discover_remote(address, own_id))
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

async fn peer_addresses(executable: &Path) -> Option<Vec<SocketAddr>> {
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
            .filter_map(|peer| preferred_address(&peer.addresses))
            .map(|ip| SocketAddr::new(ip, DEFAULT_PORT))
            .collect(),
    )
}

fn preferred_address(addresses: &[IpAddr]) -> Option<IpAddr> {
    addresses
        .iter()
        .copied()
        .find(IpAddr::is_ipv4)
        .or_else(|| addresses.first().copied())
}

async fn discover_remote(address: SocketAddr, own_id: EndpointId) -> Option<DiscoveredRemote> {
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
        address: format_address(address),
    })
}
