mod admin;
mod admission;
mod announce;
mod bundle;
pub(crate) use bundle::{HeldConfig, Logins};
mod clients;
mod compat;
mod console;
mod discovery;
mod identity;
mod keys;
pub mod send;
pub mod serve;
mod server;
mod start;
mod takeover;
mod tracker;
mod tracker_setup;
pub mod transfer;

mod client;
mod known;
mod protocol;
mod service;

use std::error::Error;

pub use {
    announce::{Announcement, RemoteState, TrackerConnection, TrackerReadiness},
    bundle::ConfigBundle,
    client::LinkClient,
    clients::ClientRecord,
    compat::{Compatibility, LOCAL_VERSION},
    discovery::{DiscoveredRemote, DiscoveryEvent, Scan},
    identity::InstallationKey,
    keys::{RemoteKey, RemoteKeys},
    known::{RemoteKeyStatus, meet},
    protocol::{
        ConfigPush, Handover, HandoverDraft, HandoverRecord, RecordedEvent, TaskOrigin,
        TaskRequest, TaskSummary, TransferArchive, TransferPart, WatchEvent,
    },
    server::LinkServer,
    takeover::bring_home,
    transfer::Transfer,
    transfer::Watching,
};

pub type LinkError = Box<dyn Error + Send + Sync>;

pub type LinkResult<T> = Result<T, LinkError>;

pub(crate) use start::{StartFuture, StartRefusal};
