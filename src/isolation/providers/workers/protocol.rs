use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::isolation::{
    BaseSandboxSnapshot, Command, CommandEvent, CredentialCheck, CredentialReport,
    EnvironmentDescription, Pressure, SandboxOptions, SandboxPurpose, SandboxRoom, VsockRoute,
};
use crate::project::Project;

// The ALPN a master dials a worker on. It is not the link's: the link is clients
// talking about their tasks, and this is a master asking an empty host for compute.
pub(crate) const WORKER_ALPN: &[u8] = b"altitude/worker/1";

// A worker only ever speaks to the master that provisioned it, and the token it
// was provisioned with is what says so. It travels with each call because a worker
// holds no session: a host that restarts is the same host.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkerRequest {
    pub token: String,
    pub call: WorkerCall,
}

// What the master can ask a host for. The arguments are the sandbox's own
// vocabulary rather than a path: a host composes its plan from its config, so the
// wire never carries a host path that only makes sense on the master.
// A profile's files as the master holds them, base64 so a token that is not text
// still travels. The host writes them where its plan will mount the profile.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkerProfile {
    pub profile: String,
    pub files: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum WorkerCall {
    // Say whether this host can make sandboxes at all.
    Probe,
    // Build a project's environment here and return the snapshot the host made.
    // The recipes are rebuilt from the agent kind, so the wire carries no
    // `ProfileRecipe`.
    EnsureProject {
        project: Project,
        agent: AgentKind,
    },
    // Make the one task sandbox this host was spawned for. The two profile ids
    // name the credentials to mount; the host rebuilds their recipes itself.
    Provision {
        id: String,
        snapshot: Option<String>,
        project: Project,
        agent_profile: String,
        vcs_profile: String,
        vsock: Vec<VsockRoute>,
        profiles: Vec<WorkerProfile>,
    },
    Create {
        id: String,
        options: SandboxOptions,
        purpose: SandboxPurpose,
    },
    RunCommand {
        id: String,
        command: String,
    },
    // A tree or a file moves as a stream on the same connection: the request names
    // the sandbox side, the bytes follow, and the reply says how it went.
    DeliverFile {
        id: String,
        guest_path: String,
    },
    DeliverFiles {
        id: String,
        root: String,
    },
    CollectFile {
        id: String,
        guest_path: String,
    },
    CollectFiles {
        id: String,
        root: String,
        excluded: Vec<String>,
    },
    // A command sandbox a credential flow or a provider's read needs. The host
    // rebuilds the recipes from the pushed profiles, as `Provision` does.
    CredentialShell {
        id: String,
        profiles: Vec<WorkerProfile>,
        snapshot: Option<String>,
        vsock: Vec<VsockRoute>,
        profile: String,
    },
    Watcher {
        id: String,
        profiles: Vec<WorkerProfile>,
        project: Project,
    },
    CredentialReports {
        id: String,
        profiles: Vec<WorkerProfile>,
        snapshot: Option<String>,
        vsock: Vec<VsockRoute>,
        checks: Vec<CredentialCheck>,
    },
    Remove {
        id: String,
    },
    HasSandbox {
        id: String,
    },
    Suspend {
        id: String,
        name: String,
    },
    RemoveSnapshot {
        path: PathBuf,
    },
    GuestFileExists {
        id: String,
        path: String,
    },
    ProjectMemoryDir {
        project_id: String,
    },
    BaseDescription,
    Pressure,
    Room,
    CleanupLeftovers,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum WorkerReply {
    // The host started a command and what follows on the stream is its output.
    Ready,
    Ok,
    Snapshot(BaseSandboxSnapshot),
    // Bytes follow on the stream: a collected file or a tar of a collected tree,
    // and what the collecting machine counted.
    Blob { len: u64, files: u64, bytes: u64 },
    Reports(Option<Vec<(String, CredentialReport)>>),
    Bool(bool),
    Path(PathBuf),
    Text(String),
    Description(Option<EnvironmentDescription>),
    Pressure(Option<Pressure>),
    Room(SandboxRoom),
    Failed(String),
}

// What flows on a command's stream in each direction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum CommandFrame {
    Drive(Command),
    Report(CommandEvent),
}
