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
//! - [`protocol`] — the wire contract, how an endpoint is wired, how a peer is
//!   addressed.
//! - [`access`] — the keys a node will talk to.
//! - [`node`] — the serve side.
//! - [`client`] — the dial side.

#![allow(dead_code)] // Remove once the link is wired into the app.

use std::error::Error;

mod access;
mod client;
mod identity;
mod node;
mod protocol;

// The facade. Nothing in the app uses it yet, which is why it reads as unused
// in a binary build; the example compiles against it. Both this and the
// `allow(dead_code)` above go away when the link is wired in.
#[allow(unused_imports)]
pub use {
    access::NodeAccess,
    client::{LinkClient, LinkClientError, LinkPath, LinkReply, PathKind},
    identity::NodeIdentity,
    node::LinkNode,
    protocol::{BlobHash, Discovery, LINK_ALPN, LinkRequest, LinkResponse, NodeTicket},
};

/// Boxed errors: a typed error enum would be speculation until a caller exists.
pub type LinkError = Box<dyn Error + Send + Sync>;
/// The module's result alias.
pub type LinkResult<T> = Result<T, LinkError>;
