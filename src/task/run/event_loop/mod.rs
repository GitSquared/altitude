use std::{
    collections::{HashMap, HashSet},
    sync::atomic::AtomicBool,
    sync::{Arc, Mutex, MutexGuard},
};

use crate::remote::Watching;
use crate::task::{SendsTasksHome, TaskOperatorSignalThread};
use tokio::sync::mpsc;

use crate::{
    agent::Activity,
    isolation::{Command, IsolationEngine, OutputStream},
    task::mirror::review::{ReviewWaits, Watchers},
    task::{Task, TaskExecutionMode, TaskHistoryEvent, TaskService, TaskServiceError},
};

mod admission;
mod control;
mod lifecycle;
mod review;
mod terminal;
mod transfer;
mod watching;

use terminal::CachedTerminalOutput;

pub enum TaskExecutionUpdate {
    HandingBack {
        task_id: String,
        address: String,
        transfer: Watching,
    },
    HandedBack(String, Result<(), String>),
    Created(Result<Task, TaskServiceError>),
    Started(Result<Task, TaskServiceError>),
    Finished(String, Result<Task, TaskServiceError>),
    Removed(String, Result<(), TaskServiceError>),
    Output {
        task_id: String,
        stream: OutputStream,
        bytes: Vec<u8>,
    },
    OperatorReady(Task),
    // What the agent is doing right now, for anyone watching. Not history: it
    // is replaced by the next one and means nothing once the run ends.
    Activity {
        task_id: String,
        activity: Activity,
    },
    Report {
        task: Task,
    },
}

#[derive(Clone)]
pub struct TaskEventLoop<E> {
    service: TaskService<E>,
    admission_enabled: Arc<AtomicBool>,
    events: mpsc::UnboundedSender<TaskExecutionUpdate>,
    runs: Arc<Mutex<HashMap<String, ActiveRun>>>,
    control_requests: Arc<Mutex<HashMap<String, RequestedControl>>>,
    terminal_output: Arc<Mutex<HashMap<String, CachedTerminalOutput>>>,
    // The newest line each running agent produced. Read by the interface and by a
    // tracker reporting on this machine, which is the same fact told twice.
    current_activity: Arc<Mutex<HashMap<String, Activity>>>,
    starting: Arc<Mutex<HashSet<String>>>,
    watchers: Arc<Watchers<E>>,
    review_waits: Arc<ReviewWaits>,
    sends_home: Arc<Mutex<Option<Arc<dyn SendsTasksHome>>>>,
}

struct ActiveRun {
    commands: mpsc::UnboundedSender<Command>,
    agent_run_id: String,
    suspending: bool,
}

struct RequestedControl {
    agent_run_id: String,
    requested_mode: TaskExecutionMode,
}

impl<E> TaskEventLoop<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub fn new(service: TaskService<E>) -> (Self, mpsc::UnboundedReceiver<TaskExecutionUpdate>) {
        Self::with_admission(service, true)
    }

    pub fn waiting_for_runtime(
        service: TaskService<E>,
    ) -> (Self, mpsc::UnboundedReceiver<TaskExecutionUpdate>) {
        Self::with_admission(service, false)
    }

    fn with_admission(
        service: TaskService<E>,
        enabled: bool,
    ) -> (Self, mpsc::UnboundedReceiver<TaskExecutionUpdate>) {
        let (events, receiver) = mpsc::unbounded_channel();
        let watchers = Arc::new(Watchers::new(service.clone()));
        let event_loop = Self {
            service,
            admission_enabled: Arc::new(AtomicBool::new(enabled)),
            events,
            runs: Arc::new(Mutex::new(HashMap::new())),
            control_requests: Arc::new(Mutex::new(HashMap::new())),
            terminal_output: Arc::new(Mutex::new(HashMap::new())),
            current_activity: Arc::new(Mutex::new(HashMap::new())),
            starting: Arc::new(Mutex::new(HashSet::new())),
            watchers,
            review_waits: Arc::new(ReviewWaits::default()),
            sends_home: Arc::new(Mutex::new(None)),
        };
        event_loop.watch_host_capacity();
        event_loop.watch_starting();
        event_loop.watch_held_reviews();
        (event_loop, receiver)
    }

    pub async fn task(&self, task_id: &str) -> Result<Option<Task>, TaskServiceError> {
        self.service.task(task_id).await
    }

    pub async fn list(&self) -> Result<Vec<Task>, TaskServiceError> {
        self.service.list().await
    }

    pub async fn events(
        &self,
        task_id: &str,
    ) -> Result<Vec<(i64, TaskHistoryEvent)>, TaskServiceError> {
        self.service.events(task_id).await
    }

    pub async fn signal_threads(
        &self,
        task_id: &str,
    ) -> Result<Vec<TaskOperatorSignalThread>, TaskServiceError> {
        self.service.signal_threads(task_id).await
    }

    pub async fn reply(
        &self,
        task_id: &str,
        signal_id: &str,
        answer: String,
    ) -> Result<(), TaskServiceError> {
        if let Some(run) = self
            .service
            .reply_operator_signal(task_id, signal_id, answer)
            .await?
        {
            self.watch_agent_run(Ok(run));
        }
        Ok(())
    }

    pub async fn express_input(&self, task_id: &str, body: String) -> Result<(), TaskServiceError> {
        if let Some(run) = self.service.express_input(task_id, body).await? {
            self.watch_agent_run(Ok(run));
        }
        Ok(())
    }

    fn activity(&self) -> MutexGuard<'_, HashMap<String, Activity>> {
        self.current_activity
            .lock()
            .expect("activity lock poisoned")
    }

    pub fn current_activity(&self, task_id: &str) -> Option<Activity> {
        self.activity().get(task_id).cloned()
    }

    pub fn attach_sends_home(&self, home: Arc<dyn SendsTasksHome>) {
        if let Ok(mut slot) = self.sends_home.lock() {
            *slot = Some(home);
        }
    }

    fn sends_home(&self) -> Option<Arc<dyn SendsTasksHome>> {
        self.sends_home.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn announce_transfer(&self, task_id: String, address: String, transfer: Watching) {
        let _ = self.events.send(TaskExecutionUpdate::HandingBack {
            task_id,
            address,
            transfer,
        });
    }

    /// Hand a borrowed task back to the machine it came from. The interface asks for
    /// this when the operator leaves; the run ending asks for it too.
    pub fn hand_back(&self, task: Task) {
        let Some(home) = self.sends_home() else {
            return;
        };
        let loop_ = self.clone();
        tokio::spawn(async move {
            let _ = loop_
                .service
                .record(&task.id, &[TaskHistoryEvent::TaskHandedBack])
                .await;
            let outcome = home.send_home(task.clone()).await;
            if let Err(error) = &outcome {
                tracing::warn!(task = %task.id, %error, "could not hand a task back to its machine");
                // The task is here with no desk holding it, which is a state the
                // operator has to be told about rather than left to discover: taking
                // it over again is the way to try the move once more.
                let _ = loop_.service.fail(&task, error.clone()).await;
            }
            let _ = loop_
                .events
                .send(TaskExecutionUpdate::HandedBack(task.id, outcome));
        });
    }
}

pub(super) fn watcher_key(task: &Task) -> Option<String> {
    let operator = task.operator_id()?;
    let project = task.project()?;
    let credential = project
        .vcs_credential
        .clone()
        .unwrap_or_else(|| project.vcs_provider().profile().to_owned());
    Some(format!("{operator}\0{credential}"))
}
