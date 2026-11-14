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
    sync::{broadcast, mpsc},
    task::JoinHandle,
    time::sleep,
};

use super::{VsockRoute, private_directory};

pub(crate) const PORT_RELAY_GUEST_PORT: u32 = 5001;

// In the guest's own home, not /usr/local/bin: on a host the sandbox is an unprivileged
// account's home and namespace, so the machine's /usr/local/bin is not the sandbox's to
// write, and a setup that tried failed before the task had started.
const HELPER: &str = "/root/.altitude/altitude-port-relay.py";

pub(crate) fn install_script() -> String {
    format!(
        "mkdir -p /root/.altitude\ncat > {HELPER} <<'ALTITUDE_PORTS'\n{}ALTITUDE_PORTS\nchmod 0755 {HELPER}\n",
        include_str!("assets/port-relay.py")
    )
}

pub(crate) fn with_port_relay(command: &str) -> String {
    format!(
        "if command -v python3 >/dev/null 2>&1; then\n\
         \tpython3 {HELPER} </dev/null >/tmp/altitude-ports.log 2>&1 &\n\
         fi\n{command}"
    )
}

const CLIENT_PATIENCE: Duration = Duration::from_secs(10);

const FIRST_FORWARDED_PORT: u16 = 1024;

#[derive(Clone, Debug)]
pub struct PortNotice {
    pub sandbox: String,
    pub port: u16,
}

#[derive(Clone)]
pub struct PortRelays {
    directory: PathBuf,
    relays: Arc<Mutex<HashMap<String, Relay>>>,
    notices: broadcast::Sender<PortNotice>,
}

struct Relay {
    path: PathBuf,
    state: Arc<Mutex<RelayState>>,
    listening: Vec<JoinHandle<()>>,
}

struct RelayState {
    notices: broadcast::Sender<PortNotice>,
    forwarded: HashMap<u16, Forwarded>,
    pending: HashMap<u16, VecDeque<Pending>>,
    refused: HashSet<u16>,
    last_token: u64,
    last_announcement: u64,
}

struct Forwarded {
    announcement: u64,
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
            notices: broadcast::channel(64).0,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PortNotice> {
        self.notices.subscribe()
    }

    pub fn channel(&self, id: &str) -> VsockRoute {
        VsockRoute {
            host_socket: self.directory.join(format!("{id}-ports.sock")),
            guest_port: PORT_RELAY_GUEST_PORT,
        }
    }

    pub fn bind(&self, id: &str) -> io::Result<VsockRoute> {
        self.release(id);
        private_directory(&self.directory).map_err(|error| io::Error::other(error.to_string()))?;
        let route = self.channel(id);
        let path = route.host_socket.clone();
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        let state = Arc::new(Mutex::new(RelayState {
            notices: self.notices.clone(),
            forwarded: HashMap::new(),
            pending: HashMap::new(),
            refused: HashSet::new(),
            last_token: 0,
            last_announcement: 0,
        }));
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
    let notices = {
        let mut state = state.lock().expect("relay state lock poisoned");
        state.refused.remove(&port);
        state.notices.clone()
    };
    let _ = notices.send(PortNotice {
        sandbox: id.clone(),
        port,
    });
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
