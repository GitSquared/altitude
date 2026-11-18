use super::*;

impl TaskFacts {
    pub fn waits_on_operator(&self) -> bool {
        self.leading_ask
            .as_ref()
            .is_some_and(LeadingAsk::waits_on_operator)
    }
}

#[derive(Clone, Debug)]
pub struct Task {
    pub id: String,
    pub agent_kind: Option<AgentKind>,
    pub host: String,
    pub code_contributions: Vec<CodeContribution>,
    pub code_contribution_observations: Vec<ContributionObservation>,
    pub facts: TaskFacts,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TaskFacts {
    pub accepted: bool,
    pub created_at_ms: i64,
    pub shown_updated_at_ms: i64,
    pub intent: String,
    pub title: Option<String>,
    pub operator_id: Option<String>,
    pub operator_label: Option<String>,
    #[sqlx(json(nullable))]
    pub project: Option<Project>,
    pub agent_conversation_id: Option<String>,
    pub model: Option<String>,
    #[sqlx(json(nullable))]
    pub plan: Option<Vec<TaskPlanStep>>,
    pub execution_status: TaskExecutionStatus,
    pub execution_pause_reason: Option<TaskExecutionPauseReason>,
    pub execution_mode: TaskExecutionMode,
    pub sandbox_id: Option<String>,
    pub sandbox_available: bool,
    pub snapshot_path: Option<String>,
    pub snapshot_available: bool,
    // When the account's limit clears, and which of its windows ran out. Read as a
    // time and not a switch: a task is held only while a pause says it is waiting on
    // one, because a value left behind by a later pause would otherwise hold work
    // nobody is waiting on.
    pub resumes_at_ms: Option<i64>,
    pub limit_window: Option<AgentWindow>,
    // What this task has spent, as its harnesses counted it. A sum of every run's
    // report, because a run is what a harness knows the cost of.
    #[sqlx(json)]
    pub usage: TaskUsage,
    // The account this task's credential runs on, as its harness last said. A reading
    // and not a state: it is as old as the run that took it, and the interface says so.
    #[sqlx(json(nullable))]
    pub standing: Option<AgentStanding>,
    pub last_error: Option<String>,
    #[sqlx(json(nullable))]
    pub latest_operator_message: Option<TaskAskMessage>,
    #[sqlx(json(nullable))]
    pub tracker_issue: Option<IssueReference>,
    // None means no delegation asked for this task, never one that is unknown: it is
    // what keeps local work out of every question about delegations.
    pub tracker_delegation: Option<String>,
    // The machine this task was carried from to be worked at a desk here. Its work
    // belongs there: it runs only while a person holds it, and it goes back when
    // they stop.
    pub arrived_from: Option<String>,
    #[sqlx(default)]
    pub tracker_status: Option<TrackerStatus>,
    // The ask the task is in the middle of, read once from the threads so the row, the
    // pause and the agent all read the same answer to "does this wait on them?".
    #[sqlx(json(nullable))]
    pub leading_ask: Option<LeadingAsk>,
    #[sqlx(json(nullable))]
    pub deferred_review: Option<DeferredReview>,
    // What the run that is starting opens with, said by whatever queued the task. The
    // second is the last thing said about this task's pull requests, which is what a
    // review poll compares itself against: saying it again unchanged is a wake loop.
    pub wake_news: Option<String>,
    pub last_wake_news: Option<String>,
    #[sqlx(json)]
    pub pending_inputs: Vec<TaskInput>,
}

impl TaskFacts {
    // A borrowed task is here for a desk, not for the queue: nothing carried to this
    // machine is this machine's to schedule.
    pub fn is_borrowed(&self) -> bool {
        self.arrived_from.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct TaskAgentTurnContext {
    pub task_id: String,
    pub title: String,
    pub host: String,
    pub project: Option<Project>,
    pub agent: Option<AgentKind>,
    pub model: Option<String>,
    pub plan: Vec<TaskPlanStep>,
    pub execution_status: TaskExecutionStatus,
    pub execution_mode: TaskExecutionMode,
    pub leading_ask: Option<LeadingAsk>,
    pub last_error: Option<String>,
    pub datetime: String,
    pub latest_operator_message: Option<TaskAskMessage>,
    pub code_contribution_observations: Vec<ContributionObservation>,
    pub pending_inputs: Vec<TaskInput>,
    // The login this project's work is contributed as, which is the one person
    // whose review the agent may answer.
    pub vcs_identity: Option<String>,
    // A rule the task itself shows is unmet, so the turn says so rather than a
    // second model call being spent finding out.
    pub title_missing: bool,
}

impl fmt::Display for TaskAgentTurnContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "task_id={}; title={:?}; harness={}; model={}; host={}; project={}",
            self.task_id,
            self.title,
            self.agent
                .map(|kind| kind.to_string())
                .unwrap_or_else(|| "none".into()),
            self.model.as_deref().unwrap_or("unknown"),
            self.host,
            self.project
                .as_ref()
                .map(|project| project.name.as_str())
                .unwrap_or("none"),
        )?;
        if let Some(project) = &self.project {
            if let Some(repository) = &project.repository {
                write!(formatter, "repository={repository}; ")?;
            }
            if let Some(checkout) = project.checkout_path() {
                write!(formatter, "checkout={checkout}; ")?;
            }
            writeln!(
                formatter,
                "project_brief=/altitude/project.md; harness=/altitude/harness.md"
            )?;
        }
        writeln!(
            formatter,
            "execution_status={:?}; execution_mode={:?}; ask={:?}; last_error={}",
            self.execution_status,
            self.execution_mode,
            self.leading_ask,
            self.last_error.as_deref().unwrap_or("none"),
        )?;
        write!(formatter, "current_datetime={}", self.datetime)?;
        if !self.plan.is_empty() {
            write!(formatter, "\nplan=")?;
            for (index, step) in self.plan.iter().enumerate() {
                if index > 0 {
                    write!(formatter, "; ")?;
                }
                write!(formatter, "{:?}: {}", step.status, step.content)?;
            }
        }
        if let Some(message) = &self.latest_operator_message {
            write!(
                formatter,
                "\nlatest_operator_message={}",
                serde_json::to_string(message).map_err(|_| fmt::Error)?
            )?;
        }
        if self.code_contribution_observations.is_empty() {
            return Ok(());
        }
        write!(formatter, "\ncode_contributions=")?;
        for (index, observation) in self.code_contribution_observations.iter().enumerate() {
            if index > 0 {
                write!(formatter, "; ")?;
            }
            write!(
                formatter,
                "{} @ {}: {}",
                observation.contribution_id, observation.state.head_sha, observation.summary
            )?;
        }
        Ok(())
    }
}

const TITLE_LIMIT: usize = 48;

pub fn normalize_title(title: &str) -> Option<String> {
    let collapsed = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= TITLE_LIMIT {
        return Some(collapsed);
    }
    let mut short = String::new();
    for word in collapsed.split(' ') {
        let separator = usize::from(!short.is_empty());
        if short.chars().count() + separator + word.chars().count() > TITLE_LIMIT {
            break;
        }
        if separator == 1 {
            short.push(' ');
        }
        short.push_str(word);
    }
    // A single word longer than the limit is cut, because a title with no words
    // in it is worse than a truncated one.
    if short.is_empty() {
        return Some(collapsed.chars().take(TITLE_LIMIT).collect());
    }
    Some(short)
}

impl Task {
    pub fn operator_id(&self) -> Option<&str> {
        self.facts.operator_id.as_deref()
    }

    pub fn title(&self) -> &str {
        self.facts
            .title
            .as_deref()
            .unwrap_or_else(|| id::name(&self.id))
    }

    pub fn project(&self) -> Option<&Project> {
        self.facts.project.as_ref()
    }

    pub fn turn_context(&self, execution_mode: TaskExecutionMode) -> TaskAgentTurnContext {
        TaskAgentTurnContext {
            task_id: self.id.clone(),
            title: self.title().to_owned(),
            host: self.host.clone(),
            project: self.facts.project.clone(),
            agent: self.agent_kind,
            model: self.facts.model.clone(),
            plan: self.facts.plan.clone().unwrap_or_default(),
            execution_status: TaskExecutionStatus::Running,
            execution_mode,
            leading_ask: self.facts.leading_ask.clone(),
            last_error: self.facts.last_error.clone(),
            datetime: Local::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            latest_operator_message: self.facts.latest_operator_message.clone(),
            code_contribution_observations: self.code_contribution_observations.clone(),
            pending_inputs: self.facts.pending_inputs.clone(),
            vcs_identity: None,
            title_missing: self.facts.title.is_none(),
        }
    }

    pub fn session_agent(&self) -> Option<String> {
        let harness = self
            .agent_kind
            .map(|kind| kind.to_string().to_ascii_lowercase());
        let model = self.facts.model.as_deref().map(|model| {
            model
                .rsplit('/')
                .next()
                .unwrap_or(model)
                .to_ascii_lowercase()
        });
        match (harness, model) {
            (Some(harness), Some(model)) => Some(format!("{harness}-{model}")),
            (Some(harness), None) => Some(harness),
            (None, Some(model)) => Some(model),
            (None, None) => None,
        }
    }
}
