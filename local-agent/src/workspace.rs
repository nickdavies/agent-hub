#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::Utc;
use protocol::Job;

use super::ExecutionCancellationContext;
use super::JobEventSink;
use super::pb;

pub(crate) const MIRROR_REFRESH_MIN_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub(crate) struct PreparedWorkspace {
    pub(crate) workspace_dir: PathBuf,
    pub(crate) mirror_dir: Option<PathBuf>,
    pub(crate) attempt_branch: Option<String>,
    pub(crate) additional_repo_mounts: Vec<AdditionalRepoMount>,
}

#[derive(Debug, Clone)]
pub(crate) struct AdditionalRepoMount {
    pub(crate) repo_url: String,
    pub(crate) host_path: PathBuf,
    pub(crate) container_path: String,
    pub(crate) writable: bool,
}

pub(crate) async fn prepare_workspace(
    job: &Job,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    repos_root: &Path,
    worktrees_root: &Path,
    cancellation: &ExecutionCancellationContext,
) -> anyhow::Result<PreparedWorkspace> {
    cancellation.check()?;
    let Some(repo) = &job.repo else {
        let workspace_dir = worktree_dir_for_job_attempt(worktrees_root, job);
        let workspace_dir = if workspace_dir.is_relative() {
            std::env::current_dir()?.join(&workspace_dir)
        } else {
            workspace_dir
        };
        journal_create_dir_all(cancellation, "workspace_create", &workspace_dir)?;
        ensure_git_workspace(&workspace_dir, cancellation).await?;
        let additional_repo_mounts =
            prepare_additional_repo_mounts(job, runtime_spec, repos_root, cancellation).await?;
        return Ok(PreparedWorkspace {
            workspace_dir,
            mirror_dir: None,
            attempt_branch: None,
            additional_repo_mounts,
        });
    };

    journal_create_dir_all(cancellation, "workspace_repos_root_create", repos_root)?;
    journal_create_dir_all(
        cancellation,
        "workspace_worktrees_root_create",
        worktrees_root,
    )?;

    let repo_key = safe_repo_key(&repo.repo_url);
    let mirror_dir = repos_root.join(repo_key);
    if !mirror_dir.join(".git").exists() {
        run_status(
            tokio::process::Command::new("git")
                .arg("clone")
                .arg("--origin")
                .arg("origin")
                .arg(repo.repo_url.as_str())
                .arg(&mirror_dir),
            "git clone failed",
            cancellation,
            &mirror_dir.to_string_lossy(),
        )
        .await?;
    }

    refresh_mirror_if_needed(&mirror_dir, cancellation).await?;

    run_status(
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(&mirror_dir)
            .arg("worktree")
            .arg("prune"),
        "git worktree prune failed",
        cancellation,
        &mirror_dir.to_string_lossy(),
    )
    .await?;

    let worktree_dir = worktree_dir_for_job_attempt(worktrees_root, job);
    // Resolve to absolute so `git -C <mirror> worktree add <path>` uses the
    // correct location instead of resolving relative to the mirror directory.
    let worktree_dir = if worktree_dir.is_relative() {
        std::env::current_dir()?.join(&worktree_dir)
    } else {
        worktree_dir
    };
    if worktree_dir.exists() {
        journal_remove_dir_all(cancellation, "workspace_stale_remove", &worktree_dir)?;
    }

    let target_branch = resolve_target_branch_for_repo_job(job, repo);
    let checkout_ref = checkout_ref_for_repo_job(job, repo);

    run_status(
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(&mirror_dir)
            .arg("worktree")
            .arg("add")
            .arg("-B")
            .arg(&target_branch)
            .arg(&worktree_dir)
            .arg(&checkout_ref),
        "git worktree add failed",
        cancellation,
        &worktree_dir.to_string_lossy(),
    )
    .await?;

    let additional_repo_mounts =
        prepare_additional_repo_mounts(job, runtime_spec, repos_root, cancellation).await?;

    Ok(PreparedWorkspace {
        workspace_dir: worktree_dir,
        mirror_dir: Some(mirror_dir),
        attempt_branch: Some(target_branch),
        additional_repo_mounts,
    })
}

pub(crate) async fn cleanup_workspace(
    job: &Job,
    prepared: &PreparedWorkspace,
    events: Option<&JobEventSink>,
    cancellation: &ExecutionCancellationContext,
) -> anyhow::Result<()> {
    if let Some(mirror_dir) = &prepared.mirror_dir {
        run_status(
            tokio::process::Command::new("git")
                .arg("-C")
                .arg(mirror_dir)
                .arg("worktree")
                .arg("remove")
                .arg("--force")
                .arg(&prepared.workspace_dir),
            "git worktree remove failed",
            cancellation,
            &prepared.workspace_dir.to_string_lossy(),
        )
        .await?;
        run_status(
            tokio::process::Command::new("git")
                .arg("-C")
                .arg(mirror_dir)
                .arg("worktree")
                .arg("prune"),
            "git worktree prune failed",
            cancellation,
            &mirror_dir.to_string_lossy(),
        )
        .await?;

        if let Some(attempt_branch) = &prepared.attempt_branch {
            let mut command = tokio::process::Command::new("git");
            command
                .arg("-C")
                .arg(mirror_dir)
                .arg("branch")
                .arg("-D")
                .arg(attempt_branch);
            let resource = format!("{};{}", mirror_dir.display(), attempt_branch);
            if let Err(error) = run_status(
                &mut command,
                "git branch delete failed",
                cancellation,
                &resource,
            )
            .await
                && let Some(events) = events
            {
                events
                    .emit(
                        job.id,
                        pb::JobEventKind::JobEventSystem,
                        format!(
                            "cleanup warning: branch delete command failed for '{}' ({error})",
                            attempt_branch
                        ),
                    )
                    .await;
            }
        }
    } else if prepared.workspace_dir.exists() {
        journal_remove_dir_all(cancellation, "workspace_remove", &prepared.workspace_dir)?;
    }

    if let Some(events) = events {
        events
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!(
                    "workspace cleaned: {}",
                    prepared.workspace_dir.to_string_lossy()
                ),
            )
            .await;
    }
    Ok(())
}

pub(crate) async fn cleanup_old_transcripts(
    transcripts_dir: &Path,
    retention_hours: u64,
) -> anyhow::Result<usize> {
    if retention_hours == 0 {
        return Ok(0);
    }
    if !transcripts_dir.exists() {
        return Ok(0);
    }

    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(
            retention_hours.saturating_mul(60 * 60),
        ))
        .unwrap_or(std::time::UNIX_EPOCH);

    let mut removed = 0usize;
    for entry in std::fs::read_dir(transcripts_dir)
        .with_context(|| format!("read transcripts dir {}", transcripts_dir.display()))?
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!(
                    "failed to read transcript directory entry under {}: {error}",
                    transcripts_dir.display()
                );
                continue;
            }
        };
        let path = entry.path();

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                eprintln!(
                    "failed to read transcript metadata {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        if !metadata.is_dir() {
            continue;
        }

        let modified_at = match metadata.modified() {
            Ok(modified_at) => modified_at,
            Err(error) => {
                eprintln!(
                    "failed to read transcript mtime for {}: {error}",
                    path.display()
                );
                continue;
            }
        };

        if modified_at > cutoff {
            continue;
        }

        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                removed = removed.saturating_add(1);
                eprintln!(
                    "info: removed expired transcript directory {}",
                    path.display()
                );
            }
            Err(error) => {
                eprintln!(
                    "failed to remove expired transcript directory {}: {error}",
                    path.display()
                );
            }
        }
    }

    Ok(removed)
}

pub(crate) fn additional_repo_claims_for_mounts<'a>(
    job: &Job,
    runtime_spec: &'a protocol::CanonicalRuntimeSpec,
) -> Vec<&'a protocol::PlannedRepoClaim> {
    let primary_repo_url = job
        .repo
        .as_ref()
        .map(|repo| repo.repo_url.trim())
        .filter(|repo_url| !repo_url.is_empty());

    runtime_spec
        .repos
        .iter()
        .filter(|claim| {
            let claim_url = claim.repo_url.trim();
            !claim_url.is_empty() && primary_repo_url != Some(claim_url)
        })
        .collect()
}

pub(crate) fn default_branch_name(work_item_id: uuid::Uuid) -> String {
    format!("agent-hub/{}", work_item_id.simple())
}

pub(crate) fn checkout_ref_for_repo_job(job: &Job, repo: &protocol::RepoSource) -> String {
    let target_branch = resolve_target_branch_for_repo_job(job, repo);
    preferred_checkout_ref_from_job_results(&job.context, repo, &target_branch)
        .unwrap_or_else(|| repo.base_ref.clone())
}

pub(crate) fn resolve_target_branch_for_repo_job(job: &Job, repo: &protocol::RepoSource) -> String {
    let configured_branch = repo
        .branch_name
        .clone()
        .filter(|branch| !branch.trim().is_empty());
    let reused_branch = configured_branch
        .is_none()
        .then(|| preferred_target_branch_from_job_results(&job.context, repo))
        .flatten();
    branch_for_job_attempt(job, configured_branch.or(reused_branch))
}

pub(crate) fn preferred_target_branch_from_job_results(
    context: &serde_json::Value,
    repo: &protocol::RepoSource,
) -> Option<String> {
    let results = context.get("job_results")?.as_object()?;

    let mut base_ref_match: Option<String> = None;
    let mut any_succeeded_branch: Option<String> = None;

    for result in results.values() {
        if result.get("outcome").and_then(serde_json::Value::as_str) != Some("succeeded") {
            continue;
        }
        let Some(repo_meta) = result.get("repo").and_then(serde_json::Value::as_object) else {
            continue;
        };

        let Some(candidate_branch) = repo_meta
            .get("target_branch")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
        else {
            continue;
        };

        if any_succeeded_branch.is_none() {
            any_succeeded_branch = Some(candidate_branch.clone());
        }

        let base_ref_matches = repo_meta
            .get("base_ref")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            == Some(repo.base_ref.as_str());
        if base_ref_matches {
            base_ref_match = Some(candidate_branch);
        }
    }

    base_ref_match.or(any_succeeded_branch)
}

pub(crate) fn preferred_checkout_ref_from_job_results(
    context: &serde_json::Value,
    repo: &protocol::RepoSource,
    target_branch: &str,
) -> Option<String> {
    let results = context.get("job_results")?.as_object()?;

    let target_branch = target_branch.trim();
    if target_branch.is_empty() {
        return None;
    }

    let mut latest_match_ref: Option<String> = None;
    let mut latest_match_finished_at: Option<chrono::DateTime<Utc>> = None;

    for result in results.values() {
        if result.get("outcome").and_then(serde_json::Value::as_str) != Some("succeeded") {
            continue;
        }
        let Some(repo_meta) = result.get("repo").and_then(serde_json::Value::as_object) else {
            continue;
        };

        let base_ref_matches = repo_meta
            .get("base_ref")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            == Some(repo.base_ref.as_str());
        if !base_ref_matches {
            continue;
        }

        let result_target_branch = repo_meta
            .get("target_branch")
            .and_then(serde_json::Value::as_str)
            .map(str::trim);
        if result_target_branch != Some(target_branch) {
            continue;
        }

        let candidate_ref = repo_meta
            .get("head_sha")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| result_target_branch.map(ToOwned::to_owned));

        let Some(candidate_ref) = candidate_ref else {
            continue;
        };

        let finished_at = result
            .get("timing")
            .and_then(|timing| timing.get("finished_at"))
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .map(|ts| ts.with_timezone(&Utc));

        let should_replace = match (latest_match_finished_at, finished_at) {
            (None, Some(_)) => true,
            (Some(previous), Some(current)) => current > previous,
            (None, None) => latest_match_ref.is_none(),
            (Some(_), None) => false,
        };

        if should_replace {
            latest_match_ref = Some(candidate_ref);
            latest_match_finished_at = finished_at;
        }
    }

    latest_match_ref
}

pub(crate) fn branch_for_job_attempt(job: &Job, configured_branch: Option<String>) -> String {
    configured_branch
        .filter(|branch| !branch.trim().is_empty())
        .unwrap_or_else(|| {
            let base = default_branch_name(job.work_item_id);
            format!("{base}--j{}-a{}", job.id.simple(), job.attempt)
        })
}

pub(crate) fn worktree_dir_for_job_attempt(worktrees_root: &Path, job: &Job) -> PathBuf {
    worktrees_root
        .join(job.work_item_id.to_string())
        .join(format!("{}-a{}", job.id.simple(), job.attempt))
}

pub(crate) fn repo_name_from_url(repo_url: &str) -> String {
    let trimmed = repo_url.trim_end_matches('/');
    let candidate = trimmed
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .or_else(|| trimmed.rsplit_once(':').map(|(_, tail)| tail))
        .unwrap_or(trimmed)
        .trim_end_matches(".git");
    let sanitized = candidate
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "repo".to_string()
    } else {
        sanitized
    }
}

pub(crate) fn additional_repo_container_path(repo_url: &str) -> String {
    format!("/repos/{}", repo_name_from_url(repo_url))
}

pub(crate) async fn capture_repo_metadata(
    workspace_dir: &Path,
    repo: Option<&protocol::RepoSource>,
    target_branch: Option<String>,
    cancellation: &ExecutionCancellationContext,
) -> Option<protocol::RepoExecutionMetadata> {
    if !workspace_dir.join(".git").exists() {
        return None;
    }

    let head_sha = git_output(workspace_dir, &["rev-parse", "HEAD"], cancellation).await;
    let branch_raw = git_output(
        workspace_dir,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        cancellation,
    )
    .await;
    let branch = branch_raw
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty() && *b != "HEAD")
        .map(ToOwned::to_owned);
    let status_porcelain =
        git_output(workspace_dir, &["status", "--porcelain"], cancellation).await;

    let mut changed_files: u32 = 0;
    let mut untracked_files: u32 = 0;
    let mut is_dirty = false;
    if let Some(status) = &status_porcelain {
        for line in status.lines() {
            if line.trim().is_empty() {
                continue;
            }
            is_dirty = true;
            if line.starts_with("??") {
                untracked_files = untracked_files.saturating_add(1);
            } else {
                changed_files = changed_files.saturating_add(1);
            }
        }
    }

    Some(protocol::RepoExecutionMetadata {
        base_ref: repo.map(|r| r.base_ref.clone()),
        target_branch,
        branch,
        head_sha,
        is_dirty,
        changed_files,
        untracked_files,
    })
}

async fn ensure_repo_mirror(
    repo_url: &str,
    repos_root: &Path,
    cancellation: &ExecutionCancellationContext,
) -> anyhow::Result<PathBuf> {
    journal_create_dir_all(
        cancellation,
        "workspace_repo_mirror_root_create",
        repos_root,
    )?;

    let repo_key = safe_repo_key(repo_url);
    let mirror_dir = repos_root.join(repo_key);
    if !mirror_dir.join(".git").exists() {
        run_status(
            tokio::process::Command::new("git")
                .arg("clone")
                .arg("--origin")
                .arg("origin")
                .arg(repo_url)
                .arg(&mirror_dir),
            "git clone failed",
            cancellation,
            &mirror_dir.to_string_lossy(),
        )
        .await?;
    }

    refresh_mirror_if_needed(&mirror_dir, cancellation).await?;
    Ok(mirror_dir)
}

async fn prepare_additional_repo_mounts(
    job: &Job,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    repos_root: &Path,
    cancellation: &ExecutionCancellationContext,
) -> anyhow::Result<Vec<AdditionalRepoMount>> {
    let additional_claims = additional_repo_claims_for_mounts(job, runtime_spec);
    if additional_claims.is_empty() {
        return Ok(Vec::new());
    }

    let mut mounts = Vec::new();
    for claim in additional_claims {
        let requested_writable = !claim.read_only;
        if requested_writable {
            eprintln!(
                "WARNING: job {} requested writable additional repo '{}'; mounting read-only",
                job.id, claim.repo_url
            );
        }
        let mirror_dir = ensure_repo_mirror(&claim.repo_url, repos_root, cancellation).await?;
        mounts.push(AdditionalRepoMount {
            repo_url: claim.repo_url.clone(),
            host_path: mirror_dir,
            container_path: additional_repo_container_path(&claim.repo_url),
            writable: false,
        });
    }

    Ok(mounts)
}

async fn refresh_mirror_if_needed(
    mirror_dir: &Path,
    cancellation: &ExecutionCancellationContext,
) -> anyhow::Result<()> {
    if !should_refresh_mirror(mirror_dir)? {
        return Ok(());
    }

    run_status(
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(mirror_dir)
            .arg("fetch")
            .arg("--prune")
            .arg("origin"),
        "git fetch failed",
        cancellation,
        &mirror_dir.to_string_lossy(),
    )
    .await?;

    let operation = cancellation.begin_external_operation(
        "workspace_mirror_refresh_stamp_write",
        &mirror_dir.to_string_lossy(),
    )?;
    write_mirror_refresh_timestamp(mirror_dir)?;
    operation.completed()?;
    Ok(())
}

pub(crate) fn should_refresh_mirror(mirror_dir: &Path) -> anyhow::Result<bool> {
    let stamp_path = mirror_refresh_stamp_path(mirror_dir);
    let now = std::time::SystemTime::now();
    let now_secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let Ok(raw) = std::fs::read_to_string(stamp_path) else {
        return Ok(true);
    };
    let last_secs = raw.trim().parse::<u64>().unwrap_or(0);
    Ok(now_secs.saturating_sub(last_secs) >= MIRROR_REFRESH_MIN_SECS)
}

pub(crate) fn write_mirror_refresh_timestamp(mirror_dir: &Path) -> anyhow::Result<()> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp_path = mirror_refresh_stamp_path(mirror_dir);
    if let Some(parent) = stamp_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(stamp_path, now_secs.to_string())?;
    Ok(())
}

pub(crate) fn mirror_refresh_stamp_path(mirror_dir: &Path) -> PathBuf {
    mirror_dir.join(".git").join("agent_hub_last_fetch")
}

fn safe_repo_key(repo_url: &str) -> String {
    let mut out = String::with_capacity(repo_url.len());
    for ch in repo_url.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

async fn run_status(
    cmd: &mut tokio::process::Command,
    context: &str,
    cancellation: &ExecutionCancellationContext,
    resource_identity: &str,
) -> anyhow::Result<()> {
    let operation_kind = match context {
        "git worktree add failed" => "workspace_worktree_create",
        "git worktree remove failed" => "workspace_worktree_remove",
        "git branch delete failed" => "workspace_branch_remove",
        _ => "workspace_git_mutation",
    };
    let operation = cancellation.begin_external_operation(operation_kind, resource_identity)?;
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.as_std_mut().process_group(0);
    let mut child = cmd.spawn()?;
    let pid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .context("workspace git helper has no valid PID")?;
    let identity = super::read_process_identity(pid)?;
    let helper = cancellation.begin_external_operation(
        "workspace_git_helper_process",
        &format!(
            "pid={};pgid={};boot={};start={}",
            identity.pid, identity.pgid, identity.linux_boot_id, identity.proc_start_time_ticks
        ),
    )?;
    let status = child.wait().await?;
    super::verify_process_group_absent(&identity).await?;
    helper.reconciled()?;
    if status.success() {
        operation.completed()?;
        if operation_kind == "workspace_worktree_remove" {
            cancellation
                .operations
                .reconcile_owned_resource(resource_identity)?;
        }
        Ok(())
    } else {
        operation.ambiguous("workspace_git_mutation_failed")?;
        anyhow::bail!("{context}: {status}")
    }
}

async fn git_output(
    workspace_dir: &Path,
    args: &[&str],
    cancellation: &ExecutionCancellationContext,
) -> Option<String> {
    let operation = cancellation
        .begin_external_operation("workspace_git_inspect", &workspace_dir.to_string_lossy())
        .ok()?;
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(workspace_dir)
        .args(args)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    command.as_std_mut().process_group(0);
    let child = command.spawn().ok()?;
    let pid = child.id().and_then(|pid| i32::try_from(pid).ok())?;
    let identity = super::read_process_identity(pid).ok()?;
    let helper = cancellation
        .begin_external_operation(
            "workspace_git_helper_process",
            &format!(
                "pid={};pgid={};boot={};start={}",
                identity.pid, identity.pgid, identity.linux_boot_id, identity.proc_start_time_ticks
            ),
        )
        .ok()?;
    let output = child.wait_with_output().await.ok()?;
    super::verify_process_group_absent(&identity).await.ok()?;
    helper.reconciled().ok()?;
    operation.completed().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

pub(crate) async fn ensure_git_workspace(
    workspace_dir: &Path,
    cancellation: &ExecutionCancellationContext,
) -> anyhow::Result<()> {
    let git_dir = workspace_dir.join(".git");
    if git_dir.exists() {
        return Ok(());
    }
    let operation = cancellation
        .begin_external_operation("workspace_git_init", &workspace_dir.to_string_lossy())?;
    let mut command = tokio::process::Command::new("git");
    command.arg("init").arg(workspace_dir);
    let status = run_status(
        &mut command,
        "git init failed",
        cancellation,
        &workspace_dir.to_string_lossy(),
    )
    .await;
    if status.is_err() {
        operation.ambiguous("workspace_git_init_failed")?;
        return status;
    }
    operation.completed()?;
    Ok(())
}

fn journal_create_dir_all(
    cancellation: &ExecutionCancellationContext,
    operation_kind: &str,
    path: &Path,
) -> anyhow::Result<()> {
    let operation =
        cancellation.begin_external_operation(operation_kind, &path.to_string_lossy())?;
    std::fs::create_dir_all(path)?;
    operation.completed()
}

fn journal_remove_dir_all(
    cancellation: &ExecutionCancellationContext,
    operation_kind: &str,
    path: &Path,
) -> anyhow::Result<()> {
    let operation =
        cancellation.begin_external_operation(operation_kind, &path.to_string_lossy())?;
    match std::fs::remove_dir_all(path) {
        Ok(()) => {
            operation.completed()?;
            cancellation
                .operations
                .reconcile_owned_resources_under(path)
        }
        Err(error) => {
            operation.cleanup_failed("workspace_remove_failed")?;
            Err(error.into())
        }
    }
}
