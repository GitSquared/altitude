use super::*;

use crate::task::ProjectMemoryFile;

impl TaskStore {
    // The newest event about each file of a project's memory: the state the memory
    // directory is written from. A path whose newest event has no contents was
    // removed. A path's time decides which of two events wins, so records that
    // reached this machine out of order still settle on the newest writing.
    pub async fn project_memory_state(
        &self,
        project_id: &str,
    ) -> Result<Vec<ProjectMemoryFile>, TaskError> {
        let rows: Vec<(String, Option<String>, i64)> = sqlx::query_as(
            r#"
            SELECT path, contents, at_ms FROM (
                SELECT json_extract(payload, '$.data.path') AS path,
                       json_extract(payload, '$.data.contents') AS contents,
                       json_extract(payload, '$.data.written_at_ms') AS at_ms,
                       ROW_NUMBER() OVER (
                           PARTITION BY json_extract(payload, '$.data.path')
                           ORDER BY json_extract(payload, '$.data.written_at_ms') DESC,
                                    sequence DESC
                       ) AS newest
                FROM task_event_logs
                WHERE event_type = 'project_memory_file_written'
                  AND json_extract(payload, '$.data.project_id') = ?1
            )
            WHERE newest = 1
            ORDER BY path
            "#,
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await
        .map_err(TaskError::Database)?;
        Ok(rows
            .into_iter()
            .map(|(path, contents, at_ms)| ProjectMemoryFile {
                path,
                contents,
                at_ms,
            })
            .collect())
    }
}
