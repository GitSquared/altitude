use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::isolation::{
    BaseSandboxSnapshot, Command, CommandEvent, EnvironmentDescription, Pressure, SandboxOptions,
    SandboxPurpose, SandboxRoom, VsockRoute,
};
use crate::project::Project;

// The ALPN a master dials a worker on. It is not the link's: the link is clients
// talking about their tasks, and this is a master asking an empty box for compute.
pub(crate) const WORKER_ALPN: &[u8] = b"altitude/worker/1";

// A worker only ever speaks to the master that provisioned it, and the token it
// was provisioned with is what says so. It travels with each call because a worker
// holds no session: a box that restarts is the same box.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkerRequest {
    pub token: String,
    pub call: WorkerCall,
}

// What the master can ask a box for. The arguments are the sandbox's own
// vocabulary rather than a path: a box composes its plan from its config, so the
// wire never carries a host path that only makes sense on the master.
// A profile's files as the master holds them, base64 so a token that is not text
// still travels. The box writes them where its plan will mount the profile.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkerProfile {
    pub profile: String,
    pub files: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum WorkerCall {
    // Say whether this box can make sandboxes at all.
    Probe,
    // Build a project's environment here and return the snapshot the box made.
    // The recipes are rebuilt from the agent kind, so the wire carries no
    // `ProfileRecipe`.
    EnsureProject {
        project: Project,
        agent: AgentKind,
    },
    // Make the one task sandbox this box was spawned for. The two profile ids
    // name the credentials to mount; the box rebuilds their recipes itself.
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
    // The box started a command and what follows on the stream is its output.
    Ready,
    Ok,
    Snapshot(BaseSandboxSnapshot),
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
