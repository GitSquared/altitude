//! The client side: dial a node by key and run one exchange.

use std::{error::Error, fmt};

use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{Connection, ConnectionError},
};

use super::{
    LinkError, LinkResult,
    protocol::{
        BlobHash, CHUNK_BYTES, LINK_ALPN, LinkRequest, LinkResponse, REFUSED, builder, read_frame,
        write_frame,
    },
};

// ------------------------------------------------------------- the client side

/// The kind of network path a connection is using.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    /// A direct IP path.
    Direct,
    /// Traffic is going through a relay server.
    ///
    /// Unreachable while Altitude configures no relay, and kept because this
    /// mirrors what iroh itself reports rather than what we expect to see.
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
    pub async fn bind() -> LinkResult<Self> {
        let endpoint = builder().bind().await?;
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
