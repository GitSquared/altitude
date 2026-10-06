//! The nodes this client has met.
//!
//! A key learned from an announcement is unauthenticated — that is what makes a
//! first sighting possible at all — so it is written down here and compared on
//! every later one. That is the whole protection: not that the first answer was
//! trustworthy, but that a *different* answer later is noticed.
//!
//! Kept beside the key rather than in the task database, because it is trust
//! state and because the link has to work without a database. Nothing here is a
//! secret: a node's key is public by definition.

use std::{
    collections::BTreeMap,
    fs,
    io::ErrorKind,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use super::{LinkResult, announce::Announcement, identity::write_private, protocol::NodeTicket};

/// What is remembered about a node, keyed by the address a person typed.
///
/// The address as typed, not the addresses the ticket resolved to: a pin is
/// about the name you use for a machine, the way a known-hosts entry is.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct KnownNode {
    /// The key this address answered with last time.
    pub id: String,
    /// How the node reads to a human, so a list can be read without connecting.
    pub label: String,
}

/// How a node's key compares to the one remembered for its address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// This address has not been seen before.
    New,
    /// The same key as last time.
    Known,
    /// A different key.
    ///
    /// A node that was replaced and a node that was never the one you thought
    /// look identical from here, so this is reported and never followed
    /// silently. The operator decides.
    Changed { previous: String },
}

/// A node, as met: what it says about itself and how it compares to memory.
pub struct Sighting {
    /// Where to reach it.
    pub ticket: NodeTicket,
    /// What it said.
    pub announcement: Announcement,
    /// How its key compares to the one remembered for this address.
    pub trust: Trust,
}

/// The nodes this client remembers.
#[derive(Clone, Debug, Default)]
pub struct KnownNodes {
    path: Option<PathBuf>,
    known: Arc<RwLock<BTreeMap<String, KnownNode>>>,
}

impl KnownNodes {
    /// A memory kept at `path`.
    ///
    /// A missing file means no node has been met, and nothing is written until
    /// one is remembered.
    pub fn open(path: impl Into<PathBuf>) -> LinkResult<Self> {
        let path = path.into();
        let known = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)?,
            Err(err) if err.kind() == ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => return Err(err.into()),
        };
        Ok(Self {
            path: Some(path),
            known: Arc::new(RwLock::new(known)),
        })
    }

    /// Meets a node at an address, without remembering it.
    ///
    /// Learning and remembering are separate on purpose: the first sighting is
    /// unauthenticated, so the operator is the one who decides to keep it.
    pub async fn learn(&self, address: &str, default_port: u16) -> LinkResult<Sighting> {
        let (ticket, announcement) = NodeTicket::from_broadcast(address, default_port).await?;
        let id: EndpointId = announcement
            .id
            .parse()
            .map_err(|error| format!("{address} announced an unreadable key: {error}"))?;
        let trust = self.verify(address, &id);
        Ok(Sighting {
            ticket,
            announcement,
            trust,
        })
    }

    /// How a key compares to the one remembered for an address.
    pub fn verify(&self, address: &str, id: &EndpointId) -> Trust {
        match self.known.read().expect("known nodes").get(address) {
            None => Trust::New,
            Some(remembered) if remembered.id == id.to_string() => Trust::Known,
            Some(remembered) => Trust::Changed {
                previous: remembered.id.clone(),
            },
        }
    }

    /// Writes down what a node said.
    ///
    /// This is also how a change is accepted: remembering a new key replaces the
    /// old one, so accepting is the same act as meeting a node for the first
    /// time.
    pub fn remember(&self, address: &str, sighting: &Sighting) -> LinkResult<()> {
        let mut known = self.known.write().expect("known nodes");
        known.insert(
            address.to_owned(),
            KnownNode {
                id: sighting.announcement.id.clone(),
                label: sighting.announcement.label.clone(),
            },
        );
        self.persist(&known)
    }

    /// Forgets a node. `true` if it was remembered.
    ///
    /// The next meeting is a first sighting again, which is what forgetting
    /// means.
    pub fn forget(&self, address: &str) -> LinkResult<bool> {
        let mut known = self.known.write().expect("known nodes");
        let removed = known.remove(address).is_some();
        if removed {
            self.persist(&known)?;
        }
        Ok(removed)
    }

    /// Every node remembered, in a stable order, as `(address, node)`.
    pub fn known(&self) -> Vec<(String, KnownNode)> {
        self.known
            .read()
            .expect("known nodes")
            .iter()
            .map(|(address, node)| (address.clone(), node.clone()))
            .collect()
    }

    fn persist(&self, known: &BTreeMap<String, KnownNode>) -> LinkResult<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let text = format!("{}\n", serde_json::to_string_pretty(known)?);
        write_private(path, &text)
    }
}
