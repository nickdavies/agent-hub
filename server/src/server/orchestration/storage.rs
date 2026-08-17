use std::collections::HashMap;

use chrono::{DateTime, Utc};
use protocol::{CanonicalRuntimeSpec, Job, WorkItem};
use rusqlite::Connection;
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
pub(super) enum DbTable {
    WorkItems,
    Jobs,
    OwnershipReturnOutbox,
    CheckpointOutbox,
}

impl DbTable {
    fn as_str(self) -> &'static str {
        match self {
            Self::WorkItems => "work_items",
            Self::Jobs => "jobs",
            Self::OwnershipReturnOutbox => "ownership_return_outbox",
            Self::CheckpointOutbox => "checkpoint_outbox",
        }
    }
}

pub(super) fn row_to_work_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkItem> {
    let id: String = row.get(0)?;
    let labels_json: Option<String> = row.get(6)?;
    let session_id: Option<String> = row.get(8)?;
    let blocked_on_json: Option<String> = row.get(16)?;
    let state: String = row.get(17)?;
    let context_json: String = row.get(18)?;
    let repo_json: Option<String> = row.get(19)?;
    let parent_work_item_id: Option<String> = row.get(20)?;
    let parent_step_id: Option<String> = row.get(21)?;
    let child_work_item_id: Option<String> = row.get(22)?;
    let created_at: String = row.get(23)?;
    let updated_at: String = row.get(24)?;
    let last_written_at: Option<String> = row.get(25)?;
    let write_back_failures: Option<u32> = row.get(26)?;
    let write_back_retry_after: Option<String> = row.get(27)?;
    let status_comment_id: Option<i64> = row.get(28)?;
    let checkpoint_comment_id: Option<i64> = row.get(29)?;
    let job_result_comment_ids_json: Option<String> = row.get(30)?;
    let job_result_comment_ids = job_result_comment_ids_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<HashMap<String, i64>>(json).ok())
        .unwrap_or_default();

    Ok(WorkItem {
        id: Uuid::parse_str(&id).map_err(invalid_uuid)?,
        workflow_id: row.get(1)?,
        external_task_id: row.get(2)?,
        external_task_source: row.get(3)?,
        title: row.get(4)?,
        description: row.get(5)?,
        labels: labels_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(invalid_json)?
            .unwrap_or_default(),
        priority: row.get(7)?,
        session_id: session_id.map(protocol::SessionId::new),
        current_step: row.get(9)?,
        projected_state: row.get(10)?,
        last_failure_reason: row.get(11)?,
        required_action_description: row.get(12)?,
        triage_status: row.get(13)?,
        human_gate: row.get::<_, Option<i64>>(14)?.unwrap_or(0) != 0,
        human_gate_state: row.get(15)?,
        blocked_on: blocked_on_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(invalid_json)?,
        state: state.parse().map_err(invalid_enum)?,
        context: serde_json::from_str(&context_json).map_err(invalid_json)?,
        repo: repo_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(invalid_json)?,
        parent_work_item_id: parent_work_item_id
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(invalid_uuid)?,
        parent_step_id,
        child_work_item_id: child_work_item_id
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(invalid_uuid)?,
        created_at: parse_datetime(&created_at)?,
        updated_at: parse_datetime(&updated_at)?,
        last_written_at: last_written_at.as_deref().map(parse_datetime).transpose()?,
        write_back_failures: write_back_failures.unwrap_or(0),
        write_back_retry_after: write_back_retry_after
            .as_deref()
            .map(parse_datetime)
            .transpose()?,
        status_comment_id,
        checkpoint_comment_id,
        job_result_comment_ids,
    })
}

pub(super) fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let id: String = row.get(0)?;
    let work_item_id: String = row.get(1)?;
    let state: String = row.get(3)?;
    let mode: String = row.get(5)?;
    let args_json: String = row.get(8)?;
    let env_json: String = row.get(9)?;
    let prompt_json: Option<String> = row.get(10)?;
    let context_json: String = row.get(11)?;
    let repo_json: Option<String> = row.get(12)?;
    let timeout_secs: Option<u64> = row.get(13)?;
    let max_retries: Option<u32> = row.get(14)?;
    let attempt: Option<u32> = row.get(15)?;
    let result_json: Option<String> = row.get(17)?;
    let lease_expires_at: Option<String> = row.get(18)?;
    let created_at: String = row.get(19)?;
    let updated_at: String = row.get(20)?;
    let runtime_spec_json: Option<String> = row.get(21)?;

    let mut job = Job {
        id: Uuid::parse_str(&id).map_err(invalid_uuid)?,
        work_item_id: Uuid::parse_str(&work_item_id).map_err(invalid_uuid)?,
        step_id: row.get(2)?,
        state: state.parse().map_err(invalid_enum)?,
        assigned_worker_id: row.get(4)?,
        execution_mode: mode.parse().map_err(invalid_enum)?,
        container_image: row.get(6)?,
        command: row.get(7)?,
        args: serde_json::from_str(&args_json).map_err(invalid_json)?,
        env: serde_json::from_str(&env_json).map_err(invalid_json)?,
        prompt: prompt_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(invalid_json)?,
        context: serde_json::from_str(&context_json).map_err(invalid_json)?,
        repo: repo_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(invalid_json)?,
        timeout_secs,
        max_retries: max_retries.unwrap_or(0),
        attempt: attempt.unwrap_or(0),
        transcript_dir: row.get(16)?,
        result: result_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(invalid_json)?,
        lease_expires_at: lease_expires_at
            .as_deref()
            .map(parse_datetime)
            .transpose()?,
        created_at: parse_datetime(&created_at)?,
        updated_at: parse_datetime(&updated_at)?,
    };

    if let Some(spec_json) = runtime_spec_json {
        let runtime: CanonicalRuntimeSpec =
            serde_json::from_str(&spec_json).map_err(invalid_json)?;
        // Compatibility boundary: Job still exposes legacy runtime fields only.
        // We intentionally project canonical environment back into those fields,
        // while canonical permissions remain persisted in runtime_spec_json.
        job.execution_mode = runtime.environment.execution_mode;
        job.container_image = runtime.environment.container_image;
        job.timeout_secs = runtime.environment.timeout_secs;
    }

    Ok(job)
}

pub(super) fn parse_datetime(v: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v)
        .map(|d| d.with_timezone(&Utc))
        .map_err(invalid_datetime)
}

fn invalid_uuid(e: uuid::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

fn invalid_datetime(e: chrono::ParseError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

pub(super) fn invalid_json(e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

pub(super) fn invalid_enum(e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

pub(super) fn column_exists(
    conn: &Connection,
    table: DbTable,
    column: &str,
) -> anyhow::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table.as_str()))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}
