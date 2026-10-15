//! The server side: accept a connection, answer one request, hold the blobs.

use std::{
    collections::HashMap,
    fmt, fs,
    future::Future,
    io::{self, Write},
    net::{IpAddr, Ipv6Addr, SocketAddr, UdpSocket},
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
};

use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt, Permissions, PermissionsExt};
use tokio::sync::{Mutex as AsyncMutex, watch};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

use iroh::{
    EndpointAddr, EndpointId,
    endpoint::{Builder, Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler, Router},
};

use crate::config::Settings;

use crate::task::TaskOperatorSignal;

use super::{
    LinkResult,
    announce::{Announcement, Broadcast, RemoteState, TrackerConnection},
    bundle::{ConfigBundle, HeldConfig, ProfileStamp},
    clients::{ClientLog, ClientRecord},
    identity::InstallationKey,
    protocol::{
        BlobHash, Handover, LINK_ALPN, LinkRequest, LinkResponse, MAX_BLOB_BYTES, RemoteTicket,
        TRANSFER_PIECE_BYTES, TaskRequest, TaskSummary, builder, read_body, read_frame,
        write_frame,
    },
};

/// The profiles a pushed configuration may carry. A pushed file is a login, and
/// only one Altitude knows how to mount is one.
const ALLOWED_PROFILES: [&str; 3] = ["agents/pi", "agents/claude", "vcs/github"];

// ------------------------------------------------------------- the server side

/// Binds the operator's address, and the same port on the other family when they
/// asked for every interface.
///
/// A server that answered on only one family would be unreachable from a client
/// that has only the other, and a client's configured `host:port` names one
/// port. IPv6 is probed first, because a host without it fails the bind rather
/// than ignoring it.
fn bind(builder: Builder, listen: SocketAddr) -> LinkResult<Builder> {
    let builder = builder.bind_addr(listen)?;
    if listen.ip().is_unspecified() && listen.is_ipv4() && ipv6_available() {
        let anywhere = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), listen.port());
        return Ok(builder.bind_addr(anywhere)?);
    }
    Ok(builder)
}

/// Whether this host can bind IPv6 at all.
///
/// Probed rather than assumed, because a host without it fails the bind instead
/// of ignoring it.
fn ipv6_available() -> bool {
    UdpSocket::bind("[::]:0").is_ok()
}

/// Blobs a server currently holds, keyed by digest.
///
/// In memory on purpose: this spike does not touch the capsule store, and a
/// restart loses them.
#[derive(Default)]
struct BlobStore(Mutex<HashMap<BlobHash, Vec<u8>>>);

impl BlobStore {
    fn put(&self, hash: BlobHash, bytes: Vec<u8>) {
        let mut store = self.0.lock().expect("blob store");
        store.insert(hash, bytes);
    }
}

#[derive(Clone)]
struct LinkHandler {
    id: EndpointId,
    blobs: Arc<BlobStore>,
    /// Where pushed configurations and logins are written, one directory per
    /// operator.
    data_dir: PathBuf,
    config_lock: Arc<AsyncMutex<()>>,
    /// The tasks this machine owns, which is the only place they exist.
    tasks: Arc<dyn TaskHost>,
}

impl fmt::Debug for LinkHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately does not print the blobs: they are payload, not identity.
        f.debug_struct("LinkHandler").field("id", &self.id).finish()
    }
}

impl ProtocolHandler for LinkHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // One request per connection. Reuse is a later concern; keeping the
        // exchange bounded makes the spike easy to reason about.
        let Ok((send, recv)) = connection.accept_bi().await else {
            return Ok(());
        };
        if let Err(err) = answer(self, connection.remote_id(), send, recv).await {
            tracing::warn!(%err, "link stream failed");
        }
        connection.closed().await;
        Ok(())
    }
}

impl LinkHandler {
    /// Takes a configuration a client sent: the settings, and the profiles this
    /// machine is missing.
    ///
    /// A profile's files are credentials, so they are written for this user alone and
    /// moved into place rather than written in place: nothing should read half a
    /// login.
    fn write_config(&self, peer: EndpointId, bytes: &[u8]) -> io::Result<()> {
        let bundle: ConfigBundle = serde_json::from_slice(bytes)
            .map_err(|error| io::Error::other(format!("not a configuration: {error}")))?;
        // Parsed before anything is written: a configuration that cannot be read
        // must not leave half a login on disk for a later run to trust.
        let _settings: Settings = toml::from_str(&bundle.settings)
            .map_err(|error| io::Error::other(format!("not a configuration: {error}")))?;
        let mut decoded = Vec::new();
        for profile in &bundle.profiles {
            if !ALLOWED_PROFILES.contains(&profile.profile.as_str()) {
                return Err(io::Error::other("unknown credential profile"));
            }
            for file in &profile.files {
                if file.path.is_empty()
                    || Path::new(&file.path)
                        .components()
                        .any(|component| !matches!(component, Component::Normal(_)))
                {
                    return Err(io::Error::other(
                        "profile file must have a confined relative path",
                    ));
                }
                decoded.push((
                    Path::new(&profile.profile).join("profile").join(&file.path),
                    BASE64.decode(&file.contents).map_err(io::Error::other)?,
                ));
            }
        }
        let operator_dir = self.operator_dir(peer);
        fs::create_dir_all(&operator_dir)?;
        let root = Dir::open_ambient_dir(&operator_dir, cap_std::ambient_authority())?;
        for (path, contents) in decoded {
            let parent = path
                .parent()
                .ok_or_else(|| io::Error::other("missing profile directory"))?;
            root.create_dir_all(parent)?;
            let directory = root.open_dir(parent)?;
            let filename = path
                .file_name()
                .ok_or_else(|| io::Error::other("missing profile filename"))?;
            let incoming = format!(".incoming-{}", uuid::Uuid::new_v4());
            let mut options = OpenOptions::new();
            options.write(true).create_new(true).mode(0o600);
            directory.set_permissions(".", Permissions::from_mode(0o700))?;
            let mut file = directory.open_with(&incoming, &options)?;
            file.set_permissions(Permissions::from_mode(0o600))?;
            file.write_all(&contents)?;
            file.sync_all()?;
            directory.rename(&incoming, &directory, filename)?;
        }
        write_atomically(
            &operator_dir.join("config.toml"),
            bundle.settings.as_bytes(),
        )?;
        let held_path = operator_dir.join("profiles.json");
        let mut held = HeldConfig::load(&held_path);
        for profile in &bundle.profiles {
            held.profiles
                .retain(|stamp| stamp.profile != profile.profile);
            held.profiles.push(ProfileStamp {
                profile: profile.profile.clone(),
                edited_at_ms: profile.edited_at_ms,
            });
        }
        held.save(&held_path)
    }

    /// Where one peer's pushed configuration and logins live.
    fn operator_dir(&self, peer: EndpointId) -> PathBuf {
        self.data_dir.join("operators").join(peer.to_string())
    }

    /// The profiles this peer has already been sent.
    fn held_profiles(&self, peer: EndpointId) -> Vec<ProfileStamp> {
        HeldConfig::load(&self.operator_dir(peer).join("profiles.json")).profiles
    }
}

/// What a server is started with: who it is, where it listens, and what it holds.
pub struct ServerParts {
    pub identity: InstallationKey,
    pub clients: ClientRecord,
    pub listen: SocketAddr,
    pub label: String,
    /// Where pushed configurations and logins are written, one directory per
    /// operator.
    pub data_dir: PathBuf,
    /// What this machine can do with a task right now.
    pub state: watch::Receiver<RemoteState>,
    pub trackers: Vec<TrackerConnection>,
    /// The tasks this machine owns, which is the only place they exist.
    pub tasks: Arc<dyn TaskHost>,
}

/// What a link can ask of the machine it is running on.
///
/// The link carries a request and an answer; what a machine does with a task is that
/// machine's business, which is why this is a trait rather than the link reaching
/// into a task service it should know nothing about.
pub trait TaskHost: Send + Sync + 'static {
    fn signals(
        &self,
        peer: EndpointId,
        task_id: String,
    ) -> BoxFuture<Result<Vec<TaskOperatorSignal>, String>>;
    fn signal(
        &self,
        peer: EndpointId,
        command: super::protocol::SignalCommand,
    ) -> BoxFuture<Result<(), String>>;
    fn pair_tracker(
        &self,
        peer: EndpointId,
        tracker_user_id: String,
    ) -> BoxFuture<Result<String, String>>;
    fn configure(&self, peer: EndpointId) -> BoxFuture<Result<(), String>>;
    fn run(&self, peer: EndpointId, request: TaskRequest) -> BoxFuture<Result<(), String>>;
    fn list(&self, peer: EndpointId) -> BoxFuture<Result<Vec<TaskSummary>, String>>;
    /// Stop a task and bundle it for another machine to continue.
    fn hand_over(&self, peer: EndpointId, task_id: String) -> BoxFuture<Result<Handover, String>>;
    fn transfer_piece(
        &self,
        peer: EndpointId,
        transfer_id: String,
        part: super::TransferPart,
        offset: u64,
        len: u64,
    ) -> BoxFuture<Result<Vec<u8>, String>>;
    /// Commit source removal after the prepared recipient verifies its arrival.
    fn handed_over(&self, peer: EndpointId, transfer_id: String) -> BoxFuture<Result<(), String>>;
    /// Accept a pushed task and say whether the base environment must follow.
    fn prepare_import(
        &self,
        peer: EndpointId,
        handover: Handover,
    ) -> BoxFuture<Result<bool, String>>;
    /// Write one bounded piece of an archive the recipient is staging.
    fn put_import_part(
        &self,
        peer: EndpointId,
        transfer_id: String,
        part: super::TransferPart,
        offset: u64,
        bytes: Vec<u8>,
    ) -> BoxFuture<Result<(), String>>;
    /// Land and boot the staged task, answering what it is called here.
    fn stage_import(
        &self,
        peer: EndpointId,
        transfer_id: String,
    ) -> BoxFuture<Result<String, String>>;
    /// The source relinquished ownership; begin the landed task.
    fn commit_import(&self, peer: EndpointId, transfer_id: String)
    -> BoxFuture<Result<(), String>>;
}

/// A future a trait object can hold, which an `async fn` in a trait cannot be.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Writes a file beside the real one and moves it into place, so nothing ever reads
/// half of it. Credentials are written for this user alone.
fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_private(parent)?;
    }
    let incoming = path.with_extension("incoming");
    {
        let mut file = fs::File::create(&incoming)?;
        set_private(&incoming)?;
        file.write_all(bytes)?;
    }
    fs::rename(&incoming, path)
}

/// Takes away everyone else's access to a file or directory this user owns.
#[cfg(unix)]
fn set_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> io::Result<()> {
    Ok(())
}

async fn answer(
    handler: &LinkHandler,
    peer: EndpointId,
    mut send: SendStream,
    mut recv: RecvStream,
) -> LinkResult<()> {
    let request: LinkRequest = read_frame(&mut recv).await?;
    match request {
        LinkRequest::Identify => {
            let response = LinkResponse::Identified {
                remote: handler.id.to_string(),
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::RunTask { hash, len } => {
            let bytes = read_body(&mut recv, len, MAX_BLOB_BYTES).await?;
            if BlobHash::of(&bytes) != hash {
                write_frame(
                    &mut send,
                    &LinkResponse::Rejected {
                        reason: "task request digest did not match".into(),
                    },
                )
                .await?;
                return Ok(());
            }
            let request = serde_json::from_slice(&bytes)?;
            let response = match handler.tasks.run(peer, request).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::ListSignals { task_id } => {
            let response = match handler.tasks.signals(peer, task_id).await {
                Ok(signals) => LinkResponse::Signals { signals },
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::Signal { command } => {
            let response = match handler.tasks.signal(peer, command).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::PairTracker { tracker_user_id } => {
            let response = match handler.tasks.pair_tracker(peer, tracker_user_id).await {
                Ok(code) => LinkResponse::PairingCode { code },
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::TakeTask { task_id } => {
            let response = match handler.tasks.hand_over(peer, task_id).await {
                Ok(handover) => LinkResponse::Handover {
                    handover: Box::new(handover),
                },
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::FetchTransferPart {
            transfer_id,
            part,
            offset,
            len,
        } => {
            match handler
                .tasks
                .transfer_piece(peer, transfer_id, part, offset, len)
                .await
            {
                Ok(bytes) => {
                    let announced = bytes.len() as u64;
                    write_frame(&mut send, &LinkResponse::TransferPiece { len: announced }).await?;
                    send.write_all(&bytes).await?;
                }
                Err(reason) => write_frame(&mut send, &LinkResponse::Rejected { reason }).await?,
            }
        }
        LinkRequest::HandedOver { transfer_id } => {
            let response = match handler.tasks.handed_over(peer, transfer_id).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::PrepareImport { handover } => {
            let response = match handler.tasks.prepare_import(peer, *handover).await {
                Ok(needs_environment) => LinkResponse::ImportPrepared { needs_environment },
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::PutImportPart {
            transfer_id,
            part,
            offset,
            hash,
            len,
        } => {
            let response = if len == 0 || len > TRANSFER_PIECE_BYTES {
                LinkResponse::Rejected {
                    reason: "invalid transfer piece length".into(),
                }
            } else {
                let bytes = read_body(&mut recv, len, TRANSFER_PIECE_BYTES).await?;
                if BlobHash::of(&bytes) != hash {
                    LinkResponse::Rejected {
                        reason: "transfer piece digest did not match".into(),
                    }
                } else {
                    match handler
                        .tasks
                        .put_import_part(peer, transfer_id, part, offset, bytes)
                        .await
                    {
                        Ok(()) => LinkResponse::Accepted,
                        Err(reason) => LinkResponse::Rejected { reason },
                    }
                }
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::StageImport { transfer_id } => {
            let response = match handler.tasks.stage_import(peer, transfer_id).await {
                Ok(task_id) => LinkResponse::Imported { task_id },
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::CommitImport { transfer_id } => {
            let response = match handler.tasks.commit_import(peer, transfer_id).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::ListTasks => {
            let response = match handler.tasks.list(peer).await {
                Ok(summaries) => LinkResponse::Tasks { summaries },
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::HeldProfiles => {
            let response = LinkResponse::HeldProfiles {
                profiles: handler.held_profiles(peer),
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::PutConfig { hash, len } => {
            let bytes = read_body(&mut recv, len, MAX_BLOB_BYTES).await?;
            let actual = BlobHash::of(&bytes);
            let response = if actual != hash {
                LinkResponse::Rejected {
                    reason: format!("digest mismatch: announced {hash}, received {actual}"),
                }
            } else {
                let _writing = handler.config_lock.lock().await;
                match handler.write_config(peer, &bytes) {
                    Ok(()) => match handler.tasks.configure(peer).await {
                        Ok(()) => LinkResponse::Configured,
                        Err(reason) => LinkResponse::Rejected { reason },
                    },
                    Err(error) => LinkResponse::Rejected {
                        reason: format!("could not take the configuration: {error}"),
                    },
                }
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::PutBlob { hash, len } => {
            let bytes = read_body(&mut recv, len, MAX_BLOB_BYTES).await?;
            let actual = BlobHash::of(&bytes);
            let response = if actual == hash {
                handler.blobs.put(hash.clone(), bytes);
                LinkResponse::Stored { hash, len }
            } else {
                LinkResponse::Rejected {
                    reason: format!("digest mismatch: announced {hash}, received {actual}"),
                }
            };
            write_frame(&mut send, &response).await?;
        }
    }
    send.finish()?;
    Ok(())
}

/// The server side of the link: an endpoint that accepts connections and the
/// blobs it holds.
pub struct LinkServer {
    router: Router,
    identity: InstallationKey,
    broadcast: Broadcast,
}

impl LinkServer {
    /// Binds an endpoint for `identity` at `listen` and starts accepting link
    /// connections.
    ///
    /// `listen` is the operator's, not the operating system's: a port nobody can
    /// predict is a port nobody can configure a client to reach.
    pub async fn serve(parts: ServerParts) -> LinkResult<Self> {
        let ServerParts {
            identity,
            clients,
            listen,
            label,
            data_dir,
            state,
            trackers,
            tasks,
        } = parts;
        let blobs = Arc::new(BlobStore::default());
        let handler = LinkHandler {
            id: identity.id(),
            blobs: Arc::clone(&blobs),
            data_dir,
            config_lock: Arc::new(AsyncMutex::new(())),
            tasks,
        };
        let log = ClientLog {
            record: clients.clone(),
        };
        let endpoint = bind(
            builder()
                .secret_key(identity.secret_key().clone())
                .alpns(vec![LINK_ALPN.to_vec()])
                .hooks(log),
            listen,
        )?
        .bind()
        .await?;
        let router = Router::builder(endpoint).accept(LINK_ALPN, handler).spawn();
        // Announced on the same address, over TCP, so that a client which has
        // only been told where this server is can still find out who it is.
        let broadcast = Broadcast::start(
            &router.endpoint().bound_sockets(),
            Announcement {
                protocol: String::from_utf8_lossy(LINK_ALPN).into_owned(),
                id: identity.id().to_string(),
                label,
                version: env!("CARGO_PKG_VERSION").to_owned(),
                state: RemoteState::Preparing,
                trackers,
            },
            state,
        )
        .await?;
        Ok(Self {
            router,
            identity,
            broadcast,
        })
    }

    /// The public key a client dials.
    pub fn id(&self) -> EndpointId {
        self.identity.id()
    }

    /// The server's address as a string to hand to a client out of band.
    pub fn ticket(&self) -> LinkResult<RemoteTicket> {
        RemoteTicket::new(self.addr())
    }

    /// How to reach the server right now: its key plus whatever addresses are
    /// currently known. Not public: a caller wants the ticket, which is the
    /// address rendered as something it can carry.
    fn addr(&self) -> EndpointAddr {
        self.router.endpoint().addr()
    }

    /// Stops accepting and closes the endpoint.
    pub async fn shutdown(self) -> LinkResult<()> {
        self.broadcast.stop();
        self.router.shutdown().await?;
        Ok(())
    }
}
