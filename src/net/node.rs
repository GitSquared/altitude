//! The node side: accept a connection, answer one request, hold the blobs.

use std::{
    collections::HashMap,
    fmt,
    net::{IpAddr, Ipv6Addr, SocketAddr, UdpSocket},
    sync::{Arc, Mutex},
};

use iroh::{
    EndpointAddr, EndpointId,
    endpoint::{Builder, Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler, Router},
};

use super::{
    LinkResult,
    announce::{Announcement, Broadcast},
    clients::{ClientLog, ClientRecord},
    identity::NodeIdentity,
    protocol::{
        BlobHash, LINK_ALPN, LinkRequest, LinkResponse, MAX_BLOB_BYTES, NodeTicket, builder,
        read_body, read_frame, write_frame,
    },
};

// --------------------------------------------------------------- the node side

/// Binds the operator's address, and the same port on the other family when they
/// asked for every interface.
///
/// A node that answered on only one family would be unreachable from a client
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
    broadcast: Broadcast,
}

impl LinkNode {
    /// Binds an endpoint for `identity` at `listen` and starts accepting link
    /// connections.
    ///
    /// `listen` is the operator's, not the operating system's: a port nobody can
    /// predict is a port nobody can configure a client to reach.
    pub async fn serve(
        identity: NodeIdentity,
        clients: ClientRecord,
        listen: SocketAddr,
        label: String,
    ) -> LinkResult<Self> {
        let blobs = Arc::new(BlobStore::default());
        let handler = LinkHandler {
            node: identity.id(),
            blobs: Arc::clone(&blobs),
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
        // only been told where this node is can still find out who it is.
        let broadcast = Broadcast::start(
            &router.endpoint().bound_sockets(),
            Announcement {
                protocol: String::from_utf8_lossy(LINK_ALPN).into_owned(),
                id: identity.id().to_string(),
                label,
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
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

    /// The node's address as a string to hand to a client out of band.
    pub fn ticket(&self) -> LinkResult<NodeTicket> {
        NodeTicket::new(self.addr())
    }

    /// How to reach the node right now: its key plus whatever addresses are
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
