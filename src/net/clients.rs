//! Who a node will talk to.
//!
//! The transport authenticates *which* key is on the other end; it has no
//! opinion on whether that key is welcome. That is this.

use std::{
    collections::BTreeSet,
    fs,
    io::ErrorKind,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, RwLock},
};

use iroh::{
    EndpointId,
    endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks},
};

use super::{LinkResult, identity::write_private, protocol::REFUSED};

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

/// The gate itself, installed as an endpoint hook so it runs once the handshake
/// has established which key is on the other end, and before any ALPN handler
/// reads a byte.
#[derive(Debug)]
pub(super) struct PairingGate {
    pub(super) access: NodeAccess,
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
