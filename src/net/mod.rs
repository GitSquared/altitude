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
// The dial side and the wire contract still have no caller, so nothing inside
// them can be told apart from speculative API yet.
mod identity;

#[allow(dead_code)]
mod client;
mod clients;
mod node;
#[allow(dead_code)]
mod protocol;

use std::error::Error;

// The facade. Only `identity` is reachable from the app, so the rest reads as
// unused in a binary build; the example compiles against all of it. This goes
// away with the last `allow(dead_code)` above.
#[allow(unused_imports)]
pub use {
    client::{LinkClient, LinkPath, LinkReply, PathKind},
    clients::ClientRecord,
    identity::NodeIdentity,
    node::LinkNode,
    protocol::{BlobHash, LINK_ALPN, LinkRequest, LinkResponse, NodeTicket},
};

/// Boxed errors: a typed error enum would be speculation until a caller exists.
pub type LinkError = Box<dyn Error + Send + Sync>;
/// The module's result alias.
pub type LinkResult<T> = Result<T, LinkError>;
