//! LAN discovery for the remote server and TUI. Advertisements are hints only;
//! the operator must add a remote before the client connects or sends credentials.

use std::net::{IpAddr, SocketAddr};

use iroh::EndpointId;
use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};

use super::LinkResult;
use crate::config::DEFAULT_PORT;

const SERVICE_TYPE: &str = "_altitude._tcp.local.";

#[derive(Clone, Debug)]
pub struct DiscoveredRemote {
    /// The full mDNS service name, which is also what a later removal names.
    pub service: String,
    pub id: String,
    pub label: String,
    pub version: String,
    /// Where to reach it. An IPv4 default port is left off; everything else
    /// keeps its `port` because the settings parser expects it.
    pub address: String,
}

#[derive(Clone, Debug)]
pub enum DiscoveryEvent {
    Found(DiscoveredRemote),
    /// The full name of a service that went away.
    Removed(String),
}

/// A live browse. Dropping it stops listening.
pub struct Browser {
    daemon: ServiceDaemon,
    events: mdns_sd::Receiver<ServiceEvent>,
}

pub fn browse() -> LinkResult<Browser> {
    let daemon = ServiceDaemon::new()?;
    match daemon.browse(SERVICE_TYPE) {
        Ok(events) => Ok(Browser { daemon, events }),
        Err(error) => {
            let _ = daemon.shutdown();
            Err(error.into())
        }
    }
}

impl Browser {
    /// Waits for the next thing worth telling the operator, skipping the
    /// bookkeeping events a browse also emits.
    ///
    /// `None` means browsing has stopped, which happens once the daemon is shut
    /// down rather than when the network is merely quiet.
    pub async fn recv(&self) -> Option<DiscoveryEvent> {
        loop {
            let event = self.events.recv_async().await.ok()?;
            if let Some(event) = describe(event) {
                return Some(event);
            }
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Err(error) = self.daemon.shutdown() {
            tracing::debug!(%error, "the discovery browser had already stopped");
        }
    }
}

/// A published service that stays visible until it is dropped.
pub(crate) struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

/// Says where this remote is, so an operator does not have to type its address.
///
/// Discovery is optional; manual addresses remain usable if it fails.
pub(crate) fn advertise(
    label: &str,
    id: &str,
    version: &str,
    bound: &[SocketAddr],
) -> LinkResult<Advertisement> {
    if bound.is_empty() {
        return Err("the link has no bound address to advertise".into());
    }
    // Every address is published on the one port the link is actually on. When
    // the operator asked for *any* port the two families can disagree, so only
    // addresses sharing the first port are advertised rather than naming a port
    // that belongs to the other socket.
    let reachable: Vec<(u16, IpAddr)> = bound
        .iter()
        .filter(|address| !address.ip().is_loopback())
        .map(|address| (address.port(), address.ip()))
        .collect();
    let Some((port, _)) = reachable.first().copied() else {
        // A service bound only to the loopback interface would advertise an
        // address no other machine can dial, which is worse than none.
        return Err("not advertising a loopback-only remote".into());
    };
    let addresses: Vec<IpAddr> = reachable
        .iter()
        .filter(|(candidate, _)| *candidate == port)
        .map(|(_, address)| *address)
        .collect();

    let label = clean(label, 64);
    let version = clean(version, 32);
    let instance = instance_name(&label, id);
    let host = hostname();
    let properties = [
        ("id", id),
        ("label", label.as_str()),
        ("version", version.as_str()),
    ];
    let info = if addresses.iter().any(|address| address.is_unspecified()) {
        // The operator asked for every interface, so let the library follow the
        // addresses as they come and go rather than freezing today's list.
        ServiceInfo::new(SERVICE_TYPE, &instance, &host, (), port, &properties[..])?
            .enable_addr_auto()
    } else {
        ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &host,
            addresses.as_slice(),
            port,
            &properties[..],
        )?
    };
    let fullname = info.get_fullname().to_owned();
    let daemon = ServiceDaemon::new()?;
    if let Err(error) = daemon.register(info) {
        // The daemon owns a thread that outlives this call, so it is stopped
        // rather than left running for a registration that never landed.
        let _ = daemon.shutdown();
        return Err(error.into());
    }
    Ok(Advertisement { daemon, fullname })
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        if let Err(error) = self.daemon.shutdown() {
            tracing::debug!(%error, "the discovery advertisement had already stopped");
        }
    }
}

fn describe(event: ServiceEvent) -> Option<DiscoveryEvent> {
    match event {
        ServiceEvent::ServiceResolved(resolved) => {
            Some(DiscoveryEvent::Found(describe_remote(*resolved)?))
        }
        ServiceEvent::ServiceRemoved(_, fullname) => Some(DiscoveryEvent::Removed(fullname)),
        _ => None,
    }
}

fn describe_remote(resolved: ResolvedService) -> Option<DiscoveredRemote> {
    if !resolved.is_valid() {
        return None;
    }
    let id = resolved.get_property_val_str("id")?;
    // A remote whose key does not parse is a remote a client cannot dial, so it
    // is not offered as a choice at all.
    id.parse::<EndpointId>().ok()?;
    Some(DiscoveredRemote {
        service: resolved.fullname.clone(),
        id: id.to_owned(),
        label: clean(
            resolved.get_property_val_str("label").unwrap_or_default(),
            64,
        ),
        version: clean(
            resolved.get_property_val_str("version").unwrap_or_default(),
            32,
        ),
        address: format_address(preferred_address(&resolved)?, resolved.port),
    })
}

/// The first address on this network a client can actually dial.
///
/// IPv4 first because a client that speaks only IPv4 is still common. A
/// link-local IPv6 address is skipped because dialing it needs a scope the
/// client was never told.
fn preferred_address(resolved: &ResolvedService) -> Option<IpAddr> {
    let addresses: Vec<IpAddr> = resolved
        .get_addresses()
        .iter()
        .map(ScopedIp::to_ip_addr)
        .filter(|address| !address.is_loopback() && !address.is_unspecified())
        .collect();
    addresses.iter().copied().find(IpAddr::is_ipv4).or_else(|| {
        addresses
            .iter()
            .copied()
            .find(|address| matches!(address, IpAddr::V6(ip) if !ip.is_unicast_link_local()))
    })
}

fn format_address(address: IpAddr, port: u16) -> String {
    match address {
        // A bracketed IPv6 always carries its port: the settings validator only
        // reads bracketed addresses as `[host]:port` and cannot take a bare one.
        IpAddr::V6(ip) => format!("[{ip}]:{port}"),
        IpAddr::V4(ip) if port == DEFAULT_PORT => ip.to_string(),
        IpAddr::V4(ip) => format!("{ip}:{port}"),
    }
}

/// A DNS hostname for the address records.
///
/// The actual bound IP addresses are published alongside this name, so a client
/// never has to resolve it through the operating system's mDNS resolver, which
/// many Linux setups do not run.
fn hostname() -> String {
    let name = dns_label(&whoami::hostname().unwrap_or_default());
    if name.is_empty() {
        "altitude.local.".to_owned()
    } else {
        format!("{name}.local.")
    }
}

fn instance_name(label: &str, id: &str) -> String {
    let name = dns_label(label);
    if !name.is_empty() {
        return name;
    }
    let short = &id[..id.len().min(8)];
    if short.is_empty() {
        "altitude".to_owned()
    } else {
        format!("altitude-{short}")
    }
}

fn dns_label(name: &str) -> String {
    let mut label: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    label.truncate(63);
    label.trim_matches('-').to_owned()
}

/// Drops control characters and caps the length, so a remote cannot put
/// terminal escape sequences or an unbounded string in front of the operator.
fn clean(value: &str, limit: usize) -> String {
    value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(limit)
        .collect()
}
