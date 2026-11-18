use super::*;

use crate::task::is_due;

impl TaskStore {
    // The bounded start: one transaction that re-reads the task, asks the queue's own
    // rule and the absence of another claim, and records the claim before any sandbox
    // work. Two callers that arrive together re-check under the same write lock, so
    // one records a claim and the other is told there was nothing to claim. The task
    // stays Pending: the claim, not the execution status, is what excludes it from
    // the next admission pass.
    pub async fn claim_start(
        &self,
        task_id: &str,
        claim_id: &str,
        owner: &str,
    ) -> Result<Option<Task>, TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        let Some(task) = Self::read_task_optional(&mut transaction, task_id).await? else {
            transaction.commit().await.map_err(TaskError::Database)?;
            return Ok(None);
        };
        // Between the pass that chose this task and this claim it may have been
        // answered, held for review, carried to a desk, taken, or claimed by a caller
        // that got in first. The queue's rule is asked here rather than restated, and
        // a task that is already claimed is not due to be claimed again.
        if !is_due(&task) || task.facts.accepted || task.facts.active_start_claim().is_some() {
            transaction.commit().await.map_err(TaskError::Database)?;
            return Ok(None);
        }
        Self::write_events(
            &mut transaction,
            task_id,
            &[TaskHistoryEvent::StartClaimed {
                claim_id: claim_id.to_owned(),
                owner: owner.to_owned(),
            }],
        )
        .await?;
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(Some(task))
    }

    // Give back the claim this caller holds. A claim that is no longer the active one
    // — because the run it produced has started, or a newer claim replaced it — is
    // left alone: a caller releases its own fact, never another's. Returns the task
    // as it stands once the release has been considered.
    pub async fn relinquish_start(
        &self,
        task_id: &str,
        claim_id: &str,
    ) -> Result<Option<Task>, TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        let active: Option<String> = sqlx::query_scalar(
            r#"
            SELECT json_extract(payload, '$.data.claim_id')
            FROM task_event_logs
            WHERE task_id = ?1
              AND event_type = 'start_claimed'
              AND sequence > COALESCE((
                  SELECT MAX(sequence) FROM task_event_logs
                  WHERE task_id = ?1
                    AND event_type IN ('start_relinquished', 'agent_run_started')
              ), 0)
            ORDER BY sequence DESC LIMIT 1
            "#,
        )
        .bind(task_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(TaskError::Database)?;
        if active.as_deref() != Some(claim_id) {
            transaction.commit().await.map_err(TaskError::Database)?;
            return Ok(None);
        }
        Self::write_events(
            &mut transaction,
            task_id,
            &[TaskHistoryEvent::StartRelinquished {
                claim_id: claim_id.to_owned(),
            }],
        )
        .await?;
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(Some(task))
    }

    // Reserve a task for a handover, in one transaction with the staged write. A start
    // claim that committed first is read here and stops the staging; a staged write
    // that committed first leaves the task Paused, which its own due rule stops a
    // claim from.
    pub async fn begin_transfer(
        &self,
        task_id: &str,
        transfer_id: &str,
        peer: &str,
    ) -> Result<TransferStaging, TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        let Some(task) = Self::read_task_optional(&mut transaction, task_id).await? else {
            transaction.commit().await.map_err(TaskError::Database)?;
            return Ok(TransferStaging::Missing);
        };
        let is_staged =
            task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::Transfer);
        if is_staged && !task.facts.sandbox_available {
            transaction.commit().await.map_err(TaskError::Database)?;
            return Ok(TransferStaging::AlreadyStopped);
        }
        if !is_staged && task.facts.active_start_claim().is_some() {
            transaction.commit().await.map_err(TaskError::Database)?;
            return Ok(TransferStaging::Starting);
        }
        Self::write_events(
            &mut transaction,
            task_id,
            &[TaskHistoryEvent::TransferStaged {
                transfer_id: transfer_id.to_owned(),
                peer: peer.to_owned(),
                execution_status: TaskExecutionStatus::Paused,
                execution_pause_reason: TaskExecutionPauseReason::Transfer,
                execution_mode: task.facts.execution_mode,
                sandbox_available: task.facts.sandbox_available,
                snapshot_available: task.facts.snapshot_available,
                snapshot_path: task.facts.snapshot_path.clone(),
            }],
        )
        .await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(TransferStaging::Staged)
    }
}
