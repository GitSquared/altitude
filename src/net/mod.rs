//! Reaching a node from a client.
//!
//! Nodes are dialled by public key. The client↔node path needs no inbound port
//! and no public URL; tracker ingress is a separate concern. See
//! `design/decisions.md` (Q8) for the reasoning.
//!
//! The serve side is in use: `altitude serve` binds a node, announces it, and
//! records whoever connects. The dial side has no caller yet — nothing in the app
//! reaches a remote until there is somewhere to configure one — and neither has
//! moved a capsule, which is what this is ultimately for. Blobs are held in
//! memory and a connection is made per operation.
//!
//! To see the serve side work, on one machine:
//!
//! ```text
//! $ altitude serve
//! listening on 0.0.0.0:25848 (udp)
//! ticket       {"id":"…","addrs":[{"Ip":"…:25848"}]}
//! $ curl http://127.0.0.1:25848/
//! {"protocol":"altitude/link/1","id":"…","label":"…","version":"…"}
//! ```
//!
//! The pieces:
//!
//! - [`identity`] — the keypair that names a node, and node-local file plumbing.
//! - [`protocol`] — the wire contract, and how a peer is addressed.
//! - [`announce`] — how a node says who it is to a client that cannot dial it yet.
//! - [`clients`] — which clients a node has seen.
//! - [`known`] — comparing a key to the one remembered for an address.
//! - [`node`] — the serve side.
//! - [`client`] — the dial side.

// `identity`, `clients`, `announce` and `node` are wired into the app, so the
// compiler reports their dead code honestly and a few accessors carry their own
// allow. What is left is the dialling side: nothing in the app reaches a remote
// yet, because that arrives with the settings screen. Each module loses its
// allow as it gains a caller, the same way the node side lost its own.
mod announce;
mod clients;
mod identity;
mod node;

#[allow(dead_code)]
mod client;
#[allow(dead_code)]
mod known;
#[allow(dead_code)]
mod protocol;

use std::error::Error;

// The facade: one import site for the link, so a caller names what it wants from
// `net` rather than from a module inside it. Part of this is the dialling side
// above, which nothing in the app reaches yet, so the re-export is allowed to
// read as unused. It stops being needed when the last `allow(dead_code)` above
// goes.
#[allow(unused_imports)]
pub use {
    announce::Announcement,
    client::{LinkClient, LinkPath, LinkReply, PathKind},
    clients::ClientRecord,
    identity::NodeIdentity,
    known::{KnownNode, Sighting, Trust, meet},
    node::LinkNode,
    protocol::{BlobHash, LINK_ALPN, LinkRequest, LinkResponse, NodeTicket},
};

/// Boxed errors: a typed error enum would be speculation until a caller exists.
pub type LinkError = Box<dyn Error + Send + Sync>;
/// The module's result alias.
pub type LinkResult<T> = Result<T, LinkError>;
