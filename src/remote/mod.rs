//! Reaching a remote from a client.
//!
//! Remotes are dialled by public key. The client↔remote path needs no inbound
//! port and no public URL; tracker ingress is a separate concern.
//!
//! The serve side is in use: `altitude serve` binds a server, announces it, and
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

mod announce;
mod clients;
mod identity;
mod keys;
pub mod serve;
mod server;

mod client;
mod known;
mod protocol;

use std::error::Error;

// One import site for the link, so a caller names what it wants from `net`
// rather than from a module inside it.
#[allow(unused_imports)] // The dial side is re-exported before it has a caller.
pub use {
    announce::Announcement,
    client::{LinkClient, LinkPath, LinkReply, PathKind},
    clients::ClientRecord,
    identity::InstallationKey,
    keys::{RemoteKey, RemoteKeys},
    known::{Sighting, Trust, meet},
    protocol::{BlobHash, LINK_ALPN, LinkRequest, LinkResponse, RemoteTicket},
    server::LinkServer,
};

/// Boxed errors: a typed error enum would be speculation until a caller exists.
pub type LinkError = Box<dyn Error + Send + Sync>;
/// The module's result alias.
pub type LinkResult<T> = Result<T, LinkError>;
