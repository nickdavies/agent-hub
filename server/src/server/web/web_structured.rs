pub(super) const STRUCTURED_IO_CONTEXT_NAMESPACES: [&str; 6] = [
    "job_outputs",
    "job_results",
    "job_artifact_paths",
    "job_artifact_metadata",
    "job_artifact_data",
    "child_results",
];
pub(super) const STRUCTURED_IO_ALIAS_ROW_LIMIT: usize = 200;
pub(super) const STRUCTURED_VALUE_PREVIEW_ROW_LIMIT: usize = 200;
pub(super) const STRUCTURED_ARTIFACT_PROJECTION_PREVIEW_ROW_LIMIT: usize = 200;
pub(super) const INLINE_STRUCTURED_PREVIEW_MAX_CHARS: usize = 16_000;
pub(super) const STRUCTURED_VALUE_PREVIEW_MAX_JSON_BYTES: usize = 64 * 1024;
pub(super) const TRANSCRIPT_JSONL_PREVIEW_MAX_RECORDS: usize = 64;
pub(super) const TRANSCRIPT_JSONL_PREVIEW_MAX_LINE_CHARS: usize = 4096;
const TRANSCRIPT_JSONL_PREVIEW_MAX_JSON_BYTES: usize = 64 * 1024;

pub(super) fn structured_io_namespace_rows(
    context: &serde_json::Value,
) -> Vec<super::StructuredIoNamespaceRow> {
    let object = match context.as_object() {
        Some(v) => v,
        None => {
            return STRUCTURED_IO_CONTEXT_NAMESPACES
                .iter()
                .map(|name| super::StructuredIoNamespaceRow {
                    name: (*name).to_string(),
                    aliases: Vec::new(),
                    truncated: false,
                })
                .collect();
        }
    };

    STRUCTURED_IO_CONTEXT_NAMESPACES
        .iter()
        .map(|name| {
            let mut aliases = Vec::new();
            let mut truncated = false;
            if let Some(value) = object.get(*name) {
                collect_structured_io_alias_rows(
                    name,
                    value,
                    &mut aliases,
                    &mut truncated,
                    STRUCTURED_IO_ALIAS_ROW_LIMIT,
                );
            }
            super::StructuredIoNamespaceRow {
                name: (*name).to_string(),
                aliases,
                truncated,
            }
        })
        .collect()
}

pub(super) fn collect_structured_io_alias_rows(
    alias: &str,
    value: &serde_json::Value,
    out: &mut Vec<super::StructuredIoAliasRow>,
    truncated: &mut bool,
    limit: usize,
) {
    if out.len() >= limit {
        *truncated = true;
        return;
    }

    if is_interpolatable_scalar(value) {
        out.push(super::StructuredIoAliasRow {
            alias: alias.to_string(),
            interpolation: format!("{{{{{alias}}}}}"),
            value_kind: json_value_kind(value),
        });
    }

    match value {
        serde_json::Value::Object(map) => {
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                if out.len() >= limit {
                    *truncated = true;
                    break;
                }
                if let Some(child) = map.get(&key) {
                    collect_structured_io_alias_rows(
                        &format!("{alias}.{key}"),
                        child,
                        out,
                        truncated,
                        limit,
                    );
                }
            }
        }
        serde_json::Value::Array(items) => {
            for (idx, child) in items.iter().enumerate() {
                if out.len() >= limit {
                    *truncated = true;
                    break;
                }
                collect_structured_io_alias_rows(
                    &format!("{alias}.{idx}"),
                    child,
                    out,
                    truncated,
                    limit,
                );
            }
        }
        _ => {}
    }
}

pub(super) fn json_value_kind(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(_) => "bool".to_string(),
        serde_json::Value::Number(_) => "number".to_string(),
        serde_json::Value::String(_) => "string".to_string(),
        serde_json::Value::Array(_) => "array".to_string(),
        serde_json::Value::Object(_) => "object".to_string(),
    }
}

pub(super) fn is_interpolatable_scalar(value: &serde_json::Value) -> bool {
    matches!(
        value,
        serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_)
    )
}

pub(super) fn structured_transcript_artifact_rows(
    step_id: &str,
    artifacts: &[protocol::StructuredTranscriptArtifact],
) -> Vec<super::StructuredTranscriptArtifactRow> {
    artifacts
        .iter()
        .map(|artifact| {
            let inline_preview = structured_inline_preview(
                &artifact.key,
                artifact.schema.as_deref(),
                artifact.inline_json.as_ref(),
            );
            super::StructuredTranscriptArtifactRow {
                key: artifact.key.clone(),
                path: artifact.path.clone(),
                bytes: artifact
                    .bytes
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                schema: artifact
                    .schema
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                record_count: artifact
                    .record_count
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                context_alias: format!("job_artifact_paths.{step_id}.{}", artifact.key),
                metadata_context_alias: format!("job_artifact_metadata.{step_id}.{}", artifact.key),
                data_context_alias: format!("job_artifact_data.{step_id}.{}", artifact.key),
                has_inline_preview: inline_preview.is_some(),
                inline_preview: inline_preview.unwrap_or_default(),
            }
        })
        .collect()
}

pub(super) fn conventional_artifact_rows(
    step_id: &str,
    artifacts: &protocol::ArtifactSummary,
) -> Vec<super::ConventionalArtifactRow> {
    let entries = [
        (
            "stdout_log",
            Some(artifacts.stdout_path.as_str()),
            artifacts.stdout_bytes,
        ),
        (
            "stderr_log",
            Some(artifacts.stderr_path.as_str()),
            artifacts.stderr_bytes,
        ),
        (
            "output_json",
            artifacts.output_path.as_deref(),
            artifacts.output_bytes,
        ),
        ("transcript_dir", artifacts.transcript_dir.as_deref(), None),
    ];

    entries
        .into_iter()
        .filter_map(|(key, path, bytes)| {
            let path = path?.trim();
            if path.is_empty() {
                return None;
            }
            Some(super::ConventionalArtifactRow {
                key: key.to_string(),
                path: path.to_string(),
                bytes: bytes
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                context_alias: format!("job_artifact_paths.{step_id}.{key}"),
                metadata_context_alias: format!("job_artifact_metadata.{step_id}.{key}"),
            })
        })
        .collect()
}

pub(super) fn structured_inline_preview(
    artifact_key: &str,
    schema: Option<&str>,
    inline_json: Option<&serde_json::Value>,
) -> Option<String> {
    if matches!(artifact_key, "transcript_jsonl") {
        let value = inline_json?;
        if !is_safe_transcript_jsonl_inline_preview(value) {
            return None;
        }
        let pretty = serde_json::to_string_pretty(value).ok()?;
        return Some(truncate_preview(
            &pretty,
            INLINE_STRUCTURED_PREVIEW_MAX_CHARS,
        ));
    }

    let schema_known = matches!(
        (artifact_key, schema),
        ("agent_context", Some(protocol::AGENT_CONTEXT_SCHEMA_NAME))
            | ("run_json", Some(protocol::JOB_RUN_SCHEMA_NAME))
            | ("job_events", Some(protocol::JOB_EVENT_SCHEMA_NAME))
    );
    if !schema_known {
        return None;
    }

    let pretty = serde_json::to_string_pretty(inline_json?).ok()?;
    Some(truncate_preview(
        &pretty,
        INLINE_STRUCTURED_PREVIEW_MAX_CHARS,
    ))
}

pub(super) fn is_safe_transcript_jsonl_inline_preview(value: &serde_json::Value) -> bool {
    let Some(lines) = value.as_array() else {
        return false;
    };
    if lines.len() > TRANSCRIPT_JSONL_PREVIEW_MAX_RECORDS {
        return false;
    }
    if !lines.iter().all(|line| {
        line.as_str()
            .map(|s| s.chars().count() <= TRANSCRIPT_JSONL_PREVIEW_MAX_LINE_CHARS)
            .unwrap_or(false)
    }) {
        return false;
    }
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() <= TRANSCRIPT_JSONL_PREVIEW_MAX_JSON_BYTES)
        .unwrap_or(false)
}

pub(super) fn work_item_structured_artifact_preview_rows(
    context: &serde_json::Value,
) -> Vec<super::StructuredArtifactPreviewRow> {
    let mut rows = Vec::new();
    let Some(context_obj) = context.as_object() else {
        return rows;
    };
    append_structured_artifact_preview_rows(
        context_obj.get("job_artifact_data"),
        context_obj.get("job_artifact_metadata"),
        "job_artifact_data",
        "job_artifact_metadata",
        None,
        &mut rows,
    );

    if let Some(child_results) = context_obj
        .get("child_results")
        .and_then(serde_json::Value::as_object)
    {
        let mut parent_steps = child_results.keys().cloned().collect::<Vec<_>>();
        parent_steps.sort();
        for parent_step in parent_steps {
            let Some(parent_result) = child_results
                .get(&parent_step)
                .and_then(serde_json::Value::as_object)
            else {
                continue;
            };
            append_structured_artifact_preview_rows(
                parent_result.get("job_artifact_data"),
                parent_result.get("job_artifact_metadata"),
                &format!("child_results.{parent_step}.job_artifact_data"),
                &format!("child_results.{parent_step}.job_artifact_metadata"),
                Some(&parent_step),
                &mut rows,
            );
        }
    }

    rows
}

pub(super) fn work_item_structured_value_preview_rows(
    context: &serde_json::Value,
) -> Vec<super::StructuredValuePreviewRow> {
    let mut rows = Vec::new();
    let Some(context_obj) = context.as_object() else {
        return rows;
    };

    append_structured_value_preview_rows(
        context_obj.get("job_outputs"),
        "job_outputs",
        "job_outputs",
        None,
        &mut rows,
        STRUCTURED_VALUE_PREVIEW_ROW_LIMIT,
    );
    append_structured_value_preview_rows(
        context_obj.get("job_results"),
        "job_results",
        "job_results",
        None,
        &mut rows,
        STRUCTURED_VALUE_PREVIEW_ROW_LIMIT,
    );

    if let Some(child_results) = context_obj
        .get("child_results")
        .and_then(serde_json::Value::as_object)
    {
        let mut parent_steps = child_results.keys().cloned().collect::<Vec<_>>();
        parent_steps.sort();
        for parent_step in parent_steps {
            if rows.len() >= STRUCTURED_VALUE_PREVIEW_ROW_LIMIT {
                break;
            }

            let Some(parent_result) = child_results
                .get(&parent_step)
                .and_then(serde_json::Value::as_object)
            else {
                continue;
            };

            append_structured_value_preview_rows(
                parent_result.get("job_outputs"),
                &format!("child_results.{parent_step}.job_outputs"),
                &format!("child_results.{parent_step}.job_outputs"),
                Some(&parent_step),
                &mut rows,
                STRUCTURED_VALUE_PREVIEW_ROW_LIMIT,
            );
            append_structured_value_preview_rows(
                parent_result.get("job_results"),
                &format!("child_results.{parent_step}.job_results"),
                &format!("child_results.{parent_step}.job_results"),
                Some(&parent_step),
                &mut rows,
                STRUCTURED_VALUE_PREVIEW_ROW_LIMIT,
            );
        }
    }

    rows
}

pub(super) fn work_item_structured_artifact_projection_preview_rows(
    context: &serde_json::Value,
) -> Vec<super::StructuredValuePreviewRow> {
    let mut rows = Vec::new();
    let Some(context_obj) = context.as_object() else {
        return rows;
    };

    append_structured_value_preview_rows(
        context_obj.get("job_artifact_paths"),
        "job_artifact_paths",
        "job_artifact_paths",
        None,
        &mut rows,
        STRUCTURED_ARTIFACT_PROJECTION_PREVIEW_ROW_LIMIT,
    );
    append_structured_value_preview_rows(
        context_obj.get("job_artifact_metadata"),
        "job_artifact_metadata",
        "job_artifact_metadata",
        None,
        &mut rows,
        STRUCTURED_ARTIFACT_PROJECTION_PREVIEW_ROW_LIMIT,
    );

    if let Some(child_results) = context_obj
        .get("child_results")
        .and_then(serde_json::Value::as_object)
    {
        let mut parent_steps = child_results.keys().cloned().collect::<Vec<_>>();
        parent_steps.sort();
        for parent_step in parent_steps {
            if rows.len() >= STRUCTURED_ARTIFACT_PROJECTION_PREVIEW_ROW_LIMIT {
                break;
            }

            let Some(parent_result) = child_results
                .get(&parent_step)
                .and_then(serde_json::Value::as_object)
            else {
                continue;
            };

            append_structured_value_preview_rows(
                parent_result.get("job_artifact_paths"),
                &format!("child_results.{parent_step}.job_artifact_paths"),
                &format!("child_results.{parent_step}.job_artifact_paths"),
                Some(&parent_step),
                &mut rows,
                STRUCTURED_ARTIFACT_PROJECTION_PREVIEW_ROW_LIMIT,
            );
            append_structured_value_preview_rows(
                parent_result.get("job_artifact_metadata"),
                &format!("child_results.{parent_step}.job_artifact_metadata"),
                &format!("child_results.{parent_step}.job_artifact_metadata"),
                Some(&parent_step),
                &mut rows,
                STRUCTURED_ARTIFACT_PROJECTION_PREVIEW_ROW_LIMIT,
            );
        }
    }

    rows
}

pub(super) fn job_detail_structured_value_preview_rows(
    step_id: &str,
    result: Option<&protocol::JobResult>,
) -> Vec<super::StructuredValuePreviewRow> {
    let Some(result) = result else {
        return Vec::new();
    };

    let mut rows = Vec::new();
    if let Some(output_json) = result.output_json.as_ref() {
        rows.push(super::StructuredValuePreviewRow {
            namespace: "job_outputs".to_string(),
            step_id: step_id.to_string(),
            context_alias: format!("job_outputs.{step_id}"),
            interpolation: format!("{{{{job_outputs.{step_id}}}}}"),
            value_kind: json_value_kind(output_json),
            inline_preview: truncate_preview(
                &bounded_pretty_json_preview(output_json, STRUCTURED_VALUE_PREVIEW_MAX_JSON_BYTES),
                INLINE_STRUCTURED_PREVIEW_MAX_CHARS,
            ),
        });
    }

    if let Ok(serialized_result) = serde_json::to_value(result) {
        rows.push(super::StructuredValuePreviewRow {
            namespace: "job_results".to_string(),
            step_id: step_id.to_string(),
            context_alias: format!("job_results.{step_id}"),
            interpolation: format!("{{{{job_results.{step_id}}}}}"),
            value_kind: json_value_kind(&serialized_result),
            inline_preview: truncate_preview(
                &bounded_pretty_json_preview(
                    &serialized_result,
                    STRUCTURED_VALUE_PREVIEW_MAX_JSON_BYTES,
                ),
                INLINE_STRUCTURED_PREVIEW_MAX_CHARS,
            ),
        });
    }

    rows
}

pub(super) fn append_structured_value_preview_rows(
    namespace_value: Option<&serde_json::Value>,
    namespace: &str,
    alias_prefix: &str,
    parent_step_id: Option<&str>,
    out: &mut Vec<super::StructuredValuePreviewRow>,
    limit: usize,
) {
    let Some(namespace_obj) = namespace_value.and_then(serde_json::Value::as_object) else {
        return;
    };

    let mut step_ids = namespace_obj.keys().cloned().collect::<Vec<_>>();
    step_ids.sort();

    for step_id in step_ids {
        if out.len() >= limit {
            break;
        }
        let Some(value) = namespace_obj.get(&step_id) else {
            continue;
        };
        let display_step_id = match parent_step_id {
            Some(parent_step) => format!("{parent_step}.{step_id}"),
            None => step_id.clone(),
        };
        let context_alias = format!("{alias_prefix}.{step_id}");
        let pretty = bounded_pretty_json_preview(value, STRUCTURED_VALUE_PREVIEW_MAX_JSON_BYTES);

        out.push(super::StructuredValuePreviewRow {
            namespace: namespace.to_string(),
            step_id: display_step_id,
            context_alias: context_alias.clone(),
            interpolation: format!("{{{{{context_alias}}}}}"),
            value_kind: json_value_kind(value),
            inline_preview: truncate_preview(&pretty, INLINE_STRUCTURED_PREVIEW_MAX_CHARS),
        });
    }
}

pub(super) fn bounded_pretty_json_preview(
    value: &serde_json::Value,
    max_json_bytes: usize,
) -> String {
    match serde_json::to_vec(value) {
        Ok(bytes) if bytes.len() > max_json_bytes => format!(
            "<preview omitted: JSON value is {} bytes, exceeds {}-byte preview budget>",
            bytes.len(),
            max_json_bytes
        ),
        Ok(_) => serde_json::to_string_pretty(value)
            .unwrap_or_else(|_| "<unable to render preview>".to_string()),
        Err(_) => "<unable to render preview>".to_string(),
    }
}

pub(super) fn append_structured_artifact_preview_rows(
    job_artifact_data: Option<&serde_json::Value>,
    job_artifact_metadata: Option<&serde_json::Value>,
    data_alias_prefix: &str,
    metadata_alias_prefix: &str,
    parent_step_id: Option<&str>,
    out: &mut Vec<super::StructuredArtifactPreviewRow>,
) {
    let Some(job_artifact_data) = job_artifact_data.and_then(serde_json::Value::as_object) else {
        return;
    };
    let job_artifact_metadata = job_artifact_metadata.and_then(serde_json::Value::as_object);

    let mut step_ids = job_artifact_data.keys().cloned().collect::<Vec<_>>();
    step_ids.sort();

    for step_id in step_ids {
        let Some(step_map) = job_artifact_data
            .get(&step_id)
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };

        let mut artifact_keys = step_map.keys().cloned().collect::<Vec<_>>();
        artifact_keys.sort();

        for artifact_key in artifact_keys {
            let Some(inline_json) = step_map.get(&artifact_key) else {
                continue;
            };
            // Some inline payloads (notably job_events) are arrays rather than schema-tagged
            // objects, so schema may need to come from sibling metadata instead.
            let inline_schema = inline_json
                .as_object()
                .and_then(|obj| obj.get("schema"))
                .and_then(serde_json::Value::as_str);
            let metadata_schema = job_artifact_metadata
                .and_then(|metadata| metadata.get(&step_id))
                .and_then(serde_json::Value::as_object)
                .and_then(|step_meta| step_meta.get(&artifact_key))
                .and_then(serde_json::Value::as_object)
                .and_then(|artifact_meta| artifact_meta.get("schema"))
                .and_then(serde_json::Value::as_str);
            let schema = inline_schema.or(metadata_schema);
            let Some(inline_preview) =
                structured_inline_preview(&artifact_key, schema, Some(inline_json))
            else {
                continue;
            };

            let display_step_id = match parent_step_id {
                Some(parent_step) => format!("{parent_step}.{step_id}"),
                None => step_id.clone(),
            };
            out.push(super::StructuredArtifactPreviewRow {
                step_id: display_step_id,
                artifact_key: artifact_key.clone(),
                data_context_alias: format!("{data_alias_prefix}.{step_id}.{artifact_key}"),
                metadata_context_alias: format!("{metadata_alias_prefix}.{step_id}.{artifact_key}"),
                schema: schema.unwrap_or("unknown").to_string(),
                inline_preview,
            });
        }
    }
}

pub(super) fn truncate_preview(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }

    let mut out = String::new();
    for ch in value.chars().take(max_chars) {
        out.push(ch);
    }
    out.push_str("\n... (preview truncated)");
    out
}
