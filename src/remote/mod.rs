mod acting;
pub(crate) mod archive;
mod bundle;
pub(crate) mod host;
pub(crate) mod identity;
mod link;
pub(crate) mod transfer;

pub(crate) use acting::send;
pub(crate) use bundle::{HeldConfig, Logins};
pub(crate) use host::console;
pub use host::serve;
pub(crate) use host::start::{StartFuture, StartRefusal};
pub(crate) use link::{announce, client, compat, discovery, known, protocol, server};

use std::error::Error;

pub use {
    acting::keys::{RemoteKey, RemoteKeys},
    acting::takeover::bring_home,
    announce::{Announcement, Phase, RemoteState, TrackerConnection, TrackerReadiness},
    bundle::ConfigBundle,
    client::LinkClient,
    compat::{Compatibility, LOCAL_VERSION},
    discovery::{DiscoveredRemote, DiscoveryEvent, Scan},
    host::clients::ClientRecord,
    identity::InstallationKey,
    known::{RemoteKeyStatus, meet},
    protocol::{
        ConfigPush, HISTORY_FILE, Handover, HandoverDraft, HandoverRecord, RecordedEvent,
        TRANSCRIPT_FILE, TaskOrigin, TaskRequest, TaskSummary, TransferArchive, TransferPart,
        WORK_FILE, WORK_PLAIN_FILE, WatchEvent,
    },
    server::LinkServer,
    transfer::Transfer,
    transfer::Watching,
};

pub type LinkError = Box<dyn Error + Send + Sync>;

pub type LinkResult<T> = Result<T, LinkError>;
