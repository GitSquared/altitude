//! This machine's loopback, joined to a sandbox's own.
//!
//! A program run from a sandbox's terminal listens on the sandbox's loopback; a
//! browser on this machine reaches this machine's. Neither is the other, so a link
//! a sandbox opens — an OAuth callback, a dev server — lands where nothing listens.
//!
//! The guest half is `assets/port-relay.py`: it dials a sandbox's relay channel and
//! announces every TCP port it sees listening. This is the host half. It binds each
//! announced number on this machine's loopback when that number is free, and joins
//! every incoming connection to a connection the sandbox dials back over the same
//! channel. A port this machine already uses is left to whatever has it.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs, io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpSocket, TcpStream, UnixListener, UnixStream},
    sync::mpsc,
    task::JoinHandle,
    time::sleep,
};

use super::{VsockRoute, private_directory};

/// The guest port a sandbox's relay helper dials. The other half of this contract
/// is `ALTITUDE_PORTS_PORT` in `assets/port-relay.py`.
pub(crate) const PORT_RELAY_GUEST_PORT: u32 = 5001;

/// Where the helper lives inside a sandbox.
const HELPER: &str = "/usr/local/bin/altitude-port-relay.py";

/// The command that writes the helper into a sandbox.
pub(crate) fn install_script() -> String {
    format!(
        "cat > {HELPER} <<'ALTITUDE_PORTS'\n{}ALTITUDE_PORTS\nchmod 0755 {HELPER}\n",
        include_str!("assets/port-relay.py")
    )
}

/// The command that opens a terminal with the relay helper behind it.
///
/// The helper is the terminal session's child: the setup session is gone before a
/// terminal arrives, and a process left by it goes with it. Its output goes to a
/// log rather than to the terminal the operator is working in.
pub(crate) fn with_helper(command: &str) -> String {
    format!(
        "if command -v python3 >/dev/null 2>&1; then\n\
         \tpython3 {HELPER} </dev/null >/tmp/altitude-ports.log 2>&1 &\n\
         fi\n{command}"
    )
}

/// How long a client waits for the sandbox to answer before it is dropped.
///
/// A sandbox that never dials back would otherwise hold the operator's browser open
/// until the browser gave up on its own, which reads as a hang rather than a refusal.
const CLIENT_PATIENCE: Duration = Duration::from_secs(10);

/// Below this a port is not forwarded: binding it needs privileges this machine may
/// not give, and what listens there is the sandbox's plumbing rather than something
/// an operator started.
const FIRST_FORWARDED_PORT: u16 = 1024;

/// The relay channels this process holds open, by sandbox id.
#[derive(Clone)]
pub struct PortRelays {
    directory: PathBuf,
    relays: Arc<Mutex<HashMap<String, Relay>>>,
}

struct Relay {
    path: PathBuf,
    state: Arc<Mutex<RelayState>>,
    listening: Vec<JoinHandle<()>>,
}

#[derive(Default)]
struct RelayState {
    forwarded: HashMap<u16, Forwarded>,
    /// Clients waiting for the sandbox to dial back, oldest first per port.
    pending: HashMap<u16, VecDeque<Pending>>,
    /// Ports that had no host number to take, so the operator is told once rather
    /// than on every retry.
    refused: HashSet<u16>,
    last_token: u64,
    last_announcement: u64,
}

struct Forwarded {
    /// Which announcement this forwarding answers, so a control connection that
    /// has already been replaced cannot take the live one down with it.
    announcement: u64,
    /// One message per client waiting to be carried.
    orders: mpsc::UnboundedSender<()>,
    tasks: Vec<JoinHandle<()>>,
}

struct Pending {
    token: u64,
    client: TcpStream,
}

impl PortRelays {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            relays: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The relay channel a sandbox is made with.
    ///
    /// A route is fixed when the sandbox is made, so a task's sandbox is given its
    /// channel at provisioning and the host end is bound when a terminal opens.
    pub fn channel(&self, id: &str) -> VsockRoute {
        VsockRoute {
            host_socket: self.directory.join(format!("{id}-ports.sock")),
            guest_port: PORT_RELAY_GUEST_PORT,
        }
    }

    /// Bind a sandbox's end of the relay, and answer with the route it carries.
    ///
    /// It answers with nothing to relay until the guest announces.
    pub fn bind(&self, id: &str) -> io::Result<VsockRoute> {
        self.release(id);
        private_directory(&self.directory).map_err(|error| io::Error::other(error.to_string()))?;
        let route = self.channel(id);
        let path = route.host_socket.clone();
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        let state = Arc::new(Mutex::new(RelayState::default()));
        let mut listening = Vec::new();
        {
            let state = state.clone();
            let id = id.to_owned();
            listening.push(tokio::spawn(async move {
                accept(listener, state, id).await;
            }));
        }
        self.relays.lock().expect("relay lock poisoned").insert(
            id.to_owned(),
            Relay {
                path: path.clone(),
                state,
                listening,
            },
        );
        Ok(route)
    }

    /// Close everything a sandbox's relay opened, including the ports it took.
    pub fn release(&self, id: &str) {
        let relay = self.relays.lock().expect("relay lock poisoned").remove(id);
        let Some(relay) = relay else {
            return;
        };
        for task in relay.listening {
            task.abort();
        }
        let mut state = relay.state.lock().expect("relay state lock poisoned");
        for (_, forwarded) in state.forwarded.drain() {
            for task in forwarded.tasks {
                task.abort();
            }
        }
        state.pending.clear();
        drop(state);
        let _ = fs::remove_file(&relay.path);
    }
}

async fn accept(listener: UnixListener, state: Arc<Mutex<RelayState>>, id: String) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let state = state.clone();
                let id = id.clone();
                tokio::spawn(async move {
                    serve(stream, state, id).await;
                });
            }
            Err(error) => {
                tracing::debug!(%error, sandbox = %id, "could not accept a relay connection");
                return;
            }
        }
    }
}

/// Decide what one connection over the relay channel is for.
///
/// The guest sends `W <port>` to announce a listener and `D <port>` to carry one
/// client. It reads the line a byte at a time so that whatever follows it is still
/// there for the connection that uses it.
async fn serve(mut stream: UnixStream, state: Arc<Mutex<RelayState>>, id: String) {
    let Some(line) = read_line(&mut stream).await else {
        return;
    };
    let mut words = line.split_whitespace();
    let kind = words.next();
    let port = words.next().and_then(|port| port.parse::<u16>().ok());
    match (kind, port) {
        // A relay helper writes `W <port>`.
        (Some("W"), Some(port)) => announce(stream, port, state, id).await,
        // The guest answered an order from this end.
        (Some("D"), Some(port)) => join(stream, port, state).await,
        _ => {}
    }
}

async fn read_line(stream: &mut UnixStream) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 32 {
        match stream.read(&mut byte).await {
            Ok(0) => return None,
            Ok(_) if byte[0] == b'\n' => return String::from_utf8(line).ok(),
            Ok(_) => line.push(byte[0]),
            Err(_) => return None,
        }
    }
    None
}

/// Take a sandbox's listener for as long as it exists here.
async fn announce(mut stream: UnixStream, port: u16, state: Arc<Mutex<RelayState>>, id: String) {
    if port < FIRST_FORWARDED_PORT {
        let _ = stream.write_all(b"R\n").await;
        return;
    }
    // A second announcement replaces the first: taking the number back before
    // binding is what keeps this from calling its own listener taken.
    let announcement = {
        let mut state = state.lock().expect("relay state lock poisoned");
        unforward_locked(&mut state, port, None);
        state.last_announcement += 1;
        state.last_announcement
    };
    let listeners = match bind_loopback(port).await {
        Ok(listeners) => listeners,
        Err(error) => {
            let first = state
                .lock()
                .expect("relay state lock poisoned")
                .refused
                .insert(port);
            if first && error.kind() == io::ErrorKind::AddrInUse {
                tracing::info!(
                    port,
                    sandbox = %id,
                    "a sandbox port has no host number to take; this machine already uses it"
                );
            } else {
                tracing::debug!(port, %error, sandbox = %id, "a sandbox port stays unreachable");
            }
            // The listener is still there; say so, so the guest tries again once
            // this machine's number is free rather than giving up on the relay.
            let _ = stream.write_all(b"R\n").await;
            return;
        }
    };
    state
        .lock()
        .expect("relay state lock poisoned")
        .refused
        .remove(&port);
    tracing::info!(port, sandbox = %id, "a sandbox port is reachable here");

    let (read_half, write_half) = stream.into_split();
    let (orders, mut wanted) = mpsc::unbounded_channel();
    let mut tasks = Vec::new();
    tasks.push(tokio::spawn(async move {
        let mut write_half = write_half;
        while wanted.recv().await.is_some() {
            if write_half.write_all(b"1").await.is_err() {
                break;
            }
        }
        let _ = write_half.shutdown().await;
    }));
    // The sandbox closes the announcement when its listener goes away.
    let leaving = state.clone();
    tasks.push(tokio::spawn(async move {
        let mut read_half = read_half;
        let mut byte = [0u8; 1];
        while let Ok(read) = read_half.read(&mut byte).await {
            if read == 0 {
                break;
            }
        }
        unforward(&leaving, port, Some(announcement));
    }));
    // A second announcement for one port is the same port: the sandbox retries
    // after this end refused it, so the newest announcement is the live one.
    let replacing = state
        .lock()
        .expect("relay state lock poisoned")
        .forwarded
        .insert(
            port,
            Forwarded {
                announcement,
                orders,
                tasks,
            },
        );
    if let Some(replacing) = replacing {
        for task in replacing.tasks {
            task.abort();
        }
    }
    for listener in listeners {
        let accept_state = state.clone();
        let orders = state
            .lock()
            .expect("relay state lock poisoned")
            .forwarded
            .get(&port)
            .map(|forwarded| forwarded.orders.clone());
        let Some(orders) = orders else {
            return;
        };
        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((client, _)) => {
                        let token = {
                            let mut state = accept_state.lock().expect("relay state lock poisoned");
                            state.last_token += 1;
                            let token = state.last_token;
                            state
                                .pending
                                .entry(port)
                                .or_default()
                                .push_back(Pending { token, client });
                            token
                        };
                        if orders.send(()).is_err() {
                            accept_state
                                .lock()
                                .expect("relay state lock poisoned")
                                .take(port, token);
                            break;
                        }
                        let waiting = accept_state.clone();
                        tokio::spawn(async move {
                            sleep(CLIENT_PATIENCE).await;
                            waiting
                                .lock()
                                .expect("relay state lock poisoned")
                                .take(port, token);
                        });
                    }
                    Err(error) => {
                        tracing::debug!(port, %error, "a relayed listener stopped accepting");
                        break;
                    }
                }
            }
        });
        // The task has to live where release can stop it: with the port's own
        // forwarding, which is what a closed announcement takes away.
        if !track(&state, port, task) {
            return;
        }
    }
}

/// Keep a task with the port it serves, or say the port is already gone.
fn track(state: &Arc<Mutex<RelayState>>, port: u16, task: JoinHandle<()>) -> bool {
    let mut state = state.lock().expect("relay state lock poisoned");
    match state.forwarded.get_mut(&port) {
        Some(forwarded) => {
            forwarded.tasks.push(task);
            true
        }
        None => {
            task.abort();
            false
        }
    }
}

/// Join one client on this machine to one connection the sandbox dials back.
async fn join(mut stream: UnixStream, port: u16, state: Arc<Mutex<RelayState>>) {
    let Some(mut client) = state
        .lock()
        .expect("relay state lock poisoned")
        .take_first(port)
    else {
        return;
    };
    if let Err(error) = copy_bidirectional(&mut client, &mut stream).await {
        tracing::debug!(port, %error, "a relayed connection ended badly");
    }
}

fn unforward(state: &Arc<Mutex<RelayState>>, port: u16, announcement: Option<u64>) {
    let mut state = state.lock().expect("relay state lock poisoned");
    unforward_locked(&mut state, port, announcement);
}

fn unforward_locked(state: &mut RelayState, port: u16, announcement: Option<u64>) {
    let current = state
        .forwarded
        .get(&port)
        .map(|forwarded| forwarded.announcement);
    let matches = current
        .is_none_or(|current| announcement.is_none_or(|announcement| announcement == current));
    if !matches {
        return;
    }
    if let Some(forwarded) = state.forwarded.remove(&port) {
        for task in forwarded.tasks {
            task.abort();
        }
    }
    state.pending.remove(&port);
}

impl RelayState {
    fn take_first(&mut self, port: u16) -> Option<TcpStream> {
        let queue = self.pending.get_mut(&port)?;
        let pending = queue.pop_front();
        if queue.is_empty() {
            self.pending.remove(&port);
        }
        pending.map(|pending| pending.client)
    }

    fn take(&mut self, port: u16, token: u64) -> Option<TcpStream> {
        let queue = self.pending.get_mut(&port)?;
        let index = queue.iter().position(|pending| pending.token == token)?;
        let pending = queue.remove(index);
        if queue.is_empty() {
            self.pending.remove(&port);
        }
        pending.map(|pending| pending.client)
    }
}

/// Bind the sandbox's number on both loopbacks, or take nothing at all.
///
/// `localhost` is both families, and one operator asking for a port should not land
/// somewhere different depending on which one their browser picks. Anything already
/// answering there belongs to this machine and is left alone.
async fn bind_loopback(port: u16) -> io::Result<Vec<TcpListener>> {
    let ipv4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    match bind_ipv6(port).await {
        Ok(ipv6) => Ok(vec![ipv4, ipv6]),
        // Something on this machine already answers at this number on the other
        // loopback, and one operator asking `localhost:port` should not land
        // somewhere different depending on which family their browser picks.
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => Err(error),
        // No IPv6 here: the IPv4 loopback is the whole of `localhost`.
        Err(_) => Ok(vec![ipv4]),
    }
}

async fn bind_ipv6(port: u16) -> io::Result<TcpListener> {
    let socket = TcpSocket::new_v6()?;
    // The address is `::1`, so a v4-mapped connection cannot arrive whatever the
    // host's dual-stack default is.
    socket.bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port)))?;
    socket.listen(1024)
}
