//! Iroh announcements and browsing on the local network.

use super::{DiscoveredRemote, DiscoveryEvent, clean, format_address};
use crate::remote::{LinkResult, compat::Compatibility};
use futures_util::{Stream, StreamExt};
use iroh::{
    Endpoint,
    address_lookup::{EndpointInfo, UserData},
};
use iroh_mdns_address_lookup::{DiscoveryEvent as MdnsEvent, MdnsAddressLookup};
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, SocketAddr},
    pin::Pin,
};

const SERVICE_NAME: &str = "altitude";

pub(super) struct Browser {
    events: Pin<Box<dyn Stream<Item = MdnsEvent> + Send>>,
    _lookup: MdnsAddressLookup,
}

#[derive(Deserialize, Serialize)]
struct Metadata {
    label: String,
    version: String,
}

/// Keep browsing separate from the endpoint's permanent lookup services so
/// dropping a scan also stops its multicast activity.
pub(super) async fn browse(endpoint: &Endpoint) -> LinkResult<Browser> {
    let lookup = MdnsAddressLookup::builder()
        .advertise(false)
        .service_name(SERVICE_NAME)
        .build(endpoint.id())?;
    let events = lookup.subscribe().await;
    Ok(Browser {
        events: Box::pin(events),
        _lookup: lookup,
    })
}

pub(super) fn advertise(endpoint: &Endpoint, label: &str) -> LinkResult<()> {
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
        address: format_address(preferred_address(info)?),
        compatibility: Compatibility::of(&metadata.version),
    })
}

/// Prefer LAN IPv4 addresses. Loopback and unspecified addresses cannot lead
/// another machine to this remote.
fn preferred_address(info: &EndpointInfo) -> Option<SocketAddr> {
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
