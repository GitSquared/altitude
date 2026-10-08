//! Saying who a server is, to a client that cannot dial it yet.
//!
//! The link dials by public key, so a client needs the key before it can
//! connect. That is the one thing an operator cannot reasonably be asked to type
//! and the one thing that cannot come from the link itself, so the server says it
//! over TCP on the same port number as its UDP. One address to remember, one
//! thing to be told.
//!
//! It speaks HTTP rather than a protocol of its own. `curl` then works, which
//! matters when a server is not answering and someone has to find out why, and a
//! reverse proxy can put TLS in front of it, which authenticates the first
//! sighting instead of trusting it.

use std::{net::SocketAddr, time::Duration};

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::{sleep, timeout},
};

use super::LinkResult;
use super::bundle::{HeldConfig, ProfileStamp};

/// How long either side waits for the other before giving up.
const EXCHANGE_LIMIT: Duration = Duration::from_secs(5);
/// Enough for a request head and then some. A body is never read, because there
/// is nothing to ask for.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// What a server says about itself.
///
/// Nothing here is a secret: the key is public by definition, and everything else
/// is what a person would read off the machine's own console.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Announcement {
    /// The protocol a client must speak to this server, so a client can tell it is
    /// talking to the right thing rather than to whatever else is on the port.
    pub protocol: String,
    /// The public key, which is the thing a client cannot otherwise obtain.
    pub id: String,
    /// How the server reads to a human, e.g. the machine it is on.
    pub label: String,
    /// Which build is answering.
    pub version: String,
    /// What it can do with a task right now.
    pub state: RemoteState,
    /// What it holds for each profile, so a client sends only what is missing rather
    /// than a directory of logins again.
    pub profiles: Vec<ProfileStamp>,
}

/// What a remote can do with a task right now.
///
/// A machine that has just been sent a configuration is building what it needs to
/// run one. Until that finishes it answers, so a client can tell it apart from a
/// machine that is down, and cannot be given work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteState {
    Preparing,
    Ready,
}

/// The listeners that answer announcements.
pub(super) struct Broadcast {
    tasks: Vec<JoinHandle<()>>,
}

impl Broadcast {
    /// Starts answering on the sockets the link itself bound, over TCP.
    ///
    /// TCP and UDP do not collide, so this costs the operator no second address
    /// to remember or to make reachable. Taking the link's own sockets rather
    /// than the address the operator asked for is what keeps the port the same
    /// when they asked for *any* port: an operating system picks a UDP port and
    /// a TCP port independently, so binding the announcement to what was asked
    /// for would quietly put it somewhere else.
    pub(super) async fn start(
        bound: &[SocketAddr],
        announcement: Announcement,
        state: watch::Receiver<RemoteState>,
        held: Arc<Mutex<HeldConfig>>,
    ) -> LinkResult<Self> {
        let mut tasks = Vec::new();
        for address in bound {
            let listener = TcpListener::bind(address).await?;
            tasks.push(tokio::spawn(answer_everyone(
                listener,
                announcement.clone(),
                state.clone(),
                Arc::clone(&held),
            )));
        }
        Ok(Self { tasks })
    }

    /// Stops answering. Dropping a server should not leave a port open.
    pub(super) fn stop(self) {
        for task in self.tasks {
            task.abort();
        }
    }
}

async fn answer_everyone(
    listener: TcpListener,
    announcement: Announcement,
    state: watch::Receiver<RemoteState>,
    held: Arc<Mutex<HeldConfig>>,
) {
    loop {
        match listener.accept().await {
            Ok((mut stream, peer)) => {
                let mut announcement = announcement.clone();
                let state = state.clone();
                let held = Arc::clone(&held);
                tokio::spawn(async move {
                    // Read per request, not once: a remote that has just been sent a
                    // configuration is building, and says so until it is done.
                    announcement.state = *state.borrow();
                    announcement.profiles = held
                        .lock()
                        .map(|held| held.profiles.clone())
                        .unwrap_or_default();
                    if let Err(error) = answer(&mut stream, &announcement).await {
                        tracing::debug!(%peer, %error, "an announcement went unanswered");
                    }
                });
            }
            Err(error) => {
                // A listener that dies because one accept failed would leave the
                // server silently unreachable, so it waits rather than stopping.
                tracing::warn!(%error, "could not accept an announcement request");
                sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn answer(stream: &mut TcpStream, announcement: &Announcement) -> LinkResult<()> {
    // Read the request head and ignore it: there is nothing to ask for, and
    // reading it at all is only so the client does not see a reset.
    let mut request = Vec::new();
    let _ = timeout(EXCHANGE_LIMIT, read_head(stream, &mut request)).await;

    let body = serde_json::to_vec(announcement)?;
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
         content-type: application/json\r\n\
         content-length: {}\r\n\
         connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_head(stream: &mut TcpStream, request: &mut Vec<u8>) -> LinkResult<()> {
    let mut buffer = [0u8; 1024];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n")
            || request.len() >= MAX_REQUEST_BYTES
        {
            return Ok(());
        }
    }
}

/// Asks a server who it is.
///
/// The answer is not authenticated by anything, which is the point: this is the
/// first sighting, and the client pins what it learns here. Everything after it
/// is authenticated by the link's own handshake.
pub async fn ask(host: &str, port: u16) -> LinkResult<Announcement> {
    let mut stream = timeout(EXCHANGE_LIMIT, TcpStream::connect((host, port))).await??;
    let request = format!("GET / HTTP/1.1\r\nhost: {host}:{port}\r\nconnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    timeout(EXCHANGE_LIMIT, stream.read_to_end(&mut response)).await??;
    parse(&response)
}

fn parse(response: &[u8]) -> LinkResult<Announcement> {
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("the server's answer had no body")?;
    let status = String::from_utf8_lossy(&response[..split])
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    if !status.contains(" 200 ") {
        return Err(format!("the server answered {status}").into());
    }
    Ok(serde_json::from_slice(&response[split + 4..])?)
}
