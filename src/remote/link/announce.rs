use std::{net::SocketAddr, time::Duration};

use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};

use tokio::sync::watch;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::{sleep, timeout},
};

use crate::remote::LinkResult;
use crate::tracker::TrackerKind;

const EXCHANGE_LIMIT: Duration = Duration::from_secs(5);
const MAX_REQUEST_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Announcement {
    pub protocol: String,
    pub id: String,
    pub label: String,
    pub version: String,
    /// Whether this machine can be given work, in the one word every version of
    /// a client reads. It is `phase` said coarsely, for the clients that cannot
    /// read `phase` at all; a client that can says what the machine is doing.
    pub state: RemoteState,
    #[serde(default)]
    pub phase: Option<Phase>,
    pub trackers: Vec<TrackerConnection>,
}

impl Announcement {
    pub fn label_or_address<'a>(&'a self, address: &'a str) -> &'a str {
        let label = self.label.trim();
        if label.is_empty() { address } else { label }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackerReadiness {
    Ready,
    NotInstalled,
    Unavailable,
}

impl TrackerReadiness {
    pub fn said(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NotInstalled => "not installed",
            Self::Unavailable => "cannot sign in",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct TrackerConnection {
    pub provider: TrackerKind,
    pub state: TrackerReadiness,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteState {
    Preparing,
    Ready,
}

/// What a machine is doing while it cannot be given work yet. A first start
/// installs a runtime, pulls the base image every environment sits on, and a
/// configuration it was sent builds one environment per project, all of it
/// minutes of work that no link frame would otherwise show.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Installing what runs sandboxes at all.
    Runtime,
    /// The base image, with how far it has come: the bytes are unknown until the
    /// manifest is read, so both halves may be zero.
    BaseImage {
        downloaded_bytes: u64,
        total_bytes: u64,
    },
    /// The tools the base image's own recipes install.
    BaseTools,
    /// One project's environment, which a first push builds from nothing.
    Project {
        name: String,
    },
    Ready,
}

impl Phase {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub fn state(&self) -> RemoteState {
        if self.is_ready() {
            RemoteState::Ready
        } else {
            RemoteState::Preparing
        }
    }
}

pub(super) struct Broadcast {
    tasks: Vec<JoinHandle<()>>,
}

impl Broadcast {
    pub(super) async fn start(
        bound: &[SocketAddr],
        announcement: Announcement,
        phase: watch::Receiver<Phase>,
    ) -> LinkResult<Self> {
        // Every listener is bound before any is served: a bind that fails half
        // way through must not leave a task running on a port nobody owns.
        let mut listeners = Vec::new();
        for address in bound {
            listeners.push(bind_listener(*address)?);
        }
        let mut tasks = Vec::new();
        for listener in listeners {
            tasks.push(tokio::spawn(answer_everyone(
                listener,
                announcement.clone(),
                phase.clone(),
            )));
        }
        Ok(Self { tasks })
    }

    pub(super) fn stop(self) {
        for task in self.tasks {
            task.abort();
        }
    }
}

fn bind_listener(address: SocketAddr) -> Result<TcpListener, String> {
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )
    .map_err(|error| format!("TCP discovery at {address}: {error}"))?;
    if address.is_ipv6() {
        socket
            .set_only_v6(true)
            .map_err(|error| format!("TCP discovery at {address}: {error}"))?;
    }
    socket
        .set_reuse_address(true)
        .map_err(|error| format!("TCP discovery at {address}: {error}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|error| format!("TCP discovery at {address}: {error}"))?;
    socket
        .bind(&address.into())
        .map_err(|error| format!("TCP discovery at {address}: {error}"))?;
    socket
        .listen(128)
        .map_err(|error| format!("TCP discovery at {address}: {error}"))?;
    TcpListener::from_std(socket.into())
        .map_err(|error| format!("TCP discovery at {address}: {error}"))
}

async fn answer_everyone(
    listener: TcpListener,
    announcement: Announcement,
    phase: watch::Receiver<Phase>,
) {
    loop {
        match listener.accept().await {
            Ok((mut stream, peer)) => {
                let mut announcement = announcement.clone();
                let phase = phase.clone();
                tokio::spawn(async move {
                    // Read per request, not once: a remote that has just been sent a
                    // configuration is building, and says so until it is done. Both
                    // halves come from one borrow, so the coarse word and the phase
                    // can never say different things.
                    let said = phase.borrow().clone();
                    announcement.state = said.state();
                    announcement.phase = Some(said);
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
