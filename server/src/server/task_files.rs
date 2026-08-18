use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use chrono::Utc;
use protocol::{ProviderOwnership, WorkItem};
use uuid::Uuid;

#[cfg(test)]
use super::task_adapter::CheckpointAuthorizationPending;
use super::task_adapter::{
    ProviderActionIdentity, ProviderActionNamespace, ProviderActionRejectionDelivery, TaskAdapter,
    TaskChange, TaskChangeType, TaskCheckpoint, TaskCreate, TaskProjection, TaskSyncRecord,
    TaskToolRecord, TaskTriageStatus, TaskUpdate, task_from_work_item,
};

pub struct TaskFileStore {
    dir: PathBuf,
    #[allow(dead_code)]
    last_poll: Mutex<SystemTime>,
    #[allow(dead_code)]
    known_file_mtimes: Mutex<HashMap<String, SystemTime>>,
}

impl TaskFileStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            last_poll: Mutex::new(SystemTime::UNIX_EPOCH),
            known_file_mtimes: Mutex::new(HashMap::new()),
        }
    }

    pub fn ensure_dir(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        Ok(())
    }

    fn write_task_file(&self, task: &TaskToolRecord) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let path = self.task_path(task.id);
        let tmp = self.temp_task_path(task.id);
        let data = serde_json::to_vec_pretty(task)?;
        std::fs::write(&tmp, data)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    fn read_all_task_files(&self) -> anyhow::Result<Vec<TaskToolRecord>> {
        read_all_tasks_from_dir(&self.dir)
    }

    fn read_task_file(&self, id: Uuid) -> anyhow::Result<Option<TaskToolRecord>> {
        let path = self.task_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path)?;
        if let Ok(task) = serde_json::from_slice::<TaskToolRecord>(&bytes) {
            return Ok(Some(task));
        }
        if let Ok(item) = serde_json::from_slice::<WorkItem>(&bytes) {
            return Ok(Some(task_from_work_item(&item, None)));
        }
        Ok(None)
    }

    #[allow(dead_code)]
    fn read_task_file_by_task_id(&self, task_id: &str) -> anyhow::Result<Option<TaskToolRecord>> {
        let id = Uuid::parse_str(task_id)?;
        self.read_task_file(id)
    }

    fn task_path(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn temp_task_path(&self, id: Uuid) -> PathBuf {
        // Temporary files intentionally do not end in `.json` so pull_tasks skips them.
        self.dir.join(format!("{id}.json.tmp"))
    }

    fn checkpoint_path(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{id}.checkpoint.json"))
    }

    fn temp_checkpoint_path(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{id}.checkpoint.json.tmp"))
    }

    #[allow(dead_code)]
    fn read_task_file_mtimes(&self) -> anyhow::Result<HashMap<String, SystemTime>> {
        if !self.dir.exists() {
            return Ok(HashMap::new());
        }

        let mut out = HashMap::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".checkpoint.json"))
            {
                continue;
            }
            let is_json = path
                .extension()
                .and_then(|v| v.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("json"))
                .unwrap_or(false);
            if !is_json {
                continue;
            }

            let Some(task_id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(ToOwned::to_owned)
            else {
                continue;
            };

            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            out.insert(task_id, modified);
        }
        Ok(out)
    }
}

fn durable_atomic_replace(path: &Path, temp_path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("checkpoint path has no parent: {}", path.display()))?;
    let mut temp = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(temp_path)?;
    temp.write_all(data)?;
    temp.flush()?;
    temp.sync_all()?;
    drop(temp);
    std::fs::rename(temp_path, path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn file_action_result_matches(
    result: &serde_json::Value,
    identity: &ProviderActionIdentity,
) -> bool {
    result.get("namespace").and_then(serde_json::Value::as_str) == Some(identity.namespace.as_str())
        && result.get("revision").and_then(|revision| {
            revision
                .as_str()
                .and_then(|revision| revision.parse::<u64>().ok())
                .or_else(|| revision.as_u64())
        }) == Some(identity.revision)
}

fn legacy_file_action_result_matches(
    result: &serde_json::Value,
    revision: u64,
    status: &str,
) -> bool {
    result.get("namespace").is_none()
        && result.get("revision").and_then(serde_json::Value::as_u64) == Some(revision)
        && result.get("status").and_then(serde_json::Value::as_str) == Some(status)
}

fn clear_matching_requested_command(task: &mut TaskToolRecord, identity: &ProviderActionIdentity) {
    if identity.namespace == ProviderActionNamespace::Command
        && task.action_revision == identity.revision
    {
        task.requested_action = None;
    }
}

impl TaskAdapter for TaskFileStore {
    fn adapter_name(&self) -> &'static str {
        "file"
    }

    fn pull_tasks(&self) -> anyhow::Result<Vec<TaskSyncRecord>> {
        self.read_all_task_files().map(|items| {
            items
                .into_iter()
                .map(|external| {
                    let ownership = external.ownership.clone();
                    TaskSyncRecord {
                        ownership_revision: (external.updated_at.timestamp_millis().max(0) as u64)
                            .saturating_mul(1_000),
                        external,
                        ownership,
                        previous_ownership: None,
                        provider_rejection: None,
                    }
                })
                .collect()
        })
    }

    fn get_task(&self, id: Uuid) -> anyhow::Result<Option<TaskToolRecord>> {
        self.read_task_file(id)
    }

    fn persist_projection(
        &self,
        projection: &TaskProjection,
        _status_comment_id: Option<i64>,
    ) -> anyhow::Result<Option<i64>> {
        self.write_task_file(&projection.task)?;
        Ok(None)
    }

    fn update_checkpoint(
        &self,
        task_id: &str,
        checkpoint: &TaskCheckpoint,
        _work_item_id: Uuid,
        _comment_id: Option<i64>,
    ) -> anyhow::Result<Option<i64>> {
        let id = Uuid::parse_str(task_id)?;
        self.ensure_dir()?;
        let tmp = self.temp_checkpoint_path(id);
        durable_atomic_replace(
            &self.checkpoint_path(id),
            &tmp,
            &serde_json::to_vec_pretty(checkpoint)?,
        )?;
        Ok(None)
    }

    fn read_checkpoint(&self, task_id: &str) -> anyhow::Result<Option<TaskCheckpoint>> {
        let id = Uuid::parse_str(task_id)?;
        let path = self.checkpoint_path(id);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&std::fs::read(path)?)?))
    }

    fn set_ownership(&self, task_id: &str, ownership: &ProviderOwnership) -> anyhow::Result<()> {
        let Some(mut task) = self.read_task_file_by_task_id(task_id)? else {
            anyhow::bail!("task not found: {task_id}");
        };
        task.ownership = ownership.clone();
        task.updated_at = Utc::now();
        self.write_task_file(&task)
    }

    fn append_log(&self, task_id: &str, message: &str) -> anyhow::Result<()> {
        let mut task = self
            .read_task_file_by_task_id(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
        let context = task
            .context
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("task context must be a JSON object"))?;
        context
            .entry("provider_logs")
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("provider_logs must be an array"))?
            .push(serde_json::json!({"message": message, "at": Utc::now()}));
        self.write_task_file(&task)
    }

    fn reject_action(
        &self,
        task_id: &str,
        rejection: ProviderActionRejectionDelivery<'_>,
    ) -> anyhow::Result<()> {
        let ProviderActionRejectionDelivery {
            identity,
            legacy_revision,
            reason,
            valid_actions,
        } = rejection;
        let mut task = self
            .read_task_file_by_task_id(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
        let context = task
            .context
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("task context must be a JSON object"))?;
        let results = context
            .entry("action_results")
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("action_results must be an array"))?;
        if !results.iter().any(|result| {
            file_action_result_matches(result, identity)
                || legacy_revision.is_some_and(|revision| {
                    legacy_file_action_result_matches(result, revision, "rejected")
                })
        }) {
            results.push(serde_json::json!({
                "namespace": identity.namespace,
                "revision": identity.revision.to_string(),
                "status": "rejected",
                "reason": reason,
                "valid_actions": valid_actions,
            }));
        }
        clear_matching_requested_command(&mut task, identity);
        self.write_task_file(&task)
    }

    fn acknowledge_action(
        &self,
        task_id: &str,
        identity: &ProviderActionIdentity,
        legacy_revision: Option<u64>,
    ) -> anyhow::Result<()> {
        let mut task = self
            .read_task_file_by_task_id(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
        let context = task
            .context
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("task context must be a JSON object"))?;
        let results = context
            .entry("action_results")
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("action_results must be an array"))?;
        if !results.iter().any(|result| {
            (file_action_result_matches(result, identity)
                && result.get("status").and_then(serde_json::Value::as_str) == Some("applied"))
                || legacy_revision.is_some_and(|revision| {
                    legacy_file_action_result_matches(result, revision, "applied")
                })
        }) {
            results.push(serde_json::json!({
                "namespace": identity.namespace,
                "revision": identity.revision.to_string(),
                "status": "applied",
            }));
        }
        clear_matching_requested_command(&mut task, identity);
        self.write_task_file(&task)
    }

    fn poll_changes(&self) -> anyhow::Result<Vec<TaskChange>> {
        let mut last_poll = self
            .last_poll
            .lock()
            .map_err(|_| anyhow::anyhow!("task file poll lock poisoned"))?;
        let mut known = self
            .known_file_mtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("task file index lock poisoned"))?;

        let current = self.read_task_file_mtimes()?;
        let mut changes = Vec::new();

        for (task_id, modified) in &current {
            if !known.contains_key(task_id) {
                changes.push(TaskChange {
                    task_id: task_id.clone(),
                    change_type: TaskChangeType::Created,
                });
                continue;
            }

            if *modified > *last_poll {
                changes.push(TaskChange {
                    task_id: task_id.clone(),
                    change_type: TaskChangeType::Updated,
                });
            }
        }

        let current_ids: HashSet<&String> = current.keys().collect();
        for task_id in known.keys() {
            if !current_ids.contains(task_id) {
                changes.push(TaskChange {
                    task_id: task_id.clone(),
                    change_type: TaskChangeType::Deleted,
                });
            }
        }

        *known = current;
        *last_poll = SystemTime::now();

        Ok(changes)
    }

    fn update_task(&self, task_id: &str, update: TaskUpdate) -> anyhow::Result<()> {
        let mut task = self
            .read_task_file_by_task_id(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;

        if let Some(status) = update.status {
            task.triage_status = serde_json::from_value(serde_json::Value::String(status))?;
        }

        if let Some(labels) = update.labels {
            let context = task
                .context
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("task context must be a JSON object"))?;
            context.insert("labels".to_string(), serde_json::json!(labels));
        }

        if let Some(assignee) = update.assignee {
            let context = task
                .context
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("task context must be a JSON object"))?;
            context.insert("assignee".to_string(), serde_json::json!(assignee));
        }

        task.updated_at = Utc::now();
        self.write_task_file(&task)
    }

    fn create_task(&self, create: TaskCreate) -> anyhow::Result<String> {
        let id = Uuid::new_v4();
        let task = TaskToolRecord {
            id,
            ownership: ProviderOwnership::Unassigned,
            title: create.title,
            description: Some(create.description),
            workflow_id: None,
            triage_status: TaskTriageStatus::Ready,
            pending_human_action: false,
            current_human_gate: None,
            workflow_phase: None,
            blocked_reason: None,
            last_failure_reason: None,
            required_action_description: None,
            human_gate_state: None,
            valid_next_events: vec![],
            allowed_actions: vec![],
            context: serde_json::json!({"labels": create.labels}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };

        self.write_task_file(&task)?;
        Ok(id.to_string())
    }
}

fn read_all_tasks_from_dir(store_dir: &Path) -> anyhow::Result<Vec<TaskToolRecord>> {
    if !store_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(store_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".checkpoint.json"))
        {
            continue;
        }
        let is_json = path
            .extension()
            .and_then(|v| v.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        if !is_json {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "failed to read task file");
                continue;
            }
        };
        let task: TaskToolRecord = match serde_json::from_slice(&bytes) {
            Ok(task) => task,
            Err(task_err) => match serde_json::from_slice::<WorkItem>(&bytes) {
                Ok(item) => task_from_work_item(&item, None),
                Err(item_err) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %task_err,
                        fallback_error = %item_err,
                        "failed to parse task file as task record or work item"
                    );
                    continue;
                }
            },
        };
        out.push(task);
    }
    out.sort_by_key(|t| t.updated_at);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::task_adapter;

    #[test]
    fn per_task_file_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("task-files-{}", Uuid::new_v4()));
        let store = TaskFileStore::new(tmp.clone());
        let task = task_adapter::task_from_work_item(
            &task_adapter::work_item_from_create(
                &protocol::CreateWorkItemRequest {
                    workflow_id: "wf".into(),
                    session_id: None,
                    context: serde_json::json!({"k":"v"}),
                    repo: None,
                },
                Uuid::new_v4(),
                "wf".into(),
                "start".into(),
                "file",
            ),
            None,
        );
        store
            .persist_projection(&TaskProjection { task: task.clone() }, None)
            .expect("write task");

        let loaded = store.pull_tasks().expect("read tasks");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].external.id, task.id);

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn authoritative_checkpoint_roundtrip_is_durable_and_not_a_task_record() {
        let tmp = std::env::temp_dir().join(format!("task-files-{}", Uuid::new_v4()));
        let store = TaskFileStore::new(tmp.clone());
        let task_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        let checkpoint = TaskCheckpoint {
            current_step: "build".to_string(),
            phase: "executing".to_string(),
            workflow_id: "wf".to_string(),
            repo: None,
            pr_url: None,
            pr_status: None,
            human_gate: false,
            failure_info: None,
            updated_at: Utc::now(),
            human_gate_state: None,
            triage_status: Some("in_progress".to_string()),
            human_owner: Some("alice".to_string()),
            human_owner_provider_id: Some("42".to_string()),
            human_owners: vec![],
            dispatch_job_ids: vec![job_id],
            dispatch_authorizations: std::collections::BTreeMap::from([(
                job_id,
                "digest".to_string(),
            )]),
            authorization_pending: Some(CheckpointAuthorizationPending {
                work_item_id: task_id,
                request_digest: "pending-digest".to_string(),
                request: serde_json::json!({"redacted":"request"}),
            }),
            proposed_authorizations: vec![serde_json::json!({"request_digest":"proposed"})],
            generation: 3,
            checkpoint_hash: "hash".to_string(),
            semantic_hash: "semantic".to_string(),
        };
        store
            .update_checkpoint(&task_id.to_string(), &checkpoint, task_id, None)
            .expect("write checkpoint");

        let loaded = store
            .read_checkpoint(&task_id.to_string())
            .expect("read checkpoint")
            .expect("checkpoint");
        assert_eq!(loaded.generation, 3);
        assert_eq!(loaded.dispatch_job_ids, checkpoint.dispatch_job_ids);
        assert_eq!(
            loaded.dispatch_authorizations,
            checkpoint.dispatch_authorizations
        );
        assert_eq!(
            loaded
                .authorization_pending
                .as_ref()
                .map(|pending| pending.request_digest.as_str()),
            Some("pending-digest")
        );
        assert_eq!(
            loaded.proposed_authorizations,
            checkpoint.proposed_authorizations
        );
        assert!(!store.temp_checkpoint_path(task_id).exists());
        assert!(
            store
                .checkpoint_path(task_id)
                .metadata()
                .expect("metadata")
                .len()
                > 0
        );
        assert!(store.pull_tasks().expect("tasks").is_empty());
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn action_results_key_namespace_and_preserve_unrelated_command() {
        let tmp = std::env::temp_dir().join(format!("task-files-{}", Uuid::new_v4()));
        let store = TaskFileStore::new(tmp.clone());
        let mut task = task_adapter::task_from_work_item(
            &task_adapter::work_item_from_create(
                &protocol::CreateWorkItemRequest {
                    workflow_id: "wf".into(),
                    session_id: None,
                    context: serde_json::json!({}),
                    repo: None,
                },
                Uuid::new_v4(),
                "wf".into(),
                "review".into(),
                "file",
            ),
            None,
        );
        task.action_revision = u64::MAX;
        task.requested_action = Some(task_adapter::TaskHumanAction {
            kind: task_adapter::HumanActionKind::Approve,
            workflow_id: None,
            reason: None,
        });
        store.write_task_file(&task).expect("task file");
        let ownership = ProviderActionIdentity {
            namespace: ProviderActionNamespace::Ownership,
            revision: u64::MAX,
        };
        let command = ProviderActionIdentity {
            namespace: ProviderActionNamespace::Command,
            revision: u64::MAX,
        };

        store
            .acknowledge_action(&task.id.to_string(), &ownership, None)
            .expect("ownership acknowledgement");
        let after_ownership = store.read_task_file(task.id).expect("read").expect("task");
        assert!(after_ownership.requested_action.is_some());
        store
            .reject_action(
                &task.id.to_string(),
                ProviderActionRejectionDelivery {
                    identity: &command,
                    legacy_revision: None,
                    reason: "invalid",
                    valid_actions: &[],
                },
            )
            .expect("command rejection");
        store
            .acknowledge_action(&task.id.to_string(), &ownership, None)
            .expect("ownership dedup");

        let stored = store.read_task_file(task.id).expect("read").expect("task");
        assert!(stored.requested_action.is_none());
        let results = stored.context["action_results"]
            .as_array()
            .expect("action results");
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|result| {
            result["namespace"] == "ownership"
                && result["revision"]
                    .as_str()
                    .and_then(|revision| revision.parse::<u64>().ok())
                    == Some(u64::MAX)
                && result["status"] == "applied"
        }));
        assert!(results.iter().any(|result| {
            result["namespace"] == "command"
                && result["revision"]
                    .as_str()
                    .and_then(|revision| revision.parse::<u64>().ok())
                    == Some(u64::MAX)
                && result["status"] == "rejected"
        }));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn legacy_action_result_alias_prevents_duplicate_namespaced_result() {
        let tmp = std::env::temp_dir().join(format!("task-files-{}", Uuid::new_v4()));
        let store = TaskFileStore::new(tmp.clone());
        let mut task = task_adapter::task_from_work_item(
            &task_adapter::work_item_from_create(
                &protocol::CreateWorkItemRequest {
                    workflow_id: "wf".into(),
                    session_id: None,
                    context: serde_json::json!({
                        "action_results": [{"revision": 99, "status": "applied"}]
                    }),
                    repo: None,
                },
                Uuid::new_v4(),
                "wf".into(),
                "review".into(),
                "file",
            ),
            None,
        );
        task.action_revision = 7;
        task.requested_action = Some(task_adapter::TaskHumanAction {
            kind: task_adapter::HumanActionKind::Approve,
            workflow_id: None,
            reason: None,
        });
        store.write_task_file(&task).expect("task file");

        store
            .acknowledge_action(
                &task.id.to_string(),
                &ProviderActionIdentity {
                    namespace: ProviderActionNamespace::Ownership,
                    revision: 7,
                },
                Some(99),
            )
            .expect("legacy acknowledgement lookup");
        let stored = store.read_task_file(task.id).expect("read").expect("task");
        assert!(stored.requested_action.is_some());
        assert_eq!(
            stored.context["action_results"]
                .as_array()
                .expect("results")
                .len(),
            1
        );
        let _ = std::fs::remove_dir_all(tmp);
    }
}
