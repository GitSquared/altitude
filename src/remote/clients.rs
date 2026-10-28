//! Which clients a server has seen.
//!
//! The transport authenticates which key is on the other end; Altitude does not
//! decide which keys are welcome. Reaching the server is the environment's job —
//! the same network, a tailnet, a firewall — and a tool that grew an account
//! system of its own would answer a question the operator has already answered
//! outside it.
//!
//! So this records every key that connects, and the record is a list of public
//! keys, never a secret: it says who has been here, and grants nothing.

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

/// The clients a server has seen.
///
/// The record lives at a path the caller supplies, with no default location, so
/// where a server writes it stays the caller's decision.
#[derive(Clone, Debug)]
pub struct ClientRecord {
    path: PathBuf,
    seen: Arc<RwLock<BTreeSet<EndpointId>>>,
}

impl ClientRecord {
    /// A record kept at `path`.
    ///
    /// A missing file means nobody has connected, and nothing is written until
    /// the first one does — starting a server does not scatter empty files around.
    pub fn open(path: impl Into<PathBuf>) -> LinkResult<Self> {
        let path = path.into();
        let seen = match fs::read_to_string(&path) {
            Ok(text) => parse_record(&text)?,
            Err(err) if err.kind() == ErrorKind::NotFound => BTreeSet::new(),
            Err(err) => return Err(err.into()),
        };
        Ok(Self {
            path,
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

    /// Forget a client, answering whether it was known.
    ///
    /// The record grants nothing, so forgetting one is housekeeping: it is the
    /// list of who has been here that stops naming someone who never will be
    /// again.
    pub fn forget(&self, key: EndpointId) -> LinkResult<bool> {
        let mut seen = self.seen.write().expect("client record");
        if !seen.remove(&key) {
            return Ok(false);
        }
        self.persist(&seen)?;
        Ok(true)
    }

    fn persist(&self, seen: &BTreeSet<EndpointId>) -> LinkResult<()> {
        let keys: Vec<String> = seen.iter().map(EndpointId::to_string).collect();
        let text = format!("{}\n", serde_json::to_string_pretty(&keys)?);
        write_private(&self.path, &text)
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
        // Only incoming connections: a server that later dials out is not a client
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
