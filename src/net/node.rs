//! The node side: accept a connection, answer one request, hold the blobs.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use iroh::{
    EndpointAddr, EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use tokio::time::timeout;

use super::{
    LinkResult,
    access::{NodeAccess, PairingGate},
    identity::NodeIdentity,
    protocol::{
        BlobHash, Discovery, LINK_ALPN, LinkRequest, LinkResponse, MAX_BLOB_BYTES, NodeTicket,
        endpoint, read_body, read_frame, write_frame,
    },
};

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
    /// currently known. Not public: a caller wants the ticket, which is the
    /// address rendered as something it can carry.
    fn addr(&self) -> EndpointAddr {
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
