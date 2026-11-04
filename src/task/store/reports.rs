use super::*;


use sqlx::types::Json;

use crate::task::{
    Task, TaskAgentRun, TaskAgentTurnReport, TaskError, TaskHistoryEvent, TaskOperatorAttentionLevel, TaskOperatorSignal,
    TaskOperatorSignalAuthor, TaskOperatorSignalDraft, TaskOperatorSignalKind,
};
use crate::vcs::{CodeContribution, ContributionObservation};

impl TaskStore {
    pub async fn record_report(
        &self,
        task_id: &str,
        agent_run: &TaskAgentRun,
        report: &TaskAgentTurnReport,
        attention: TaskOperatorAttentionLevel,
        review: ReviewRequest<'_>,
    ) -> Result<Task, TaskError> {
        let mut events = vec![TaskHistoryEvent::AgentTurnReported {
            agent_run_id: agent_run.id.clone(),
            requested_action: report.requested_action(),
            operator_attention_level: attention,
        }];
        if let Some(model) = report
            .model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
        {
            events.push(TaskHistoryEvent::AgentModelObserved {
                model: model.to_owned(),
            });
        }
        // A run that is answering a thread keeps speaking on it. The thread id is
        // reused only while it is unresolved, and never for a review: a review is the
        // operator's to close, so the agent's next words open a thread of their own.
        let continuing = match &agent_run.answering_signal_id {
            Some(signal_id)
                if self.is_signal_unresolved(task_id, signal_id).await?
                    && self.thread_of(task_id, signal_id).await?.kind
                        != TaskOperatorSignalKind::InReview =>
            {
                Some(signal_id.clone())
            }
            _ => None,
        };
        let raised_id = match &continuing {
            Some(signal_id) => Some(signal_id.clone()),
            None if report.operator_signal.is_some() => Some(self.next_signal_id(task_id).await?),
            None => None,
        };
        let signal = |draft: &TaskOperatorSignalDraft| TaskOperatorSignal {
            id: raised_id.clone().unwrap_or_default(),
            task_id: task_id.to_owned(),
            kind: draft.kind,
            title: draft.title.clone(),
            body: draft.body.clone(),
            raised_at_ms: now_ms(),
            proposal: None,
        };
        match review {
            // A held request is not raised now; the host raises it later, once
            // the linked contributions can answer. Either way it is recorded
            // here, so the agent's request is never lost to a restart.
            ReviewRequest::Hold(draft) => events.push(TaskHistoryEvent::ReviewDeferred {
                signal: signal(draft),
                agent_run_id: agent_run.id.clone(),
            }),
            ReviewRequest::Raise => events.extend(report.operator_signal.iter().map(|draft| {
                TaskHistoryEvent::OperatorSignalRaised {
                    signal: signal(draft),
                }
            })),
        }
        // A report that asks for nothing resolves the thread it was continuing:
        // the agent has what it needed and moved on.
        if report.operator_signal.is_none()
            && let Some(signal_id) = &continuing
        {
            events.push(TaskHistoryEvent::OperatorSignalResolved {
                signal_id: signal_id.clone(),
                resolved: true,
                by: TaskOperatorSignalAuthor::Agent,
            });
        }
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        Self::write_events(&mut transaction, task_id, &events).await?;
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(task)
    }

    pub async fn link_code_contribution(
        &self,
        task_id: &str,
        contribution: &CodeContribution,
    ) -> Result<(Task, bool), TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        let event = TaskHistoryEvent::CodeContributionLinked {
            contribution: contribution.clone(),
        };
        let payload = serde_json::to_string(&event).map_err(TaskError::HistoryData)?;
        let inserted = sqlx::query(
            r#"
            INSERT INTO task_event_logs (id, task_id, recorded_at_ms, payload)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(id) DO NOTHING
        "#,
        )
        .bind(format!("code-contribution:{task_id}:{}", contribution.id))
        .bind(task_id)
        .bind(now_ms())
        .bind(payload)
        .execute(&mut *transaction)
        .await
        .map_err(TaskError::Database)?
        .rows_affected()
            > 0;
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok((task, inserted))
    }

    pub async fn record_code_contribution_observations(
        &self,
        task_id: &str,
        observations: &[ContributionObservation],
    ) -> Result<(Task, bool), TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        let mut moved = false;
        for observation in observations {
            let previous: Option<Json<ContributionObservation>> = sqlx::query_scalar(
                r#"
                SELECT observation
                FROM task_code_contribution_observations
                WHERE task_id = ?1
                    AND json_extract(observation, '$.contribution_id') = ?2
                ORDER BY sequence DESC
                LIMIT 1
                "#,
            )
            .bind(task_id)
            .bind(&observation.contribution_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(TaskError::Database)?;
            if previous
                .as_ref()
                .is_some_and(|previous| previous.0 == *observation)
            {
                continue;
            }
            moved = true;
            Self::write_events(
                &mut transaction,
                task_id,
                &[TaskHistoryEvent::CodeContributionObserved {
                    observation: observation.clone(),
                }],
            )
            .await?;
        }
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok((task, moved))
    }
}
