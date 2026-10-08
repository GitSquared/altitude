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

mod announce;
mod bundle;
mod clients;
mod identity;
mod keys;
pub mod serve;
mod server;

mod client;
mod known;
mod protocol;

use std::error::Error;

// Callers name the link, not the modules inside it, and not every part of the link
// has a caller yet.
#[allow(unused_imports)]
pub use {
    announce::{Announcement, RemoteState},
    bundle::{ConfigBundle, ProfileStamp},
    client::{LinkClient, LinkPath, LinkReply, PathKind},
    clients::ClientRecord,
    identity::InstallationKey,
    keys::{RemoteKey, RemoteKeys},
    known::{RemoteKeyStatus, Sighting, meet},
    protocol::{
        BlobHash, CapsuleArchive, Handover, HandoverDraft, LINK_ALPN, LinkRequest, LinkResponse,
        RecordedEvent, RemoteTicket, TaskRequest, TaskSummary,
    },
    server::LinkServer,
};

/// Boxed errors: a typed error enum would be speculation until a caller exists.
pub type LinkError = Box<dyn Error + Send + Sync>;

pub type LinkResult<T> = Result<T, LinkError>;
