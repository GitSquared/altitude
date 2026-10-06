//! Which clients a node has seen.
//!
//! The transport authenticates *which* key is on the other end. Whether that key
//! is welcome is a separate question, and the answer today is "yes, and write it
//! down": a node records the identity of everyone who connects and says so, so
//! that a new one is visible rather than silent. Refusing is not built, because a
//! half-answer to "who may connect" is worse than none — the shape of the answer
//! depends on how identity is delegated, which is not settled.
//!
//! The record is a list of public keys, never a secret, so the file says who has
//! been here rather than granting anything.

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

use super::{LinkResult, identity::write_private};

/// The clients a node has seen.
///
/// Two constructors, because both are real. [`ClientRecord::in_memory`] forgets
/// on restart and is what a test or an example wants; [`ClientRecord::open`]
/// keeps the record at a path the caller supplies, with no default location, so
/// where a node writes it stays the caller's decision.
#[derive(Clone, Debug, Default)]
pub struct ClientRecord {
    /// Where the record is kept, if anywhere. `None` means in memory only.
    path: Option<PathBuf>,
    /// Kept sorted so the file has a stable order and diffs stay readable.
    seen: Arc<RwLock<BTreeSet<EndpointId>>>,
}

impl ClientRecord {
    /// A record that forgets on restart.
    #[allow(dead_code)]
    pub fn in_memory(keys: impl IntoIterator<Item = EndpointId>) -> Self {
        Self {
            path: None,
            seen: Arc::new(RwLock::new(keys.into_iter().collect())),
        }
    }

    /// A record kept at `path`.
    ///
    /// A missing file means nobody has connected, and nothing is written until
    /// the first one does — starting a node does not scatter empty files around.
    pub fn open(path: impl Into<PathBuf>) -> LinkResult<Self> {
        let path = path.into();
        let seen = match fs::read_to_string(&path) {
            Ok(text) => parse_record(&text)?,
            Err(err) if err.kind() == ErrorKind::NotFound => BTreeSet::new(),
            Err(err) => return Err(err.into()),
        };
        Ok(Self {
            path: Some(path),
            seen: Arc::new(RwLock::new(seen)),
        })
    }

    /// Notes a client, answering whether this is the first time.
    ///
    /// Written before returning, so a client that was recorded is a client that
    /// survives a restart.
    pub fn record(&self, key: EndpointId) -> LinkResult<bool> {
        let mut seen = self.seen.write().expect("client record");
        let first = seen.insert(key);
        if first {
            self.persist(&seen)?;
        }
        Ok(first)
    }

    /// Whether a client has been seen before.
    #[allow(dead_code)]
    pub fn has_seen(&self, key: &EndpointId) -> bool {
        self.seen.read().expect("client record").contains(key)
    }

    /// Every client seen, in a stable order.
    #[allow(dead_code)]
    pub fn seen(&self) -> Vec<EndpointId> {
        self.seen
            .read()
            .expect("client record")
            .iter()
            .copied()
            .collect()
    }

    fn persist(&self, seen: &BTreeSet<EndpointId>) -> LinkResult<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let keys: Vec<String> = seen.iter().map(EndpointId::to_string).collect();
        let text = format!("{}\n", serde_json::to_string_pretty(&keys)?);
        write_private(path, &text)
    }
}

fn parse_record(text: &str) -> LinkResult<BTreeSet<EndpointId>> {
    let keys: Vec<String> = serde_json::from_str(text)?;
    keys.iter()
        .map(|key| {
            EndpointId::from_str(key)
                .map_err(|err| format!("client record holds an invalid public key: {err}").into())
        })
        .collect()
}

/// Notes who connects, and lets them in.
///
/// Installed as an endpoint hook so it runs once the handshake has established
/// which key is on the other end, and before any ALPN handler reads a byte.
#[derive(Debug)]
pub(super) struct ClientLog {
    pub(super) record: ClientRecord,
}

impl EndpointHooks for ClientLog {
    async fn after_handshake(&self, connection: &Connection) -> AfterHandshakeOutcome {
        // Only incoming connections: a node that later dials out is not a client
        // of itself.
        if !connection.side().is_server() {
            return AfterHandshakeOutcome::accept();
        }
        let remote = connection.remote_id();
        match self.record.record(remote) {
            Ok(true) => tracing::info!(%remote, "a client connected for the first time"),
            Ok(false) => tracing::debug!(%remote, "a known client connected"),
            Err(error) => tracing::warn!(%remote, %error, "could not record a client"),
        }
        AfterHandshakeOutcome::accept()
    }
}
