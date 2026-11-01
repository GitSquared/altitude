use std::{
    collections::HashSet,
    fmt, fs,
    future::Future,
    io::{self, Write},
    net::{IpAddr, Ipv6Addr, SocketAddr, UdpSocket},
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use crate::remote::TransferPart;
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt, Permissions, PermissionsExt};
use tokio::sync::{Mutex as AsyncMutex, mpsc::UnboundedReceiver, watch};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{
    EndpointAddr, EndpointId,
    endpoint::{Builder, Connection, RecvStream, SendStream},
};

use crate::config::Settings;

use crate::remote::host::clients::{ClientLog, ClientRecord};
use crate::task::TaskOperatorSignalThread;

use crate::remote::{
    LinkResult,
    announce::{Announcement, Broadcast, RemoteState, TrackerConnection},
    bundle::{ConfigBundle, HeldConfig, ProfileDigest},
    discovery,
    identity::InstallationKey,
};

use crate::credentials::Service;
use crate::remote::protocol::SignalCommand;
use crate::remote::protocol::{
    BlobHash, Handover, LINK_ALPN, LinkRequest, LinkResponse, MAX_BLOB_BYTES, RemoteTicket,
    TRANSFER_PIECE_BYTES, TaskRequest, TaskSummary, WatchEvent, builder, read_body, read_frame,
    write_frame,
};
use crate::remote::tracker::{claim_account, logins_of, yield_account};
use crate::remote::transfer::{TransferUpdate, Unit};
use crate::tracker::{ACCOUNT_FILE, TrackerAccount, TrackerKind};

enum ConfigWrite {
    Written {
        forget_logins: bool,
    },
    LinkedElsewhere {
        provider: TrackerKind,
        display_name: String,
    },
}

fn arriving_accounts(decoded: &[(PathBuf, Vec<u8>)]) -> Vec<(TrackerKind, Option<TrackerAccount>)> {
    let logins: Vec<(String, &[u8])> = decoded
        .iter()
        .filter_map(|(path, bytes)| {
            if path.file_name()? != ACCOUNT_FILE {
                return None;
            }
            // `trackers/linear/gaby/profile/identity.json` names the login two
            // directories up.
            let login = path.parent()?.parent()?;
            Some((login.to_string_lossy().into_owned(), bytes.as_slice()))
        })
        .collect();
    TrackerKind::ALL
        .iter()
        .map(|provider| {
            let account = logins
                .iter()
                .find(|(login, _)| provider.owns(login))
                .and_then(|(_, bytes)| serde_json::from_slice::<TrackerAccount>(bytes).ok())
                .filter(|account| account.provider == *provider);
            (*provider, account)
        })
        .collect()
}

fn is_allowed_profile(profile: &str) -> bool {
    // A pushed profile is a credential id, and the id says which service it
    // belongs to. The path checks below keep it confined; this keeps it a
    // service this build knows.
    Service::from_id(profile).is_some()
}

// ------------------------------------------------------------- the server side

fn bind(builder: Builder, listen: SocketAddr) -> LinkResult<Builder> {
    let builder = builder
        .clear_ip_transports()
        .bind_addr(listen)
        .map_err(|error| format!("UDP task bind at {listen}: {error}"))?;
    if listen.ip().is_unspecified() && listen.is_ipv4() && ipv6_available() {
        let anywhere = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), listen.port());
        return Ok(builder
            .bind_addr(anywhere)
            .map_err(|error| format!("UDP task bind at {anywhere}: {error}"))?);
    }
    Ok(builder)
}

fn ipv6_available() -> bool {
    UdpSocket::bind("[::]:0").is_ok()
}

#[derive(Clone)]
struct LinkHandler {
    id: EndpointId,
    data_dir: PathBuf,
    config_lock: Arc<AsyncMutex<()>>,
    tasks: Arc<dyn TaskHost>,
}

impl fmt::Debug for LinkHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
    fn write_config(
        &self,
        peer: EndpointId,
        bytes: &[u8],
        take_tracker: bool,
    ) -> io::Result<ConfigWrite> {
        let bundle: ConfigBundle = serde_json::from_slice(bytes)
            .map_err(|error| io::Error::other(format!("not a configuration: {error}")))?;
        // Parsed before anything is written: a configuration that cannot be read
        // must not leave half a login on disk for a later run to trust.
        let _settings: Settings = toml::from_str(&bundle.settings)
            .map_err(|error| io::Error::other(format!("not a configuration: {error}")))?;
        let mut decoded = Vec::new();
        for profile in &bundle.profiles {
            if !is_allowed_profile(&profile.profile) {
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
        // What the arrival names, before anything is on disk to read back, because
        // a refusal must leave nothing behind.
        let arriving = arriving_accounts(&decoded);
        let holders = self.tracker_holders(peer, &arriving);
        if !take_tracker && let Some((_, provider, display_name)) = holders.first() {
            return Ok(ConfigWrite::LinkedElsewhere {
                provider: *provider,
                display_name: display_name.clone(),
            });
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
        for profile in &bundle.profiles {
            let path = Path::new(&profile.profile).join("profile");
            root.create_dir_all(&path)?;
            let directory = root.open_dir(&path)?;
            let wanted: HashSet<_> = profile
                .files
                .iter()
                .map(|file| PathBuf::from(&file.path))
                .collect();
            remove_missing_profile_files(&directory, Path::new(""), &wanted)?;
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
            held.profiles.push(ProfileDigest {
                profile: profile.profile.clone(),
                digest: profile.digest.clone(),
            });
        }
        held.save(&held_path)?;
        // Taking an account over says whose it is now, so the client it was linked
        // to stops claiming it. What it pushed stays where it is: the login is the
        // same account, and deleting another client's credentials is not what a
        // question about attribution should be able to authorise.
        for (other, provider, _) in &holders {
            if let Err(error) = yield_account(other, *provider) {
                tracing::warn!(%error, operator = %other.display(), "could not release a taken-over tracker account");
            }
        }
        // A client that pushes a login again claims it again, so a machine that was
        // paired, yielded an account and is paired again is not left silent.
        for (provider, account) in &arriving {
            if account.is_some()
                && let Err(error) = claim_account(&operator_dir, *provider)
            {
                tracing::warn!(%error, operator = %operator_dir.display(), "could not record a claim on a tracker account");
            }
        }
        Ok(ConfigWrite::Written {
            forget_logins: bundle.remove_logins,
        })
    }

    fn tracker_holders(
        &self,
        peer: EndpointId,
        pushed: &[(TrackerKind, Option<TrackerAccount>)],
    ) -> Vec<(PathBuf, TrackerKind, String)> {
        let mut holders = Vec::new();
        let operators = self.data_dir.join("operators");
        let Ok(entries) = fs::read_dir(&operators) else {
            return holders;
        };
        for entry in entries.flatten() {
            let directory = entry.path();
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            if entry.file_name().to_string_lossy() == peer.to_string() {
                continue;
            }
            let owner = entry.file_name().to_string_lossy().into_owned();
            for (provider, account) in pushed {
                let Some(account) = account else { continue };
                if logins_of(&directory, &owner, *provider)
                    .iter()
                    .any(|login| login.account.user_id == account.user_id)
                {
                    holders.push((directory.clone(), *provider, account.display_name.clone()));
                }
            }
        }
        holders
    }

    fn operator_dir(&self, peer: EndpointId) -> PathBuf {
        self.data_dir.join("operators").join(peer.to_string())
    }

    fn held_profiles(&self, peer: EndpointId) -> Vec<ProfileDigest> {
        HeldConfig::load(&self.operator_dir(peer).join("profiles.json")).profiles
    }
}

fn remove_missing_profile_files(
    directory: &Dir,
    relative: &Path,
    wanted: &HashSet<PathBuf>,
) -> io::Result<()> {
    for entry in directory.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let path = relative.join(&name);
        if entry.file_type()?.is_dir() {
            remove_missing_profile_files(&directory.open_dir(&name)?, &path, wanted)?;
        } else if !wanted.contains(&path) {
            directory.remove_file(&name)?;
        }
    }
    Ok(())
}

pub struct ServerParts {
    pub identity: InstallationKey,
    pub clients: ClientRecord,
    pub listen: SocketAddr,
    pub label: String,
    pub data_dir: PathBuf,
    pub state: watch::Receiver<RemoteState>,
    pub trackers: Vec<TrackerConnection>,
    pub tasks: Arc<dyn TaskHost>,
}

pub trait TaskHost: Send + Sync + 'static {
    fn signal_threads(
        &self,
        peer: EndpointId,
        task_id: String,
    ) -> BoxFuture<Result<Vec<TaskOperatorSignalThread>, String>>;
    fn signal(&self, peer: EndpointId, command: SignalCommand) -> BoxFuture<Result<(), String>>;
    fn input(
        &self,
        peer: EndpointId,
        task_id: String,
        body: String,
    ) -> BoxFuture<Result<(), String>>;
    fn configure(&self, peer: EndpointId, forget_logins: bool) -> BoxFuture<Result<(), String>>;
    fn run(&self, peer: EndpointId, request: TaskRequest) -> BoxFuture<Result<(), String>>;
    fn list(&self, peer: EndpointId) -> BoxFuture<Result<Vec<TaskSummary>, String>>;
    fn watch(&self, peer: EndpointId) -> BoxFuture<Result<Watching, String>>;
    fn remove(&self, peer: EndpointId, task_id: String) -> BoxFuture<Result<(), String>>;
    fn take_progress(&self, task_id: &str) -> Option<TransferUpdate>;
    fn hand_over(&self, peer: EndpointId, task_id: String) -> BoxFuture<Result<Handover, String>>;
    fn transfer_piece(
        &self,
        peer: EndpointId,
        transfer_id: String,
        part: TransferPart,
        offset: u64,
        len: u64,
    ) -> BoxFuture<Result<Vec<u8>, String>>;
    fn abandon_take(&self, peer: EndpointId, transfer_id: String) -> BoxFuture<Result<(), String>>;
    fn abandon_import(
        &self,
        peer: EndpointId,
        transfer_id: String,
    ) -> BoxFuture<Result<(), String>>;
    fn handed_over(&self, peer: EndpointId, transfer_id: String) -> BoxFuture<Result<(), String>>;
    fn prepare_import(&self, peer: EndpointId, handover: Handover)
    -> BoxFuture<Result<(), String>>;
    fn put_import_part(
        &self,
        peer: EndpointId,
        transfer_id: String,
        part: TransferPart,
        offset: u64,
        bytes: Vec<u8>,
    ) -> BoxFuture<Result<(), String>>;
    fn stage_import(
        &self,
        peer: EndpointId,
        transfer_id: String,
    ) -> BoxFuture<Result<String, String>>;
    fn commit_import(&self, peer: EndpointId, transfer_id: String)
    -> BoxFuture<Result<(), String>>;
}

pub struct Watching {
    pub snapshot: Vec<TaskSummary>,
    pub changes: UnboundedReceiver<WatchEvent>,
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        make_private(parent)?;
    }
    let incoming = path.with_extension("incoming");
    {
        let mut file = fs::File::create(&incoming)?;
        make_private(&incoming)?;
        file.write_all(bytes)?;
    }
    fs::rename(&incoming, path)
}

#[cfg(unix)]
fn make_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn make_private(_path: &Path) -> io::Result<()> {
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
        LinkRequest::WatchTasks => {
            let watching = match handler.tasks.watch(peer).await {
                Ok(watching) => watching,
                Err(reason) => {
                    write_frame(&mut send, &LinkResponse::Rejected { reason }).await?;
                    return Ok(());
                }
            };
            // The snapshot is written before the changes queued behind it are
            // drained, so what a watch opens with is never older than what follows.
            write_frame(
                &mut send,
                &LinkResponse::Tasks {
                    summaries: watching.snapshot,
                },
            )
            .await?;
            let mut changes = watching.changes;
            while let Some(change) = changes.recv().await {
                let response = match change {
                    WatchEvent::Changed(summary) => LinkResponse::TaskChanged { summary },
                    WatchEvent::Removed(id) => LinkResponse::TaskRemoved { id },
                    WatchEvent::Ready => LinkResponse::Ready,
                    WatchEvent::Snapshot(_) => continue,
                };
                write_frame(&mut send, &response).await?;
            }
            return Ok(());
        }
        LinkRequest::ListSignals { task_id } => {
            let response = match handler.tasks.signal_threads(peer, task_id).await {
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
        LinkRequest::Input { task_id, body } => {
            let response = match handler.tasks.input(peer, task_id, body).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::AbandonTake { transfer_id } => {
            let response = match handler.tasks.abandon_take(peer, transfer_id).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::AbandonImport { transfer_id } => {
            let response = match handler.tasks.abandon_import(peer, transfer_id).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::RemoveTask { task_id } => {
            let response = match handler.tasks.remove(peer, task_id).await {
                Ok(()) => LinkResponse::Removed,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::TakeProgress { task_id } => {
            let response = match handler.tasks.take_progress(&task_id) {
                Some(progress) => LinkResponse::Collecting {
                    stage: progress.stage,
                    done: progress.done,
                    total: progress.total,
                    unit: match progress.unit {
                        Unit::Bytes => "bytes".to_owned(),
                        Unit::Files => "files".to_owned(),
                    },
                },
                None => LinkResponse::Rejected {
                    reason: "nothing is being collected for that task".to_owned(),
                },
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
                Ok(()) => LinkResponse::ImportPrepared,
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
        LinkRequest::PutConfig {
            hash,
            len,
            take_tracker,
        } => {
            let bytes = read_body(&mut recv, len, MAX_BLOB_BYTES).await?;
            let actual = BlobHash::of(&bytes);
            let response = if actual != hash {
                LinkResponse::Rejected {
                    reason: format!("digest mismatch: announced {hash}, received {actual}"),
                }
            } else {
                let _writing = handler.config_lock.lock().await;
                match handler.write_config(peer, &bytes, take_tracker) {
                    Ok(ConfigWrite::Written { forget_logins }) => {
                        match handler.tasks.configure(peer, forget_logins).await {
                            Ok(()) => LinkResponse::Configured,
                            Err(reason) => LinkResponse::Rejected { reason },
                        }
                    }
                    Ok(ConfigWrite::LinkedElsewhere {
                        provider,
                        display_name,
                    }) => LinkResponse::TrackerLinkedElsewhere {
                        provider,
                        display_name,
                    },
                    Err(error) => LinkResponse::Rejected {
                        reason: format!("could not take the configuration: {error}"),
                    },
                }
            };
            write_frame(&mut send, &response).await?;
        }
    }
    send.finish()?;
    Ok(())
}

pub struct LinkServer {
    router: Router,
    identity: InstallationKey,
    broadcast: Broadcast,
}

impl LinkServer {
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
        let handler = LinkHandler {
            id: identity.id(),
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
        .await
        .map_err(|error| format!("UDP task bind at {listen}: {error}"))?;
        let router = Router::builder(endpoint).accept(LINK_ALPN, handler).spawn();
        // Announced on the same address, over TCP, so that a client which has
        // only been told where this server is can still find out who it is.
        let broadcast = Broadcast::start(
            &router.endpoint().bound_sockets(),
            Announcement {
                protocol: String::from_utf8_lossy(LINK_ALPN).into_owned(),
                id: identity.id().to_string(),
                label: label.clone(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                state: RemoteState::Preparing,
                trackers,
            },
            state,
        )
        .await?;
        // Discovery is a convenience; a network that refuses mDNS does not stop
        // the remote from being reachable by the address an operator types.
        if let Err(error) = discovery::advertise(router.endpoint(), &label) {
            tracing::warn!(
                %error,
                "mDNS discovery is unavailable; add this remote by its address instead"
            );
        }
        Ok(Self {
            router,
            identity,
            broadcast,
        })
    }

    pub fn id(&self) -> EndpointId {
        self.identity.id()
    }

    pub fn ticket(&self) -> LinkResult<RemoteTicket> {
        RemoteTicket::new(self.addr())
    }

    fn addr(&self) -> EndpointAddr {
        self.router.endpoint().addr()
    }

    pub async fn shutdown(self) -> LinkResult<()> {
        self.broadcast.stop();
        self.router.shutdown().await?;
        Ok(())
    }
}
