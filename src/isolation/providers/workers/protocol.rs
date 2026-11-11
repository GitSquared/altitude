use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::isolation::{
    Command, CommandEvent, EnvironmentDescription, Pressure, SandboxOptions, SandboxPurpose,
    SandboxRoom,
};

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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum WorkerCall {
    // Say whether this box can make sandboxes at all.
    Probe,
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
