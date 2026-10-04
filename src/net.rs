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
    collections::HashMap,
    error::Error,
    fmt, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, SecretKey,
    endpoint::{Connection, RecvStream, SendStream, presets},
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
    blobs: Arc<BlobStore>,
}

impl LinkNode {
    /// Binds an endpoint for `identity` and starts accepting link connections.
    pub async fn serve(identity: NodeIdentity) -> LinkResult<Self> {
        let blobs = Arc::new(BlobStore::default());
        let handler = LinkHandler {
            node: identity.id(),
            blobs: Arc::clone(&blobs),
        };
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(identity.secret_key().clone())
            .alpns(vec![LINK_ALPN.to_vec()])
            .bind()
            .await?;
        let router = Router::builder(endpoint).accept(LINK_ALPN, handler).spawn();
        Ok(Self {
            router,
            identity,
            blobs,
        })
    }

    /// The public key a client dials.
    pub fn id(&self) -> EndpointId {
        self.identity.id()
    }

    /// How to reach the node right now: its key plus whatever addresses are
    /// currently known. Dialling needs only the key if address discovery can
    /// resolve it; the addresses are a shortcut.
    pub fn addr(&self) -> EndpointAddr {
        self.router.endpoint().addr()
    }

    /// Waits up to `limit` for a relay handshake. `false` means the node is
    /// only reachable on its direct addresses, which is enough locally and
    /// nowhere else.
    pub async fn wait_online(&self, limit: Duration) -> bool {
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

/// The client side of the link: an endpoint that dials nodes by public key.
pub struct LinkClient {
    endpoint: Endpoint,
}

impl LinkClient {
    /// Binds a client endpoint. Nothing about a node is needed to do this.
    pub async fn bind() -> LinkResult<Self> {
        let endpoint = Endpoint::bind(presets::N0).await?;
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
    /// the address has to be discoverable.
    pub async fn request(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &LinkRequest,
    ) -> LinkResult<LinkReply<LinkResponse>> {
        let connection = self.endpoint.connect(peer, LINK_ALPN).await?;
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
    ) -> LinkResult<LinkReply<BlobHash>> {
        let hash = BlobHash::of(bytes);
        let request = LinkRequest::PutBlob {
            hash: hash.clone(),
            len: bytes.len() as u64,
        };
        let connection = self.endpoint.connect(peer, LINK_ALPN).await?;
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
        connection.close(0u32.into(), b"done");
        result
    }
}
