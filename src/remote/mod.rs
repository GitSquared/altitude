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

//! The link between machines, and the rules a transfer obeys.
//!
//! A task is owned by exactly one machine, and only that machine may run it. A transfer
//! hands ownership to another machine, and two rules make that safe:
//!
//! 1. The source stops running the task before the payload leaves it.
//! 2. The destination owns the task only once it holds the whole payload.
//!
//! Break either and two machines can run the same task. Keep both and the worst case is
//! that nobody runs it for a moment, which is the failure that repairs itself. Authority
//! is possession rather than a version number, because a counter would need a second
//! party to be monotonic relative to and there is no shared record left to version.

mod announce;
mod bundle;
mod clients;
mod identity;
mod keys;
pub mod serve;
mod server;
mod takeover;

mod client;
mod known;
mod protocol;

use std::error::Error;

pub use {
    announce::{Announcement, RemoteState},
    bundle::ConfigBundle,
    client::LinkClient,
    clients::ClientRecord,
    identity::InstallationKey,
    keys::{RemoteKey, RemoteKeys},
    known::{RemoteKeyStatus, meet},
    protocol::{CapsuleArchive, Handover, HandoverDraft, RecordedEvent, TaskRequest, TaskSummary},
    server::LinkServer,
    takeover::bring_home,
};

/// Boxed errors: a typed error enum would be speculation until a caller exists.
pub type LinkError = Box<dyn Error + Send + Sync>;

pub type LinkResult<T> = Result<T, LinkError>;
