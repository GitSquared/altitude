//! The socket an agent harness dials to reach Altitude's tools.
//!
//! One per sandbox, serving whatever registry it was bound with: a task's tools for a
//! task's run, and nothing for a credential shell, which has no task for tools to act
//! on. It lives under the cache because a Unix socket path is length-limited, and the
//! data path plus a sandbox id is already at the limit on macOS.

use std::{
    collections::HashMap,
    fs, io,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use tokio::{net::UnixListener, task::JoinHandle};

use crate::{
    agent::{mcp, tools::AgentToolRegistry},
    isolation::private_directory,
};

struct Channel {
    handle: JoinHandle<()>,
    path: PathBuf,
}

#[derive(Clone)]
pub struct AgentChannels {
    directory: PathBuf,
    channels: Arc<Mutex<HashMap<String, Channel>>>,
}

impl AgentChannels {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            channels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Bind the socket `id`'s harness dials, serving `tools`, and answer with it.
    ///
    /// Replacing rather than refusing: a sandbox id is never reused while its socket
    /// is alive, so one already here is a leftover from a run that is gone.
    pub fn bind(&self, id: &str, tools: AgentToolRegistry) -> io::Result<PathBuf> {
        self.release(id);
        private_directory(&self.directory).map_err(|error| io::Error::other(error.to_string()))?;
        let path = self.directory.join(format!("{id}.sock"));
        // A socket file left by a process that is gone cannot be bound over.
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        let listening = id.to_owned();
        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let tools = tools.clone();
                        let session = listening.clone();
                        tokio::spawn(async move {
                            let (reader, writer) = stream.into_split();
                            if let Err(error) = mcp::serve_agent(tools, reader, writer).await {
                                tracing::debug!(%error, sandbox = %session, "an agent session ended");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, sandbox = %listening, "the agent channel stopped listening");
                        break;
                    }
                }
            }
        });
        self.channels
            .lock()
            .expect("agent channel lock poisoned")
            .insert(
                id.to_owned(),
                Channel {
                    handle,
                    path: path.clone(),
                },
            );
        Ok(path)
    }

    pub fn release(&self, id: &str) {
        if let Some(channel) = self
            .channels
            .lock()
            .expect("agent channel lock poisoned")
            .remove(id)
        {
            channel.handle.abort();
            let _ = fs::remove_file(&channel.path);
        }
    }
}
