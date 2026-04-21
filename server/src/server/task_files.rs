use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use chrono::Utc;
use protocol::WorkItem;
use uuid::Uuid;

use super::task_adapter::{
    TaskAdapter, TaskChange, TaskChangeType, TaskCreate, TaskProjection, TaskSyncRecord,
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

    #[allow(dead_code)]
    fn read_task_file_mtimes(&self) -> anyhow::Result<HashMap<String, SystemTime>> {
        if !self.dir.exists() {
            return Ok(HashMap::new());
        }

        let mut out = HashMap::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
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

impl TaskAdapter for TaskFileStore {
    fn adapter_name(&self) -> &'static str {
        "file"
    }

    fn pull_tasks(&self) -> anyhow::Result<Vec<TaskSyncRecord>> {
        self.read_all_task_files().map(|items| {
            items
                .into_iter()
                .map(|external| TaskSyncRecord { external })
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
}
