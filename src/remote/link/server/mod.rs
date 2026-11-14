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
use tokio::{
    fs as async_fs,
    io::{self as tokio_io, AsyncWriteExt},
    net::UnixStream,
    sync::{Mutex as AsyncMutex, mpsc::UnboundedReceiver, watch},
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{
    EndpointAddr, EndpointId,
    endpoint::{Builder, Connection, RecvStream, SendStream},
};

use crate::config::{Settings, attachments_dir};
use crate::task::ProjectMemoryFile;
use crate::task::memory;
use crate::task::picture;

use crate::remote::host::clients::{ClientLog, ClientRecord};
use crate::remote::layers::{LAYER_ALPN, LayerHandler, LayerStore};
use crate::task::TaskAsk;

use crate::remote::{
    LinkResult,
    announce::{Announcement, Broadcast, Phase, TrackerConnection},
    bundle::{ConfigBundle, HeldConfig, ProfileDigest},
    discovery,
    identity::InstallationKey,
};

use crate::credentials::Service;
use crate::remote::protocol::SignalCommand;
use crate::remote::protocol::{
    BlobHash, Handover, LINK_ALPN, LinkRequest, LinkResponse, MAX_BLOB_BYTES, RemoteTicket,
    TRANSFER_PIECE_BYTES, TaskRequest, TaskSummary, WatchEvent, WorkerReport, builder, read_body,
    read_frame, write_frame,
};
use crate::remote::transfer::{TransferUpdate, Unit};
use crate::tracker::{ACCOUNT_FILE, TrackerAccount, TrackerKind};
use crate::tracker::{claim_account, logins_of, yield_account};

enum ConfigWrite {
    Written {
        forget_logins: bool,
    },
    LinkedElsewhere {
        provider: TrackerKind,
        display_name: String,
    },
}

mod answer;
mod files;
mod hosts;

use answer::answer;
use files::{
    arriving_accounts, is_allowed_profile, remove_missing_profile_files, write_atomically,
};

pub use hosts::{AgentChannelAccess, BoxFuture, ServerParts, TaskHost, Watching, WorkerHost};

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
    workers: Option<Arc<dyn WorkerHost>>,
    agent_channels: Option<AgentChannelAccess>,
}

impl fmt::Debug for LinkHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkHandler").field("id", &self.id).finish()
    }
}

impl ProtocolHandler for LinkHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // A connection carries a conversation, and a conversation is any number of
        // messages: a host's heartbeats, a client's tasks, a host's layers. They are
        // taken one at a time, in the order they arrive, and the connection is the
        // client's to keep.
        loop {
            let Ok((send, recv)) = connection.accept_bi().await else {
                return Ok(());
            };
            // Each message is its own task: a watch runs for as long as the operator
            // watches, and the statements sent beside it are not going to wait for it.
            let handler = self.clone();
            let remote = connection.remote_id();
            tokio::spawn(async move {
                if let Err(err) = answer(&handler, remote, send, recv).await {
                    tracing::warn!(%err, "link stream failed");
                }
            });
        }
    }
}

impl LinkHandler {
    pub(super) fn write_config(
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
                // Named, because an operator on a newer build is looking at the
                // other end of this: the profile is what they have to act on.
                return Err(io::Error::other(format!(
                    "this machine knows no credential {:?}",
                    profile.profile
                )));
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
        // A project's memory arrives before any of its tasks do, so a machine just
        // given a project starts from what earlier tasks elsewhere knew.
        for pushed in &bundle.memory {
            if pushed.project_id.parse::<uuid::Uuid>().is_err() {
                continue;
            }
            let directory = operator_dir
                .join("projects")
                .join(&pushed.project_id)
                .join("memory");
            let files: Vec<ProjectMemoryFile> = pushed
                .files
                .iter()
                .map(|file| ProjectMemoryFile {
                    path: file.path.clone(),
                    contents: file.contents.clone(),
                    at_ms: file.at_ms,
                })
                .collect();
            if let Err(error) = memory::write(&directory, &files) {
                tracing::warn!(%error, "could not write a project's memory from a configuration");
            }
        }
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

    pub(super) fn held_profiles(&self, peer: EndpointId) -> Vec<ProfileDigest> {
        HeldConfig::load(&self.operator_dir(peer).join("profiles.json")).profiles
    }
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
            phase,
            trackers,
            tasks,
            workers,
            agent_channels,
            layers,
        } = parts;
        let handler = LinkHandler {
            id: identity.id(),
            data_dir,
            config_lock: Arc::new(AsyncMutex::new(())),
            tasks,
            workers,
            agent_channels,
        };
        let log = ClientLog {
            record: clients.clone(),
        };
        let mut alpns = vec![LINK_ALPN.to_vec()];
        if layers.is_some() {
            alpns.push(LAYER_ALPN.to_vec());
        }
        let endpoint = bind(
            builder()
                .secret_key(identity.secret_key().clone())
                .alpns(alpns)
                .hooks(log),
            listen,
        )?
        .bind()
        .await
        .map_err(|error| format!("UDP task bind at {listen}: {error}"))?;
        let mut router = Router::builder(endpoint).accept(LINK_ALPN, handler);
        if let Some(layers) = layers {
            router = router.accept(LAYER_ALPN, LayerHandler::new(layers));
        }
        let router = router.spawn();
        // Announced on the same address, over TCP, so that a client which has
        // only been told where this server is can still find out who it is.
        let broadcast = Broadcast::start(
            &router.endpoint().bound_sockets(),
            Announcement {
                protocol: String::from_utf8_lossy(LINK_ALPN).into_owned(),
                id: identity.id().to_string(),
                label: label.clone(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                state: Phase::Runtime.state(),
                phase: Some(Phase::Runtime),
                trackers,
            },
            phase,
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
