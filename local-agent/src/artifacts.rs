use std::collections::VecDeque;
use std::path::Path;

use chrono::Utc;
use protocol::{
    AGENT_CONTEXT_SCHEMA_NAME, AGENT_CONTEXT_SCHEMA_VERSION, AgentContext, AgentContextArtifacts,
    AgentContextJob, AgentContextPaths, AgentContextRepos, AgentContextWorkflow, ExecutionMode,
    GithubPullRequestMetadata, JOB_RUN_SCHEMA_NAME, Job, StructuredTranscriptArtifact,
};

pub(crate) const TRANSCRIPT_JSONL_INLINE_TAIL_LIMIT: usize = 20;
pub(crate) const TRANSCRIPT_JSONL_INLINE_MAX_BYTES: usize = 64 * 1024;
pub(crate) const JOB_EVENTS_INLINE_TAIL_LIMIT: usize = 20;
pub(crate) const JOB_EVENTS_INLINE_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct AgentContextWriteParams {
    pub transcript_dir: String,
    pub workspace_dir: std::path::PathBuf,
    pub runtime_spec: protocol::CanonicalRuntimeSpec,
    pub additional_repo_mounts: Vec<AdditionalRepoMountView>,
}

#[derive(Debug, Clone)]
pub(crate) struct AdditionalRepoMountView {
    pub repo_url: String,
    pub container_path: String,
    pub writable: bool,
}

pub(crate) fn file_len_u64(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

pub(crate) fn file_line_count_u64(path: &Path) -> Option<u64> {
    use std::io::BufRead;

    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    Some(reader.lines().count() as u64)
}

pub(crate) fn write_agent_context_file(
    agent_context_path: &Path,
    job: &Job,
    params: AgentContextWriteParams,
) -> anyhow::Result<()> {
    let (workspace_dir_display, transcript_dir_display) = display_paths_for_runtime(
        &params.workspace_dir,
        &params.transcript_dir,
        &params.runtime_spec,
    );
    let transcript_root = std::path::PathBuf::from(&transcript_dir_display);
    let workflow_inputs = workflow_inputs_contract_from_job_context(&job.context);
    let work_item = work_item_contract_from_job_context(&job.context);
    let workflow_valid_next_events = workflow_inputs
        .as_object()
        .and_then(|obj| obj.get("valid_next_events"))
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
    let artifact_paths = workflow_inputs
        .as_object()
        .and_then(|obj| obj.get("job_artifact_paths"))
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    let artifact_metadata = workflow_inputs
        .as_object()
        .and_then(|obj| obj.get("job_artifact_metadata"))
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    let valid_next_events = workflow_valid_next_events
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let repo_value = serde_json::to_value(&job.repo)?;
    let additional_repo_values = params
        .additional_repo_mounts
        .iter()
        .map(|mount| {
            serde_json::json!({
                "url": mount.repo_url,
                "path": mount.container_path,
                "writable": mount.writable,
            })
        })
        .collect::<Vec<_>>();
    let execution_mode = serde_json::to_value(&params.runtime_spec.environment.execution_mode)?
        .as_str()
        .unwrap_or_default()
        .to_string();
    let primary_repo_value = job.repo.as_ref().map(|repo| {
        serde_json::json!({
            "url": repo.repo_url,
            "path": workspace_dir_display,
            "writable": true,
        })
    });
    let payload = AgentContext {
        schema: AGENT_CONTEXT_SCHEMA_VERSION.to_string(),
        job: AgentContextJob {
            id: job.id,
            work_item_id: Some(job.work_item_id),
            step_id: job.step_id.clone(),
            attempt: job.attempt,
            max_retries: job.max_retries,
            execution_mode,
            timeout_secs: params.runtime_spec.environment.timeout_secs,
            container_image: params.runtime_spec.environment.container_image.clone(),
        },
        paths: AgentContextPaths {
            workspace_dir: workspace_dir_display,
            transcript_dir: transcript_dir_display,
            context_json: transcript_root
                .join("context.json")
                .to_string_lossy()
                .to_string(),
            agent_context_json: transcript_root
                .join(".agent-context.json")
                .to_string_lossy()
                .to_string(),
            output_json: transcript_root
                .join("output.json")
                .to_string_lossy()
                .to_string(),
            run_json: transcript_root
                .join("run.json")
                .to_string_lossy()
                .to_string(),
            transcript_jsonl: transcript_root
                .join("transcript.jsonl")
                .to_string_lossy()
                .to_string(),
            job_events_jsonl: transcript_root
                .join("job-events.jsonl")
                .to_string_lossy()
                .to_string(),
            stdout_log: transcript_root
                .join("stdout.log")
                .to_string_lossy()
                .to_string(),
            stderr_log: transcript_root
                .join("stderr.log")
                .to_string_lossy()
                .to_string(),
            prompt_user: transcript_root
                .join("prompt")
                .join("user.txt")
                .to_string_lossy()
                .to_string(),
            prompt_system: transcript_root
                .join("prompt")
                .join("system.txt")
                .to_string_lossy()
                .to_string(),
        },
        work_item: Some(work_item),
        workflow: AgentContextWorkflow {
            inputs: workflow_inputs,
            valid_next_events,
        },
        artifacts: AgentContextArtifacts {
            paths: artifact_paths,
            metadata: artifact_metadata,
        },
        repo: Some(repo_value.clone()),
        repos: AgentContextRepos {
            primary: primary_repo_value,
            additional: additional_repo_values,
        },
        permissions: Some(params.runtime_spec.permissions.clone()),
        approval_rules: params.runtime_spec.approval_rules.clone(),
        generated_at: Utc::now(),
        advisory: true,
    };
    std::fs::write(agent_context_path, serde_json::to_vec_pretty(&payload)?)?;
    Ok(())
}

pub(crate) fn collect_structured_transcript_artifacts(
    agent_context_path: &Path,
    run_json_path: &Path,
    transcript_jsonl_path: &Path,
    job_events_path: &Path,
) -> Vec<StructuredTranscriptArtifact> {
    let mut artifacts = Vec::new();

    if agent_context_path.exists() {
        let agent_context_inline = inline_agent_context_json(agent_context_path);
        let agent_context_schema = agent_context_inline
            .as_ref()
            .and_then(|v| v.get("schema"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        artifacts.push(StructuredTranscriptArtifact {
            key: "agent_context".to_string(),
            path: agent_context_path.to_string_lossy().to_string(),
            bytes: file_len_u64(agent_context_path),
            schema: agent_context_schema,
            record_count: None,
            inline_json: agent_context_inline,
        });
    }

    if run_json_path.exists() {
        let run_json_inline = inline_run_json(run_json_path);
        let run_json_schema = run_json_inline
            .as_ref()
            .and_then(|v| v.get("schema"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        artifacts.push(StructuredTranscriptArtifact {
            key: "run_json".to_string(),
            path: run_json_path.to_string_lossy().to_string(),
            bytes: file_len_u64(run_json_path),
            schema: run_json_schema,
            record_count: None,
            inline_json: run_json_inline,
        });
    }

    if transcript_jsonl_path.exists() {
        let transcript_jsonl_inline = inline_transcript_jsonl_tail(
            transcript_jsonl_path,
            TRANSCRIPT_JSONL_INLINE_TAIL_LIMIT,
            TRANSCRIPT_JSONL_INLINE_MAX_BYTES,
        );
        artifacts.push(StructuredTranscriptArtifact {
            key: "transcript_jsonl".to_string(),
            path: transcript_jsonl_path.to_string_lossy().to_string(),
            bytes: file_len_u64(transcript_jsonl_path),
            schema: None,
            record_count: file_line_count_u64(transcript_jsonl_path),
            inline_json: transcript_jsonl_inline,
        });
    }

    if job_events_path.exists() {
        let job_events_inline = inline_job_events_json_tail(
            job_events_path,
            JOB_EVENTS_INLINE_TAIL_LIMIT,
            JOB_EVENTS_INLINE_MAX_BYTES,
        );
        artifacts.push(StructuredTranscriptArtifact {
            key: "job_events".to_string(),
            path: job_events_path.to_string_lossy().to_string(),
            bytes: file_len_u64(job_events_path),
            schema: Some(protocol::JOB_EVENT_SCHEMA_NAME.to_string()),
            record_count: file_line_count_u64(job_events_path),
            inline_json: job_events_inline,
        });
    }

    artifacts
}

/// A parsed GitHub-shaped repository reference.
///
/// Captures the `host` (for both github.com and self-hosted GitHub Enterprise
/// installations) plus the `owner/repo` slug. Use [`parse_github_repo_url`]
/// for the raw parse and [`normalize_github_repo_slug`] when you want
/// allowlisted-only behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubRepoRef {
    pub host: String,
    pub slug: String,
}

/// Default allowlisted GitHub host. Matches `gh`'s default and is the
/// effective allowlist when no `github_hosts` is set in `local-agent.json`.
pub(crate) const GITHUB_DEFAULT_HOST: &str = "github.com";

/// Parse a GitHub or GitHub Enterprise URL into its host and `owner/repo`
/// slug. Returns `None` if the URL doesn't look GitHub-shaped (no
/// `<owner>/<repo>` path, missing host, etc.).
///
/// This intentionally does *not* enforce the host allowlist — callers that
/// only care about allowlisted hosts should use [`normalize_github_repo_slug`]
/// instead.
pub(crate) fn parse_github_repo_url(repo_url: &str) -> Option<GithubRepoRef> {
    let trimmed = repo_url.trim();
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);

    // https://<host>/<owner>/<repo>[/...]
    if let Some(rest) = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
    {
        let (host, path) = rest.split_once('/')?;
        let host = host.trim();
        if host.is_empty() {
            return None;
        }
        let mut parts = path.split('/').filter(|p| !p.is_empty());
        let owner = parts.next()?;
        let repo = parts.next()?;
        return Some(GithubRepoRef {
            host: host.to_string(),
            slug: format!("{owner}/{repo}"),
        });
    }

    // git@<host>:<owner>/<repo>
    if let Some(rest) = trimmed.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        let host = host.trim();
        if host.is_empty() {
            return None;
        }
        let mut parts = path.split('/').filter(|p| !p.is_empty());
        let owner = parts.next()?;
        let repo = parts.next()?;
        return Some(GithubRepoRef {
            host: host.to_string(),
            slug: format!("{owner}/{repo}"),
        });
    }

    None
}

/// Returns the `owner/repo` slug for `repo_url`, but only if the URL's host
/// is in the supplied allowlist. Callers should pass
/// [`crate::egress_config::LocalAgentConfig::github_host_allowlist`] — this
/// keeps the file the single source of truth for GHE opt-in.
pub(crate) fn normalize_github_repo_slug_with_allowlist(
    repo_url: &str,
    allowed_hosts: &[String],
) -> Option<String> {
    let parsed = parse_github_repo_url(repo_url)?;
    if allowed_hosts
        .iter()
        .any(|h| h.eq_ignore_ascii_case(&parsed.host))
    {
        Some(parsed.slug)
    } else {
        None
    }
}

pub(crate) fn parse_github_pr_from_output(
    output_json: &serde_json::Value,
) -> Option<GithubPullRequestMetadata> {
    let pr = output_json.get("github_pr")?;
    let number = pr.get("number")?.as_u64()?;
    let url = pr.get("url")?.as_str()?.to_string();
    if url.trim().is_empty() {
        return None;
    }

    Some(GithubPullRequestMetadata {
        number,
        url,
        state: pr
            .get("state")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        is_draft: pr.get("is_draft").and_then(serde_json::Value::as_bool),
        head_ref_name: pr
            .get("head_ref_name")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        base_ref_name: pr
            .get("base_ref_name")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
    })
}

pub(crate) fn display_paths_for_runtime(
    workspace_dir: &Path,
    transcript_dir: &str,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
) -> (String, String) {
    if matches!(
        runtime_spec.environment.execution_mode,
        ExecutionMode::Docker
    ) {
        ("/workspace".to_string(), "/transcript".to_string())
    } else {
        (
            workspace_dir.to_string_lossy().to_string(),
            transcript_dir.to_string(),
        )
    }
}

pub(crate) fn workflow_valid_next_events_from_job_context(
    job_context: &serde_json::Value,
) -> Option<Vec<String>> {
    job_context
        .as_object()
        .and_then(|obj| obj.get("workflow"))
        .and_then(serde_json::Value::as_object)
        .and_then(|obj| obj.get("inputs"))
        .and_then(serde_json::Value::as_object)
        .and_then(|obj| obj.get("valid_next_events"))
        .and_then(serde_json::Value::as_array)
        .map(|events| {
            events
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
}

pub(crate) fn work_item_contract_from_job_context(
    job_context: &serde_json::Value,
) -> serde_json::Value {
    let work_item = job_context
        .as_object()
        .and_then(|obj| obj.get("work_item"))
        .and_then(serde_json::Value::as_object);

    serde_json::json!({
        "id": work_item
            .and_then(|obj| obj.get("id"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "title": work_item
            .and_then(|obj| obj.get("title"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "description": work_item
            .and_then(|obj| obj.get("description"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "labels": work_item
            .and_then(|obj| obj.get("labels"))
            .and_then(serde_json::Value::as_array)
            .cloned()
            .map(serde_json::Value::Array)
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
        "priority": work_item
            .and_then(|obj| obj.get("priority"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "workflow_state": work_item
            .and_then(|obj| obj.get("workflow_state"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "workflow_id": work_item
            .and_then(|obj| obj.get("workflow_id"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "owner_github_login": work_item
            .and_then(|obj| obj.get("owner_github_login"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "repo_catalog": work_item
            .and_then(|obj| obj.get("repo_catalog"))
            .and_then(serde_json::Value::as_array)
            .cloned()
            .map(serde_json::Value::Array)
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
    })
}

fn clone_context_object_field_or_empty(
    context: &serde_json::Value,
    key: &str,
) -> serde_json::Value {
    context
        .as_object()
        .and_then(|obj| obj.get(key))
        .and_then(serde_json::Value::as_object)
        .cloned()
        .map(serde_json::Value::Object)
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()))
}

fn workflow_inputs_contract_from_job_context(job_context: &serde_json::Value) -> serde_json::Value {
    let workflow_inputs = job_context
        .as_object()
        .and_then(|obj| obj.get("workflow"))
        .and_then(serde_json::Value::as_object)
        .and_then(|obj| obj.get("inputs"))
        .cloned();

    let valid_next_events = workflow_inputs
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .and_then(|obj| obj.get("valid_next_events"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    let job_artifact_paths = workflow_inputs
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .and_then(|obj| obj.get("job_artifact_paths"))
        .and_then(serde_json::Value::as_object)
        .cloned()
        .map(serde_json::Value::Object)
        .unwrap_or_else(|| clone_context_object_field_or_empty(job_context, "job_artifact_paths"));

    let job_artifact_metadata = workflow_inputs
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .and_then(|obj| obj.get("job_artifact_metadata"))
        .and_then(serde_json::Value::as_object)
        .cloned()
        .map(serde_json::Value::Object)
        .unwrap_or_else(|| {
            clone_context_object_field_or_empty(job_context, "job_artifact_metadata")
        });

    serde_json::json!({
        "valid_next_events": valid_next_events,
        "job_artifact_paths": job_artifact_paths,
        "job_artifact_metadata": job_artifact_metadata,
    })
}

fn inline_agent_context_json(path: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(path).ok()?;
    let parsed = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
    if parsed
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        == AGENT_CONTEXT_SCHEMA_NAME
    {
        Some(parsed)
    } else {
        None
    }
}

fn inline_run_json(path: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(path).ok()?;
    let parsed = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
    if parsed
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        == JOB_RUN_SCHEMA_NAME
    {
        Some(parsed)
    } else {
        None
    }
}

pub(crate) fn inline_job_events_json_tail(
    path: &Path,
    max_records: usize,
    max_total_bytes: usize,
) -> Option<serde_json::Value> {
    use std::io::BufRead;

    if max_records == 0 || max_total_bytes == 0 {
        return Some(serde_json::Value::Array(Vec::new()));
    }

    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut tail: VecDeque<(serde_json::Value, usize)> = VecDeque::with_capacity(max_records);
    let mut total_bytes = 0usize;

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<protocol::JobEvent>(&line) else {
            continue;
        };
        let Ok(value) = serde_json::to_value(event) else {
            continue;
        };
        let Ok(serialized) = serde_json::to_vec(&value) else {
            continue;
        };
        let value_bytes = serialized.len();

        if value_bytes > max_total_bytes {
            tail.clear();
            total_bytes = 0;
            continue;
        }

        if tail.len() == max_records
            && let Some((_, evicted_bytes)) = tail.pop_front()
        {
            total_bytes = total_bytes.saturating_sub(evicted_bytes);
        }
        tail.push_back((value, value_bytes));
        total_bytes += value_bytes;

        while total_bytes > max_total_bytes {
            let Some((_, evicted_bytes)) = tail.pop_front() else {
                break;
            };
            total_bytes = total_bytes.saturating_sub(evicted_bytes);
        }
    }

    Some(serde_json::Value::Array(
        tail.into_iter().map(|(value, _)| value).collect(),
    ))
}

pub(crate) fn inline_transcript_jsonl_tail(
    path: &Path,
    max_records: usize,
    max_total_bytes: usize,
) -> Option<serde_json::Value> {
    use std::io::BufRead;

    if max_records == 0 || max_total_bytes == 0 {
        return Some(serde_json::Value::Array(Vec::new()));
    }

    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut tail: VecDeque<(String, usize)> = VecDeque::with_capacity(max_records);
    let mut total_bytes = 0usize;

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let line_bytes = line.len();

        if line_bytes > max_total_bytes {
            tail.clear();
            total_bytes = 0;
            continue;
        }

        if tail.len() == max_records
            && let Some((_, evicted_bytes)) = tail.pop_front()
        {
            total_bytes = total_bytes.saturating_sub(evicted_bytes);
        }
        tail.push_back((line, line_bytes));
        total_bytes += line_bytes;

        while total_bytes > max_total_bytes {
            let Some((_, evicted_bytes)) = tail.pop_front() else {
                break;
            };
            total_bytes = total_bytes.saturating_sub(evicted_bytes);
        }
    }

    Some(serde_json::Value::Array(
        tail.into_iter()
            .map(|(line, _)| serde_json::Value::String(line))
            .collect(),
    ))
}
