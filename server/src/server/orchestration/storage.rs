use std::collections::HashMap;

use chrono::{DateTime, Utc};
use protocol::{CanonicalRuntimeSpec, Job, WorkItem};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::server::task_adapter::ProviderActionIdentity;

#[derive(Debug, Clone, Copy)]
pub(super) enum DbTable {
    WorkItems,
    Jobs,
    OwnershipReturnOutbox,
    CheckpointOutbox,
    ProviderActionInbox,
}

impl DbTable {
    fn as_str(self) -> &'static str {
        match self {
            Self::WorkItems => "work_items",
            Self::Jobs => "jobs",
            Self::OwnershipReturnOutbox => "ownership_return_outbox",
            Self::CheckpointOutbox => "checkpoint_outbox",
            Self::ProviderActionInbox => "provider_action_inbox",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProviderActionTerminalState {
    Applied,
    Rejected,
}

impl ProviderActionTerminalState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone)]
pub(super) struct ProviderActionJobInsert {
    pub job: Job,
    pub runtime: CanonicalRuntimeSpec,
    pub checkpoint_owner_id: Uuid,
}

#[derive(Clone)]
pub(super) struct ProviderActionCommit {
    pub task_id: String,
    pub identity: ProviderActionIdentity,
    pub work_item_id: Uuid,
    pub action_json: String,
    pub terminal_state: ProviderActionTerminalState,
    pub rejection_reason: Option<String>,
    pub work_item_updates: Vec<WorkItem>,
    pub work_item_inserts: Vec<WorkItem>,
    pub job_inserts: Vec<ProviderActionJobInsert>,
    pub cancel_active_work_item_ids: Vec<Uuid>,
    pub cancel_unreleased_checkpoint_owner_ids: Vec<Uuid>,
    pub revision_update: Option<ProviderActionRevisionUpdate>,
}

#[derive(Clone)]
pub(super) struct ProviderActionRevisionUpdate {
    pub work_item_id: Uuid,
    pub context_key: &'static str,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProviderActionCommitOutcome {
    Committed,
    AlreadyTerminal(ProviderActionTerminalState),
}

pub(super) fn commit_provider_action(
    conn: &mut Connection,
    commit: ProviderActionCommit,
) -> anyhow::Result<ProviderActionCommitOutcome> {
    let tx = conn.transaction()?;
    if let Some((stored_work_item_id, stored_action_json, stored_state)) = tx
        .query_row(
            "SELECT work_item_id, action_json, state FROM provider_action_inbox WHERE task_id = ?1 AND action_namespace = ?2 AND revision = ?3",
            params![commit.task_id, commit.identity.namespace.as_str(), commit.identity.revision.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
    {
        if stored_work_item_id != commit.work_item_id.to_string()
            || stored_action_json != commit.action_json
        {
            anyhow::bail!(
                "provider action identity collision for task {} namespace {} revision {}",
                commit.task_id,
                commit.identity.namespace.as_str(),
                commit.identity.revision
            );
        }
        let terminal = match stored_state.as_str() {
            "applied" => Some(ProviderActionTerminalState::Applied),
            "rejected" => Some(ProviderActionTerminalState::Rejected),
            "pending" => None,
            state => anyhow::bail!("unknown provider action inbox state '{state}'"),
        };
        if let Some(terminal) = terminal {
            return Ok(ProviderActionCommitOutcome::AlreadyTerminal(terminal));
        }
    } else {
        tx.execute(
            "INSERT INTO provider_action_inbox (task_id, action_namespace, revision, work_item_id, action_json, state, rejection_reason, provider_ack_state, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', NULL, 'pending', ?6)",
            params![
                commit.task_id,
                commit.identity.namespace.as_str(),
                commit.identity.revision.to_string(),
                commit.work_item_id.to_string(),
                commit.action_json,
                Utc::now().to_rfc3339(),
            ],
        )?;
    }

    for item in &commit.work_item_inserts {
        insert_work_item_in_transaction(&tx, item)?;
    }
    for item in &commit.work_item_updates {
        update_work_item_in_transaction(&tx, item)?;
    }
    for job in &commit.job_inserts {
        insert_job_in_transaction(&tx, job)?;
    }
    for work_item_id in &commit.cancel_active_work_item_ids {
        tx.execute(
            "UPDATE jobs SET state = 'cancelled', lease_expires_at = NULL, updated_at = ?2 WHERE work_item_id = ?1 AND state IN ('pending', 'assigned', 'running')",
            params![work_item_id.to_string(), Utc::now().to_rfc3339()],
        )?;
    }
    for checkpoint_owner_id in &commit.cancel_unreleased_checkpoint_owner_ids {
        tx.execute(
            "UPDATE jobs SET state = 'cancelled', lease_expires_at = NULL, updated_at = ?2 WHERE checkpoint_owner_id = ?1 AND dispatch_released = 0 AND state = 'pending'",
            params![checkpoint_owner_id.to_string(), Utc::now().to_rfc3339()],
        )?;
    }
    if let Some(revision_update) = &commit.revision_update {
        let json_path = format!("$.task_adapter.{}", revision_update.context_key);
        let updated = tx.execute(
            "UPDATE work_items SET context_json = json_set(context_json, ?2, ?3) WHERE id = ?1",
            params![
                revision_update.work_item_id.to_string(),
                json_path,
                revision_update.revision.to_string(),
            ],
        )?;
        if updated != 1 {
            anyhow::bail!(
                "provider action revision work item {} disappeared",
                revision_update.work_item_id
            );
        }
    }
    let updated = tx.execute(
        "UPDATE provider_action_inbox SET state = ?4, rejection_reason = ?5, provider_ack_state = 'pending', provider_ack_error = NULL, updated_at = ?6 WHERE task_id = ?1 AND action_namespace = ?2 AND revision = ?3 AND state = 'pending'",
        params![
            commit.task_id,
            commit.identity.namespace.as_str(),
            commit.identity.revision.to_string(),
            commit.terminal_state.as_str(),
            commit.rejection_reason,
            Utc::now().to_rfc3339(),
        ],
    )?;
    if updated != 1 {
        anyhow::bail!(
            "provider action inbox transition lost task {} namespace {} revision {}",
            commit.task_id,
            commit.identity.namespace.as_str(),
            commit.identity.revision
        );
    }
    tx.commit()?;
    Ok(ProviderActionCommitOutcome::Committed)
}

fn insert_work_item_in_transaction(tx: &Transaction<'_>, item: &WorkItem) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO work_items (id, workflow_id, external_task_id, external_task_source, title, description, labels_json, priority, session_id, current_step, projected_state, last_failure_reason, required_action_description, triage_status, human_gate, human_gate_state, blocked_on, state, context_json, repo_json, parent_work_item_id, parent_step_id, child_work_item_id, created_at, updated_at, last_written_at, write_back_failures, write_back_retry_after, status_comment_id, checkpoint_comment_id, job_result_comment_ids_json, provider_ownership_json, remembered_human_owners_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33)",
        work_item_params(item)?,
    )?;
    Ok(())
}

fn update_work_item_in_transaction(tx: &Transaction<'_>, item: &WorkItem) -> anyhow::Result<()> {
    let updated = tx.execute(
        "UPDATE work_items SET workflow_id = ?2, external_task_id = ?3, external_task_source = ?4, title = ?5, description = ?6, labels_json = ?7, priority = ?8, current_step = ?9, projected_state = ?10, last_failure_reason = ?11, required_action_description = ?12, triage_status = ?13, human_gate = ?14, human_gate_state = ?15, blocked_on = ?16, state = ?17, context_json = ?18, repo_json = ?19, parent_work_item_id = ?20, parent_step_id = ?21, child_work_item_id = ?22, updated_at = ?23, last_written_at = ?24, write_back_failures = ?25, write_back_retry_after = ?26, status_comment_id = ?27, checkpoint_comment_id = ?28, job_result_comment_ids_json = ?29, provider_ownership_json = ?30, remembered_human_owners_json = ?31 WHERE id = ?1",
        params![
            item.id.to_string(), item.workflow_id, item.external_task_id, item.external_task_source,
            item.title, item.description, serde_json::to_string(&item.labels)?, item.priority,
            item.current_step, item.projected_state, item.last_failure_reason,
            item.required_action_description, item.triage_status, i64::from(item.human_gate),
            item.human_gate_state, item.blocked_on.as_ref().map(serde_json::to_string).transpose()?,
            item.state.to_string(), serde_json::to_string(&item.context)?,
            item.repo.as_ref().map(serde_json::to_string).transpose()?,
            item.parent_work_item_id.map(|id| id.to_string()), item.parent_step_id,
            item.child_work_item_id.map(|id| id.to_string()), item.updated_at.to_rfc3339(),
            item.last_written_at.map(|at| at.to_rfc3339()), item.write_back_failures,
            item.write_back_retry_after.map(|at| at.to_rfc3339()), item.status_comment_id,
            item.checkpoint_comment_id, serde_json::to_string(&item.job_result_comment_ids)?,
            serde_json::to_string(&item.provider_ownership)?,
            serde_json::to_string(&item.remembered_human_owners)?,
        ],
    )?;
    if updated != 1 {
        anyhow::bail!("provider action work item {} disappeared", item.id);
    }
    Ok(())
}

fn work_item_params(
    item: &WorkItem,
) -> anyhow::Result<rusqlite::ParamsFromIter<Vec<rusqlite::types::Value>>> {
    use rusqlite::types::Value;
    let values = vec![
        Value::Text(item.id.to_string()),
        Value::Text(item.workflow_id.clone()),
        item.external_task_id
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        item.external_task_source
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        item.title.clone().map(Value::Text).unwrap_or(Value::Null),
        item.description
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        Value::Text(serde_json::to_string(&item.labels)?),
        item.priority
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        item.session_id
            .as_ref()
            .map(|id| Value::Text(id.to_string()))
            .unwrap_or(Value::Null),
        Value::Text(item.current_step.clone()),
        item.projected_state
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        item.last_failure_reason
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        item.required_action_description
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        item.triage_status
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        Value::Integer(i64::from(item.human_gate)),
        item.human_gate_state
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        item.blocked_on
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?
            .map(Value::Text)
            .unwrap_or(Value::Null),
        Value::Text(item.state.to_string()),
        Value::Text(serde_json::to_string(&item.context)?),
        item.repo
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?
            .map(Value::Text)
            .unwrap_or(Value::Null),
        item.parent_work_item_id
            .map(|id| Value::Text(id.to_string()))
            .unwrap_or(Value::Null),
        item.parent_step_id
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        item.child_work_item_id
            .map(|id| Value::Text(id.to_string()))
            .unwrap_or(Value::Null),
        Value::Text(item.created_at.to_rfc3339()),
        Value::Text(item.updated_at.to_rfc3339()),
        item.last_written_at
            .map(|at| Value::Text(at.to_rfc3339()))
            .unwrap_or(Value::Null),
        Value::Integer(item.write_back_failures.into()),
        item.write_back_retry_after
            .map(|at| Value::Text(at.to_rfc3339()))
            .unwrap_or(Value::Null),
        item.status_comment_id
            .map(Value::Integer)
            .unwrap_or(Value::Null),
        item.checkpoint_comment_id
            .map(Value::Integer)
            .unwrap_or(Value::Null),
        Value::Text(serde_json::to_string(&item.job_result_comment_ids)?),
        Value::Text(serde_json::to_string(&item.provider_ownership)?),
        Value::Text(serde_json::to_string(&item.remembered_human_owners)?),
    ];
    Ok(rusqlite::params_from_iter(values))
}

fn insert_job_in_transaction(
    tx: &Transaction<'_>,
    insert: &ProviderActionJobInsert,
) -> anyhow::Result<()> {
    let job = &insert.job;
    tx.execute(
        "INSERT INTO jobs (id, work_item_id, step_id, state, assigned_worker_id, execution_mode, container_image, command, args_json, env_json, prompt_json, context_json, repo_json, timeout_secs, max_retries, attempt, transcript_dir, result_json, lease_expires_at, created_at, updated_at, runtime_spec_json, checkpoint_owner_id, dispatch_released) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, 0)",
        params![
            job.id.to_string(), job.work_item_id.to_string(), job.step_id, job.state.to_string(),
            job.assigned_worker_id, job.execution_mode.to_string(), job.container_image, job.command,
            serde_json::to_string(&job.args)?, serde_json::to_string(&job.env)?,
            job.prompt.as_ref().map(serde_json::to_string).transpose()?,
            serde_json::to_string(&job.context)?, job.repo.as_ref().map(serde_json::to_string).transpose()?,
            job.timeout_secs, job.max_retries, job.attempt, job.transcript_dir,
            job.result.as_ref().map(serde_json::to_string).transpose()?,
            job.lease_expires_at.map(|at| at.to_rfc3339()), job.created_at.to_rfc3339(),
            job.updated_at.to_rfc3339(), serde_json::to_string(&insert.runtime)?,
            insert.checkpoint_owner_id.to_string(),
        ],
    )?;
    Ok(())
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
    let provider_ownership_json: String = row.get(31)?;
    let remembered_human_owners_json: String = row.get(32)?;

    Ok(WorkItem {
        id: Uuid::parse_str(&id).map_err(invalid_uuid)?,
        workflow_id: row.get(1)?,
        external_task_id: row.get(2)?,
        external_task_source: row.get(3)?,
        provider_ownership: serde_json::from_str(&provider_ownership_json).map_err(invalid_json)?,
        remembered_human_owners: serde_json::from_str(&remembered_human_owners_json)
            .map_err(invalid_json)?,
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

pub(super) fn migrate_provider_action_inbox_schema(tx: &Transaction<'_>) -> anyhow::Result<()> {
    if column_exists(tx, DbTable::ProviderActionInbox, "action_namespace")? {
        return Ok(());
    }

    tx.execute_batch(
        r#"
        ALTER TABLE provider_action_inbox RENAME TO provider_action_inbox_legacy;
        CREATE TABLE provider_action_inbox (
            task_id TEXT NOT NULL,
            action_namespace TEXT NOT NULL,
            revision TEXT NOT NULL,
            work_item_id TEXT NOT NULL,
            action_json TEXT NOT NULL,
            state TEXT NOT NULL,
            rejection_reason TEXT,
            provider_ack_state TEXT NOT NULL DEFAULT 'pending',
            provider_ack_error TEXT,
            legacy_delivery_revision TEXT,
            updated_at TEXT NOT NULL,
            PRIMARY KEY(task_id, action_namespace, revision)
        );
        INSERT INTO provider_action_inbox (
            task_id,
            action_namespace,
            revision,
            work_item_id,
            action_json,
            state,
            rejection_reason,
            provider_ack_state,
            provider_ack_error,
            legacy_delivery_revision,
            updated_at
        )
        SELECT
            task_id,
            CASE
                WHEN json_valid(action_json)
                    AND json_extract(action_json, '$.namespace') = 'provider_ownership'
                THEN 'ownership'
                ELSE 'command'
            END,
            CASE
                WHEN json_valid(action_json)
                    AND json_extract(action_json, '$.namespace') = 'provider_ownership'
                    AND json_type(action_json, '$.ownership_revision') = 'integer'
                THEN CAST(json_extract(action_json, '$.ownership_revision') AS TEXT)
                ELSE CAST(revision AS TEXT)
            END,
            work_item_id,
            action_json,
            state,
            rejection_reason,
            provider_ack_state,
            provider_ack_error,
            CAST(revision AS TEXT),
            updated_at
        FROM provider_action_inbox_legacy;
        UPDATE work_items
        SET context_json = json_remove(
            context_json,
            '$.task_adapter.last_ownership_action_revision'
        )
        WHERE json_valid(context_json);
        DROP TABLE provider_action_inbox_legacy;
        "#,
    )?;
    Ok(())
}
