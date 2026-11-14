use std::{fs, net::Ipv4Addr, path::PathBuf, process::Command, time::Instant};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

use iroh::endpoint::{RecvStream, SendStream};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::{
    sync::mpsc,
    task::{JoinHandle, spawn_blocking},
};

use crate::{
    agent::{AgentEngines, mcp},
    config::Config,
    credentials::profile_recipe,
    isolation::{
        CommandEvent, CredentialSandbox, EnvironmentProgress, EnvironmentProgressSink,
        IsolationEngine, ProfileRecipe, Unwatched, VsockRoute, private_directory,
        worker_protocol::{CommandFrame, WorkerCall, WorkerProfile, WorkerReply},
    },
    logging,
    remote::cache::share::client_ip,
    remote::protocol::{MAX_BLOB_BYTES, read_frame, write_frame},
};

use super::WorkerService;

// What a host says while it works, and the answer it ends with. A provision is minutes
// of work — an installs layer is built or restored inside it — so the host keeps
// talking instead of going quiet and looking dead.
pub(super) enum Statement {
    Progress(String),
    Log(String),
    Answer(WorkerReply),
}

#[derive(Clone)]
pub(super) struct Progress(mpsc::UnboundedSender<Statement>);

// The host's half of a call: what it says while it works, and the one answer at the end.
pub(super) struct Narrator {
    statements: mpsc::UnboundedSender<Statement>,
    task: JoinHandle<()>,
}

impl Narrator {
    pub(super) fn start(mut send: SendStream) -> Self {
        let (statements, mut said) = mpsc::unbounded_channel::<Statement>();
        let task = tokio::spawn(async move {
            while let Some(statement) = said.recv().await {
                match statement {
                    Statement::Progress(label) => {
                        let frame = WorkerReply::Progress { label };
                        if write_frame(&mut send, &frame).await.is_err() {
                            break;
                        }
                    }
                    Statement::Log(line) => {
                        if write_frame(&mut send, &WorkerReply::Log { line })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Statement::Answer(reply) => {
                        let _ = write_frame(&mut send, &reply).await;
                        let _ = send.finish();
                        break;
                    }
                }
            }
        });
        Self { statements, task }
    }

    pub(super) fn progress(&self) -> Progress {
        Progress(self.statements.clone())
    }

    // The host's log travels to the master for the length of the call, and only then:
    // outside it there is nobody to tell.
    pub(super) fn relay_log(&self) -> mpsc::UnboundedSender<String> {
        let (lines, mut said) = mpsc::unbounded_channel::<String>();
        let statements = self.statements.clone();
        tokio::spawn(async move {
            while let Some(line) = said.recv().await {
                if statements.send(Statement::Log(line)).is_err() {
                    break;
                }
            }
        });
        lines
    }

    pub(super) async fn finish(self, reply: WorkerReply) {
        let _ = self.statements.send(Statement::Answer(reply));
        let _ = self.task.await;
        logging::relay(None);
    }
}

impl Progress {
    pub(super) fn say(&self, label: impl Into<String>) {
        let _ = self.0.send(Statement::Progress(label.into()));
    }
}

// Everything the engine narrates on this host travels to the master's status line, so a
// provision reads the same whether the sandbox is local or a machine we cannot see.
impl EnvironmentProgressSink for Progress {
    fn report(&self, progress: EnvironmentProgress) {
        self.say(progress.label);
    }
}

impl WorkerService {
    // The host's end of the agent channel: it listens on the loopback the sandbox
    // shares and carries each connection to the master over the link. A vsock route
    // for the agent's channel is what says a sandbox has one.
    fn start_channels(&self, id: &str, vsock: &[VsockRoute]) {
        for route in vsock {
            if route.guest_port != mcp::AGENT_CHANNEL_PORT {
                continue;
            }
            let socket = route.host_socket.to_string_lossy().into_owned();
            let port = route.guest_port as u16;
            let client = self.client.clone();
            let master = self.master.clone();
            let token = self.token.clone();
            let handle = tokio::spawn(async move {
                let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
                    Ok(listener) => listener,
                    Err(error) => {
                        tracing::warn!(%error, port, "could not open an agent channel on this host");
                        return;
                    }
                };
                loop {
                    match listener.accept().await {
                        Ok((tcp, _)) => {
                            let client = client.clone();
                            let master = master.clone();
                            let token = token.clone();
                            let socket = socket.clone();
                            tokio::spawn(async move {
                                if let Err(error) = client
                                    .pipe_agent_channel(master, &socket, &token, tcp)
                                    .await
                                {
                                    tracing::debug!(%error, "an agent channel connection ended");
                                }
                            });
                        }
                        Err(error) => {
                            tracing::warn!(%error, "an agent channel listener stopped");
                            return;
                        }
                    }
                }
            });
            if let Ok(mut channels) = self.channels.lock() {
                channels.insert(id.to_owned(), handle);
            }
        }
    }

    fn stop_channel(&self, id: &str) {
        if let Ok(mut channels) = self.channels.lock()
            && let Some(handle) = channels.remove(id)
        {
            handle.abort();
        }
    }

    // The project's cache, as the master exports it to this host for as long as its
    // sandbox lives. Mounting is best-effort: a cache that is not there costs time and
    // nothing else, so a host with nothing exported to it — no owner named, no export
    // named, no address to reach it at — works on a cold one.
    fn mount_cache(&self, cache: Option<&str>, owner: &str, project: &str) -> Result<(), String> {
        if owner.is_empty() {
            return Ok(());
        }
        let (Some(source), Some(server)) = (cache, client_ip(&self.master)) else {
            return Ok(());
        };
        let path = self.config.project_cache_dir(project);
        if fs::create_dir_all(&path).is_err() {
            return Ok(());
        }
        let mounted = Command::new("sudo")
            .arg("-n")
            .arg("mount")
            .arg("-t")
            .arg("nfs4")
            .arg(format!("{server}:{source}"))
            .arg(&path)
            .output();
        match mounted {
            Ok(output) if output.status.success() => {
                tracing::info!(project, "the project cache is mounted from the master")
            }
            Ok(output) => tracing::warn!(
                project,
                reason = %String::from_utf8_lossy(&output.stderr).trim(),
                "no export for this project's cache; the sandbox starts with a cold one"
            ),
            Err(error) => tracing::warn!(%error, "could not run mount"),
        }
        Ok(())
    }

    // The lend ends with the sandbox: the mount goes, so nothing of the master's stays
    // reachable on a host that is about to be thrown away.
    fn unmount_cache(&self, path: PathBuf) {
        let Some(project) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            return;
        };
        let unmounted = Command::new("sudo")
            .arg("-n")
            .arg("umount")
            .arg(&path)
            .output();
        match unmounted {
            Ok(output) if output.status.success() => {
                tracing::info!(project, "the project cache is no longer mounted")
            }
            Ok(output) => tracing::warn!(
                project,
                reason = %String::from_utf8_lossy(&output.stderr).trim(),
                "could not unmount the project cache"
            ),
            Err(error) => tracing::warn!(%error, "could not run umount"),
        }
    }

    // The calls that reach here are quick; the long one, a provision, has its own path
    // because it reads the profile files before it answers.
    pub(super) async fn dispatch(&self, call: WorkerCall, _progress: &Progress) -> WorkerReply {
        match call {
            WorkerCall::Create {
                id,
                options,
                purpose,
            } => match self.engine.create(id, options, purpose).await {
                Ok(()) => WorkerReply::Ok,
                Err(error) => WorkerReply::Failed(error.to_string()),
            },
            WorkerCall::Remove { id } => {
                self.stop_channel(&id);
                let mounted = self
                    .mounted
                    .lock()
                    .ok()
                    .and_then(|mut mounted| mounted.remove(&id));
                if let Some(path) = mounted {
                    self.unmount_cache(path);
                }
                match self.engine.remove(id).await {
                    Ok(()) => WorkerReply::Ok,
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::HasSandbox { id } => WorkerReply::Bool(self.engine.has_sandbox(id).await),
            WorkerCall::GuestFileExists { id, path } => {
                match self.engine.guest_regular_file_exists(id, path).await {
                    Ok(exists) => WorkerReply::Bool(exists),
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::CredentialShell {
                id,
                profiles,
                snapshot,
                vsock,
                profile,
            } => {
                if let Err(error) = write_profiles(&self.config, &profiles) {
                    return WorkerReply::Failed(error);
                }
                let recipes = match rebuild_recipes(&profiles, &self.agents) {
                    Ok(recipes) => recipes,
                    Err(error) => return WorkerReply::Failed(error),
                };
                let sandbox = CredentialSandbox {
                    id,
                    recipes,
                    snapshot,
                    vsock,
                };
                match self
                    .engine
                    .credential_shell(sandbox, &profile, &Unwatched)
                    .await
                {
                    Ok(command) => WorkerReply::Text(command),
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::Watcher {
                id,
                profiles,
                project,
            } => {
                if let Err(error) = write_profiles(&self.config, &profiles) {
                    return WorkerReply::Failed(error);
                }
                let recipes = match rebuild_recipes(&profiles, &self.agents) {
                    Ok(recipes) => recipes,
                    Err(error) => return WorkerReply::Failed(error),
                };
                match self.engine.watcher(id, project, recipes, &Unwatched).await {
                    Ok(()) => WorkerReply::Ok,
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::CredentialReports {
                id,
                profiles,
                snapshot,
                vsock,
                checks,
            } => {
                if let Err(error) = write_profiles(&self.config, &profiles) {
                    return WorkerReply::Failed(error);
                }
                let recipes = match rebuild_recipes(&profiles, &self.agents) {
                    Ok(recipes) => recipes,
                    Err(error) => return WorkerReply::Failed(error),
                };
                let sandbox = CredentialSandbox {
                    id,
                    recipes,
                    snapshot,
                    vsock,
                };
                match self
                    .engine
                    .credential_reports(sandbox, checks, &Unwatched)
                    .await
                {
                    Ok(reports) => WorkerReply::Reports(reports),
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::Provision { .. }
            | WorkerCall::RunCommand { .. }
            | WorkerCall::DeliverFile { .. }
            | WorkerCall::DeliverFiles { .. }
            | WorkerCall::CollectFile { .. }
            | WorkerCall::CollectFiles { .. }
            | WorkerCall::Snapshot { .. }
            | WorkerCall::Restore { .. } => unreachable!("this call is streamed"),
        }
    }

    // A provision's profiles are the rest of the stream: a CLI's home is megabytes of
    // plugins and caches, which no frame would hold.
    pub(super) async fn provision(
        &self,
        mut recv: RecvStream,
        call: WorkerCall,
        progress: &Progress,
    ) -> WorkerReply {
        let mut body = Vec::new();
        let read = (&mut recv)
            .take(MAX_BLOB_BYTES)
            .read_to_end(&mut body)
            .await;
        let profiles = match read.map_err(|error| error.to_string()).and_then(|_| {
            serde_json::from_slice::<Vec<WorkerProfile>>(&body).map_err(|error| error.to_string())
        }) {
            Ok(profiles) => profiles,
            Err(error) => {
                return WorkerReply::Failed(format!("could not read the profile files: {error}"));
            }
        };
        self.provision_call(call, profiles, progress).await
    }

    async fn provision_call(
        &self,
        call: WorkerCall,
        profiles: Vec<WorkerProfile>,
        progress: &Progress,
    ) -> WorkerReply {
        let WorkerCall::Provision {
            id,
            snapshot,
            project,
            owner,
            cache,
            agent_profile,
            vcs_profile,
            vsock,
        } = call
        else {
            return WorkerReply::Failed("not a provision".into());
        };
        tracing::info!("a provision arrived; writing its profiles");
        progress.say("writing this sandbox's profile files");
        let began = Instant::now();
        match write_profiles_off_thread(self.config.clone(), profiles).await {
            Ok((files, bytes)) => tracing::info!(
                files,
                bytes,
                elapsed_ms = began.elapsed().as_millis() as u64,
                "wrote this sandbox's profile files"
            ),
            Err(error) => return WorkerReply::Failed(error),
        }
        progress.say("mounting this project's cache");
        if let Err(error) = self.mount_cache(cache.as_deref(), &owner, &project.id) {
            return WorkerReply::Failed(error);
        }
        if cache.is_some()
            && !owner.is_empty()
            && let Ok(mut mounted) = self.mounted.lock()
        {
            mounted.insert(id.clone(), self.config.project_cache_dir(&project.id));
        }
        let recipes = match (
            profile_recipe(&agent_profile, &self.agents),
            profile_recipe(&vcs_profile, &self.agents),
        ) {
            (Ok(agent), Ok(vcs)) => Ok(vec![agent, vcs]),
            (Err(error), _) | (_, Err(error)) => Err(error),
        };
        let recipes = match recipes {
            Ok(recipes) => recipes,
            Err(error) => return WorkerReply::Failed(error.to_string()),
        };
        let channels = vsock.clone();
        tracing::info!(sandbox = %id, "composing and creating the sandbox");
        let outcome = self
            .engine
            .provision(id.clone(), snapshot, project, recipes, vsock, progress)
            .await;
        tracing::info!(
            sandbox = %id,
            ok = outcome.is_ok(),
            "the sandbox is made"
        );
        match outcome {
            Ok(()) => {
                self.start_channels(&id, &channels);
                WorkerReply::Ok
            }
            Err(error) => WorkerReply::Failed(error.to_string()),
        }
    }

    pub(super) async fn run_command(
        &self,
        mut send: SendStream,
        mut recv: RecvStream,
        id: String,
        command: String,
    ) {
        let mut session = match self.engine.run_command(id, command).await {
            Ok(session) => session,
            Err(error) => {
                let _ = write_frame(&mut send, &WorkerReply::Failed(error.to_string())).await;
                return;
            }
        };
        if write_frame(&mut send, &WorkerReply::Ready).await.is_err() {
            return;
        }
        loop {
            tokio::select! {
                event = session.events.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    let done = matches!(event, CommandEvent::Exited(_) | CommandEvent::Failed(_));
                    if write_frame(&mut send, &CommandFrame::Report(event)).await.is_err() {
                        return;
                    }
                    if done {
                        break;
                    }
                }
                frame = read_frame::<_, CommandFrame>(&mut recv) => {
                    match frame {
                        Ok(CommandFrame::Drive(command)) => {
                            if session.commands.send(command).is_err() {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            }
        }
        let _ = send.finish();
    }
}

fn rebuild_recipes(
    profiles: &[WorkerProfile],
    agents: &AgentEngines,
) -> Result<Vec<ProfileRecipe>, String> {
    profiles
        .iter()
        .map(|profile| profile_recipe(&profile.profile, agents).map_err(|error| error.to_string()))
        .collect()
}

// The profile files the master pushed with this sandbox, written where the plan
// will mount the profile. A file's path is relative to the profile's directory and
// may not climb out of it.
// A profile is thousands of small files, and this process has one thread to answer the
// master with: writing them there is a silence the master reads as a dead host.
async fn write_profiles_off_thread(
    config: Config,
    profiles: Vec<WorkerProfile>,
) -> Result<(usize, u64), String> {
    spawn_blocking(move || {
        let files = profiles.iter().map(|profile| profile.files.len()).sum();
        let bytes = profiles
            .iter()
            .flat_map(|profile| profile.files.iter())
            .map(|(_, contents)| contents.len() as u64)
            .sum();
        write_profiles(&config, &profiles)?;
        Ok((files, bytes))
    })
    .await
    .map_err(|error| error.to_string())?
}

fn write_profiles(config: &Config, profiles: &[WorkerProfile]) -> Result<(), String> {
    for profile in profiles {
        let root = config.profile_dir(&profile.profile);
        private_directory(&root).map_err(|error| error.to_string())?;
        for (path, contents) in &profile.files {
            let target = root.join(path);
            if !target.starts_with(&root) {
                return Err(format!("a profile path leaves its directory: {path}"));
            }
            let bytes = BASE64
                .decode(contents)
                .map_err(|error| format!("an unreadable profile file: {error}"))?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            fs::write(&target, bytes).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}
