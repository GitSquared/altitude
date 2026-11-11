use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use iroh::EndpointId;
use tokio::{process::Command, sync::Notify, time};

use crate::remote::identity::write_private;
use crate::remote::protocol::WorkerReport;
use crate::remote::server::{BoxFuture, WorkerHost};

use super::settings::WorkersSettings;

// The whole infra contract. A script is handed who it is growing for and which
// worker to grow, and nothing about altitude beyond that.
pub(crate) const MASTER_TICKET_ENV: &str = "ALTITUDE_MASTER_TICKET";
pub(crate) const WORKER_TOKEN_ENV: &str = "ALTITUDE_WORKER_TOKEN";
pub(crate) const WORKER_NAME_ENV: &str = "ALTITUDE_WORKER_NAME";

// The cadence the worker says it is alive at, and the silence that means it is
// not: three missed beats. The worker role and this master have to agree on the
// first, which is why it lives here.
pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const WORKER_SILENCE: Duration = HEARTBEAT_INTERVAL.saturating_mul(3);

// How often the fleet compares what is connected with what it was asked to keep.
const SUPERVISE_INTERVAL: Duration = Duration::from_secs(10);

// A provision that cannot land is tried a few times, then left alone for a while:
// an infrastructure outage should not turn into a script running every ten
// seconds forever.
const PROVISION_ATTEMPTS: usize = 3;
const AFTER_FAILURE: Duration = Duration::from_secs(60);

struct ConnectedWorker {
    peer: EndpointId,
    capacity: WorkerReport,
    last_seen: Instant,
}

// What the fleet is carrying: hosts given a sandbox, and the most it may run.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FleetCapacity {
    pub assigned: usize,
    pub ceiling: usize,
}

#[derive(Default)]
struct FleetState {
    connected: HashMap<String, ConnectedWorker>,
    provisioned: HashSet<String>,
    provisioning: HashSet<String>,
    // Boxes a sandbox has been placed on, so a second sandbox is not given the
    // same host.
    assigned: HashSet<String>,
    // Boxes the isolation provider is waiting for. It is raised when a sandbox
    // needs compute and lowered when one ends; nothing raises it yet.
    demand: usize,
    retry_after: Option<Instant>,
    stopping: bool,
}

// A remote's isolation workers and the scripts that make them. It knows a worker
// by the name it was provisioned under, because a restarted worker is a new peer
// and the same host.
pub(crate) struct WorkerFleet {
    settings: WorkersSettings,
    token: String,
    ticket: Mutex<Option<String>>,
    state: Mutex<FleetState>,
    wake: Notify,
}

impl WorkerFleet {
    pub(crate) fn open(settings: WorkersSettings, data_dir: &Path) -> io::Result<Self> {
        let token = load_or_create_token(&data_dir.join("workers.token"))?;
        Ok(Self {
            settings,
            token,
            ticket: Mutex::new(None),
            state: Mutex::new(FleetState::default()),
            wake: Notify::new(),
        })
    }

    // The ticket only exists once the link is listening, and a worker cannot find
    // this machine without it, so the fleet starts after the server does.
    pub(crate) fn start(self: &Arc<Self>, master_ticket: String) {
        *self.ticket.lock().expect("worker fleet lock poisoned") = Some(master_ticket);
        let fleet = Arc::clone(self);
        tokio::spawn(async move { fleet.supervise().await });
    }

    pub(crate) async fn shutdown(self: &Arc<Self>) {
        let names: Vec<String> = {
            let mut state = self.state.lock().expect("worker fleet lock poisoned");
            state.stopping = true;
            state.provisioned.drain().collect()
        };
        self.wake.notify_waiters();
        let mut stopping = Vec::new();
        for name in names {
            let fleet = Arc::clone(self);
            stopping.push(tokio::spawn(async move { fleet.deprovision(&name).await }));
        }
        for task in stopping {
            let _ = task.await;
        }
    }

    // The shared secret a host was provisioned with, sent on every call.
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn peer(&self, name: &str) -> Option<EndpointId> {
        self.state
            .lock()
            .expect("worker fleet lock poisoned")
            .connected
            .get(name)
            .map(|worker| worker.peer)
    }

    // How many hosts are joined, how many are given a sandbox, and how many this
    // remote will ever run at once.
    // How many hosts are given a sandbox and how many this remote will ever run at
    // once.
    pub(crate) fn capacity(&self) -> FleetCapacity {
        let state = self.state.lock().expect("worker fleet lock poisoned");
        FleetCapacity {
            assigned: state.assigned.len(),
            ceiling: self.settings.ceiling(),
        }
    }

    // Provision a host for one sandbox. The fleet spawns toward the demand, and
    // this waits for one that has not already been given a sandbox. A host is not
    // returned to the pool: deprovisioning it destroys it.
    pub(crate) async fn provision_worker(&self) -> Result<String, String> {
        {
            let mut state = self.state.lock().expect("worker fleet lock poisoned");
            state.demand += 1;
        }
        self.wake.notify_waiters();
        let deadline = Instant::now() + Duration::from_secs(self.settings.provision_timeout_secs);
        loop {
            {
                let mut state = self.state.lock().expect("worker fleet lock poisoned");
                if let Some(name) = state
                    .connected
                    .keys()
                    .find(|name| !state.assigned.contains(*name))
                    .cloned()
                {
                    state.assigned.insert(name.clone());
                    return Ok(name);
                }
            }
            if Instant::now() >= deadline {
                let mut state = self.state.lock().expect("worker fleet lock poisoned");
                state.demand = state.demand.saturating_sub(1);
                return Err("no isolation worker joined in time".to_owned());
            }
            tokio::select! {
                _ = time::sleep(Duration::from_millis(250)) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    // A sandbox is over, so its host is destroyed rather than reused.
    pub(crate) async fn deprovision_worker(&self, name: &str) {
        {
            let mut state = self.state.lock().expect("worker fleet lock poisoned");
            state.assigned.remove(name);
            state.provisioned.remove(name);
            state.connected.remove(name);
            state.demand = state.demand.saturating_sub(1);
        }
        self.deprovision(name).await;
        self.wake.notify_waiters();
    }

    fn ticket(&self) -> String {
        self.ticket
            .lock()
            .expect("worker fleet lock poisoned")
            .clone()
            .unwrap_or_default()
    }

    fn script_env(&self, name: &str) -> Vec<(String, String)> {
        vec![
            (MASTER_TICKET_ENV.to_owned(), self.ticket()),
            (WORKER_TOKEN_ENV.to_owned(), self.token.clone()),
            (WORKER_NAME_ENV.to_owned(), name.to_owned()),
        ]
    }

    async fn supervise(self: Arc<Self>) {
        loop {
            let reaped = self.reap();
            for name in reaped {
                let fleet = Arc::clone(&self);
                tokio::spawn(async move { fleet.deprovision(&name).await });
            }
            self.consider_provisioning();
            {
                let state = self.state.lock().expect("worker fleet lock poisoned");
                tracing::debug!(
                    workers = state.connected.len(),
                    provisioning = state.provisioning.len(),
                    memory_mib = state
                        .connected
                        .values()
                        .map(|worker| worker.capacity.memory_mib)
                        .sum::<u64>(),
                    "isolation worker capacity"
                );
                if state.stopping {
                    break;
                }
            }
            tokio::select! {
                _ = time::sleep(SUPERVISE_INTERVAL) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    // A worker this machine made is this machine's to remove, however it left:
    // only the ones that are still here can be deprovisioned meaningfully, but the
    // deprovision script is the place that knows whether a host is already gone.
    fn reap(&self) -> Vec<String> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("worker fleet lock poisoned");
        let mut gone = Vec::new();
        for (name, worker) in &state.connected {
            if now.duration_since(worker.last_seen) > WORKER_SILENCE {
                gone.push(name.clone());
            }
        }
        for name in &gone {
            state.connected.remove(name);
            state.provisioned.remove(name);
            tracing::warn!(worker = %name, "an isolation worker stopped answering");
        }
        gone
    }

    fn consider_provisioning(self: &Arc<Self>) {
        let mut state = self.state.lock().expect("worker fleet lock poisoned");
        if state.stopping {
            return;
        }
        if state
            .retry_after
            .is_some_and(|retry_after| Instant::now() < retry_after)
        {
            return;
        }
        let desired = state.demand.min(self.settings.ceiling());
        let mut current = state.connected.len() + state.provisioning.len();
        while current < desired {
            let name = worker_name();
            state.provisioning.insert(name.clone());
            current += 1;
            let fleet = Arc::clone(self);
            tokio::spawn(async move { fleet.provision(name).await });
        }
    }

    async fn provision(self: Arc<Self>, name: String) {
        let env = self.script_env(&name);
        for attempt in 1..=PROVISION_ATTEMPTS {
            match run_script(
                &self.settings.provision,
                &env,
                Duration::from_secs(self.settings.provision_timeout_secs),
            )
            .await
            {
                Ok(()) => {
                    if self.wait_until_joined(&name).await {
                        let stopping = self
                            .state
                            .lock()
                            .expect("worker fleet lock poisoned")
                            .stopping;
                        if stopping {
                            // Shutdown already drained what it knew about, so a
                            // worker that joined during it would be left behind.
                            let _ = run_script(
                                &self.settings.deprovision,
                                &env,
                                Duration::from_secs(self.settings.deprovision_timeout_secs),
                            )
                            .await;
                            self.state
                                .lock()
                                .expect("worker fleet lock poisoned")
                                .provisioning
                                .remove(&name);
                            return;
                        }
                        tracing::info!(worker = %name, "a provisioned isolation worker joined");
                        let mut state = self.state.lock().expect("worker fleet lock poisoned");
                        state.provisioning.remove(&name);
                        state.provisioned.insert(name);
                        self.wake.notify_waiters();
                        return;
                    }
                    tracing::warn!(
                        worker = %name,
                        attempt,
                        "a provisioned worker never joined; removing it"
                    );
                    let _ = run_script(
                        &self.settings.deprovision,
                        &env,
                        Duration::from_secs(self.settings.deprovision_timeout_secs),
                    )
                    .await;
                }
                Err(error) => {
                    tracing::warn!(worker = %name, attempt, %error, "could not provision a worker");
                }
            }
            if attempt < PROVISION_ATTEMPTS {
                time::sleep(provision_backoff(attempt)).await;
            }
        }
        tracing::error!(worker = %name, "gave up on a worker after {PROVISION_ATTEMPTS} attempts");
        let mut state = self.state.lock().expect("worker fleet lock poisoned");
        state.provisioning.remove(&name);
        state.retry_after = Some(Instant::now() + AFTER_FAILURE);
    }

    async fn wait_until_joined(&self, name: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if self
                .state
                .lock()
                .expect("worker fleet lock poisoned")
                .connected
                .contains_key(name)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn deprovision(&self, name: &str) {
        let env = self.script_env(name);
        match run_script(
            &self.settings.deprovision,
            &env,
            Duration::from_secs(self.settings.deprovision_timeout_secs),
        )
        .await
        {
            Ok(()) => tracing::info!(worker = %name, "an isolation worker was removed"),
            Err(error) => {
                tracing::warn!(worker = %name, %error, "could not remove an isolation worker")
            }
        }
        self.state
            .lock()
            .expect("worker fleet lock poisoned")
            .connected
            .remove(name);
    }
}

impl WorkerHost for WorkerFleet {
    fn register_worker(
        &self,
        peer: EndpointId,
        token: String,
        name: String,
        capacity: WorkerReport,
    ) -> BoxFuture<Result<(), String>> {
        let answer = {
            let mut state = self.state.lock().expect("worker fleet lock poisoned");
            let ceiling = self.settings.ceiling();
            if token != self.token {
                Err("this worker does not hold this machine's token".to_owned())
            } else if state.stopping {
                Err("this machine is stopping".to_owned())
            } else if state.connected.len() >= ceiling && !state.connected.contains_key(&name) {
                Err(format!(
                    "this machine is already carrying its {ceiling} isolation workers"
                ))
            } else {
                if !state.connected.contains_key(&name) {
                    tracing::info!(
                        worker = %name,
                        cpus = capacity.cpus,
                        memory_mib = capacity.memory_mib,
                        "an isolation worker joined"
                    );
                }
                state.connected.insert(
                    name,
                    ConnectedWorker {
                        peer,
                        capacity,
                        last_seen: Instant::now(),
                    },
                );
                Ok(())
            }
        };
        self.wake.notify_waiters();
        Box::pin(async move { answer })
    }

    fn worker_heartbeat(&self, peer: EndpointId, name: String) -> BoxFuture<Result<(), String>> {
        let answer = {
            let mut state = self.state.lock().expect("worker fleet lock poisoned");
            match state.connected.get_mut(&name) {
                Some(worker) if worker.peer == peer => {
                    worker.last_seen = Instant::now();
                    Ok(())
                }
                _ => Err("this worker is not registered here".to_owned()),
            }
        };
        Box::pin(async move { answer })
    }
}

fn worker_name() -> String {
    format!(
        "worker-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

fn provision_backoff(attempt: usize) -> Duration {
    Duration::from_secs(5 * attempt as u64)
}

fn load_or_create_token(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => Ok(text.trim().to_owned()),
        Ok(_) => Err(io::Error::other("the worker token file is empty")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Two UUIDs are 256 bits of randomness without a second source of it.
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            write_private(path, &format!("{token}\n")).map_err(io::Error::other)?;
            Ok(token)
        }
        Err(error) => Err(error),
    }
}

async fn run_script(
    command: &str,
    env: &[(String, String)],
    limit: Duration,
) -> Result<(), String> {
    let mut process = Command::new("sh");
    process.arg("-c").arg(command);
    for (key, value) in env {
        process.env(key, value);
    }
    process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = process
        .spawn()
        .map_err(|error| format!("could not run {command:?}: {error}"))?;
    let output = time::timeout(limit, child.wait_with_output())
        .await
        .map_err(|_| format!("{command:?} did not finish within {}s", limit.as_secs()))?
        .map_err(|error| format!("could not wait for {command:?}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        Err(format!("{command:?} exited {}", output.status))
    } else {
        Err(format!("{command:?} exited {}: {stderr}", output.status))
    }
}
