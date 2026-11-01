use iroh::EndpointId;

use super::{LinkResult, announce::Announcement, compat::Compatibility, protocol::RemoteTicket};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteKeyStatus {
    New,
    Known,
    Changed,
}

impl RemoteKeyStatus {
    pub fn classify(recorded: Option<&str>, answering: &EndpointId) -> Self {
        match recorded {
            None => Self::New,
            Some(recorded) if recorded == answering.to_string() => Self::Known,
            Some(_) => Self::Changed,
        }
    }
}

pub struct Sighting {
    pub ticket: RemoteTicket,
    pub announcement: Announcement,
    pub key_status: RemoteKeyStatus,
    pub compatibility: Compatibility,
}

pub async fn meet(
    address: &str,
    default_port: u16,
    remembered: Option<&str>,
) -> LinkResult<Sighting> {
    let (ticket, announcement) = RemoteTicket::from_broadcast(address, default_port).await?;
    let id: EndpointId = announcement
        .id
        .parse()
        .map_err(|error| format!("{address} announced an unreadable key: {error}"))?;
    let compatibility = Compatibility::with_local_version(&announcement.version);
    Ok(Sighting {
        ticket,
        announcement,
        key_status: RemoteKeyStatus::classify(remembered, &id),
        compatibility,
    })
}
