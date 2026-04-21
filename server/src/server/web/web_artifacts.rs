use serde::Deserialize;
use uuid::Uuid;

#[derive(Deserialize)]
pub(crate) struct ArtifactQuery {
    #[serde(default)]
    pub download: Option<u8>,
}

#[derive(Clone)]
pub(crate) struct RetrievableArtifactRow {
    pub key: String,
    pub path: String,
    pub content_type: String,
    pub open_href: String,
    pub download_href: String,
}

#[derive(Clone, Copy)]
pub(crate) struct ArtifactSpec {
    pub key: &'static str,
    pub content_type: &'static str,
}

const RETRIEVABLE_ARTIFACT_SPECS: [ArtifactSpec; 7] = [
    ArtifactSpec {
        key: "stdout_log",
        content_type: "text/plain; charset=utf-8",
    },
    ArtifactSpec {
        key: "stderr_log",
        content_type: "text/plain; charset=utf-8",
    },
    ArtifactSpec {
        key: "output_json",
        content_type: "application/json",
    },
    ArtifactSpec {
        key: "agent_context",
        content_type: "application/json",
    },
    ArtifactSpec {
        key: "run_json",
        content_type: "application/json",
    },
    ArtifactSpec {
        key: "job_events",
        content_type: "application/x-ndjson",
    },
    ArtifactSpec {
        key: "transcript_jsonl",
        content_type: "application/x-ndjson",
    },
];

pub(crate) fn artifact_spec_for_key(key: &str) -> Option<ArtifactSpec> {
    RETRIEVABLE_ARTIFACT_SPECS
        .iter()
        .copied()
        .find(|spec| spec.key == key)
}

fn non_empty_artifact_path(path: Option<&str>) -> Option<String> {
    path.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub(crate) fn resolve_job_artifact_path(
    result: &protocol::JobResult,
    key: &str,
) -> Result<String, String> {
    match key {
        "stdout_log" => non_empty_artifact_path(Some(result.artifacts.stdout_path.as_str()))
            .ok_or_else(|| {
                "Artifact 'stdout_log' path is missing in persisted metadata".to_string()
            }),
        "stderr_log" => non_empty_artifact_path(Some(result.artifacts.stderr_path.as_str()))
            .ok_or_else(|| {
                "Artifact 'stderr_log' path is missing in persisted metadata".to_string()
            }),
        "output_json" => non_empty_artifact_path(result.artifacts.output_path.as_deref())
            .ok_or_else(|| {
                "Artifact 'output_json' path is missing in persisted metadata".to_string()
            }),
        "agent_context" | "run_json" | "job_events" | "transcript_jsonl" => result
            .artifacts
            .structured_transcript_artifacts
            .iter()
            .find(|artifact| artifact.key == key)
            .and_then(|artifact| non_empty_artifact_path(Some(artifact.path.as_str())))
            .ok_or_else(|| format!("Artifact '{key}' path is missing in persisted metadata")),
        _ => Err(format!("unsupported artifact key: {key}")),
    }
}

#[allow(dead_code)]
pub(crate) fn resolve_job_artifact_path_in_store(
    result: &protocol::JobResult,
    key: &str,
    artifact_store_root: &str,
) -> Result<std::path::PathBuf, String> {
    let stored_path = resolve_job_artifact_path(result, key)?;
    super::super::artifact_store::resolve_in_store(artifact_store_root, &stored_path)
}

pub(crate) fn retrievable_artifact_rows_for_job(
    job_id: Uuid,
    result: &protocol::JobResult,
) -> Vec<RetrievableArtifactRow> {
    RETRIEVABLE_ARTIFACT_SPECS
        .iter()
        .filter_map(|spec| {
            let path = resolve_job_artifact_path(result, spec.key).ok()?;
            Some(RetrievableArtifactRow {
                key: spec.key.to_string(),
                path,
                content_type: spec.content_type.to_string(),
                open_href: format!("/orchestration/jobs/{job_id}/artifacts/{}", spec.key),
                download_href: format!(
                    "/orchestration/jobs/{job_id}/artifacts/{}?download=1",
                    spec.key
                ),
            })
        })
        .collect()
}

pub(crate) fn sanitize_content_disposition_filename(raw: &str) -> String {
    raw.chars()
        .map(|ch| match ch {
            '\0' | '\x01'..='\x1f' | '\x7f' | '"' | '\\' => '_',
            _ => ch,
        })
        .collect()
}
