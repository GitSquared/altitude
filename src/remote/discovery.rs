//! Iroh-native LAN discovery for the server and TUI. Discovered metadata is only
//! a hint; the operator must adopt a remote before configuration is sent.

use std::{
    net::{IpAddr, SocketAddr},
    pin::Pin,
};

use futures_util::{Stream, StreamExt};
use iroh::{
    Endpoint, EndpointId,
    address_lookup::{EndpointInfo, UserData},
};
use iroh_mdns_address_lookup::{DiscoveryEvent as MdnsEvent, MdnsAddressLookup};
use serde::{Deserialize, Serialize};

use super::LinkResult;
use crate::config::DEFAULT_PORT;

/// The namespace Altitude publishes and browses, so a shared network does not
/// mix Altitude remotes with unrelated iroh peers.
const SERVICE_NAME: &str = "altitude";

/// A remote seen announcing on the network, before the operator adopts it.
#[derive(Clone, Debug)]
pub struct DiscoveredRemote {
    pub id: EndpointId,
    pub label: String,
    pub version: String,
    /// Where to reach it. An IPv4 default port is left off; everything else
    /// keeps its `port` because the settings parser expects it.
    pub address: String,
}

#[derive(Clone, Debug)]
pub enum DiscoveryEvent {
    Found(DiscoveredRemote),
    Removed(EndpointId),
}

/// A subscription to the endpoint's local discovery stream.
pub struct Browser {
    events: Pin<Box<dyn Stream<Item = MdnsEvent> + Send>>,
}

/// What a server publishes about itself in its user data.
#[derive(Deserialize, Serialize)]
struct Metadata {
    label: String,
    version: String,
}

/// Starts browsing for remotes on this endpoint.
///
/// The lookup advertises nothing, so a browsing client is not itself offered as
/// a remote. It is subscribed before it is attached, so the first events are not
/// missed while the endpoint takes ownership of it.
pub(crate) async fn browse(endpoint: &Endpoint) -> LinkResult<Browser> {
    let lookup = MdnsAddressLookup::builder()
        .advertise(false)
        .service_name(SERVICE_NAME)
        .build(endpoint.id())?;
    let events = lookup.subscribe().await;
    endpoint.address_lookup()?.add(lookup);
    Ok(Browser {
        events: Box::pin(events),
    })
}

/// Says where this server is, so an operator does not have to type its address.
///
/// Discovery is optional; manual addresses remain usable if it fails.
pub(crate) fn advertise(endpoint: &Endpoint, label: &str) -> LinkResult<()> {
    let metadata = Metadata {
        label: clean(label, 64),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let user_data: UserData = serde_json::to_string(&metadata)?.parse()?;
    endpoint.set_user_data_for_address_lookup(Some(user_data));
    let lookup = MdnsAddressLookup::builder()
        .service_name(SERVICE_NAME)
        .build(endpoint.id())?;
    endpoint.address_lookup()?.add(lookup);
    Ok(())
}

impl Browser {
    /// Waits for the next thing worth telling the operator.
    ///
    /// `None` means browsing has stopped, which happens when the endpoint closes
    /// rather than when the network is merely quiet.
    pub async fn recv(&mut self) -> Option<DiscoveryEvent> {
        loop {
            match self.events.next().await? {
                MdnsEvent::Discovered { endpoint_info, .. } => {
                    if let Some(remote) = describe(&endpoint_info) {
                        return Some(DiscoveryEvent::Found(remote));
                    }
                }
                MdnsEvent::Expired { endpoint_id } => {
                    return Some(DiscoveryEvent::Removed(endpoint_id));
                }
                _ => {}
            }
        }
    }
}

fn describe(info: &EndpointInfo) -> Option<DiscoveredRemote> {
    let metadata: Metadata = serde_json::from_str(info.user_data()?.as_ref()).ok()?;
    Some(DiscoveredRemote {
        id: info.endpoint_id,
        label: clean(&metadata.label, 64),
        version: clean(&metadata.version, 32),
        address: format_address(usable_address(info)?),
    })
}

/// The first address on this network a client can actually dial.
///
/// IPv4 first because a client that speaks only IPv4 is still common, and a
/// private IPv4 before a public one because a remote on the same LAN is the
/// point. A loopback or unspecified address is never offered: a remote bound
/// only to itself is not a remote anyone else can reach.
fn usable_address(info: &EndpointInfo) -> Option<SocketAddr> {
    let mut addresses: Vec<SocketAddr> = info
        .ip_addrs()
        .copied()
        .filter(|address| !address.ip().is_loopback() && !address.ip().is_unspecified())
        .collect();
    addresses.sort();
    addresses
        .iter()
        .find(|address| matches!(address.ip(), IpAddr::V4(ip) if ip.is_private()))
        .or_else(|| addresses.iter().find(|address| address.ip().is_ipv4()))
        .or_else(|| {
            addresses.iter().find(
                |address| matches!(address.ip(), IpAddr::V6(ip) if !ip.is_unicast_link_local()),
            )
        })
        .copied()
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
