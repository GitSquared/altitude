//! Reaching a node from a client.
//!
//! Nodes are dialled by public key. The client↔node path needs no inbound port
//! and no public URL; tracker ingress is a separate concern. See
//! `design/decisions.md` (Q8) for the reasoning.
//!
//! This is a spike of the transport only: an identity, a serve side, a dial
//! side, one blob path, and the pairing that decides who may connect. It is
//! deliberately not wired into the task layer, it holds blobs in memory, and it
//! reconnects per operation. Run `cargo run --example iroh_link` to see both
//! sides work locally.
//!
//! The pieces:
//!
//! - [`identity`] — the keypair that names a node, and node-local file plumbing.
//! - [`protocol`] — the wire contract, and how a peer is addressed.
//! - [`clients`] — which clients a node has seen.
//! - [`node`] — the serve side.
//! - [`client`] — the dial side.

// `identity`, `clients` and `node` are wired into the app, so the compiler
// reports their dead code honestly and a few accessors carry their own allow.
// The rest has no caller in the app yet — only the example — so nothing inside
// it can be told apart from speculative API. Each loses its allow as it gains a
// caller, the same way the node side lost its own.
mod announce;
mod identity;
mod node;

#[allow(dead_code)]
mod client;
mod clients;
#[allow(dead_code)]
mod known;
#[allow(dead_code)]
mod protocol;

use std::error::Error;

// The facade. Only part of this is reachable from the app, so the rest reads as
// unused in a binary build; the example compiles against all of it. This goes
// away with the last `allow(dead_code)` above.
#[allow(unused_imports)]
pub use {
    announce::Announcement,
    client::{LinkClient, LinkPath, LinkReply, PathKind},
    clients::ClientRecord,
    identity::NodeIdentity,
    known::{KnownNode, KnownNodes, Sighting, Trust},
    node::LinkNode,
    protocol::{BlobHash, LINK_ALPN, LinkRequest, LinkResponse, NodeTicket},
};

/// Boxed errors: a typed error enum would be speculation until a caller exists.
pub type LinkError = Box<dyn Error + Send + Sync>;
/// The module's result alias.
pub type LinkResult<T> = Result<T, LinkError>;
