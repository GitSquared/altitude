//! Task transport between independently owned installations.
//!
//! The source retains ownership while the destination synchronizes the exact base,
//! imports the disk delta, and durably stages its history. The destination cannot
//! run work until the source commits relinquishment. Prepared transfers bind all
//! downloads and acknowledgements to one recipient and survive process restarts.

mod announce;
mod bundle;
mod clients;
mod discovery;
mod identity;
mod keys;
pub mod send;
pub mod serve;
mod server;
mod takeover;
mod tracker;
pub mod transfer;

mod client;
mod known;
mod protocol;

use std::error::Error;

pub use {
    announce::{Announcement, RemoteState, TrackerConnection},
    bundle::ConfigBundle,
    client::LinkClient,
    clients::ClientRecord,
    discovery::{DiscoveredRemote, DiscoveryEvent},
    identity::InstallationKey,
    keys::{RemoteKey, RemoteKeys},
    known::{RemoteKeyStatus, meet},
    protocol::{
        EnvironmentArchive, Handover, HandoverDraft, HandoverRecord, RecordedEvent, TaskOrigin,
        TaskRequest, TaskSummary, TransferArchive, TransferPart,
    },
    server::LinkServer,
    takeover::bring_home,
    transfer::Transfer,
};

/// Boxed errors: a typed error enum would be speculation until a caller exists.
pub type LinkError = Box<dyn Error + Send + Sync>;

pub type LinkResult<T> = Result<T, LinkError>;
