//! Comparing a remote's key to the one you remember for its address.
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

// The dial side of the link: nothing in the app reaches a remote until the
// settings screen can configure one, which is the next piece of work on remotes.
#![allow(dead_code)]
use iroh::EndpointId;

use super::{LinkResult, announce::Announcement, protocol::RemoteTicket};

/// How a remote's key compares to the one remembered for its address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// This address has not been seen before.
    New,
    /// The same key as last time.
    Known,
    /// A different key.
    ///
    /// A remote that was replaced and a remote that was never the one you thought
    /// look identical from here, so this is reported and never followed
    /// silently. The operator decides.
    Changed { previous: String },
}

impl Trust {
    /// Compares a key to the one remembered for an address, if any.
    pub fn between(remembered: Option<&str>, id: &EndpointId) -> Self {
        match remembered {
            None => Self::New,
            Some(previous) if previous == id.to_string() => Self::Known,
            Some(previous) => Self::Changed {
                previous: previous.to_owned(),
            },
        }
    }
}

/// A remote, as met: what it says about itself, and how it compares to memory.
pub struct Sighting {
    /// Where to reach it.
    pub ticket: RemoteTicket,
    /// What it said.
    pub announcement: Announcement,
    /// How its key compares to the one remembered for this address.
    pub trust: Trust,
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
        trust: Trust::between(remembered, &id),
    })
}
