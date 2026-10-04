//! Reaching a node from a client.
//!
//! Nodes are dialled by public key. The client↔node path needs no inbound port
//! and no public URL; tracker ingress is a separate concern. See
//! `design/decisions.md` (Q8) for the reasoning.
//!
//! This is a spike of the transport only: an identity, a serve side, a dial
//! side, and one blob path. It is deliberately not wired into the task layer,
//! it holds blobs in memory, and it reconnects per operation. Run
//! `cargo run --example iroh_link` to see both sides work locally.

#![allow(dead_code)] // Remove once the link is wired into the app.

use std::{
    collections::{BTreeSet, HashMap},
    error::Error,
    fmt, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, SecretKey,
    endpoint::{
        AfterHandshakeOutcome, Builder, Connection, ConnectionError, EndpointHooks, RecvStream,
        SendStream, VarInt, presets,
    },
    protocol::{AcceptError, ProtocolHandler, Router},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::timeout,
};

/// The one ALPN this protocol speaks. Two endpoints that do not agree on this
/// string never complete a handshake.
pub const LINK_ALPN: &[u8] = b"altitude/link/1";

/// Boxed errors: a typed error enum would be speculation until a caller exists.
pub type LinkError = Box<dyn Error + Send + Sync>;
/// The module's result alias.
pub type LinkResult<T> = Result<T, LinkError>;

/// Upper bound on a framed message. Requests are small; blob bytes travel
/// outside the frame.
const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Upper bound on a single blob, so a peer cannot make the node allocate
/// without limit. The capsule sizes this has to carry are not yet known.
const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;
/// Write granularity for blob bytes.
const CHUNK_BYTES: usize = 64 * 1024;
/// Close code sent to a client the node will not talk to. Distinguishable from
/// a transport error, so a client can tell "refused" from "unreachable".
const REFUSED: VarInt = VarInt::from_u32(1);

// ------------------------------------------------------------------ identity

/// A node's transport identity: an Ed25519 keypair on disk.
///
/// The caller supplies the path. There is no default location and no global
/// state, so a test, an example and the app can each hold their own.
pub struct NodeIdentity {
    secret_key: SecretKey,
    path: PathBuf,
}

impl NodeIdentity {
    /// Loads the keypair at `path`, generating and writing one if absent.
    ///
    /// The file holds the 32 secret bytes, base64, on one line. On unix it is
    /// created with mode 0600.
    pub fn load_or_create(path: impl Into<PathBuf>) -> LinkResult<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let secret_key = match fs::read_to_string(&path) {
            Ok(text) => parse_secret(&text)?,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let secret_key = SecretKey::generate();
                write_secret(&path, &secret_key)?;
                secret_key
            }
            Err(err) => return Err(err.into()),
        };
        Ok(Self { secret_key, path })
    }

    /// The public key other peers dial.
    pub fn id(&self) -> EndpointId {
        self.secret_key.public()
    }

    /// The key itself, for building an endpoint.
    pub fn secret_key(&self) -> &SecretKey {
        &self.secret_key
    }

    /// Where the key is stored.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn parse_secret(text: &str) -> LinkResult<SecretKey> {
    let decoded = BASE64.decode(text.trim())?;
    let bytes: [u8; 32] = decoded
        .try_into()
        .map_err(|_| "secret key file must hold exactly 32 bytes")?;
    Ok(SecretKey::from_bytes(&bytes))
}

fn write_secret(path: &Path, secret_key: &SecretKey) -> LinkResult<()> {
    let text = format!("{}\n", BASE64.encode(secret_key.to_bytes()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // `create_new` also means two processes racing to create the same
        // identity cannot silently overwrite each other's key.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(text.as_bytes())?;
    }
    #[cfg(not(unix))]
    fs::write(path, text)?;
    Ok(())
}

// ------------------------------------------------------------------- pairing

/// How an endpoint finds and reaches peers.
///
/// `presets::N0` is Iroh's default wiring: n0's relays, plus n0's DNS to
/// publish and resolve addresses. It makes a bare public key dialable from
/// anywhere, and it puts n0's servers in the addressing path. `presets::Minimal`
/// is the empty base — no relay, no lookup — so an endpoint reaches only what a
/// [`NodeTicket`] told it about.
///
/// Which one Altitude ships is a real decision, not a default to inherit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discovery {
    /// n0's relay servers and DNS.
    Directory,
    /// No relay and no directory: only what a ticket carries.
    TicketOnly,
}

/// The builder a wiring choice implies.
fn endpoint(wiring: Discovery) -> Builder {
    match wiring {
        Discovery::Directory => Endpoint::builder(presets::N0),
        Discovery::TicketOnly => Endpoint::builder(presets::Minimal),
    }
}

/// Who a node will talk to.
///
/// The transport authenticates *which* key is on the other end; it has no
/// opinion on whether that key is welcome. That is this, and it belongs to the
/// caller: a node starts paired with exactly the keys it was given and nobody
/// else. There is no "open" mode, because a shared node that accepts anyone is
/// not a mode worth having one line of code for.
///
/// Two constructors, because both are real. [`NodeAccess::in_memory`] forgets on
/// restart and is what a test or an example wants;
/// [`NodeAccess::open`] persists to a path the caller supplies, with no default
/// location, so where a node keeps its grants is the caller's decision and not
/// this module's.
#[derive(Clone, Debug, Default)]
pub struct NodeAccess {
    /// Where the grants are kept, if anywhere. `None` means in memory only.
    path: Option<PathBuf>,
    /// Kept sorted so the file has a stable order and diffs stay readable.
    paired: Arc<RwLock<BTreeSet<EndpointId>>>,
}

impl NodeAccess {
    /// A node paired with exactly these keys, forgotten on restart.
    pub fn in_memory(keys: impl IntoIterator<Item = EndpointId>) -> Self {
        Self {
            path: None,
            paired: Arc::new(RwLock::new(keys.into_iter().collect())),
        }
    }

    /// A node whose grants live at `path`.
    ///
    /// A missing file means nobody is paired, and nothing is written until the
    /// first pairing — starting a node does not scatter empty files around.
    pub fn open(path: impl Into<PathBuf>) -> LinkResult<Self> {
        let path = path.into();
        let paired = match fs::read_to_string(&path) {
            Ok(text) => parse_grants(&text)?,
            Err(err) if err.kind() == ErrorKind::NotFound => BTreeSet::new(),
            Err(err) => return Err(err.into()),
        };
        Ok(Self {
            path: Some(path),
            paired: Arc::new(RwLock::new(paired)),
        })
    }

    /// Where the grants are kept, or `None` if this access is in memory only.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Pairs a client. This is the out-of-band gesture: the operator has to
    /// carry the client's public key to the node for this to happen.
    ///
    /// Persisted before returning, so a grant that was reported as made is a
    /// grant that survives a restart.
    pub fn pair(&self, key: EndpointId) -> LinkResult<()> {
        let mut paired = self.paired.write().expect("node access");
        paired.insert(key);
        self.persist(&paired)
    }

    /// Unpairs a client. `true` if it was paired.
    ///
    /// Refuses new connections only. An exchange already in flight finishes,
    /// per `design/decisions.md` (Q7).
    pub fn unpair(&self, key: &EndpointId) -> LinkResult<bool> {
        let mut paired = self.paired.write().expect("node access");
        let removed = paired.remove(key);
        if removed {
            self.persist(&paired)?;
        }
        Ok(removed)
    }

    /// Whether a key may connect.
    pub fn is_paired(&self, key: &EndpointId) -> bool {
        self.paired.read().expect("node access").contains(key)
    }

    /// Every paired key, in a stable order.
    pub fn paired_keys(&self) -> Vec<EndpointId> {
        self.paired
            .read()
            .expect("node access")
            .iter()
            .copied()
            .collect()
    }

    /// Writes the grants. Only public keys, never a secret, so this file is a
    /// list of who is allowed rather than a credential itself.
    fn persist(&self, paired: &BTreeSet<EndpointId>) -> LinkResult<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let keys: Vec<String> = paired.iter().map(EndpointId::to_string).collect();
        let text = format!("{}\n", serde_json::to_string_pretty(&keys)?);
        write_private(path, &text)
    }
}

fn parse_grants(text: &str) -> LinkResult<BTreeSet<EndpointId>> {
    let keys: Vec<String> = serde_json::from_str(text)?;
    keys.iter()
        .map(|key| {
            EndpointId::from_str(key)
                .map_err(|err| format!("grant file holds an invalid public key: {err}").into())
        })
        .collect()
}

/// Writes a file only the owner can read, replaced atomically.
///
/// The rename means a reader sees either the old grants or the new ones, never
/// a half-written list, and it carries the temporary file's mode across.
fn write_private(path: &Path, text: &str) -> LinkResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_file_name(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> LinkResult<()> {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        fs::write(&temporary, text)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// The gate itself, installed as an endpoint hook so it runs once the handshake
/// has established which key is on the other end, and before any ALPN handler
/// reads a byte.
#[derive(Debug)]
struct PairingGate {
    access: NodeAccess,
}

impl EndpointHooks for PairingGate {
    async fn after_handshake(&self, connection: &Connection) -> AfterHandshakeOutcome {
        // Only incoming connections: the gate is about who may call in, and a
        // node that later dials out must not lock itself out.
        if !connection.side().is_server() {
            return AfterHandshakeOutcome::accept();
        }
        let remote = connection.remote_id();
        if self.access.is_paired(&remote) {
            return AfterHandshakeOutcome::accept();
        }
        tracing::info!(%remote, "refused an unpaired client");
        AfterHandshakeOutcome::Reject {
            error_code: REFUSED,
            reason: b"not paired with this node".to_vec(),
        }
    }
}

/// A node's address, as a string to carry out of band.
///
/// This is the whole reason to have it: `presets::N0` resolves a bare public
/// key through n0's DNS and publishes the node's addresses through n0's relays,
/// which puts a third party in the addressing path. A ticket carries the key
/// and its addresses in the operator's own hands instead, so dialling a node
/// needs nobody's directory. The cost is that the string has to be moved
/// between machines out of band.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeTicket {
    addr: EndpointAddr,
    text: String,
}

impl NodeTicket {
    /// Renders an address as a ticket.
    pub fn new(addr: EndpointAddr) -> LinkResult<Self> {
        let text = serde_json::to_string(&addr)?;
        Ok(Self { addr, text })
    }

    /// The address it carries.
    pub fn addr(&self) -> &EndpointAddr {
        &self.addr
    }

    /// The key to dial, without the network addresses.
    pub fn id(&self) -> EndpointId {
        self.addr.id
    }

    /// The ticket text.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl fmt::Display for NodeTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl FromStr for NodeTicket {
    type Err = LinkError;

    fn from_str(text: &str) -> LinkResult<Self> {
        let addr: EndpointAddr = serde_json::from_str(text.trim())?;
        Self::new(addr)
    }
}

impl From<NodeTicket> for EndpointAddr {
    fn from(ticket: NodeTicket) -> Self {
        ticket.addr
    }
}

// ------------------------------------------------------------------ protocol

/// A BLAKE3 digest, as lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlobHash(String);

impl BlobHash {
    /// Hashes a buffer.
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    /// The digest as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One request. A blob's bytes follow the frame on the same stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LinkRequest {
    /// Ask the node who it is. The answer is authenticated by the handshake,
    /// so it is really a check that both sides agree on the key.
    Identify,
    /// Announce an upload: `len` raw bytes follow, whose digest must be `hash`.
    PutBlob { hash: BlobHash, len: u64 },
}

/// The node's answer to a [`LinkRequest`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LinkResponse {
    /// Who the node is.
    Identified { node: String },
    /// The upload arrived and its digest matched.
    Stored { hash: BlobHash, len: u64 },
    /// The request was understood and refused.
    Rejected { reason: String },
}

/// A framed message: a big-endian u32 length, then that many bytes of JSON.
async fn write_frame<W, T>(writer: &mut W, value: &T) -> LinkResult<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(value)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(format!("frame of {} bytes is too large", body.len()).into());
    }
    writer.write_all(&(body.len() as u32).to_be_bytes()).await?;
    writer.write_all(&body).await?;
    Ok(())
}

async fn read_frame<R, T>(reader: &mut R) -> LinkResult<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut length = [0u8; 4];
    reader.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(format!("frame of {length} bytes is too large").into());
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Reads exactly `len` bytes, refusing anything above `max` before allocating.
async fn read_body<R>(reader: &mut R, len: u64, max: u64) -> LinkResult<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    if len > max {
        return Err(format!("{len} bytes exceeds the {max} byte limit").into());
    }
    let mut bytes = vec![0u8; len as usize];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

// --------------------------------------------------------------- the node side

/// Blobs a node currently holds, keyed by digest.
///
/// In memory on purpose: this spike does not touch the capsule store, and a
/// restart loses them. See `design/decisions.md` (Q4, Q15).
#[derive(Default)]
struct BlobStore(Mutex<HashMap<BlobHash, Vec<u8>>>);

impl BlobStore {
    fn put(&self, hash: BlobHash, bytes: Vec<u8>) {
        let mut store = self.0.lock().expect("blob store");
        store.insert(hash, bytes);
    }

    fn count(&self) -> usize {
        let store = self.0.lock().expect("blob store");
        store.len()
    }
}

#[derive(Clone)]
struct LinkHandler {
    node: EndpointId,
    blobs: Arc<BlobStore>,
}

impl fmt::Debug for LinkHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately does not print the blobs: they are payload, not identity.
        f.debug_struct("LinkHandler")
            .field("node", &self.node)
            .finish()
    }
}

impl ProtocolHandler for LinkHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // One request per connection. Reuse is a later concern; keeping the
        // exchange bounded makes the spike easy to reason about.
        let Ok((send, recv)) = connection.accept_bi().await else {
            return Ok(());
        };
        if let Err(err) = answer(self, send, recv).await {
            tracing::warn!(%err, "link stream failed");
        }
        connection.closed().await;
        Ok(())
    }
}

async fn answer(
    handler: &LinkHandler,
    mut send: SendStream,
    mut recv: RecvStream,
) -> LinkResult<()> {
    let request: LinkRequest = read_frame(&mut recv).await?;
    match request {
        LinkRequest::Identify => {
            let response = LinkResponse::Identified {
                node: handler.node.to_string(),
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

/// The node side of the link: an endpoint that accepts connections and the
/// blobs it holds.
pub struct LinkNode {
    router: Router,
    identity: NodeIdentity,
    access: NodeAccess,
    blobs: Arc<BlobStore>,
}

impl LinkNode {
    /// Binds an endpoint for `identity`, pairs it with exactly the keys in
    /// `access`, and starts accepting link connections.
    pub async fn serve(
        identity: NodeIdentity,
        access: NodeAccess,
        discovery: Discovery,
    ) -> LinkResult<Self> {
        let blobs = Arc::new(BlobStore::default());
        let handler = LinkHandler {
            node: identity.id(),
            blobs: Arc::clone(&blobs),
        };
        let gate = PairingGate {
            access: access.clone(),
        };
        let endpoint = endpoint(discovery)
            .secret_key(identity.secret_key().clone())
            .alpns(vec![LINK_ALPN.to_vec()])
            .hooks(gate)
            .bind()
            .await?;
        let router = Router::builder(endpoint).accept(LINK_ALPN, handler).spawn();
        Ok(Self {
            router,
            identity,
            access,
            blobs,
        })
    }

    /// The public key a client dials.
    pub fn id(&self) -> EndpointId {
        self.identity.id()
    }

    /// Who this node will talk to. Pairing and unpairing while running take
    /// effect on the next connection.
    pub fn access(&self) -> &NodeAccess {
        &self.access
    }

    /// The node's address as a string to hand to a client out of band.
    pub fn ticket(&self) -> LinkResult<NodeTicket> {
        NodeTicket::new(self.addr())
    }

    /// How to reach the node right now: its key plus whatever addresses are
    /// currently known. Dialling needs only the key if address discovery can
    /// resolve it; the addresses are a shortcut.
    pub fn addr(&self) -> EndpointAddr {
        self.router.endpoint().addr()
    }

    /// Waits up to `limit` for a relay handshake, which is what makes a bare
    /// public key dialable from elsewhere. `false` means the node is only
    /// reachable on the addresses a ticket carries.
    ///
    /// With [`Discovery::TicketOnly`] there is no relay, so this always times
    /// out — Iroh's `online` pends forever without one, which is why the limit
    /// is not optional here.
    pub async fn wait_for_relay(&self, limit: Duration) -> bool {
        timeout(limit, self.router.endpoint().online())
            .await
            .is_ok()
    }

    /// How many blobs the node is holding.
    pub fn blob_count(&self) -> usize {
        self.blobs.count()
    }

    /// Stops accepting and closes the endpoint.
    pub async fn shutdown(self) -> LinkResult<()> {
        self.router.shutdown().await?;
        Ok(())
    }
}

// ------------------------------------------------------------- the client side

/// The kind of network path a connection is using.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    /// A direct IP path.
    Direct,
    /// Traffic is going through a relay server.
    Relay,
    /// No path was selected at the time it was observed.
    Unestablished,
}

/// How a connection travelled when the client looked, which is the difference
/// between "hole punching worked" and "we are paying a relay".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkPath {
    /// The path carrying application data.
    pub selected: PathKind,
    /// Whether a direct IP path was open as well. Reading relay with a direct
    /// path open means hole punching succeeded and iroh's deliberately sticky
    /// selector has not moved yet.
    pub direct_open: bool,
}

impl LinkPath {
    /// Observed after an exchange, which gives hole punching a chance to have
    /// replaced the relay path with a direct one.
    fn observe(connection: &Connection) -> Self {
        let paths = connection.paths();
        let selected = match paths.iter().find(|path| path.is_selected()) {
            Some(path) if path.is_relay() => PathKind::Relay,
            Some(_) => PathKind::Direct,
            None => PathKind::Unestablished,
        };
        Self {
            selected,
            direct_open: paths.iter().any(|path| path.is_ip()),
        }
    }
}

impl fmt::Display for LinkPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.selected, self.direct_open) {
            (PathKind::Direct, _) => f.write_str("direct"),
            (PathKind::Relay, true) => f.write_str("relay (direct path also open)"),
            (PathKind::Relay, false) => f.write_str("relay, no direct path"),
            (PathKind::Unestablished, _) => f.write_str("unestablished"),
        }
    }
}

/// A completed exchange, plus what the client could see about the peer.
pub struct LinkReply<T> {
    /// The answer itself.
    pub value: T,
    /// The key that answered. The handshake authenticates this, so it is the
    /// proof that dialling a public key reached that public key.
    pub remote: EndpointId,
    /// How the bytes travelled.
    pub path: LinkPath,
}

/// Why a client could not finish an exchange.
#[derive(Debug)]
pub enum LinkClientError {
    /// The node will not talk to this client. Retrying does not help until the
    /// node has paired this key.
    Refused {
        /// What the node said.
        reason: String,
    },
    /// Anything else: discovery, transport, framing.
    Transport(LinkError),
}

impl fmt::Display for LinkClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { reason } => write!(f, "node refused this client: {reason}"),
            Self::Transport(err) => write!(f, "{err}"),
        }
    }
}

impl Error for LinkClientError {}

/// Reads a node's refusal out of the connection that carried it.
///
/// A refusal arrives as an application close with a known code. Without
/// looking for it, the client sees only "connection lost" and cannot tell a
/// node that will never accept it from a network that dropped.
fn refusal(connection: &Connection, err: LinkError) -> LinkClientError {
    match connection.close_reason() {
        Some(ConnectionError::ApplicationClosed(close)) if close.error_code == REFUSED => {
            LinkClientError::Refused {
                reason: String::from_utf8_lossy(&close.reason).into_owned(),
            }
        }
        _ => LinkClientError::Transport(err),
    }
}

/// The client side of the link: an endpoint that dials nodes by public key.
pub struct LinkClient {
    endpoint: Endpoint,
}

impl LinkClient {
    /// Binds a client endpoint. Nothing about a node is needed to do this.
    pub async fn bind(discovery: Discovery) -> LinkResult<Self> {
        let endpoint = endpoint(discovery).bind().await?;
        Ok(Self { endpoint })
    }

    /// The client's own public key.
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Closes the endpoint.
    pub async fn close(self) -> LinkResult<()> {
        self.endpoint.close().await;
        Ok(())
    }

    /// Dials a node and runs one request/response exchange.
    ///
    /// `peer` may be a bare [`EndpointId`] — no URL, no port — in which case
    /// the address has to be discoverable — a [`NodeTicket`], or an
    /// [`EndpointAddr`].
    pub async fn request(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &LinkRequest,
    ) -> Result<LinkReply<LinkResponse>, LinkClientError> {
        let connection = self
            .endpoint
            .connect(peer, LINK_ALPN)
            .await
            .map_err(|err| LinkClientError::Transport(err.into()))?;
        let result = async {
            let (mut send, mut recv) = connection.open_bi().await?;
            write_frame(&mut send, request).await?;
            send.finish()?;
            let value = read_frame(&mut recv).await?;
            Ok(LinkReply {
                value,
                remote: connection.remote_id(),
                path: LinkPath::observe(&connection),
            })
        }
        .await;
        // Classified before the local close, which would overwrite the reason.
        let result = result.map_err(|err| refusal(&connection, err));
        connection.close(0u32.into(), b"done");
        result
    }

    /// Sends a blob and returns the digest the node confirmed it received.
    ///
    /// The client hashes what it sends; the node hashes what it receives and
    /// answers with the digest only when the two agree.
    pub async fn put_blob(
        &self,
        peer: impl Into<EndpointAddr>,
        bytes: &[u8],
    ) -> Result<LinkReply<BlobHash>, LinkClientError> {
        let hash = BlobHash::of(bytes);
        let request = LinkRequest::PutBlob {
            hash: hash.clone(),
            len: bytes.len() as u64,
        };
        let connection = self
            .endpoint
            .connect(peer, LINK_ALPN)
            .await
            .map_err(|err| LinkClientError::Transport(err.into()))?;
        let result = async {
            let (mut send, mut recv) = connection.open_bi().await?;
            write_frame(&mut send, &request).await?;
            for chunk in bytes.chunks(CHUNK_BYTES) {
                send.write_all(chunk).await?;
            }
            send.finish()?;
            let response: LinkResponse = read_frame(&mut recv).await?;
            let value = match response {
                LinkResponse::Stored {
                    hash: confirmed, ..
                } if confirmed == hash => confirmed,
                LinkResponse::Rejected { reason } => return Err(reason.into()),
                other => return Err(format!("unexpected answer: {other:?}").into()),
            };
            Ok(LinkReply {
                value,
                remote: connection.remote_id(),
                path: LinkPath::observe(&connection),
            })
        }
        .await;
        // Classified before the local close, which would overwrite the reason.
        let result = result.map_err(|err| refusal(&connection, err));
        connection.close(0u32.into(), b"done");
        result
    }
}
