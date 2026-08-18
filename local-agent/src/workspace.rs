use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::Utc;
use protocol::Job;

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
) -> anyhow::Result<PreparedWorkspace> {
    let Some(repo) = &job.repo else {
        let workspace_dir = worktree_dir_for_job_attempt(worktrees_root, job);
        let workspace_dir = if workspace_dir.is_relative() {
            std::env::current_dir()?.join(&workspace_dir)
        } else {
            workspace_dir
        };
        std::fs::create_dir_all(&workspace_dir)?;
        ensure_git_workspace(&workspace_dir).await?;
        let additional_repo_mounts =
            prepare_additional_repo_mounts(job, runtime_spec, repos_root).await?;
        return Ok(PreparedWorkspace {
            workspace_dir,
            mirror_dir: None,
            attempt_branch: None,
            additional_repo_mounts,
        });
    };

    std::fs::create_dir_all(repos_root)?;
    std::fs::create_dir_all(worktrees_root)?;

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
        )
        .await?;
    }

    refresh_mirror_if_needed(&mirror_dir).await?;

    run_status(
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(&mirror_dir)
            .arg("worktree")
            .arg("prune"),
        "git worktree prune failed",
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
        let _ = std::fs::remove_dir_all(&worktree_dir);
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
    )
    .await?;

    let additional_repo_mounts =
        prepare_additional_repo_mounts(job, runtime_spec, repos_root).await?;

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
        )
        .await?;
        run_status(
            tokio::process::Command::new("git")
                .arg("-C")
                .arg(mirror_dir)
                .arg("worktree")
                .arg("prune"),
            "git worktree prune failed",
        )
        .await?;

        if let Some(attempt_branch) = &prepared.attempt_branch {
            match tokio::process::Command::new("git")
                .arg("-C")
                .arg(mirror_dir)
                .arg("branch")
                .arg("-D")
                .arg(attempt_branch)
                .status()
                .await
            {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    if let Some(events) = events {
                        events
                            .emit(
                                job.id,
                                pb::JobEventKind::JobEventSystem,
                                format!(
                                    "cleanup warning: failed to delete attempt branch '{}' ({status})",
                                    attempt_branch
                                ),
                            )
                            .await;
                    }
                }
                Err(error) => {
                    if let Some(events) = events {
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
            }
        }
    } else if prepared.workspace_dir.exists() {
        std::fs::remove_dir_all(&prepared.workspace_dir)?;
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
    if let Some(request) = protocol::authorization_request_from_job_context(&job.context) {
        return request
            .behavior
            .primary_repo_branch
            .filter(|branch| !branch.trim().is_empty())
            .expect("authorized repository execution requires a signed branch choice");
    }
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
) -> Option<protocol::RepoExecutionMetadata> {
    if !workspace_dir.join(".git").exists() {
        return None;
    }

    let head_sha = git_output(workspace_dir, &["rev-parse", "HEAD"]).await;
    let branch_raw = git_output(workspace_dir, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    let branch = branch_raw
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty() && *b != "HEAD")
        .map(ToOwned::to_owned);
    let status_porcelain = git_output(workspace_dir, &["status", "--porcelain"]).await;

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

async fn ensure_repo_mirror(repo_url: &str, repos_root: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(repos_root)?;

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
        )
        .await?;
    }

    refresh_mirror_if_needed(&mirror_dir).await?;
    Ok(mirror_dir)
}

async fn prepare_additional_repo_mounts(
    job: &Job,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    repos_root: &Path,
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
        let mirror_dir = ensure_repo_mirror(&claim.repo_url, repos_root).await?;
        mounts.push(AdditionalRepoMount {
            repo_url: claim.repo_url.clone(),
            host_path: mirror_dir,
            container_path: additional_repo_container_path(&claim.repo_url),
            writable: false,
        });
    }

    Ok(mounts)
}

async fn refresh_mirror_if_needed(mirror_dir: &Path) -> anyhow::Result<()> {
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
    )
    .await?;

    write_mirror_refresh_timestamp(mirror_dir)?;
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

async fn run_status(cmd: &mut tokio::process::Command, context: &str) -> anyhow::Result<()> {
    let status = cmd.status().await?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("{context}: {status}")
    }
}

async fn git_output(workspace_dir: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(workspace_dir)
        .args(args)
        .output()
        .await
        .ok()?;
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

pub(crate) async fn ensure_git_workspace(workspace_dir: &Path) -> anyhow::Result<()> {
    let git_dir = workspace_dir.join(".git");
    if git_dir.exists() {
        return Ok(());
    }
    let status = tokio::process::Command::new("git")
        .arg("init")
        .arg(workspace_dir)
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("git init failed");
    }
    Ok(())
}
