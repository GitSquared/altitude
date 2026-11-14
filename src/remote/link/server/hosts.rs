use super::*;

pub struct ServerParts {
    pub identity: InstallationKey,
    pub clients: ClientRecord,
    pub listen: SocketAddr,
    pub label: String,
    pub data_dir: PathBuf,
    // What this machine is doing, from which the announced word is derived: one
    // fact, so the coarse word and the phase can never say different things.
    pub phase: watch::Receiver<Phase>,
    pub trackers: Vec<TrackerConnection>,
    pub tasks: Arc<dyn TaskHost>,
    // How isolation workers announce themselves, when this machine takes them.
    // A remote with none rejects the request rather than pretending to listen.
    pub workers: Option<Arc<dyn WorkerHost>>,
    // Where the agent channels live and the token that may reach them, when this
    // machine lets its workers carry an agent's channel through.
    pub agent_channels: Option<AgentChannelAccess>,
    // The installs layers its hosts built, when this machine keeps them for the next
    // host rather than making every host build its own.
    pub layers: Option<Arc<LayerStore>>,
}

// A worker is trusted to carry the connections an agent inside its sandbox makes,
// and to name the channel it carries them to. The token says which remote may.
#[derive(Clone)]
pub struct AgentChannelAccess {
    pub directory: PathBuf,
    pub token: String,
}

// The other half of the link that is not about tasks: a provisioned worker
// proving it belongs and saying it is still there. It is a separate contract from
// `TaskHost` because a worker is compute, not a client with work of its own.
pub trait WorkerHost: Send + Sync + 'static {
    fn register_worker(
        &self,
        peer: EndpointAddr,
        token: String,
        name: String,
        capacity: WorkerReport,
    ) -> BoxFuture<Result<(), String>>;
    fn worker_heartbeat(&self, peer: EndpointId, name: String) -> BoxFuture<Result<(), String>>;
}

pub trait TaskHost: Send + Sync + 'static {
    fn signal_threads(
        &self,
        peer: EndpointId,
        task_id: String,
    ) -> BoxFuture<Result<Vec<TaskAsk>, String>>;
    fn signal(&self, peer: EndpointId, command: SignalCommand) -> BoxFuture<Result<(), String>>;
    fn input(
        &self,
        peer: EndpointId,
        task_id: String,
        body: String,
    ) -> BoxFuture<Result<(), String>>;
    fn configure(&self, peer: EndpointId, forget_logins: bool) -> BoxFuture<Result<(), String>>;
    fn run(&self, peer: EndpointId, request: TaskRequest) -> BoxFuture<Result<(), String>>;
    fn list(&self, peer: EndpointId) -> BoxFuture<Result<Vec<TaskSummary>, String>>;
    fn watch(&self, peer: EndpointId) -> BoxFuture<Result<Watching, String>>;
    fn remove(&self, peer: EndpointId, task_id: String) -> BoxFuture<Result<(), String>>;
    fn take_progress(&self, task_id: &str) -> Option<TransferUpdate>;
    fn hand_over(&self, peer: EndpointId, task_id: String) -> BoxFuture<Result<Handover, String>>;
    fn transfer_piece(
        &self,
        peer: EndpointId,
        transfer_id: String,
        part: TransferPart,
        offset: u64,
        len: u64,
    ) -> BoxFuture<Result<Vec<u8>, String>>;
    fn abandon_take(&self, peer: EndpointId, transfer_id: String) -> BoxFuture<Result<(), String>>;
    fn abandon_import(
        &self,
        peer: EndpointId,
        transfer_id: String,
    ) -> BoxFuture<Result<(), String>>;
    fn handed_over(&self, peer: EndpointId, transfer_id: String) -> BoxFuture<Result<(), String>>;
    fn prepare_import(&self, peer: EndpointId, handover: Handover)
    -> BoxFuture<Result<(), String>>;
    fn put_import_part(
        &self,
        peer: EndpointId,
        transfer_id: String,
        part: TransferPart,
        offset: u64,
        bytes: Vec<u8>,
    ) -> BoxFuture<Result<(), String>>;
    fn stage_import(
        &self,
        peer: EndpointId,
        transfer_id: String,
    ) -> BoxFuture<Result<String, String>>;
    fn commit_import(&self, peer: EndpointId, transfer_id: String)
    -> BoxFuture<Result<(), String>>;
}

pub struct Watching {
    pub snapshot: Vec<TaskSummary>,
    pub changes: UnboundedReceiver<WatchEvent>,
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
