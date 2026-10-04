//! The wire contract, and the vocabulary both sides share.
//!
//! Frames are a big-endian u32 length followed by that many bytes of JSON. Blob
//! bytes never travel inside a frame; a frame announces them and `len` raw bytes
//! follow on the same stream.
//!
//! This is also where the two things both sides need to agree on live: how an
//! endpoint is wired ([`Discovery`]), and how a peer is addressed ([`NodeTicket`]).

use std::{fmt, str::FromStr};

use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{Builder, VarInt, presets},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{LinkError, LinkResult};

/// The one ALPN this protocol speaks. Two endpoints that do not agree on this
/// string never complete a handshake.
pub const LINK_ALPN: &[u8] = b"altitude/link/1";

/// Upper bound on a framed message. Requests are small; blob bytes travel
/// outside the frame.
const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Upper bound on a single blob, so a peer cannot make the node allocate
/// without limit. The capsule sizes this has to carry are not yet known.
pub(super) const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;
/// Write granularity for blob bytes.
pub(super) const CHUNK_BYTES: usize = 64 * 1024;
/// Close code sent to a client the node will not talk to. Distinguishable from
/// a transport error, so a client can tell "refused" from "unreachable".
pub(super) const REFUSED: VarInt = VarInt::from_u32(1);

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
pub(super) fn endpoint(wiring: Discovery) -> Builder {
    match wiring {
        Discovery::Directory => Endpoint::builder(presets::N0),
        Discovery::TicketOnly => Endpoint::builder(presets::Minimal),
    }
}

// ------------------------------------------------------------------- the wire

/// A BLAKE3 digest, as lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlobHash(String);

impl BlobHash {
    /// Hashes a buffer.
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
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
pub(super) async fn write_frame<W, T>(writer: &mut W, value: &T) -> LinkResult<()>
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

pub(super) async fn read_frame<R, T>(reader: &mut R) -> LinkResult<T>
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
pub(super) async fn read_body<R>(reader: &mut R, len: u64, max: u64) -> LinkResult<Vec<u8>>
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

    /// The key to dial, without the network addresses.
    pub fn id(&self) -> EndpointId {
        self.addr.id
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
