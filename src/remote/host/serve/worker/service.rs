use std::{fs, net::Ipv4Addr};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

use iroh::endpoint::{RecvStream, SendStream};
use tokio::net::TcpListener;

#[cfg(target_os = "linux")]
use tokio::runtime::Handle;

use crate::{
    agent::{AgentEngines, mcp},
    config::Config,
    credentials::profile_recipe,
    isolation::{
        CommandEvent, CredentialSandbox, IsolationEngine, ProfileRecipe, Unwatched, VsockRoute,
        mounts, private_directory,
        worker_protocol::{CommandFrame, WorkerCall, WorkerProfile, WorkerReply},
    },
    remote::protocol::{read_frame, write_frame},
};

#[cfg(target_os = "linux")]
use crate::remote::cache::{self, CacheFs};

use super::{Readiness, WorkerService};

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

    // Mount the master's cache for this project where the sandbox will look for it.
    // A host whose owner is not named, or whose master serves no cache, simply
    // builds without one.
    #[cfg(target_os = "linux")]
    fn mount_cache(&self, id: &str, owner: &str, project: &str) -> Result<(), String> {
        if owner.is_empty() {
            return Ok(());
        }
        let mountpoint = self.config.project_cache_dir(project);
        fs::create_dir_all(&mountpoint).map_err(|error| error.to_string())?;
        let cache = CacheFs::new(
            self.client.clone(),
            self.master.clone(),
            self.token.clone(),
            owner.to_owned(),
            project.to_owned(),
            Handle::current(),
        );
        let session = cache::mount(cache, &mountpoint)?;
        if let Ok(mut caches) = self.caches.lock() {
            caches.insert(id.to_owned(), session);
        }
        Ok(())
    }

    // A host that is not Linux cannot make sandboxes, so it never gets here.
    #[cfg(not(target_os = "linux"))]
    fn mount_cache(&self, _id: &str, _owner: &str, _project: &str) -> Result<(), String> {
        Err("the network cache is only available on Linux hosts".to_owned())
    }

    #[cfg(target_os = "linux")]
    fn unmount_cache(&self, id: &str) {
        if let Ok(mut caches) = self.caches.lock() {
            caches.remove(id);
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn unmount_cache(&self, _id: &str) {}
    pub(super) async fn dispatch(&self, call: WorkerCall) -> WorkerReply {
        match call {
            WorkerCall::Probe => {
                let readiness = self.readiness.lock().expect("worker readiness");
                match &*readiness {
                    Readiness::Ready => WorkerReply::Ready,
                    Readiness::Preparing => WorkerReply::Failed(
                        "this host is still preparing its sandbox runtime".into(),
                    ),
                    Readiness::Failed(error) => WorkerReply::Failed(format!(
                        "this host cannot prepare its sandbox runtime: {error}"
                    )),
                }
            }
            WorkerCall::EnsureProject { project, agent } => {
                let recipes = mounts::task(&self.agents, agent);
                match self
                    .engine
                    .ensure_project(project, recipes, &Unwatched)
                    .await
                {
                    Ok(snapshot) => WorkerReply::Snapshot(snapshot),
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::Provision {
                id,
                snapshot,
                project,
                owner,
                agent_profile,
                vcs_profile,
                vsock,
                profiles,
            } => {
                if let Err(error) = write_profiles(&self.config, &profiles) {
                    return WorkerReply::Failed(error);
                }
                if let Err(error) = self.mount_cache(&id, &owner, &project.id) {
                    return WorkerReply::Failed(error);
                }
                let recipes = match (
                    profile_recipe(&agent_profile, &self.agents),
                    profile_recipe(&vcs_profile, &self.agents),
                ) {
                    (Ok(agent), Ok(vcs)) => Ok(vec![agent, vcs]),
                    (Err(error), _) | (_, Err(error)) => Err(error),
                };
                match recipes {
                    Ok(recipes) => {
                        let channels = vsock.clone();
                        match self
                            .engine
                            .provision(id.clone(), snapshot, project, recipes, vsock, &Unwatched)
                            .await
                        {
                            Ok(()) => {
                                self.start_channels(&id, &channels);
                                WorkerReply::Ok
                            }
                            Err(error) => WorkerReply::Failed(error.to_string()),
                        }
                    }
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
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
                self.unmount_cache(&id);
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
            WorkerCall::ProjectMemoryDir { project_id } => WorkerReply::Text(
                self.engine
                    .project_memory_dir(&project_id)
                    .to_string_lossy()
                    .into_owned(),
            ),
            WorkerCall::BaseDescription => WorkerReply::Description(self.engine.base_description()),
            WorkerCall::Pressure => WorkerReply::Pressure(self.engine.pressure()),
            WorkerCall::Room => WorkerReply::Room(self.engine.sandbox_room()),
            WorkerCall::CleanupLeftovers => {
                self.engine.remove_leftover_sandboxes().await;
                WorkerReply::Ok
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
            WorkerCall::RunCommand { .. }
            | WorkerCall::DeliverFile { .. }
            | WorkerCall::DeliverFiles { .. }
            | WorkerCall::CollectFile { .. }
            | WorkerCall::CollectFiles { .. }
            | WorkerCall::Snapshot { .. }
            | WorkerCall::Restore { .. } => unreachable!("this call is streamed"),
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
