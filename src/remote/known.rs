//! Whether a remote's key is the one you remember for its address.
//!
//! A key learned from an announcement is unauthenticated — that is what makes a
//! first sighting possible at all — so the protection is not that the first
//! answer was trustworthy but that a *different* answer later is noticed. This
//! says whether it is different. Remembering is the caller's business, because
//! the first sighting is the part that cannot be authenticated and so the
//! operator is the one who decides.
//!
//! Nothing here stores anything, which is why a machine with no database can use
//! the link. A client keeps what it remembers in the database, and a server keeps
//! its own record of who connected in a file; this module keeps neither.

use iroh::EndpointId;

use super::{LinkResult, announce::Announcement, protocol::RemoteTicket};

/// How the key a remote answered with stands next to the one recorded for its
/// address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteKeyStatus {
    /// Nothing is recorded for this address: it has not answered before.
    New,
    /// The same key as the one recorded.
    Known,
    /// A different key.
    ///
    /// A remote that was replaced and a remote that was never the one you thought
    /// look identical from here, so this is reported and never followed
    /// silently. The operator decides. The key it changed from is the record the
    /// caller compared against, and is not repeated here.
    Changed,
}

impl RemoteKeyStatus {
    /// Classifies an answering key against whatever is recorded for its address.
    pub fn classify(recorded: Option<&str>, answering: &EndpointId) -> Self {
        match recorded {
            None => Self::New,
            Some(recorded) if recorded == answering.to_string() => Self::Known,
            Some(_) => Self::Changed,
        }
    }
}

/// A remote, as met: what it says about itself, and how it compares to memory.
pub struct Sighting {
    /// Where to reach it.
    #[allow(dead_code)] // A check reads what it said; the transfer needs where it is.
    pub ticket: RemoteTicket,
    /// What it said.
    pub announcement: Announcement,
    /// How the key it answered with stands next to the one recorded.
    pub key_status: RemoteKeyStatus,
}

/// Meets a remote at an address, and says how its key compares to what was
/// remembered there.
///
/// `remembered` is the key previously seen for this address, which the caller
/// looks up wherever it keeps such things. Learning and remembering are separate
/// on purpose: the first sighting is unauthenticated, so keeping it is a
/// decision rather than a side effect.
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
    Ok(Sighting {
        ticket,
        announcement,
        key_status: RemoteKeyStatus::classify(remembered, &id),
    })
}
