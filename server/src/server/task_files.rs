use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use chrono::Utc;
use protocol::{ProviderOwnership, WorkItem};
use uuid::Uuid;

#[cfg(test)]
use super::task_adapter::{CheckpointAuthorizationPending, HumanActionKind, TaskHumanAction};
use super::task_adapter::{
    JobResultComment, ProviderActionIdentity, ProviderActionNamespace,
    ProviderActionRejectionDelivery, TaskAdapter, TaskChange, TaskChangeType, TaskCheckpoint,
    TaskCreate, TaskProjection, TaskSyncRecord, TaskToolRecord, TaskTriageStatus, TaskUpdate,
    task_from_work_item,
};

pub struct TaskFileStore {
    dir: PathBuf,
    state_dir: PathBuf,
    #[allow(dead_code)]
    last_poll: Mutex<SystemTime>,
    #[allow(dead_code)]
    known_file_mtimes: Mutex<HashMap<String, SystemTime>>,
}

impl TaskFileStore {
    #[cfg(test)]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let state_dir = dir.with_extension("server-private-state");
        Self::new_with_state_dir(dir, state_dir)
    }

    pub fn new_with_state_dir(dir: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            state_dir: state_dir.into(),
            last_poll: Mutex::new(SystemTime::UNIX_EPOCH),
            known_file_mtimes: Mutex::new(HashMap::new()),
        }
    }

    pub fn ensure_dir(&self) -> anyhow::Result<()> {
        self.ensure_dir_base()?;
        let deadline =
            super::task_adapter::ProviderOperationDeadline::from_now(self.operation_timeout());
        self.with_provider_lock_existing(deadline, || self.reap_stale_temps_locked())
    }

    fn ensure_dir_base(&self) -> anyhow::Result<()> {
        if self.state_dir == self.dir || self.state_dir.starts_with(&self.dir) {
            anyhow::bail!("TASK_STATE_DIR must be distinct from and outside TASK_DIR");
        }
        if self
            .state_dir
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            anyhow::bail!("TASK_STATE_DIR must not contain parent traversal");
        }
        validate_path_ancestors_without_symlinks(&self.dir)?;
        validate_path_ancestors_without_symlinks(&self.state_dir)?;
        let state_existed = self.state_dir.exists();
        std::fs::create_dir_all(&self.dir)?;
        std::fs::create_dir_all(&self.state_dir)?;
        let canonical_task = std::fs::canonicalize(&self.dir)?;
        let canonical_state = std::fs::canonicalize(&self.state_dir)?;
        if canonical_task == canonical_state
            || canonical_task.starts_with(&canonical_state)
            || canonical_state.starts_with(&canonical_task)
        {
            anyhow::bail!("TASK_DIR and TASK_STATE_DIR must not alias or contain each other");
        }
        let metadata = std::fs::symlink_metadata(&self.state_dir)?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            anyhow::bail!("TASK_STATE_DIR must be a regular directory without symlinks");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            use std::os::unix::fs::PermissionsExt;
            if !state_existed {
                std::fs::set_permissions(&self.state_dir, std::fs::Permissions::from_mode(0o700))?;
            }
            let state_metadata = std::fs::metadata(&self.state_dir)?;
            let task_metadata = std::fs::metadata(&self.dir)?;
            if state_metadata.mode() & 0o777 != 0o700 {
                anyhow::bail!("TASK_STATE_DIR must have exact mode 0700");
            }
            if task_metadata.mode() & 0o022 != 0 {
                anyhow::bail!("TASK_DIR must not be group/world writable");
            }
            if state_metadata.uid() != task_metadata.uid() {
                anyhow::bail!("TASK_DIR and TASK_STATE_DIR must have the same owner");
            }
            if state_metadata.dev() != task_metadata.dev() {
                anyhow::bail!("TASK_DIR and TASK_STATE_DIR must be on the same filesystem");
            }
        }
        let task_fs = reviewed_filesystem_type(&canonical_task)?;
        let state_fs = reviewed_filesystem_type(&canonical_state)?;
        if task_fs != state_fs {
            anyhow::bail!("TASK_DIR and TASK_STATE_DIR filesystem types differ");
        }
        tracing::info!(
            task_filesystem = task_fs,
            state_filesystem = state_fs,
            "validated file-adapter filesystem boundary"
        );
        Ok(())
    }

    fn write_task_file(&self, task: &TaskToolRecord) -> anyhow::Result<()> {
        let deadline =
            super::task_adapter::ProviderOperationDeadline::from_now(self.operation_timeout());
        self.with_provider_lock(deadline, || self.write_task_file_locked(task))
    }

    fn write_task_file_locked(&self, task: &TaskToolRecord) -> anyhow::Result<()> {
        self.ensure_dir_base()?;
        let path = self.task_path(task.id);
        let tmp = self.temp_task_path(task.id);
        let data = serde_json::to_vec_pretty(task)?;
        durable_atomic_replace(&path, &tmp, &data)
    }

    fn with_provider_lock<T>(
        &self,
        deadline: super::task_adapter::ProviderOperationDeadline,
        operation: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.ensure_dir_base()?;
        self.with_provider_lock_existing(deadline, operation)
    }

    fn with_provider_lock_existing<T>(
        &self,
        deadline: super::task_adapter::ProviderOperationDeadline,
        operation: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let lock_path = self.state_dir.join("provider.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        loop {
            deadline.check()?;
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        file_fault_hook("while_provider_lock_held")?;
        let result = operation();
        lock.unlock()?;
        result
    }

    fn reap_stale_temps_locked(&self) -> anyhow::Result<()> {
        const MAX_CANDIDATES: usize = 128;
        const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(60 * 60);
        let deadline =
            super::task_adapter::ProviderOperationDeadline::from_now(self.operation_timeout());
        for directory in [&self.dir, &self.state_dir] {
            let directory_metadata = std::fs::metadata(directory)?;
            let mut candidates = Vec::new();
            for entry in std::fs::read_dir(directory)? {
                deadline.check()?;
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("non-UTF-8 file in provider directory"))?;
                if !name.ends_with(".tmp") {
                    continue;
                }
                if parse_owned_temp_name(&name).is_none() {
                    continue;
                }
                candidates.push(name);
                if candidates.len() > MAX_CANDIDATES {
                    anyhow::bail!("too many temporary files in {}", directory.display());
                }
            }
            let removed = reap_directory_candidates(
                directory,
                &candidates,
                directory_metadata.uid(),
                directory_metadata.dev(),
                STALE_AFTER,
                deadline,
            )?;
            if removed > 0 {
                std::fs::File::open(directory)?.sync_all()?;
            }
        }
        Ok(())
    }

    fn mutate_task_locked<T>(
        &self,
        task_id: &str,
        operation: impl FnOnce(&mut TaskToolRecord) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let deadline =
            super::task_adapter::ProviderOperationDeadline::from_now(self.operation_timeout());
        self.with_provider_lock(deadline, || {
            let mut task = self
                .read_task_file_by_task_id(task_id)?
                .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
            let result = operation(&mut task)?;
            self.write_task_file_locked(&task)?;
            Ok(result)
        })
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
        anyhow::bail!("malformed task file: {}", path.display())
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
        self.dir.join(format!("{id}.json.{}.tmp", Uuid::new_v4()))
    }

    fn checkpoint_path(&self, id: Uuid) -> PathBuf {
        self.state_dir.join(format!("{id}.checkpoint.json"))
    }

    fn temp_checkpoint_path(&self, id: Uuid) -> PathBuf {
        self.state_dir
            .join(format!("{id}.checkpoint.json.{}.tmp", Uuid::new_v4()))
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

fn validate_path_ancestors_without_symlinks(path: &Path) -> anyhow::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!("path contains symlink component: {}", current.display())
            }
            Ok(metadata) if !metadata.file_type().is_dir() && current != path => {
                anyhow::bail!("path ancestor is not a directory: {}", current.display())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn parse_owned_temp_name(name: &str) -> Option<()> {
    let stem = name.strip_suffix(".tmp")?;
    let (base, nonce) = stem.rsplit_once('.')?;
    Uuid::parse_str(nonce).ok()?;
    if let Some(id) = base.strip_suffix(".json") {
        let id = id.strip_suffix(".checkpoint").unwrap_or(id);
        Uuid::parse_str(id).ok()?;
        return Some(());
    }
    let key = base.strip_prefix("terminal-")?;
    if !key.is_empty()
        && key
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
    {
        return Some(());
    }
    None
}

#[cfg(target_os = "linux")]
fn reap_directory_candidates(
    directory: &Path,
    candidates: &[String],
    expected_uid: u32,
    expected_device: u64,
    stale_after: std::time::Duration,
    deadline: super::task_adapter::ProviderOperationDeadline,
) -> anyhow::Result<usize> {
    let directory_c = std::ffi::CString::new(directory.as_os_str().as_bytes())?;
    // SAFETY: CString is NUL-terminated and flags request a directory descriptor only.
    let fd = unsafe {
        libc::open(
            directory_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let result = (|| -> anyhow::Result<usize> {
        let now = SystemTime::now();
        let mut removed = 0;
        for candidate in candidates {
            deadline.check()?;
            let name = std::ffi::CString::new(candidate.as_bytes())?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
            // SAFETY: fd is open, name is NUL-terminated, and stat is writable.
            if unsafe {
                libc::fstatat(
                    fd,
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: successful fstatat initialized stat.
            let stat = unsafe { stat.assume_init() };
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG
                || stat.st_uid != expected_uid
                || stat.st_dev != expected_device
                || stat.st_nlink != 1
            {
                continue;
            }
            let modified = SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(u64::try_from(stat.st_mtime).unwrap_or(0));
            if now.duration_since(modified).unwrap_or_default() < stale_after {
                continue;
            }
            // SAFETY: unlinkat is constrained to the open directory and validated basename.
            if unsafe { libc::unlinkat(fd, name.as_ptr(), 0) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            removed += 1;
        }
        Ok(removed)
    })();
    // SAFETY: fd was returned by open and is closed exactly once.
    unsafe { libc::close(fd) };
    result
}

#[cfg(not(target_os = "linux"))]
fn reap_directory_candidates(
    _directory: &Path,
    _candidates: &[String],
    _expected_uid: u32,
    _expected_device: u64,
    _stale_after: std::time::Duration,
    _deadline: super::task_adapter::ProviderOperationDeadline,
) -> anyhow::Result<usize> {
    anyhow::bail!("file adapter requires Linux descriptor-relative unlink semantics")
}

#[cfg(target_os = "linux")]
fn reviewed_filesystem_type(path: &Path) -> anyhow::Result<&'static str> {
    let test_temp_overlay = cfg!(test) && path.starts_with(std::env::temp_dir());
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `path` is NUL-terminated and `stats` points to writable memory.
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful statfs initialized the structure.
    let magic = unsafe { stats.assume_init() }.f_type as u64;
    let name = match magic {
        0xEF53 => "ext4",
        0x9123_683E => "btrfs",
        0x5846_5342 => "xfs",
        0x0102_1994 if ephemeral_filesystem_waiver(path.to_str()?) => "tmpfs-waived",
        0x0102_1994 => anyhow::bail!(
            "tmpfs is unsupported for durable file mode; checkpoints and terminal evidence must survive reboot"
        ),
        0x6969 => anyhow::bail!("NFS is unsupported for the file adapter"),
        0x6573_5546 => anyhow::bail!("FUSE is unsupported for the file adapter"),
        0xFF53_4D42 | 0xFE53_4D42 => {
            anyhow::bail!("CIFS/SMB is unsupported for the file adapter")
        }
        0x794C_7630 if test_temp_overlay || ephemeral_filesystem_waiver(path.to_str()?) => {
            "overlay-waived"
        }
        0x794C_7630 => anyhow::bail!("overlayfs is unsupported for production file mode"),
        _ => anyhow::bail!("unreviewed filesystem type 0x{magic:x}"),
    };
    Ok(name)
}

fn ephemeral_filesystem_waiver(path: &str) -> bool {
    #[cfg(test)]
    {
        Path::new(path).starts_with(std::env::temp_dir())
    }
    #[cfg(not(test))]
    {
        let _ = path;
        false
    }
}

#[cfg(not(target_os = "linux"))]
fn reviewed_filesystem_type(_path: &Path) -> anyhow::Result<&'static str> {
    anyhow::bail!("file adapter requires reviewed Linux statfs semantics")
}

fn durable_atomic_replace(path: &Path, temp_path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("checkpoint path has no parent: {}", path.display()))?;
    let mut temp = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(temp_path)?;
    if let Err(error) = (|| -> anyhow::Result<()> {
        temp.write_all(data)?;
        temp.flush()?;
        file_fault_hook("before_file_fsync")?;
        temp.sync_all()?;
        file_fault_hook("after_file_fsync")?;
        Ok(())
    })() {
        drop(temp);
        let _ = std::fs::remove_file(temp_path);
        return Err(error);
    }
    drop(temp);
    std::fs::rename(temp_path, path)?;
    file_fault_hook("after_rename")?;
    file_fault_hook("before_directory_fsync")?;
    std::fs::File::open(parent)?.sync_all()?;
    file_fault_hook("after_directory_fsync")?;
    Ok(())
}

#[cfg(test)]
fn file_fault_hook(boundary: &str) -> anyhow::Result<()> {
    let Ok(spec) = std::env::var("AGENT_HUB_TEST_FILE_FAULT") else {
        return Ok(());
    };
    let Some((action, selected)) = spec.split_once(':') else {
        anyhow::bail!("invalid AGENT_HUB_TEST_FILE_FAULT");
    };
    if selected != boundary {
        return Ok(());
    }
    if let Ok(path) = std::env::var("AGENT_HUB_TEST_FILE_FAULT_READY") {
        std::fs::write(path, boundary)?;
    }
    match action {
        "error" => anyhow::bail!("injected file fault at {boundary}"),
        "abort" => std::process::abort(),
        "pause" => {
            let release = std::env::var("AGENT_HUB_TEST_FILE_FAULT_RELEASE")?;
            while !Path::new(&release).exists() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(())
        }
        _ => anyhow::bail!("unknown file fault action '{action}'"),
    }
}

#[cfg(not(test))]
fn file_fault_hook(_boundary: &str) -> anyhow::Result<()> {
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

fn validate_action_delivery_identity(
    task: &TaskToolRecord,
    identity: &ProviderActionIdentity,
) -> anyhow::Result<()> {
    if identity.namespace == ProviderActionNamespace::Command
        && task.action_revision != identity.revision
    {
        anyhow::bail!(
            "provider action revision mismatch: current={}, delivered={}",
            task.action_revision,
            identity.revision
        );
    }
    Ok(())
}

impl TaskAdapter for TaskFileStore {
    fn adapter_name(&self) -> &'static str {
        "file"
    }

    fn operation_timeout(&self) -> std::time::Duration {
        #[cfg(test)]
        if let Ok(value) = std::env::var("AGENT_HUB_TEST_FILE_HELPER_DEADLINE_MS") {
            return std::time::Duration::from_millis(
                value.parse().expect("numeric helper deadline"),
            );
        }
        std::time::Duration::from_secs(30)
    }

    fn pull_tasks(&self) -> anyhow::Result<Vec<TaskSyncRecord>> {
        let deadline =
            super::task_adapter::ProviderOperationDeadline::from_now(self.operation_timeout());
        self.with_provider_lock(deadline, || {
            self.reap_stale_temps_locked()?;
            self.read_all_task_files()
        })
        .map(|items| {
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
        let deadline =
            super::task_adapter::ProviderOperationDeadline::from_now(self.operation_timeout());
        self.with_provider_lock(deadline, || {
            let mut merged = projection.task.clone();
            if let Some(current) = self.read_task_file(merged.id)? {
                if current.action_revision >= merged.action_revision {
                    merged.requested_action = current.requested_action;
                    merged.action_revision = current.action_revision;
                }
                if let (Some(target), Some(source)) =
                    (merged.context.as_object_mut(), current.context.as_object())
                {
                    for key in ["labels", "assignee", "provider_logs", "action_results"] {
                        if let Some(value) = source.get(key) {
                            target.insert(key.to_string(), value.clone());
                        }
                    }
                }
            }
            self.write_task_file_locked(&merged)
        })?;
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
        let data = serde_json::to_vec_pretty(checkpoint)?;
        let deadline =
            super::task_adapter::ProviderOperationDeadline::from_now(self.operation_timeout());
        self.with_provider_lock(deadline, || {
            let tmp = self.temp_checkpoint_path(id);
            durable_atomic_replace(&self.checkpoint_path(id), &tmp, &data)
        })?;
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
        let deadline =
            super::task_adapter::ProviderOperationDeadline::from_now(self.operation_timeout());
        self.with_provider_lock(deadline, || {
            let Some(mut task) = self.read_task_file_by_task_id(task_id)? else {
                anyhow::bail!("task not found: {task_id}");
            };
            task.ownership = ownership.clone();
            task.updated_at = Utc::now();
            self.write_task_file_locked(&task)
        })
    }

    fn append_log(&self, task_id: &str, message: &str) -> anyhow::Result<()> {
        self.mutate_task_locked(task_id, |task| {
            task.context
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("task context must be a JSON object"))?
                .entry("provider_logs")
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("provider_logs must be an array"))?
                .push(serde_json::json!({"message": message, "at": Utc::now()}));
            Ok(())
        })
    }

    fn append_log_idempotent(
        &self,
        task_id: &str,
        idempotency_key: &str,
        message: &str,
    ) -> anyhow::Result<()> {
        let deadline =
            super::task_adapter::ProviderOperationDeadline::from_now(self.operation_timeout());
        self.with_provider_lock(deadline, || {
            let mut task = self
                .read_task_file_by_task_id(task_id)?
                .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
            let logs = task
                .context
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("task context must be a JSON object"))?
                .entry("provider_logs")
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("provider_logs must be an array"))?;
            if logs.iter().any(|entry| {
                entry
                    .get("idempotency_key")
                    .and_then(serde_json::Value::as_str)
                    == Some(idempotency_key)
            }) {
                return Ok(());
            }
            logs.push(serde_json::json!({
                "idempotency_key": idempotency_key,
                "message": message,
                "at": Utc::now(),
            }));
            self.write_task_file_locked(&task)
        })
    }

    fn write_job_result_comment(
        &self,
        result: JobResultComment<'_>,
    ) -> anyhow::Result<Option<i64>> {
        let Some(key) = result.idempotency_key else {
            return Ok(None);
        };
        if !key
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, ':' | '-' | '_'))
        {
            anyhow::bail!("invalid terminal idempotency key");
        }
        let deadline =
            super::task_adapter::ProviderOperationDeadline::from_now(self.operation_timeout());
        let record = serde_json::json!({
            "idempotency_key": key,
            "task_id": result.task_id,
            "step_id": result.step_id,
            "work_item_id": result.work_item_id,
            "succeeded": result.succeeded,
            "output": result.output,
            "context": result.context,
        });
        let data = serde_json::to_vec_pretty(&record)?;
        self.with_provider_lock(deadline, || {
            let path = self
                .state_dir
                .join(format!("terminal-{}.json", key.replace(':', "_")));
            if path.exists() {
                if std::fs::read(&path)? == data {
                    return Ok(None);
                }
                anyhow::bail!("conflicting terminal evidence for {key}");
            }
            let tmp = self.state_dir.join(format!(
                "terminal-{}.{}.tmp",
                key.replace(':', "_"),
                Uuid::new_v4()
            ));
            durable_atomic_replace(&path, &tmp, &data)?;
            Ok(None)
        })
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
        self.mutate_task_locked(task_id, |task| {
            validate_action_delivery_identity(task, identity)?;
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
            clear_matching_requested_command(task, identity);
            Ok(())
        })
    }

    fn acknowledge_action(
        &self,
        task_id: &str,
        identity: &ProviderActionIdentity,
        legacy_revision: Option<u64>,
    ) -> anyhow::Result<()> {
        self.mutate_task_locked(task_id, |task| {
            validate_action_delivery_identity(task, identity)?;
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
            clear_matching_requested_command(task, identity);
            Ok(())
        })
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
        self.mutate_task_locked(task_id, |task| {
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
            Ok(())
        })
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
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            anyhow::bail!("unexpected non-regular task file: {}", path.display());
        }
        #[cfg(unix)]
        if std::os::unix::fs::MetadataExt::nlink(&metadata) != 1 {
            anyhow::bail!("task file has unexpected hard links: {}", path.display());
        }
        let bytes = std::fs::read(&path)?;
        let task: TaskToolRecord = match serde_json::from_slice(&bytes) {
            Ok(task) => task,
            Err(task_err) => match serde_json::from_slice::<WorkItem>(&bytes) {
                Ok(item) => task_from_work_item(&item, None),
                Err(item_err) => anyhow::bail!(
                    "malformed task file {}: task={task_err}; work_item={item_err}",
                    path.display()
                ),
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
    use std::process::{Child, Command, ExitStatus, Output, Stdio};
    use std::time::{Duration, Instant};

    struct SubprocessFixture {
        root: PathBuf,
        tasks: PathBuf,
        state: PathBuf,
    }

    impl SubprocessFixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("{name}-{}", Uuid::new_v4()));
            Self {
                tasks: root.join("tasks"),
                state: root.join("state"),
                root,
            }
        }

        fn store(&self) -> TaskFileStore {
            TaskFileStore::new_with_state_dir(&self.tasks, &self.state)
        }

        fn helper_command(&self, operation: &str) -> Command {
            let mut command = Command::new(std::env::current_exe().expect("current test binary"));
            command
                .arg("server::task_files::tests::subprocess_file_adapter_helper")
                .arg("--ignored")
                .arg("--exact")
                .arg("--nocapture")
                .env("AGENT_HUB_FILE_HELPER", "1")
                .env("AGENT_HUB_FILE_HELPER_OP", operation)
                .env("TASK_DIR", &self.tasks)
                .env("TASK_STATE_DIR", &self.state);
            command
        }

        fn task(&self, id: Uuid, title: &str) -> TaskToolRecord {
            let mut task = task_adapter::task_from_work_item(
                &task_adapter::work_item_from_create(
                    &protocol::CreateWorkItemRequest {
                        workflow_id: "wf".into(),
                        session_id: None,
                        context: serde_json::json!({"preserved": "original"}),
                        repo: None,
                    },
                    id,
                    "wf".into(),
                    "start".into(),
                    "file",
                ),
                None,
            );
            task.title = title.to_string();
            task
        }
    }

    impl Drop for SubprocessFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn wait_for_child(child: &mut Child, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait().expect("poll child") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("subprocess exceeded {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_path(path: &Path, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn run_helper(mut command: Command, timeout: Duration) -> Output {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn helper");
        let status = wait_for_child(&mut child, timeout);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        use std::io::Read;
        child
            .stdout
            .take()
            .expect("helper stdout")
            .read_to_end(&mut stdout)
            .expect("read helper stdout");
        child
            .stderr
            .take()
            .expect("helper stderr")
            .read_to_end(&mut stderr)
            .expect("read helper stderr");
        Output {
            status,
            stdout,
            stderr,
        }
    }

    fn assert_helper_success(output: &Output) {
        assert!(
            output.status.success(),
            "helper failed: status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "subprocess helper; requires AGENT_HUB_FILE_HELPER=1"]
    fn subprocess_file_adapter_helper() {
        if std::env::var("AGENT_HUB_FILE_HELPER").as_deref() != Ok("1") {
            return;
        }
        let task_dir = PathBuf::from(std::env::var("TASK_DIR").expect("TASK_DIR"));
        let state_dir = PathBuf::from(std::env::var("TASK_STATE_DIR").expect("TASK_STATE_DIR"));
        let store = TaskFileStore::new_with_state_dir(task_dir, state_dir);
        store.ensure_dir().expect("validate helper paths");
        match std::env::var("AGENT_HUB_FILE_HELPER_OP")
            .unwrap_or_else(|_| "hold-lock".to_string())
            .as_str()
        {
            "ensure" => {}
            "pull" => {
                store.pull_task_snapshot().expect("pull task snapshot");
            }
            "hold-lock" => store
                .with_provider_lock(
                    super::super::task_adapter::ProviderOperationDeadline::from_now(
                        std::env::var("AGENT_HUB_TEST_FILE_HELPER_DEADLINE_MS")
                            .ok()
                            .map(|value| value.parse().expect("numeric helper deadline"))
                            .map(std::time::Duration::from_millis)
                            .unwrap_or(std::time::Duration::from_secs(30)),
                    ),
                    || Ok(()),
                )
                .expect("hold provider lock"),
            "mutate" => {
                let task_id = std::env::var("AGENT_HUB_FILE_HELPER_TASK_ID").expect("task id");
                let key = std::env::var("AGENT_HUB_FILE_HELPER_KEY").expect("key");
                let message = std::env::var("AGENT_HUB_FILE_HELPER_MESSAGE").expect("message");
                let revision = std::env::var("AGENT_HUB_FILE_HELPER_REVISION")
                    .expect("revision")
                    .parse()
                    .expect("numeric revision");
                store
                    .append_log_idempotent(&task_id, &key, &message)
                    .expect("append idempotent log");
                store
                    .acknowledge_action(
                        &task_id,
                        &ProviderActionIdentity {
                            namespace: ProviderActionNamespace::Ownership,
                            revision,
                        },
                        None,
                    )
                    .expect("acknowledge action");
            }
            "set-title" => {
                let task_id = std::env::var("AGENT_HUB_FILE_HELPER_TASK_ID").expect("task id");
                let title = std::env::var("AGENT_HUB_FILE_HELPER_TITLE").expect("title");
                store
                    .mutate_task_locked(&task_id, |task| {
                        task.title = title;
                        Ok(())
                    })
                    .expect("set title");
            }
            other => panic!("unknown AGENT_HUB_FILE_HELPER_OP '{other}'"),
        }
    }

    #[test]
    fn independent_processes_serialize_distinct_task_rmw_updates() {
        let fixture = SubprocessFixture::new("task-file-concurrent-rmw");
        let store = fixture.store();
        let task_id = Uuid::new_v4();
        store
            .write_task_file(&fixture.task(task_id, "original"))
            .expect("initial task");
        let ready = fixture.root.join("first-ready");
        let release = fixture.root.join("first-release");

        let mut first = fixture.helper_command("mutate");
        first
            .env("AGENT_HUB_FILE_HELPER_TASK_ID", task_id.to_string())
            .env("AGENT_HUB_FILE_HELPER_KEY", "delivery-one")
            .env("AGENT_HUB_FILE_HELPER_MESSAGE", "first")
            .env("AGENT_HUB_FILE_HELPER_REVISION", "11")
            .env(
                "AGENT_HUB_TEST_FILE_FAULT",
                "pause:while_provider_lock_held",
            )
            .env("AGENT_HUB_TEST_FILE_FAULT_READY", &ready)
            .env("AGENT_HUB_TEST_FILE_FAULT_RELEASE", &release)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut first = first.spawn().expect("spawn first mutation");
        wait_for_path(&ready, Duration::from_secs(5));

        let mut second = fixture.helper_command("mutate");
        second
            .env("AGENT_HUB_FILE_HELPER_TASK_ID", task_id.to_string())
            .env("AGENT_HUB_FILE_HELPER_KEY", "delivery-two")
            .env("AGENT_HUB_FILE_HELPER_MESSAGE", "second")
            .env("AGENT_HUB_FILE_HELPER_REVISION", "12")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut second = second.spawn().expect("spawn second mutation");
        std::fs::write(&release, b"release").expect("release first mutation");
        assert!(wait_for_child(&mut first, Duration::from_secs(5)).success());
        assert!(wait_for_child(&mut second, Duration::from_secs(5)).success());

        let stored = store.read_task_file(task_id).unwrap().unwrap();
        assert_eq!(stored.context["preserved"], "original");
        let logs = stored.context["provider_logs"].as_array().expect("logs");
        assert_eq!(logs.len(), 2);
        assert!(
            logs.iter()
                .any(|entry| entry["idempotency_key"] == "delivery-one")
        );
        assert!(
            logs.iter()
                .any(|entry| entry["idempotency_key"] == "delivery-two")
        );
        let actions = stored.context["action_results"]
            .as_array()
            .expect("actions");
        assert_eq!(actions.len(), 2);
        assert!(actions.iter().any(|entry| entry["revision"] == "11"));
        assert!(actions.iter().any(|entry| entry["revision"] == "12"));
    }

    #[test]
    fn aborted_lock_owner_releases_os_lock_for_next_process() {
        let fixture = SubprocessFixture::new("task-file-aborted-lock");
        fixture.store().ensure_dir().expect("fixture dirs");
        let ready = fixture.root.join("owner-ready");
        let mut owner = fixture.helper_command("hold-lock");
        owner
            .env(
                "AGENT_HUB_TEST_FILE_FAULT",
                "abort:while_provider_lock_held",
            )
            .env("AGENT_HUB_TEST_FILE_FAULT_READY", &ready)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut owner = owner.spawn().expect("spawn lock owner");
        wait_for_path(&ready, Duration::from_secs(5));
        assert!(!wait_for_child(&mut owner, Duration::from_secs(5)).success());

        let output = run_helper(fixture.helper_command("hold-lock"), Duration::from_secs(5));
        assert_helper_success(&output);
    }

    #[test]
    fn aborts_at_atomic_replace_boundaries_leave_only_permitted_snapshots() {
        let cases: [(&str, &[&str]); 5] = [
            ("before_file_fsync", &["old"]),
            ("after_file_fsync", &["old"]),
            ("after_rename", &["old", "new"]),
            ("before_directory_fsync", &["old", "new"]),
            ("after_directory_fsync", &["new"]),
        ];
        for (boundary, permitted) in cases {
            let fixture = SubprocessFixture::new(&format!("task-file-{boundary}"));
            let store = fixture.store();
            let task_id = Uuid::new_v4();
            store
                .write_task_file(&fixture.task(task_id, "old"))
                .expect("old snapshot");
            let ready = fixture.root.join("fault-ready");
            let mut command = fixture.helper_command("set-title");
            command
                .env("AGENT_HUB_FILE_HELPER_TASK_ID", task_id.to_string())
                .env("AGENT_HUB_FILE_HELPER_TITLE", "new")
                .env("AGENT_HUB_TEST_FILE_FAULT", format!("abort:{boundary}"))
                .env("AGENT_HUB_TEST_FILE_FAULT_READY", &ready);
            let output = run_helper(command, Duration::from_secs(5));
            assert!(!output.status.success(), "{boundary} helper did not abort");
            wait_for_path(&ready, Duration::from_secs(1));

            let restart = run_helper(fixture.helper_command("pull"), Duration::from_secs(5));
            assert_helper_success(&restart);
            let bytes = std::fs::read(store.task_path(task_id)).expect("canonical snapshot");
            let snapshot: TaskToolRecord = serde_json::from_slice(&bytes)
                .unwrap_or_else(|error| panic!("{boundary} left corrupt JSON: {error}"));
            assert!(
                permitted.contains(&snapshot.title.as_str()),
                "{boundary} produced disallowed snapshot {:?}; permitted={permitted:?}",
                snapshot.title
            );
        }
    }

    #[test]
    fn stale_temp_reaping_requires_valid_owned_name_and_regular_file() {
        let fixture = SubprocessFixture::new("task-file-stale-temp");
        let store = fixture.store();
        let task_id = Uuid::new_v4();
        store
            .write_task_file(&fixture.task(task_id, "canonical"))
            .expect("canonical task");
        let valid = fixture
            .tasks
            .join(format!("{task_id}.json.{}.tmp", Uuid::new_v4()));
        let fresh = fixture
            .tasks
            .join(format!("{task_id}.json.{}.tmp", Uuid::new_v4()));
        let malformed = fixture.tasks.join(format!("{task_id}.json.not-a-uuid.tmp"));
        std::fs::write(&valid, b"stale unique temporary contents").expect("valid stale temp");
        std::fs::write(&fresh, b"fresh unique temporary contents").expect("fresh temp");
        std::fs::write(&malformed, b"unowned temp").expect("malformed temp");
        let stale_times = std::fs::FileTimes::new()
            .set_accessed(SystemTime::UNIX_EPOCH)
            .set_modified(SystemTime::UNIX_EPOCH);
        std::fs::File::open(&valid)
            .unwrap()
            .set_times(stale_times)
            .expect("age valid temp");
        std::fs::File::open(&malformed)
            .unwrap()
            .set_times(stale_times)
            .expect("age malformed temp");
        #[cfg(unix)]
        let linked = {
            let path = fixture
                .tasks
                .join(format!("{task_id}.json.{}.tmp", Uuid::new_v4()));
            std::os::unix::fs::symlink(&malformed, &path).expect("symlink temp");
            Some(path)
        };
        #[cfg(unix)]
        let hardlinked = {
            let backing = fixture.root.join("hardlink-backing");
            let path = fixture
                .tasks
                .join(format!("{task_id}.json.{}.tmp", Uuid::new_v4()));
            std::fs::write(&backing, b"hardlinked temp").expect("hardlink backing");
            std::fs::hard_link(&backing, &path).expect("hardlinked temp");
            std::fs::File::open(&path)
                .unwrap()
                .set_times(stale_times)
                .expect("age hardlinked temp");
            Some(path)
        };
        #[cfg(not(unix))]
        let linked: Option<PathBuf> = None;
        #[cfg(not(unix))]
        let hardlinked: Option<PathBuf> = None;

        let output = run_helper(fixture.helper_command("pull"), Duration::from_secs(5));
        assert_helper_success(&output);
        assert!(malformed.exists(), "invalid temp name must not be reaped");
        if let Some(linked) = linked {
            assert!(linked.exists(), "non-regular temp must not be reaped");
        }
        if let Some(hardlinked) = hardlinked {
            assert!(hardlinked.exists(), "hardlinked temp must not be reaped");
        }
        assert!(fresh.exists(), "fresh owned temp must not be reaped");
        assert!(
            !valid.exists(),
            "valid uniquely named stale temp was not reaped"
        );
        assert_eq!(
            store.read_task_file(task_id).unwrap().unwrap().title,
            "canonical"
        );
    }

    #[test]
    fn malformed_canonical_file_fails_subprocess_snapshot_before_inference() {
        let fixture = SubprocessFixture::new("task-file-malformed-canonical");
        let store = fixture.store();
        store.ensure_dir().expect("fixture dirs");
        let task_id = Uuid::new_v4();
        let path = store.task_path(task_id);
        std::fs::write(&path, b"{not-json").expect("malformed canonical file");

        let output = run_helper(fixture.helper_command("pull"), Duration::from_secs(5));
        assert!(!output.status.success());
        assert!(path.exists(), "failed snapshot must not infer deletion");
        assert_eq!(std::fs::read(path).unwrap(), b"{not-json");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_unsafe_paths_links_modes_and_filesystems() {
        use std::os::unix::fs::PermissionsExt;

        let equal = SubprocessFixture::new("task-file-equal-dir");
        let same = TaskFileStore::new_with_state_dir(&equal.tasks, &equal.tasks);
        assert!(same.ensure_dir().is_err());

        let nested = SubprocessFixture::new("task-file-nested-dir");
        let nested_store =
            TaskFileStore::new_with_state_dir(&nested.tasks, nested.tasks.join("private"));
        assert!(nested_store.ensure_dir().is_err());

        let symlinked = SubprocessFixture::new("task-file-symlink-dir");
        std::fs::create_dir_all(&symlinked.root).unwrap();
        let real_tasks = symlinked.root.join("real-tasks");
        std::fs::create_dir(&real_tasks).unwrap();
        std::os::unix::fs::symlink(&real_tasks, &symlinked.tasks).unwrap();
        assert!(symlinked.store().ensure_dir().is_err());

        let bad_mode = SubprocessFixture::new("task-file-bad-mode");
        std::fs::create_dir_all(&bad_mode.tasks).unwrap();
        std::fs::create_dir(&bad_mode.state).unwrap();
        std::fs::set_permissions(&bad_mode.state, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(bad_mode.store().ensure_dir().is_err());

        let linked_file = SubprocessFixture::new("task-file-linked-canonical");
        let linked_store = linked_file.store();
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        linked_store
            .write_task_file(&linked_file.task(first_id, "first"))
            .unwrap();
        std::fs::hard_link(
            linked_store.task_path(first_id),
            linked_store.task_path(second_id),
        )
        .unwrap();
        assert!(linked_store.pull_tasks().is_err());

        assert!(reviewed_filesystem_type(Path::new("/proc")).is_err());
    }

    #[test]
    fn provider_lock_outlives_delivery_lease_and_replay_is_idempotent() {
        const TEST_OUTBOX_LEASE: Duration = Duration::from_millis(100);
        const CONTENDER_DEADLINE: Duration = Duration::from_millis(300);
        let fixture = SubprocessFixture::new("task-file-lease-fence");
        let store = fixture.store();
        let task_id = Uuid::new_v4();
        store
            .write_task_file(&fixture.task(task_id, "original"))
            .expect("initial task");
        let ready = fixture.root.join("lease-owner-ready");
        let release = fixture.root.join("lease-owner-release");
        let configure = |command: &mut Command| {
            command
                .env("AGENT_HUB_FILE_HELPER_TASK_ID", task_id.to_string())
                .env("AGENT_HUB_FILE_HELPER_KEY", "terminal-delivery")
                .env("AGENT_HUB_FILE_HELPER_MESSAGE", "terminal result")
                .env("AGENT_HUB_FILE_HELPER_REVISION", "21");
        };

        let mut owner = fixture.helper_command("mutate");
        configure(&mut owner);
        owner
            .env(
                "AGENT_HUB_TEST_FILE_FAULT",
                "pause:while_provider_lock_held",
            )
            .env("AGENT_HUB_TEST_FILE_FAULT_READY", &ready)
            .env("AGENT_HUB_TEST_FILE_FAULT_RELEASE", &release)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut owner = owner.spawn().expect("spawn lease owner");
        wait_for_path(&ready, Duration::from_secs(5));

        let mut replay = fixture.helper_command("mutate");
        configure(&mut replay);
        replay
            .env(
                "AGENT_HUB_TEST_FILE_HELPER_DEADLINE_MS",
                CONTENDER_DEADLINE.as_millis().to_string(),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut replay = replay.spawn().expect("spawn replay");
        let replay_started = Instant::now();
        let replay_status = wait_for_child(&mut replay, Duration::from_secs(2));
        assert!(replay_started.elapsed() >= TEST_OUTBOX_LEASE);
        assert!(!replay_status.success());
        std::fs::write(release, b"release").expect("release owner");
        assert!(wait_for_child(&mut owner, Duration::from_secs(10)).success());

        let mut retry = fixture.helper_command("mutate");
        configure(&mut retry);
        assert_helper_success(&run_helper(retry, Duration::from_secs(10)));

        let stored = store.read_task_file(task_id).unwrap().unwrap();
        assert_eq!(stored.context["provider_logs"].as_array().unwrap().len(), 1);
        assert_eq!(
            stored.context["action_results"].as_array().unwrap().len(),
            1
        );
        assert_eq!(stored.context["preserved"], "original");
    }

    #[test]
    fn subprocess_helper_is_inert_without_explicit_gate() {
        let fixture = SubprocessFixture::new("task-file-helper-gate");
        let mut command = Command::new(std::env::current_exe().expect("current test binary"));
        command
            .arg("server::task_files::tests::subprocess_file_adapter_helper")
            .arg("--ignored")
            .arg("--exact")
            .env_remove("AGENT_HUB_FILE_HELPER")
            .env("AGENT_HUB_FILE_HELPER_OP", "ensure")
            .env("TASK_DIR", &fixture.tasks)
            .env("TASK_STATE_DIR", &fixture.state);
        assert_helper_success(&run_helper(command, Duration::from_secs(5)));
        assert!(!fixture.tasks.exists());
        assert!(!fixture.state.exists());
    }

    #[test]
    fn authoritative_state_is_separate_from_human_task_projection() {
        let root = std::env::temp_dir().join(format!("task-state-separation-{}", Uuid::new_v4()));
        let tasks = root.join("tasks");
        let state = root.join("private");
        let store = TaskFileStore::new_with_state_dir(&tasks, &state);
        store.ensure_dir().unwrap();
        let id = Uuid::new_v4();
        let checkpoint = task_adapter::TaskCheckpoint {
            current_step: "done".to_string(),
            phase: "terminal".to_string(),
            workflow_id: "wf".to_string(),
            repo: None,
            pr_url: None,
            pr_status: None,
            human_gate: false,
            failure_info: None,
            updated_at: Utc::now(),
            human_gate_state: None,
            triage_status: None,
            human_owner: None,
            human_owner_provider_id: None,
            human_owners: Vec::new(),
            dispatch_job_ids: Vec::new(),
            dispatch_authorizations: std::collections::BTreeMap::new(),
            authorization_pending: None,
            proposed_authorizations: Vec::new(),
            generation: 1,
            checkpoint_hash: "hash".to_string(),
            semantic_hash: "semantic".to_string(),
        };
        store
            .update_checkpoint(&id.to_string(), &checkpoint, id, None)
            .unwrap();
        assert!(state.join(format!("{id}.checkpoint.json")).is_file());
        assert!(!tasks.join(format!("{id}.checkpoint.json")).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn owned_temp_grammar_is_strict() {
        let id = Uuid::new_v4();
        let nonce = Uuid::new_v4();
        assert!(parse_owned_temp_name(&format!("{id}.json.{nonce}.tmp")).is_some());
        assert!(parse_owned_temp_name(&format!("{id}.checkpoint.json.{nonce}.tmp")).is_some());
        assert!(parse_owned_temp_name(&format!("terminal-terminal_{id}.{nonce}.tmp")).is_some());
        for invalid in [
            format!("{id}.json.tmp"),
            format!("../{id}.json.{nonce}.tmp"),
            format!("anything.{nonce}.tmp"),
            format!("terminal-bad.name.{nonce}.tmp"),
        ] {
            assert!(parse_owned_temp_name(&invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn projection_preserves_unacknowledged_same_or_newer_action_revision() {
        let fixture = SubprocessFixture::new("task-action-projection-merge");
        let id = Uuid::new_v4();
        let mut current = fixture.task(id, "current");
        current.action_revision = 5;
        current.requested_action = Some(TaskHumanAction {
            kind: HumanActionKind::Approve,
            workflow_id: None,
            reason: None,
        });
        fixture.store().write_task_file(&current).unwrap();

        for projection_revision in [4, 5] {
            let mut projection = fixture.task(id, "projection");
            projection.action_revision = projection_revision;
            projection.requested_action = None;
            fixture
                .store()
                .persist_projection(&TaskProjection { task: projection }, None)
                .unwrap();
            let stored = fixture.store().read_task_file(id).unwrap().unwrap();
            assert_eq!(stored.action_revision, 5);
            assert_eq!(
                stored
                    .requested_action
                    .as_ref()
                    .map(|action| action.kind.clone()),
                Some(HumanActionKind::Approve)
            );
        }

        let stale = ProviderActionIdentity {
            namespace: ProviderActionNamespace::Command,
            revision: 4,
        };
        assert!(
            fixture
                .store()
                .acknowledge_action(&id.to_string(), &stale, None)
                .is_err()
        );
        assert_eq!(
            fixture
                .store()
                .read_task_file(id)
                .unwrap()
                .unwrap()
                .requested_action
                .as_ref()
                .map(|action| action.kind.clone()),
            Some(HumanActionKind::Approve)
        );

        let exact = ProviderActionIdentity {
            namespace: ProviderActionNamespace::Command,
            revision: 5,
        };
        fixture
            .store()
            .acknowledge_action(&id.to_string(), &exact, None)
            .unwrap();
        assert!(
            fixture
                .store()
                .read_task_file(id)
                .unwrap()
                .unwrap()
                .requested_action
                .is_none()
        );
    }

    #[test]
    fn acknowledged_revision_cannot_be_lowered_resurrected_or_cleared_by_projection() {
        let fixture = SubprocessFixture::new("task-action-ack-projection");
        let id = Uuid::new_v4();
        let approve = || TaskHumanAction {
            kind: HumanActionKind::Approve,
            workflow_id: None,
            reason: None,
        };
        let mut current = fixture.task(id, "current");
        current.action_revision = 5;
        current.requested_action = Some(approve());
        fixture.store().write_task_file(&current).unwrap();
        let exact = ProviderActionIdentity {
            namespace: ProviderActionNamespace::Command,
            revision: 5,
        };
        fixture
            .store()
            .acknowledge_action(&id.to_string(), &exact, None)
            .unwrap();

        for revision in [4, 5] {
            let mut stale = fixture.task(id, "stale projection");
            stale.action_revision = revision;
            stale.requested_action = Some(approve());
            fixture
                .store()
                .persist_projection(&TaskProjection { task: stale }, None)
                .unwrap();
            let stored = fixture.store().read_task_file(id).unwrap().unwrap();
            assert_eq!(stored.action_revision, 5);
            assert!(stored.requested_action.is_none());
        }

        let stale_identity = ProviderActionIdentity {
            namespace: ProviderActionNamespace::Command,
            revision: 4,
        };
        assert!(
            fixture
                .store()
                .acknowledge_action(&id.to_string(), &stale_identity, None)
                .is_err()
        );

        let mut newer = fixture.task(id, "new command");
        newer.action_revision = 6;
        newer.requested_action = Some(approve());
        fixture
            .store()
            .persist_projection(&TaskProjection { task: newer }, None)
            .unwrap();
        let stored = fixture.store().read_task_file(id).unwrap().unwrap();
        assert_eq!(stored.action_revision, 6);
        assert_eq!(
            stored
                .requested_action
                .as_ref()
                .map(|action| action.kind.clone()),
            Some(HumanActionKind::Approve)
        );

        let mut concurrent = stored;
        concurrent.action_revision = 7;
        concurrent.requested_action = Some(approve());
        fixture.store().write_task_file(&concurrent).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let first_store = TaskFileStore::new_with_state_dir(&fixture.tasks, &fixture.state);
        let second_store = TaskFileStore::new_with_state_dir(&fixture.tasks, &fixture.state);
        let first_barrier = Arc::clone(&barrier);
        let second_barrier = Arc::clone(&barrier);
        let ack = std::thread::spawn(move || {
            first_barrier.wait();
            first_store.acknowledge_action(
                &id.to_string(),
                &ProviderActionIdentity {
                    namespace: ProviderActionNamespace::Command,
                    revision: 7,
                },
                None,
            )
        });
        let projection = std::thread::spawn(move || {
            second_barrier.wait();
            let mut replay = concurrent;
            replay.requested_action = Some(approve());
            second_store.persist_projection(&TaskProjection { task: replay }, None)
        });
        barrier.wait();
        ack.join().unwrap().unwrap();
        projection.join().unwrap().unwrap();
        let stored = fixture.store().read_task_file(id).unwrap().unwrap();
        assert_eq!(stored.action_revision, 7);
        assert!(stored.requested_action.is_none());
    }

    #[test]
    fn stale_owned_temp_is_reaped_but_fresh_temp_is_preserved() {
        let root = std::env::temp_dir().join(format!("task-temp-reap-{}", Uuid::new_v4()));
        let tasks = root.join("tasks");
        let state = root.join("state");
        let store = TaskFileStore::new_with_state_dir(&tasks, &state);
        store.ensure_dir().unwrap();
        let old = tasks.join(format!("{}.json.{}.tmp", Uuid::new_v4(), Uuid::new_v4()));
        let fresh = tasks.join(format!("{}.json.{}.tmp", Uuid::new_v4(), Uuid::new_v4()));
        std::fs::write(&old, b"old").unwrap();
        std::fs::write(&fresh, b"fresh").unwrap();
        #[cfg(target_os = "linux")]
        {
            let old_c = std::ffi::CString::new(old.as_os_str().as_bytes()).unwrap();
            let times = [
                libc::timespec {
                    tv_sec: 1,
                    tv_nsec: 0,
                },
                libc::timespec {
                    tv_sec: 1,
                    tv_nsec: 0,
                },
            ];
            // SAFETY: path and timespec pointers remain valid for the call.
            assert_eq!(
                unsafe { libc::utimensat(libc::AT_FDCWD, old_c.as_ptr(), times.as_ptr(), 0) },
                0
            );
        }
        store.ensure_dir().unwrap();
        assert!(!old.exists());
        assert!(fresh.exists());
        let _ = std::fs::remove_dir_all(root);
    }

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
