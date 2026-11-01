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

use crate::remote::{LinkResult, identity::write_private};

#[derive(Clone, Debug)]
pub struct ClientRecord {
    path: PathBuf,
    seen: Arc<RwLock<BTreeSet<EndpointId>>>,
}

impl ClientRecord {
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

    pub fn record(&self, key: EndpointId) -> LinkResult<bool> {
        let mut seen = self.seen.write().expect("client record");
        let first = seen.insert(key);
        if first {
            self.persist(&seen)?;
        }
        Ok(first)
    }

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

#[derive(Debug)]
pub(crate) struct ClientLog {
    pub(crate) record: ClientRecord,
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
