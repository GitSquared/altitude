use std::{
    collections::HashSet,
    error::Error,
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use iroh::EndpointAddr;
use tokio::{sync::mpsc, time};

use crate::{
    agent::AgentKind,
    cli::ProbeArgs,
    config::Settings,
    remote::{
        ConfigBundle, ConfigPush, InstallationKey, LinkClient, Logins, TaskOrigin, TaskRequest,
        TaskSummary, WatchEvent,
    },
    task::TaskExecutionStatus,
};

type Boxed = Box<dyn Error>;

pub struct Probe {
    pub master: EndpointAddr,
    pub data_dir: PathBuf,
    pub settings: PathBuf,
    pub host: String,
    pub project: Option<String>,
    pub agent: AgentKind,
    pub prompt: String,
    pub patience: Duration,
}

pub async fn run(args: ProbeArgs) -> Result<(), Boxed> {
    probe(Probe {
        master: serde_json::from_str(&args.master)?,
        data_dir: args.data_dir,
        settings: args.settings,
        host: args.host,
        project: args.project,
        agent: match args.agent.as_str() {
            "claude" => AgentKind::Claude,
            "pi" => AgentKind::Pi,
            other => return Err(format!("no agent called {other}").into()),
        },
        prompt: args.prompt,
        patience: Duration::from_secs(args.patience),
    })
    .await
}

// What it costs an operator to find out whether a master works: the tracker is not the
// only way in, and a terminal is not the only way to read what happened. One intent, one
// project, every change printed with the time it took to arrive.
pub async fn probe(probe: Probe) -> Result<(), Boxed> {
    let key = InstallationKey::load_or_create(probe.data_dir.join("identity.key"))
        .map_err(|error| error.to_string())?;
    let link = LinkClient::bind(&key)
        .await
        .map_err(|error| error.to_string())?;
    let (events, mut received) = mpsc::unbounded_channel();
    {
        let link = link.clone();
        let master = probe.master.clone();
        tokio::spawn(async move {
            if let Err(error) = link.watch_tasks(master, events).await {
                eprintln!("probe: the watch ended: {error}");
            }
        });
    }
    let known = first_snapshot(&mut received).await;
    let project = publish(&link, &probe).await?;
    let started = Instant::now();
    link.run_task(
        probe.master.clone(),
        TaskRequest {
            origin: TaskOrigin::Intent(probe.prompt.clone()),
            project_id: project.clone(),
            agent: probe.agent,
            identity_label: "probe".to_string(),
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    println!(
        "probe: task sent to {project}; watching for {:?}",
        probe.patience
    );
    let mut ours: Option<String> = None;
    let mut last = String::new();
    loop {
        let event = match time::timeout(probe.patience, received.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => return Err("the watch ended before the task did".into()),
            Err(_) => {
                println!(
                    "probe: nothing changed for {:?} · last was {last}",
                    probe.patience
                );
                return Err("the task did not reach an ending in time".into());
            }
        };
        let summary = match event {
            WatchEvent::Snapshot(summaries) => summaries
                .into_iter()
                .find(|summary| Some(&summary.id) == ours.as_ref()),
            WatchEvent::Changed(summary) => {
                // The task this probe made is the one that was not here when it started.
                let is_ours = ours.as_deref() == Some(&summary.id)
                    || (ours.is_none() && !known.contains(&summary.id));
                if !is_ours {
                    None
                } else {
                    ours = Some(summary.id.clone());
                    Some(*summary)
                }
            }
            WatchEvent::Removed(id) => {
                if ours.as_deref() == Some(&id) {
                    println!("[{:>6.1}s] removed", started.elapsed().as_secs_f32());
                    return Ok(());
                }
                None
            }
            WatchEvent::Ready => None,
        };
        let Some(summary) = summary else {
            continue;
        };
        let line = describe(&summary);
        if line != last {
            println!("[{:>6.1}s] {line}", started.elapsed().as_secs_f32());
            last = line;
        }
        match summary.status {
            TaskExecutionStatus::Failed => {
                return Err(format!("the task failed · {last}").into());
            }
            TaskExecutionStatus::Completed => return Ok(()),
            TaskExecutionStatus::Paused => {
                // A pause is the end of a run, not the end of the answer: what matters
                // here is that it got as far as running, and why it stopped.
                if last.contains("running") || ours.is_some() {
                    println!("probe: the task is paused · {last}");
                }
                return Ok(());
            }
            TaskExecutionStatus::Pending | TaskExecutionStatus::Running => {}
        }
    }
}

// The project this probe runs is one it brings with it: its own settings, its own
// credentials from the data directory it was given. A master has to be told about both
// before it can run anything, and saying so here is what makes a probe worth trusting —
// it exercises the path an operator's client takes, not a project the master happened
// to have already.
async fn publish(link: &LinkClient, probe: &Probe) -> Result<String, Boxed> {
    let settings = fs::read_to_string(&probe.settings)
        .map_err(|error| format!("{}: {error}", probe.settings.display()))?;
    let parsed: Settings = toml::from_str(&settings).map_err(|error| error.to_string())?;
    let project = match &probe.project {
        Some(id) => id.clone(),
        None => parsed
            .projects()
            .iter()
            .find(|project| project.host.as_deref() == Some(probe.host.as_str()))
            .map(|project| project.id.clone())
            .ok_or_else(|| {
                format!(
                    "{} names no project for host {}",
                    probe.settings.display(),
                    probe.host
                )
            })?,
    };
    let held = link
        .held_profiles(probe.master.clone())
        .await
        .map_err(|error| error.to_string())?;
    let bundle = ConfigBundle::of(
        &settings,
        &probe.data_dir,
        &held,
        Some(probe.host.as_str()),
        Logins::Always,
    )
    .map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(&bundle).map_err(|error| error.to_string())?;
    match link
        .put_config(probe.master.clone(), &bytes, false)
        .await
        .map_err(|error| error.to_string())?
    {
        ConfigPush::Stored => println!("probe: config stored · project {project}"),
        other => return Err(format!("the master refused the configuration: {other:?}").into()),
    }
    Ok(project)
}

fn describe(summary: &TaskSummary) -> String {
    let mut parts = vec![format!("{:?}", summary.status)];
    if let Some(reason) = &summary.pause_reason {
        parts.push(format!("{reason:?}"));
    }
    if let Some(activity) = &summary.activity
        && !activity.is_empty()
    {
        parts.push(activity.clone());
    }
    if let Some(ask) = &summary.leading_ask
        && ask.waits_on_operator()
    {
        parts.push(format!("asked: {}", ask.title));
    }
    parts.join(" · ")
}

async fn first_snapshot(received: &mut mpsc::UnboundedReceiver<WatchEvent>) -> HashSet<String> {
    let mut known = HashSet::new();
    while let Ok(Some(event)) = time::timeout(Duration::from_secs(20), received.recv()).await {
        if let WatchEvent::Snapshot(summaries) = event {
            known = summaries.into_iter().map(|summary| summary.id).collect();
            break;
        }
    }
    known
}
