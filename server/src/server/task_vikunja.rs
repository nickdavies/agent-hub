use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use comrak::{ComrakOptions, markdown_to_html as comrak_markdown_to_html};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    let opt = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

use super::task_adapter::{
    HumanActionKind, TaskAdapter, TaskAdapterError, TaskAssignee, TaskChange, TaskChangeType,
    TaskCheckpoint, TaskCreate, TaskHumanAction, TaskHumanGateState, TaskLabels, TaskProjection,
    TaskSyncRecord, TaskToolRecord, TaskTriageStatus, TaskUpdate, TriageResult,
};

const VIKUNJA_UUID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x76, 0x69, 0x6b, 0x75, 0x6e, 0x6a, 0x61, 0x2d, 0x74, 0x61, 0x73, 0x6b, 0x2d, 0x6e, 0x73, 0x00,
]);

const CHECKPOINT_MARKER: &str = "<!-- agent-hub:checkpoint -->";
const TRIAGE_RESULT_MARKER: &str = "<!-- agent-hub:triage_result -->";
const TRIAGE_RESULT_HEADING: &str = "## 📋 Triage Complete";

fn vikunja_task_uuid(base_url: &str, task_id: i64) -> Uuid {
    let key = format!("vikunja:{base_url}:task:{task_id}");
    Uuid::new_v5(&VIKUNJA_UUID_NAMESPACE, key.as_bytes())
}

#[derive(Debug, Deserialize)]
struct VikunjaTask {
    id: i64,
    title: String,
    description: String,
    done: bool,
    priority: i64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    labels: Vec<VikunjaLabel>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    assignees: Vec<VikunjaUser>,
    created: String,
    updated: String,
    project_id: i64,
    index: i64,
    identifier: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct VikunjaLabel {
    id: i64,
    title: String,
}

#[derive(Debug, Deserialize, Clone)]
struct VikunjaUser {
    id: i64,
    username: String,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VikunjaComment {
    id: i64,
    comment: String,
    #[allow(dead_code)]
    author: VikunjaUser,
    created: String,
    updated: String,
}

#[derive(Debug, Clone)]
struct VikunjaPollKnownTask {
    #[allow(dead_code)]
    updated_at: DateTime<Utc>,
    #[allow(dead_code)]
    assignee_ids: Vec<i64>,
}

#[derive(Debug, Deserialize)]
struct VikunjaView {
    id: i64,
    #[allow(dead_code)]
    title: String,
}

#[derive(Debug, Deserialize, Default)]
struct AgentHubMetadata {
    workflow_id: Option<String>,
    repo: Option<AgentHubRepoMeta>,
}

#[derive(Debug, Deserialize)]
struct AgentHubRepoMeta {
    repo_url: String,
    base_ref: Option<String>,
    branch_name: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct AgentHubMetadataEnvelope {
    #[serde(default)]
    agent_hub: AgentHubMetadata,
}

fn parse_description_metadata(description: &str) -> (String, Option<AgentHubMetadata>) {
    let Some((body, metadata)) = description.rsplit_once("\n---\n") else {
        return (description.to_string(), None);
    };

    let body = body.trim_end().to_string();
    let metadata = metadata.trim();
    if metadata.is_empty() {
        return (body, None);
    }

    let parsed = match serde_yaml::from_str::<AgentHubMetadataEnvelope>(metadata) {
        Ok(doc) => {
            if doc.agent_hub.workflow_id.is_some() || doc.agent_hub.repo.is_some() {
                Some(doc.agent_hub)
            } else {
                None
            }
        }
        Err(err) => {
            tracing::warn!(error = ?err, "failed to parse task description metadata yaml");
            None
        }
    };

    (body, parsed)
}

fn rest_as_reason(parts: &[&str], from: usize) -> Option<String> {
    if parts.len() > from {
        Some(parts[from..].join(" "))
    } else {
        None
    }
}

/// Strip HTML tags and decode HTML entities so a Vikunja rich-text comment
/// like `<p><code>/ah unblock</code>&nbsp;</p>` round-trips to a parseable
/// `/ah unblock` line. Tag stripping is a tiny state machine; entity
/// decoding is delegated to the `html-escape` crate so we get the full
/// HTML5 named-entity table plus numeric refs without maintaining our own
/// (incomplete) lookup.
///
/// This is intentionally permissive: tags become spaces (so adjacent words
/// don't collide), and we don't try to be a fully spec-compliant HTML
/// parser. The goal is just to recover plain text from a WYSIWYG comment
/// well enough to find a slash command.
fn html_to_plain_text(input: &str) -> String {
    let mut stripped = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '<' {
            for inner in chars.by_ref() {
                if inner == '>' {
                    break;
                }
            }
            stripped.push(' ');
        } else {
            stripped.push(c);
        }
    }
    html_escape::decode_html_entities(&stripped).into_owned()
}

fn parse_comment_action(comment: &str) -> Option<TaskHumanAction> {
    // Vikunja's rich-text editor wraps slash-commands in HTML; flatten the
    // input so the same line-prefix logic works for both raw API posts and
    // anything typed in the UI.
    let plain = html_to_plain_text(comment);
    let line = plain.lines().find_map(|line| {
        let trimmed = line.trim();
        if trimmed
            .get(..3)
            .map(|prefix| prefix.eq_ignore_ascii_case("/ah"))
            .unwrap_or(false)
        {
            Some(trimmed.to_string())
        } else {
            None
        }
    })?;

    let command = line.get(3..)?.trim();
    let parts: Vec<&str> = line.trim().get(3..)?.split_whitespace().collect();
    if command.is_empty() {
        return None;
    }

    match parts.first()?.to_lowercase().as_str() {
        "workflow" => Some(TaskHumanAction {
            kind: HumanActionKind::ChooseWorkflow,
            workflow_id: parts.get(1).map(|s| s.to_string()),
            reason: None,
        }),
        "submit" => Some(TaskHumanAction {
            kind: HumanActionKind::Submit,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        "approve" => Some(TaskHumanAction {
            kind: HumanActionKind::Approve,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        "request_changes" => Some(TaskHumanAction {
            kind: HumanActionKind::RequestChanges,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        "unblock" => Some(TaskHumanAction {
            kind: HumanActionKind::Unblock,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        "retry" => Some(TaskHumanAction {
            kind: HumanActionKind::Retry,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        "cancel" => Some(TaskHumanAction {
            kind: HumanActionKind::Cancel,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        // Manual override for the `await_ci` human gate when the SCM
        // poller can't observe the PR (token missing, host unreachable,
        // repo without CI configured). Workflows that don't use a CI
        // gate just won't accept these events.
        "checks_passed" => Some(TaskHumanAction {
            kind: HumanActionKind::ChecksPassed,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        "checks_failed" => Some(TaskHumanAction {
            kind: HumanActionKind::ChecksFailed,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        _ => None,
    }
}

fn parse_rfc3339_to_utc(value: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn action_revision_from_timestamp(timestamp: DateTime<Utc>, tie_breaker: u64) -> u64 {
    let millis = timestamp.timestamp_millis().max(0) as u64;
    millis
        .saturating_mul(1_000)
        .saturating_add(tie_breaker % 1_000)
}

fn format_marked_yaml_comment<T: Serialize>(marker: &str, payload: &T) -> anyhow::Result<String> {
    let yaml =
        serde_yaml::to_string(payload).context("failed to serialize comment payload to YAML")?;
    Ok(format!("{marker}\n```yaml\n{yaml}```"))
}

fn markdown_to_html(markdown: &str) -> String {
    let mut options = ComrakOptions::default();
    options.extension.table = true;
    options.render.unsafe_ = true;
    comrak_markdown_to_html(markdown, &options)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommentKind {
    Status,
    Checkpoint,
    JobResult(String),
}

impl CommentKind {
    fn marker(&self, work_item_id: &str) -> String {
        match self {
            Self::Status => format!("<!-- agent-hub:status:{work_item_id} -->"),
            Self::Checkpoint => format!("<!-- agent-hub:checkpoint:{work_item_id} -->"),
            Self::JobResult(step_id) => {
                format!("<!-- agent-hub:job_result:{step_id}:{work_item_id} -->")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentUpdateOutcome {
    Updated,
    Missing,
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn extract_yaml_block(comment: &str, marker: &str) -> Option<String> {
    let marker_idx = comment.find(marker)?;
    let mut body = comment[marker_idx + marker.len()..].trim();
    if body.is_empty() {
        return None;
    }

    if body.starts_with("```") {
        let first_newline_idx = body.find('\n')?;
        body = body[first_newline_idx + 1..].trim_start();
        let end_idx = body.rfind("```")?;
        return Some(body[..end_idx].trim().to_string());
    }

    if let Some(code_start) = body.find("<code") {
        let code = &body[code_start..];
        let content_start = code.find('>')? + 1;
        let code_content = &code[content_start..];
        let content_end = code_content.find("</code>")?;
        return Some(decode_html_entities(code_content[..content_end].trim()));
    }

    Some(body.to_string())
}

fn extract_agent_hub_marker(comment: &str, marker_prefix: &str) -> Option<String> {
    let start = comment.find(marker_prefix)?;
    let tail = &comment[start..];
    let end = tail.find("-->")?;
    Some(tail[..end + 3].to_string())
}

fn latest_checkpoint_from_comments(comments: &[VikunjaComment]) -> Option<TaskCheckpoint> {
    comments.iter().rev().find_map(|comment| {
        let marker = extract_agent_hub_marker(&comment.comment, "<!-- agent-hub:checkpoint")?;
        let yaml = extract_yaml_block(&comment.comment, &marker)?;
        serde_yaml::from_str::<TaskCheckpoint>(&yaml).ok()
    })
}

#[derive(Debug, Serialize)]
#[allow(dead_code)]
struct VikunjaCreateTaskRequest {
    title: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct VikunjaCommentCreateRequest {
    comment: String,
}

#[derive(Debug, Serialize)]
struct VikunjaAddLabelRequest {
    label_id: i64,
}

#[derive(Debug, Serialize)]
struct VikunjaCreateLabelRequest {
    title: String,
}

#[derive(Debug, Serialize)]
struct VikunjaTaskDoneRequest {
    done: bool,
}

#[derive(Debug, Serialize)]
struct VikunjaTaskAssignUserRequest {
    user_id: i64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct VikunjaTaskCreated {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct VikunjaErrorBody {
    #[serde(default)]
    code: Option<i64>,
}

pub enum VikunjaAuthConfig {
    StaticToken(String),
    BotCredentials { username: String, password: String },
}

struct VikunjaJwtAuth {
    username: String,
    password: String,
    token: Arc<RwLock<String>>,
    login_lock: Mutex<()>,
}

enum VikunjaAuth {
    StaticToken(String),
    Jwt(VikunjaJwtAuth),
}

#[derive(Debug, Serialize)]
struct VikunjaLoginRequest<'a> {
    username: &'a str,
    password: &'a str,
}

#[derive(Debug, Deserialize)]
struct VikunjaLoginResponse {
    token: String,
}

pub struct VikunjaAdapter {
    client: reqwest::blocking::Client,
    base_url: String,
    auth: VikunjaAuth,
    project_id: i64,
    view_id: Mutex<Option<i64>>,
    label_prefix: String,
    task_id_cache: Mutex<HashMap<Uuid, i64>>,
    #[allow(dead_code)]
    poll_known: Mutex<HashMap<String, VikunjaPollKnownTask>>,
    assignment_known: Mutex<HashMap<String, Vec<i64>>>,
    bot_user_id: i64,
    operator_user_id: i64,
}

impl VikunjaAdapter {
    const RETRY_BACKOFFS: [Duration; 3] = [
        Duration::from_millis(100),
        Duration::from_millis(500),
        Duration::from_millis(2000),
    ];

    pub fn new(
        base_url: String,
        auth: VikunjaAuthConfig,
        project_id: i64,
        label_prefix: Option<String>,
        bot_username: String,
        operator_username: String,
    ) -> anyhow::Result<Self> {
        let normalized = if base_url.trim().is_empty() {
            "http://localhost:3456".to_string()
        } else {
            base_url.trim_end_matches('/').to_string()
        };

        let auth = match auth {
            VikunjaAuthConfig::StaticToken(token) => VikunjaAuth::StaticToken(token),
            VikunjaAuthConfig::BotCredentials { username, password } => {
                VikunjaAuth::Jwt(VikunjaJwtAuth {
                    username,
                    password,
                    token: Arc::new(RwLock::new(String::new())),
                    login_lock: Mutex::new(()),
                })
            }
        };

        let mut adapter = Self {
            client: std::thread::spawn(|| {
                reqwest::blocking::Client::builder()
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(Duration::from_secs(30))
                    .build()
                    .expect("Failed to create HTTP client")
            })
            .join()
            .expect("Failed to create HTTP client thread"),
            base_url: normalized,
            auth,
            project_id,
            view_id: Mutex::new(None),
            label_prefix: label_prefix
                .unwrap_or_else(|| "ah".to_string())
                .trim_end_matches(':')
                .to_string(),
            task_id_cache: Mutex::new(HashMap::new()),
            poll_known: Mutex::new(HashMap::new()),
            assignment_known: Mutex::new(HashMap::new()),
            bot_user_id: 0,
            operator_user_id: 0,
        };

        adapter.bot_user_id = adapter.resolve_required_user_id(&bot_username).with_context(|| {
            format!(
                "failed to resolve Vikunja bot user '{bot_username}' via GET /api/v1/users?s=..."
            )
        })?;
        adapter.operator_user_id = adapter
            .resolve_required_user_id(&operator_username)
            .with_context(|| {
                format!(
                    "failed to resolve Vikunja operator user '{operator_username}' via GET /api/v1/users?s=..."
                )
            })?;

        Ok(adapter)
    }

    fn api_url(&self, path: &str) -> String {
        let trimmed = path.trim_start_matches('/');
        format!("{}/api/v1/{trimmed}", self.base_url)
    }

    fn extract_task_id_from_path(path: &str) -> Option<i64> {
        let mut parts = path.split('/');
        match (parts.next(), parts.next()) {
            (Some("tasks"), Some(task_id)) => task_id.parse::<i64>().ok(),
            _ => None,
        }
    }

    fn is_task_not_found_response(status: reqwest::StatusCode, body: &str) -> bool {
        status == reqwest::StatusCode::NOT_FOUND
            || (status == reqwest::StatusCode::BAD_REQUEST
                && Self::decode_error_code(body) == Some(4002))
    }

    fn task_not_found_error(task_id: i64) -> anyhow::Error {
        anyhow::Error::new(TaskAdapterError::TaskNotFound {
            task_id: task_id.to_string(),
        })
    }

    fn api_get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let url = self.api_url(path);
        let response = self.send_with_auth_retry("GET", path, &url, true, |token| {
            self.client.get(&url).bearer_auth(token).send()
        })?;
        Self::decode_response(response, "GET", path, &url)
    }

    fn api_put<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        let url = self.api_url(path);
        let response = self.send_with_auth_retry("PUT", path, &url, true, |token| {
            self.client.put(&url).bearer_auth(token).json(body).send()
        })?;
        Self::decode_response(response, "PUT", path, &url)
    }

    fn api_post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        let url = self.api_url(path);
        let response = self.send_with_auth_retry("POST", path, &url, true, |token| {
            self.client.post(&url).bearer_auth(token).json(body).send()
        })?;
        Self::decode_response(response, "POST", path, &url)
    }

    fn api_delete(&self, path: &str) -> anyhow::Result<()> {
        let url = self.api_url(path);
        let response = self.send_with_auth_retry("DELETE", path, &url, true, |token| {
            self.client.delete(&url).bearer_auth(token).send()
        })?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().unwrap_or_default();
        if let Some(task_id) = Self::extract_task_id_from_path(path)
            && Self::is_task_not_found_response(status, &body)
        {
            return Err(Self::task_not_found_error(task_id));
        }
        Err(anyhow::anyhow!(
            "DELETE path {path} ({url}) failed with status {status}: {body}"
        ))
    }

    fn send_with_retry<F>(
        &self,
        method: &str,
        path: &str,
        url: &str,
        idempotent: bool,
        mut send: F,
    ) -> anyhow::Result<reqwest::blocking::Response>
    where
        F: FnMut() -> Result<reqwest::blocking::Response, reqwest::Error>,
    {
        for (attempt, backoff) in Self::RETRY_BACKOFFS.iter().enumerate() {
            match send() {
                Ok(response) if response.status().is_server_error() => {
                    if !idempotent {
                        return Ok(response);
                    }
                    tracing::warn!(
                        method,
                        path,
                        url,
                        status = %response.status(),
                        retry_attempt = attempt + 1,
                        "request returned 5xx, retrying"
                    );
                    thread::sleep(*backoff);
                }
                Ok(response) => return Ok(response),
                Err(error) if error.is_connect() || error.is_timeout() => {
                    if !idempotent {
                        return Err(error).with_context(|| {
                            format!(
                                "{method} request failed for path {path} ({url}) without retries"
                            )
                        });
                    }
                    tracing::warn!(
                        method,
                        path,
                        url,
                        retry_attempt = attempt + 1,
                        error = ?error,
                        "request failed due to connection issue, retrying"
                    );
                    thread::sleep(*backoff);
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("{method} request failed for path {path} ({url})")
                    });
                }
            }
        }

        let response = send().with_context(|| {
            format!("{method} request failed for path {path} ({url}) after retries")
        })?;

        if response.status().is_server_error() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            return Err(anyhow::anyhow!(
                "{method} path {path} ({url}) failed after retries with status {status}: {body}"
            ));
        }

        Ok(response)
    }

    fn current_token(&self) -> anyhow::Result<String> {
        match &self.auth {
            VikunjaAuth::StaticToken(token) => Ok(token.clone()),
            VikunjaAuth::Jwt(jwt) => {
                let token = jwt
                    .token
                    .try_read()
                    .map(|guard| guard.clone())
                    .unwrap_or_default();
                if !token.is_empty() {
                    return Ok(token);
                }
                self.login_and_cache_jwt()
            }
        }
    }

    fn login_and_cache_jwt(&self) -> anyhow::Result<String> {
        let VikunjaAuth::Jwt(jwt) = &self.auth else {
            return Err(anyhow::anyhow!(
                "login attempted for static Vikunja token auth"
            ));
        };

        let _guard = jwt
            .login_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("vikunja login lock poisoned"))?;

        let existing = jwt
            .token
            .try_read()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        if !existing.is_empty() {
            return Ok(existing);
        }

        let url = self.api_url("login");
        let request = VikunjaLoginRequest {
            username: &jwt.username,
            password: &jwt.password,
        };
        let response = self.send_with_retry("POST", "login", &url, true, || {
            self.client.post(&url).json(&request).send()
        })?;
        let login: VikunjaLoginResponse = Self::decode_response(response, "POST", "login", &url)?;

        if let Ok(mut token) = jwt.token.try_write() {
            *token = login.token.clone();
        }
        Ok(login.token)
    }

    fn refresh_jwt_token(&self) -> anyhow::Result<String> {
        let VikunjaAuth::Jwt(jwt) = &self.auth else {
            return Err(anyhow::anyhow!(
                "refresh attempted for static Vikunja token auth"
            ));
        };

        let _guard = jwt
            .login_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("vikunja login lock poisoned"))?;

        let url = self.api_url("login");
        let request = VikunjaLoginRequest {
            username: &jwt.username,
            password: &jwt.password,
        };
        let response = self.send_with_retry("POST", "login", &url, true, || {
            self.client.post(&url).json(&request).send()
        })?;
        let login: VikunjaLoginResponse = Self::decode_response(response, "POST", "login", &url)?;

        if let Ok(mut token) = jwt.token.try_write() {
            *token = login.token.clone();
        }
        Ok(login.token)
    }

    fn send_with_auth_retry<F>(
        &self,
        method: &str,
        path: &str,
        url: &str,
        idempotent: bool,
        mut send: F,
    ) -> anyhow::Result<reqwest::blocking::Response>
    where
        F: FnMut(&str) -> Result<reqwest::blocking::Response, reqwest::Error>,
    {
        let mut token = self.current_token()?;
        let mut refreshed = false;

        loop {
            let response = self.send_with_retry(method, path, url, idempotent, || send(&token))?;
            if response.status() != reqwest::StatusCode::UNAUTHORIZED {
                return Ok(response);
            }

            let VikunjaAuth::Jwt(_) = &self.auth else {
                return Ok(response);
            };

            if refreshed {
                return Ok(response);
            }

            tracing::warn!("vikunja JWT expired, re-authenticating...");
            token = self.refresh_jwt_token()?;
            refreshed = true;
        }
    }

    fn decode_response<T: DeserializeOwned>(
        response: reqwest::blocking::Response,
        method: &str,
        path: &str,
        url: &str,
    ) -> anyhow::Result<T> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            if let Some(task_id) = Self::extract_task_id_from_path(path)
                && Self::is_task_not_found_response(status, &body)
            {
                return Err(Self::task_not_found_error(task_id));
            }
            return Err(anyhow::anyhow!(
                "{method} path {path} ({url}) failed with status {status}: {body}"
            ));
        }

        response.json::<T>().with_context(|| {
            format!(
                "failed to decode response body for {method} path {path} ({url}), status {status}"
            )
        })
    }

    fn decode_error_code(body: &str) -> Option<i64> {
        serde_json::from_str::<VikunjaErrorBody>(body)
            .ok()
            .and_then(|error| error.code)
    }

    fn assign_user_to_task(&self, task_id: i64, user_id: i64) -> anyhow::Result<()> {
        let path = format!("tasks/{task_id}/assignees");
        let url = self.api_url(&path);
        let response = self.send_with_auth_retry("PUT", &path, &url, true, |token| {
            self.client
                .put(&url)
                .bearer_auth(token)
                .json(&VikunjaTaskAssignUserRequest { user_id })
                .send()
        })?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let body = response.text().unwrap_or_default();
        if status == reqwest::StatusCode::BAD_REQUEST
            && Self::decode_error_code(&body) == Some(4021)
        {
            return Ok(());
        }

        Err(anyhow::anyhow!(
            "PUT path {path} ({url}) failed with status {status}: {body}"
        ))
    }

    fn resolve_view_id(&self) -> anyhow::Result<i64> {
        let mut guard = self
            .view_id
            .lock()
            .map_err(|_| anyhow::anyhow!("vikunja view id lock poisoned"))?;
        if let Some(view_id) = *guard {
            return Ok(view_id);
        }

        let views: Vec<VikunjaView> =
            self.api_get(&format!("projects/{}/views", self.project_id))?;
        let first = views
            .first()
            .ok_or_else(|| anyhow::anyhow!("project {} has no views", self.project_id))?;
        *guard = Some(first.id);
        Ok(first.id)
    }

    fn get_task_comments(&self, task_id: i64) -> anyhow::Result<Vec<VikunjaComment>> {
        self.api_get(&format!("tasks/{task_id}/comments"))
    }

    fn find_users(&self, search: &str) -> anyhow::Result<Vec<VikunjaUser>> {
        let path = "users";
        let url = self.api_url(path);
        let response = self.send_with_auth_retry("GET", path, &url, true, |token| {
            self.client
                .get(&url)
                .bearer_auth(token)
                .query(&[("s", search)])
                .send()
        })?;
        Self::decode_response(response, "GET", path, &url)
    }

    fn resolve_required_user_id(&self, username: &str) -> anyhow::Result<i64> {
        let users = self.find_users(username)?;
        users
            .iter()
            .find(|user| user.username == username)
            .or_else(|| {
                users
                    .iter()
                    .find(|user| user.username.eq_ignore_ascii_case(username))
            })
            .map(|user| user.id)
            .ok_or_else(|| anyhow::anyhow!("vikunja user '{username}' not found"))
    }

    fn remove_user_from_task(&self, task_id: i64, user_id: i64) -> anyhow::Result<()> {
        self.api_delete(&format!("tasks/{task_id}/assignees/{user_id}"))
    }

    fn clear_task_assignees(&self, task_id: i64) -> anyhow::Result<()> {
        let task: VikunjaTask = self.api_get(&format!("tasks/{task_id}"))?;
        for assignee in task.assignees {
            self.remove_user_from_task(task_id, assignee.id)?;
        }
        Ok(())
    }

    fn append_task_comment(&self, task_id: i64, comment: String) -> anyhow::Result<()> {
        let _ = self.write_comment(task_id, None, None, &comment, None)?;
        Ok(())
    }

    fn create_task_comment_html(&self, task_id: i64, comment: String) -> anyhow::Result<i64> {
        let path = format!("tasks/{task_id}/comments");
        let url = self.api_url(&path);
        let response = self.send_with_auth_retry("PUT", &path, &url, false, |token| {
            self.client
                .put(&url)
                .bearer_auth(token)
                .json(&VikunjaCommentCreateRequest {
                    comment: comment.clone(),
                })
                .send()
        })?;
        let created: VikunjaComment = Self::decode_response(response, "PUT", &path, &url)?;
        Ok(created.id)
    }

    fn namespaced_label(&self, suffix: &str) -> String {
        if self.label_prefix.is_empty() {
            suffix.to_string()
        } else {
            format!("{}:{suffix}", self.label_prefix)
        }
    }

    fn update_task_comment_html(
        &self,
        task_id: i64,
        comment_id: i64,
        comment: String,
    ) -> anyhow::Result<CommentUpdateOutcome> {
        let path = format!("tasks/{task_id}/comments/{comment_id}");
        let url = self.api_url(&path);
        let response = self.send_with_auth_retry("POST", &path, &url, false, |token| {
            self.client
                .post(&url)
                .bearer_auth(token)
                .json(&VikunjaCommentCreateRequest {
                    comment: comment.clone(),
                })
                .send()
        })?;

        let status = response.status();
        if status.is_success() {
            return Ok(CommentUpdateOutcome::Updated);
        }

        let body = response.text().unwrap_or_default();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(CommentUpdateOutcome::Missing);
        }
        if Self::is_task_not_found_response(status, &body) {
            return Err(Self::task_not_found_error(task_id));
        }
        Err(anyhow::anyhow!(
            "POST path {path} ({url}) failed with status {status}: {body}"
        ))
    }

    fn write_comment(
        &self,
        task_id: i64,
        kind: Option<CommentKind>,
        work_item_id: Option<&str>,
        content_markdown: &str,
        stored_comment_id: Option<i64>,
    ) -> anyhow::Result<i64> {
        let html = markdown_to_html(content_markdown);
        let marker = match (kind.as_ref(), work_item_id) {
            (Some(kind), Some(work_item_id)) => Some(kind.marker(work_item_id)),
            _ => None,
        };
        let html_with_marker = if let Some(marker) = marker.as_ref() {
            format!("{html}\n{marker}")
        } else {
            html
        };

        if let Some(comment_id) = stored_comment_id {
            match self.update_task_comment_html(task_id, comment_id, html_with_marker.clone())? {
                CommentUpdateOutcome::Updated => return Ok(comment_id),
                CommentUpdateOutcome::Missing => {}
            }
        }

        if let Some(marker) = marker.as_ref() {
            let comments = self.get_task_comments(task_id)?;
            if let Some(existing) = comments
                .iter()
                .find(|comment| comment.comment.contains(marker))
            {
                match self.update_task_comment_html(
                    task_id,
                    existing.id,
                    html_with_marker.clone(),
                )? {
                    CommentUpdateOutcome::Updated => return Ok(existing.id),
                    CommentUpdateOutcome::Missing => {}
                }
            }
        }

        self.create_task_comment_html(task_id, html_with_marker)
    }

    fn get_or_create_label(&self, title: &str) -> anyhow::Result<i64> {
        let labels: Vec<VikunjaLabel> = self.api_get("labels")?;
        if let Some(existing) = labels.into_iter().find(|l| l.title == title) {
            return Ok(existing.id);
        }

        let path = "labels";
        let url = self.api_url(path);
        let response = self.send_with_auth_retry("PUT", path, &url, true, |token| {
            self.client
                .put(&url)
                .bearer_auth(token)
                .json(&VikunjaCreateLabelRequest {
                    title: title.to_string(),
                })
                .send()
        })?;
        let status = response.status();

        if status.is_success() {
            let created = response.json::<VikunjaLabel>().with_context(|| {
                format!(
                    "failed to decode response body for PUT path {path} ({url}), status {status}"
                )
            })?;
            return Ok(created.id);
        }

        let body = response.text().unwrap_or_default();
        if status == reqwest::StatusCode::BAD_REQUEST {
            let labels: Vec<VikunjaLabel> = self.api_get("labels")?;
            if let Some(existing) = labels.into_iter().find(|l| l.title == title) {
                return Ok(existing.id);
            }
        }

        Err(anyhow::anyhow!(
            "PUT path {path} ({url}) failed with status {status}: {body}"
        ))
    }

    fn attach_label_to_task(&self, task_id: i64, label_id: i64) -> anyhow::Result<()> {
        let path = format!("tasks/{task_id}/labels");
        let url = self.api_url(&path);
        let response = self.send_with_auth_retry("PUT", &path, &url, true, |token| {
            self.client
                .put(&url)
                .bearer_auth(token)
                .json(&VikunjaAddLabelRequest { label_id })
                .send()
        })?;

        let status = response.status();
        if status == reqwest::StatusCode::OK || status == reqwest::StatusCode::CREATED {
            return Ok(());
        }

        let body = response.text().unwrap_or_default();
        if status == reqwest::StatusCode::BAD_REQUEST
            && Self::decode_error_code(&body) == Some(8001)
        {
            return Ok(());
        }

        Err(anyhow::anyhow!(
            "PUT path {path} ({url}) failed with status {status}: {body}"
        ))
    }

    fn remove_label_from_task(&self, task_id: i64, label_id: i64) -> anyhow::Result<()> {
        let path = format!("tasks/{task_id}/labels/{label_id}");
        let url = self.api_url(&path);
        let response = self.send_with_auth_retry("DELETE", &path, &url, true, |token| {
            self.client.delete(&url).bearer_auth(token).send()
        })?;

        let status = response.status();
        if status.is_success()
            || status == reqwest::StatusCode::BAD_REQUEST
            || status == reqwest::StatusCode::NOT_FOUND
        {
            return Ok(());
        }

        let body = response.text().unwrap_or_default();
        Err(anyhow::anyhow!(
            "DELETE path {path} ({url}) failed with status {status}: {body}"
        ))
    }

    fn set_task_labels(
        &self,
        task_id: i64,
        desired: &[String],
        current: &[VikunjaLabel],
    ) -> anyhow::Result<()> {
        let desired_set = desired.iter().cloned().collect::<HashSet<_>>();
        let current_map = current
            .iter()
            .map(|label| (label.title.clone(), label.id))
            .collect::<HashMap<_, _>>();

        for title in desired_set.difference(&current_map.keys().cloned().collect()) {
            let label_id = self.get_or_create_label(title)?;
            self.attach_label_to_task(task_id, label_id)?;
        }

        for label in current {
            if !desired_set.contains(&label.title) {
                self.remove_label_from_task(task_id, label.id)?;
            }
        }

        Ok(())
    }

    fn vikunja_task_to_record(
        &self,
        task: &VikunjaTask,
        comments: &[VikunjaComment],
        assignment_to_bot_transition: bool,
    ) -> TaskToolRecord {
        let uuid = vikunja_task_uuid(&self.base_url, task.id);
        let (clean_desc, metadata) = parse_description_metadata(&task.description);
        let labels: Vec<String> = task.labels.iter().map(|l| l.title.clone()).collect();

        let updated_at = parse_rfc3339_to_utc(&task.updated).unwrap_or_else(Utc::now);

        let comment_requested_action = comments
            .iter()
            .rev()
            .find_map(|c| parse_comment_action(&c.comment));
        let comment_action_revision = comments
            .iter()
            .rev()
            .find(|c| parse_comment_action(&c.comment).is_some())
            .map(|c| {
                let timestamp = parse_rfc3339_to_utc(&c.updated)
                    .or_else(|| parse_rfc3339_to_utc(&c.created))
                    .unwrap_or(updated_at);
                action_revision_from_timestamp(timestamp, c.id as u64)
            })
            .unwrap_or(0);

        let checkpoint = latest_checkpoint_from_comments(comments);
        // A human → bot reassignment is only treated as an Approve when the
        // most recent checkpoint comment says `human_gate: true`. The
        // orchestrator writes the checkpoint *before* changing the assignee
        // in `write_back_state`, so even if a later regression caused the
        // orchestrator to flip `human_gate` on its own, the freshly-written
        // checkpoint would say `human_gate: false` and this guard would
        // suppress the spurious Approve. Do not weaken this guard without
        // also reviewing the write-back ordering.
        let assignment_requested_action = if assignment_to_bot_transition
            && checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.human_gate)
        {
            Some(TaskHumanAction {
                kind: HumanActionKind::Approve,
                workflow_id: None,
                reason: Some("approval inferred from assignment to bot".to_string()),
            })
        } else {
            None
        };
        let assignment_action_revision = if assignment_requested_action.is_some() {
            action_revision_from_timestamp(updated_at, 0)
        } else {
            0
        };

        let (requested_action, action_revision) =
            if assignment_action_revision > comment_action_revision {
                (assignment_requested_action, assignment_action_revision)
            } else {
                (comment_requested_action, comment_action_revision)
            };

        let workflow_id = metadata.as_ref().and_then(|m| m.workflow_id.clone());
        let repo = metadata
            .as_ref()
            .and_then(|m| m.repo.as_ref())
            .map(|r| protocol::RepoSource {
                repo_url: r.repo_url.clone(),
                base_ref: r
                    .base_ref
                    .clone()
                    .unwrap_or_else(|| "origin/main".to_string()),
                branch_name: r.branch_name.clone(),
            });

        let needs_triage = labels.iter().any(|l| l == "needs-triage");

        // When a checkpoint comment is present, it is the authoritative source
        // for the orchestrator-owned `human_gate`, `human_gate_state`, and
        // `triage_status` fields — Vikunja itself doesn't natively model them.
        // The raw strings are also stashed in `context["checkpoint"]` so that
        // `apply_task_fields` can restore the full orchestrator string without
        // being constrained by the `TaskHumanGateState` / `TaskTriageStatus`
        // enums (which don't include states like `triage_completed` or
        // `pending_approval`).
        let checkpoint_human_gate = checkpoint.as_ref().is_some_and(|c| c.human_gate);
        let checkpoint_human_gate_state_raw =
            checkpoint.as_ref().and_then(|c| c.human_gate_state.clone());
        let checkpoint_triage_status_raw =
            checkpoint.as_ref().and_then(|c| c.triage_status.clone());

        let pending_human_action = needs_triage || checkpoint_human_gate;
        let human_gate_state: Option<TaskHumanGateState> = checkpoint_human_gate_state_raw
            .as_deref()
            .and_then(|v| {
                serde_json::from_value(serde_json::Value::String(v.to_string())).ok()
            });
        let triage_status_from_checkpoint = checkpoint_triage_status_raw.as_deref().and_then(|v| {
            serde_json::from_value::<TaskTriageStatus>(serde_json::Value::String(v.to_string()))
                .ok()
        });
        let triage_status = triage_status_from_checkpoint.unwrap_or_else(|| {
            if needs_triage || workflow_id.is_none() {
                TaskTriageStatus::NeedsWorkflowSelection
            } else {
                TaskTriageStatus::Ready
            }
        });

        let mut context = serde_json::json!({
            "vikunja_task_id": task.id,
            "vikunja_project_id": task.project_id,
            "vikunja_index": task.index,
            "vikunja_identifier": task.identifier,
            "labels": labels,
            "assignees": task.assignees.iter().map(|a| &a.username).collect::<Vec<_>>(),
            "assignee_ids": task.assignees.iter().map(|a| a.id).collect::<Vec<_>>(),
            "done": task.done,
            "priority": task.priority,
            "vikunja_created": task.created,
            "vikunja_updated": task.updated,
        });
        if checkpoint.is_some() {
            let mut cp_context = serde_json::Map::new();
            cp_context.insert(
                "human_gate".to_string(),
                serde_json::Value::Bool(checkpoint_human_gate),
            );
            if let Some(state) = checkpoint_human_gate_state_raw.as_ref() {
                cp_context.insert(
                    "human_gate_state".to_string(),
                    serde_json::Value::String(state.clone()),
                );
            }
            if let Some(status) = checkpoint_triage_status_raw.as_ref() {
                cp_context.insert(
                    "triage_status".to_string(),
                    serde_json::Value::String(status.clone()),
                );
            }
            if let Some(obj) = context.as_object_mut() {
                obj.insert("checkpoint".to_string(), serde_json::Value::Object(cp_context));
            }
        }

        TaskToolRecord {
            id: uuid,
            title: task.title.clone(),
            description: Some(clean_desc),
            workflow_id,
            triage_status,
            pending_human_action,
            current_human_gate: None,
            workflow_phase: None,
            blocked_reason: None,
            last_failure_reason: None,
            required_action_description: None,
            human_gate_state,
            valid_next_events: vec![],
            allowed_actions: vec![],
            context,
            repo,
            updated_at,
            requested_action,
            action_revision,
        }
    }

    fn assignee_ids(task: &VikunjaTask) -> Vec<i64> {
        let mut ids = task
            .assignees
            .iter()
            .map(|assignee| assignee.id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn transitioned_human_to_bot(
        &self,
        previous_assignee_ids: Option<&[i64]>,
        current_assignee_ids: &[i64],
    ) -> bool {
        let Some(previous) = previous_assignee_ids else {
            return false;
        };

        let previously_assigned_to_bot = previous.contains(&self.bot_user_id);
        let previously_assigned_to_human = previous.iter().any(|id| *id != self.bot_user_id);
        let currently_assigned_to_bot = current_assignee_ids.contains(&self.bot_user_id);

        currently_assigned_to_bot && !previously_assigned_to_bot && previously_assigned_to_human
    }

    fn cache_task_id(&self, task_uuid: Uuid, vikunja_task_id: i64) -> anyhow::Result<()> {
        let mut cache = self
            .task_id_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("vikunja task id cache lock poisoned"))?;
        cache.insert(task_uuid, vikunja_task_id);
        Ok(())
    }

    fn extract_vikunja_task_id(context: &serde_json::Value) -> Option<i64> {
        context
            .get("vikunja_task_id")
            .and_then(serde_json::Value::as_i64)
    }

    fn resolve_task_numeric_id(&self, task_id: &str) -> anyhow::Result<i64> {
        if let Ok(numeric) = task_id.parse::<i64>() {
            return Ok(numeric);
        }

        if let Ok(uuid) = Uuid::parse_str(task_id)
            && let Ok(cache) = self.task_id_cache.lock()
            && let Some(id) = cache.get(&uuid)
        {
            return Ok(*id);
        }

        let records = self.pull_tasks_with_assignment_tracking(false)?;
        let found = records
            .into_iter()
            .find(|r| r.external.id.to_string() == task_id)
            .and_then(|r| Self::extract_vikunja_task_id(&r.external.context));
        found.ok_or_else(|| {
            anyhow::Error::new(TaskAdapterError::TaskNotFound {
                task_id: task_id.to_string(),
            })
        })
    }

    fn pull_tasks_with_assignment_tracking(
        &self,
        track_assignment_transitions: bool,
    ) -> anyhow::Result<Vec<TaskSyncRecord>> {
        let view_id = self.resolve_view_id()?;
        let per_page = 200;
        let mut page = 1;
        let mut tasks = Vec::new();
        loop {
            let mut page_tasks: Vec<VikunjaTask> = self.api_get(&format!(
                "projects/{}/views/{view_id}/tasks?page={page}&per_page={per_page}",
                self.project_id
            ))?;
            let page_len = page_tasks.len();
            tasks.append(&mut page_tasks);

            if page_len < per_page {
                break;
            }
            page += 1;
        }

        let previous_assignment_known = if track_assignment_transitions {
            self.assignment_known
                .lock()
                .map_err(|_| anyhow::anyhow!("vikunja assignment index lock poisoned"))?
                .clone()
        } else {
            HashMap::new()
        };
        let mut current_assignment_known = HashMap::new();

        let mut records = Vec::with_capacity(tasks.len());
        for task in tasks {
            let task_id = vikunja_task_uuid(&self.base_url, task.id).to_string();
            let assignee_ids = Self::assignee_ids(&task);
            let transitioned_human_to_bot = track_assignment_transitions
                && self.transitioned_human_to_bot(
                    previous_assignment_known.get(&task_id).map(Vec::as_slice),
                    &assignee_ids,
                );
            current_assignment_known.insert(task_id, assignee_ids);

            let comments = self.get_task_comments(task.id)?;
            let record = self.vikunja_task_to_record(&task, &comments, transitioned_human_to_bot);
            self.cache_task_id(record.id, task.id)?;
            records.push(TaskSyncRecord { external: record });
        }

        if track_assignment_transitions {
            let mut assignment_known = self
                .assignment_known
                .lock()
                .map_err(|_| anyhow::anyhow!("vikunja assignment index lock poisoned"))?;
            *assignment_known = current_assignment_known;
        }

        Ok(records)
    }

    fn projection_comment(&self, task: &TaskToolRecord) -> String {
        let state_info = self.projection_state_info(task);
        format!("## 🔄 Status Update\n\n{state_info}")
    }

    fn projection_state_info(&self, task: &TaskToolRecord) -> String {
        let triage_status = serde_json::to_value(&task.triage_status)
            .ok()
            .and_then(|v| v.as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "unknown".to_string());

        let phase = task
            .workflow_phase
            .clone()
            .unwrap_or_else(|| "unknown".to_string());

        let waiting_on_human = if task.pending_human_action {
            "✅ Yes"
        } else {
            "❌ No"
        };

        let blocked_reason = task
            .blocked_reason
            .clone()
            .unwrap_or_else(|| "None".to_string());

        format!(
            "| Field | Value |\n|-------|-------|\n| **Phase** | {} |\n| **Status** | {} |\n| **Waiting on human** | {} |\n| **Reason** | {} |",
            markdown_table_cell(&phase),
            markdown_table_cell(&triage_status),
            markdown_table_cell(waiting_on_human),
            markdown_table_cell(&blocked_reason)
        )
    }

    fn format_failure_log_comment(&self, message: &str) -> String {
        let (step, error_summary) = parse_failed_step_message(message)
            .unwrap_or(("unknown_step".to_string(), message.trim().to_string()));

        format!(
            "## ❌ Step Failed: {step}\n\n**Error:** {error_summary}\n\n<details>\n<summary>Details</summary>\n\n{}\n</details>",
            message.trim()
        )
    }

    fn format_triage_result_comment(&self, result: &TriageResult) -> String {
        let workflow = result.workflow_id.as_deref().unwrap_or("unknown");
        let repository = result.repo.as_deref().unwrap_or("unknown");
        let environment = format_environment_selection(result.environment.as_ref());
        let summary = result
            .agent_summary
            .as_deref()
            .unwrap_or(result.summary.as_str());

        let mut out = String::new();
        out.push_str(TRIAGE_RESULT_HEADING);
        out.push_str("\n\n");
        out.push_str(&format!("**Workflow:** {workflow}\n"));
        out.push_str(&format!("**Repository:** {repository}\n"));
        out.push_str(&format!("**Environment:** {environment}\n"));

        out.push_str("\n### Summary\n");
        out.push_str(summary.trim());
        out.push('\n');

        if let Some(scope) = result.scope.as_deref() {
            out.push_str("\n### Scope\n");
            out.push_str(scope.trim());
            out.push('\n');
        }

        if let Some(complexity) = result.complexity.as_deref() {
            out.push_str("\n### Complexity\n");
            out.push_str(complexity.trim());
            out.push('\n');
        }

        if let Some(approach) = result.plan_approach.as_ref()
            && !approach.is_empty()
        {
            out.push_str("\n### Plan\n");
            for (idx, step) in approach.iter().enumerate() {
                out.push_str(&format!("{}. {}\n", idx + 1, step));
            }
        }

        if let Some(files) = result.plan_files.as_ref()
            && !files.is_empty()
        {
            out.push_str("\n### Files to modify\n");
            for file in files {
                out.push_str(&format!("- `{file}`\n"));
            }
        }

        if let Some(risks) = result.plan_risks.as_ref()
            && !risks.is_empty()
        {
            out.push_str("\n### Risks\n");
            for risk in risks {
                out.push_str(&format!("- {risk}\n"));
            }
        }

        out.push_str("\n---\n*Awaiting human approval to proceed.*");
        out
    }

    fn format_job_result_comment(
        &self,
        step_id: &str,
        job_state: &str,
        output: &serde_json::Value,
        context: &serde_json::Value,
    ) -> String {
        let succeeded = job_state.eq_ignore_ascii_case("succeeded");
        if succeeded
            && step_id.contains("triage")
            && let Some(triage_result) = triage_result_from_job_payload(output, context)
        {
            return self.format_triage_result_comment(&triage_result);
        }

        let heading = if succeeded {
            format!("## ✅ {step_id} completed")
        } else {
            format!("## ❌ {step_id} failed")
        };

        let details = if succeeded {
            format_job_result_output_summary(output)
        } else {
            format_job_result_error_details(output)
        };

        format!("{heading}\n\n{details}")
    }
}

fn triage_result_from_job_payload(
    output: &serde_json::Value,
    context: &serde_json::Value,
) -> Option<TriageResult> {
    let workflow_id = non_empty_json_string(output.get("workflow_id"))
        .or_else(|| non_empty_json_string(context.get("workflow_id")));
    let summary = non_empty_json_string(output.get("summary"))
        .or_else(|| non_empty_json_string(context.get("triage_agent_summary")))?;

    let repo = output
        .get("repo")
        .and_then(|value| {
            non_empty_json_string(Some(value))
                .or_else(|| non_empty_json_string(value.get("repo_url")))
        })
        .or_else(|| non_empty_json_string(context.get("repo")));

    let triage = output
        .get("outputs")
        .and_then(|outputs| outputs.get("triage"))
        .or_else(|| output.get("triage"));
    let plan = triage.and_then(|triage| triage.get("plan"));

    Some(TriageResult {
        workflow_id,
        repo,
        environment: None,
        summary: summary.clone(),
        confidence: None,
        scope: non_empty_json_string(triage.and_then(|triage| triage.get("scope")))
            .or_else(|| non_empty_json_string(context.get("triage_scope"))),
        complexity: non_empty_json_string(triage.and_then(|triage| triage.get("complexity")))
            .or_else(|| non_empty_json_string(context.get("triage_complexity"))),
        plan_files: non_empty_json_string_array(plan.and_then(|plan| plan.get("files")))
            .or_else(|| non_empty_json_string_array(context.get("triage_plan_files"))),
        plan_approach: non_empty_json_string_array(plan.and_then(|plan| plan.get("approach")))
            .or_else(|| non_empty_json_string_array(context.get("triage_plan_approach"))),
        plan_risks: non_empty_json_string_array(plan.and_then(|plan| plan.get("risks")))
            .or_else(|| non_empty_json_string_array(context.get("triage_plan_risks"))),
        agent_summary: Some(summary),
    })
}

fn non_empty_json_string(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn non_empty_json_string_array(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let values = value
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn format_job_result_output_summary(output: &serde_json::Value) -> String {
    match output {
        serde_json::Value::Null => "No output provided.".to_string(),
        serde_json::Value::String(raw) => raw.trim().to_string(),
        serde_json::Value::Object(map) => {
            let mut sorted = BTreeMap::new();
            for (key, value) in map {
                sorted.insert(key, value);
            }
            let mut lines = vec!["### Output summary".to_string()];
            for (idx, (key, value)) in sorted.iter().enumerate() {
                if idx >= 8 {
                    lines.push("- … (additional fields omitted)".to_string());
                    break;
                }
                lines.push(format!("- **{key}:** {}", summarize_json_value(value)));
            }
            lines.join("\n")
        }
        _ => format!("```json\n{}\n```", pretty_json(output)),
    }
}

fn format_job_result_error_details(output: &serde_json::Value) -> String {
    if let Some(message) = output
        .get("error")
        .and_then(serde_json::Value::as_str)
        .or_else(|| output.get("message").and_then(serde_json::Value::as_str))
        .or_else(|| output.get("reason").and_then(serde_json::Value::as_str))
    {
        let mut out = format!("**Error:** {}", message.trim());
        if let Some(details) = output.get("details") {
            out.push_str("\n\n### Details\n");
            out.push_str(&format!("```json\n{}\n```", pretty_json(details)));
        }
        return out;
    }

    format!("```json\n{}\n```", pretty_json(output))
}

/// Outer cap so a runaway agent dumping multi-MB into a string field doesn't
/// overflow Vikunja's comment limit. Real summaries are typically a few
/// hundred characters; this leaves plenty of headroom while still bounding
/// pathological output. Truncation is char-aware to avoid panics on
/// multi-byte boundaries.
const SUMMARIZE_STRING_HARD_CAP: usize = 8192;

fn summarize_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(v) => v.to_string(),
        serde_json::Value::Number(v) => v.to_string(),
        serde_json::Value::String(v) => truncate_chars_with_ellipsis(v.trim(), SUMMARIZE_STRING_HARD_CAP),
        serde_json::Value::Array(values) => format!("array({})", values.len()),
        serde_json::Value::Object(values) => {
            let keys = values.keys().take(3).cloned().collect::<Vec<_>>();
            if keys.is_empty() {
                "object{}".to_string()
            } else {
                format!("object{{{}}}", keys.join(", "))
            }
        }
    }
}

fn truncate_chars_with_ellipsis(input: &str, max_chars: usize) -> String {
    let mut iter = input.char_indices();
    if let Some((boundary, _)) = iter.nth(max_chars) {
        let mut out = String::with_capacity(boundary + 1);
        out.push_str(&input[..boundary]);
        out.push('…');
        out
    } else {
        input.to_string()
    }
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn markdown_table_cell(value: &str) -> String {
    value.trim().replace('|', "\\|").replace('\n', " ")
}

fn parse_failed_step_message(message: &str) -> Option<(String, String)> {
    let trimmed = message.trim();
    let rest = trimmed.strip_prefix("❌ Step '")?;
    let (step, reason) = rest.split_once("' failed: ")?;
    Some((step.trim().to_string(), reason.trim().to_string()))
}

fn format_environment_selection(environment: Option<&protocol::EnvironmentSelection>) -> String {
    let Some(environment) = environment else {
        return "unknown".to_string();
    };
    if environment.overlays.is_empty() {
        return environment.base.clone();
    }
    format!(
        "{} (overlays: {})",
        environment.base,
        environment.overlays.join(", ")
    )
}

fn parse_bold_field(comment: &str, name: &str) -> Option<String> {
    let prefix = format!("**{name}:**");
    comment.lines().find_map(|line| {
        line.trim()
            .strip_prefix(&prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn parse_environment_selection_comment_value(
    value: &str,
) -> Option<protocol::EnvironmentSelection> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("unknown") {
        return None;
    }

    if let Some((base, overlays_part)) = trimmed.split_once(" (overlays:") {
        let base = base.trim();
        let overlays = overlays_part
            .trim()
            .trim_start_matches(':')
            .trim()
            .trim_end_matches(')')
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if base.is_empty() {
            return None;
        }
        return Some(protocol::EnvironmentSelection {
            base: base.to_string(),
            overlays,
        });
    }

    Some(protocol::EnvironmentSelection {
        base: trimmed.to_string(),
        overlays: Vec::new(),
    })
}

fn extract_markdown_section(comment: &str, heading: &str) -> Option<String> {
    let marker = format!("### {heading}");
    let start = comment.find(&marker)?;
    let after = &comment[start + marker.len()..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    let end = after
        .find("\n### ")
        .or_else(|| after.find("\n---"))
        .unwrap_or(after.len());
    let content = after[..end].trim();
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

fn parse_numbered_markdown_list(section: &str) -> Option<Vec<String>> {
    let entries = section
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let dot = trimmed.find('.')?;
            if !trimmed[..dot].chars().all(|ch| ch.is_ascii_digit()) {
                return None;
            }
            let value = trimmed[dot + 1..].trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
}

fn parse_bulleted_markdown_list(section: &str) -> Option<Vec<String>> {
    let entries = section
        .lines()
        .filter_map(|line| {
            let value = line.trim().strip_prefix("- ")?.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.trim_matches('`').to_string())
            }
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
}

fn parse_triage_result_markdown_comment(comment: &str) -> Option<TriageResult> {
    if !comment.contains(TRIAGE_RESULT_HEADING) {
        return None;
    }

    let workflow_id = parse_bold_field(comment, "Workflow")
        .filter(|value| !value.eq_ignore_ascii_case("unknown"));
    let repo = parse_bold_field(comment, "Repository")
        .filter(|value| !value.eq_ignore_ascii_case("unknown"));
    let environment = parse_bold_field(comment, "Environment")
        .as_deref()
        .and_then(parse_environment_selection_comment_value);

    let summary_section = extract_markdown_section(comment, "Summary");
    let summary = summary_section
        .clone()
        .unwrap_or_else(|| "triage completed".to_string());

    let scope = extract_markdown_section(comment, "Scope");
    let complexity = extract_markdown_section(comment, "Complexity");
    let plan_approach = extract_markdown_section(comment, "Plan")
        .as_deref()
        .and_then(parse_numbered_markdown_list);
    let plan_files = extract_markdown_section(comment, "Files to modify")
        .as_deref()
        .and_then(parse_bulleted_markdown_list);
    let plan_risks = extract_markdown_section(comment, "Risks")
        .as_deref()
        .and_then(parse_bulleted_markdown_list);

    Some(TriageResult {
        workflow_id,
        repo,
        environment,
        summary: summary.clone(),
        confidence: None,
        scope,
        complexity,
        plan_files,
        plan_approach,
        plan_risks,
        agent_summary: Some(summary),
    })
}

impl TaskAdapter for VikunjaAdapter {
    fn adapter_name(&self) -> &'static str {
        "vikunja"
    }

    fn pull_tasks(&self) -> anyhow::Result<Vec<TaskSyncRecord>> {
        self.pull_tasks_with_assignment_tracking(true)
    }

    fn get_task(&self, id: Uuid) -> anyhow::Result<Option<TaskToolRecord>> {
        let tasks = self.pull_tasks_with_assignment_tracking(false)?;
        Ok(tasks
            .into_iter()
            .map(|t| t.external)
            .find(|task| task.id == id))
    }

    fn persist_projection(
        &self,
        projection: &TaskProjection,
        status_comment_id: Option<i64>,
    ) -> anyhow::Result<Option<i64>> {
        let task_id = Self::extract_vikunja_task_id(&projection.task.context)
            .ok_or_else(|| anyhow::anyhow!("missing vikunja_task_id in task context"))?;

        let comments = self.get_task_comments(task_id)?;
        let projection_work_item_id = projection.task.id.to_string();
        let status_comment_id = self.write_comment(
            task_id,
            Some(CommentKind::Status),
            Some(&projection_work_item_id),
            &self.projection_comment(&projection.task),
            status_comment_id,
        )?;

        let current_labels: Vec<VikunjaLabel> = self.api_get(&format!("tasks/{task_id}/labels"))?;
        let state_prefix = self.namespaced_label("state:");
        let mut desired = current_labels
            .iter()
            .map(|label| label.title.clone())
            .filter(|title| !title.starts_with(&state_prefix))
            .collect::<Vec<_>>();

        let phase = projection
            .task
            .workflow_phase
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        desired.push(self.namespaced_label(&format!("state:{phase}")));
        self.set_task_labels(task_id, &desired, &current_labels)?;

        // Only mark the Vikunja task as done when work genuinely completed.
        // Cancelled/failed work items should NOT mark the external task done — the
        // human may want to retry or reassign.
        let terminal_comment = match projection.task.workflow_phase.as_deref() {
            Some("completed") => Some("✅ Task completed — work item reached terminal state"),
            _ => match projection.task.triage_status {
                TaskTriageStatus::Completed => {
                    Some("✅ Task completed — work item reached terminal state")
                }
                _ => None,
            },
        };

        if let Some(terminal_comment) = terminal_comment {
            let already_done = projection
                .task
                .context
                .get("done")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);

            if !already_done {
                let _: serde_json::Value = self.api_post(
                    &format!("tasks/{task_id}"),
                    &VikunjaTaskDoneRequest { done: true },
                )?;
            }

            let has_terminal_comment = comments
                .iter()
                .any(|comment| comment.comment.contains(terminal_comment));

            if !has_terminal_comment {
                let _ = self.write_comment(task_id, None, None, terminal_comment, None)?;
            }
        }

        Ok(Some(status_comment_id))
    }

    fn update_task(&self, task_id: &str, update: TaskUpdate) -> anyhow::Result<()> {
        let vikunja_task_id = self.resolve_task_numeric_id(task_id)?;

        if let Some(labels) = update.labels {
            let current_labels: Vec<VikunjaLabel> =
                self.api_get(&format!("tasks/{vikunja_task_id}/labels"))?;
            self.set_task_labels(vikunja_task_id, &labels, &current_labels)?;
        }

        if let Some(status) = update.status {
            let _ = self.write_comment(
                vikunja_task_id,
                None,
                None,
                &format!("agent-hub status updated to: {status}"),
                None,
            )?;
        }

        Ok(())
    }

    fn create_task(&self, create: TaskCreate) -> anyhow::Result<String> {
        let created: VikunjaTaskCreated = self.api_put(
            &format!("projects/{}/tasks", self.project_id),
            &VikunjaCreateTaskRequest {
                title: create.title,
                description: create.description,
            },
        )?;

        if !create.labels.is_empty() {
            let current_labels: Vec<VikunjaLabel> =
                self.api_get(&format!("tasks/{}/labels", created.id))?;
            self.set_task_labels(created.id, &create.labels, &current_labels)?;
        }

        let uuid = vikunja_task_uuid(&self.base_url, created.id);
        self.cache_task_id(uuid, created.id)?;
        Ok(uuid.to_string())
    }

    fn update_checkpoint(
        &self,
        task_id: &str,
        checkpoint: &TaskCheckpoint,
        work_item_id: Uuid,
        comment_id: Option<i64>,
    ) -> anyhow::Result<Option<i64>> {
        let vikunja_task_id = self.resolve_task_numeric_id(task_id)?;
        let comment = format_marked_yaml_comment(CHECKPOINT_MARKER, checkpoint)?;
        let work_item_id = work_item_id.to_string();
        let comment_id = self.write_comment(
            vikunja_task_id,
            Some(CommentKind::Checkpoint),
            Some(&work_item_id),
            &comment,
            comment_id,
        )?;
        Ok(Some(comment_id))
    }

    fn append_log(&self, task_id: &str, message: &str) -> anyhow::Result<()> {
        let vikunja_task_id = self.resolve_task_numeric_id(task_id)?;
        let comment = self.format_failure_log_comment(message);
        self.append_task_comment(vikunja_task_id, comment)
    }

    fn write_job_result_comment(
        &self,
        task_id: &str,
        step_id: &str,
        work_item_id: &str,
        job_succeeded: bool,
        output: &serde_json::Value,
        context: &serde_json::Value,
        stored_comment_id: Option<i64>,
    ) -> anyhow::Result<Option<i64>> {
        let vikunja_task_id = self.resolve_task_numeric_id(task_id)?;
        let job_state = if job_succeeded { "succeeded" } else { "failed" };
        let comment = self.format_job_result_comment(step_id, job_state, output, context);
        let comment_id = self.write_comment(
            vikunja_task_id,
            Some(CommentKind::JobResult(step_id.to_string())),
            Some(work_item_id),
            &comment,
            stored_comment_id,
        )?;
        Ok(Some(comment_id))
    }

    fn read_checkpoint(&self, task_id: &str) -> anyhow::Result<Option<TaskCheckpoint>> {
        let vikunja_task_id = self.resolve_task_numeric_id(task_id)?;
        let comments = self.get_task_comments(vikunja_task_id)?;

        for comment in comments.iter().rev() {
            let Some(marker) =
                extract_agent_hub_marker(&comment.comment, "<!-- agent-hub:checkpoint")
            else {
                continue;
            };

            let Some(yaml) = extract_yaml_block(&comment.comment, &marker) else {
                tracing::warn!(
                    comment_id = comment.id,
                    task_id = vikunja_task_id,
                    "checkpoint marker found but no YAML block present"
                );
                continue;
            };

            match serde_yaml::from_str::<TaskCheckpoint>(&yaml) {
                Ok(parsed) => return Ok(Some(parsed)),
                Err(error) => {
                    tracing::warn!(
                        comment_id = comment.id,
                        task_id = vikunja_task_id,
                        error = ?error,
                        "failed to parse checkpoint YAML comment"
                    );
                }
            }
        }

        Ok(None)
    }

    fn read_triage_result(&self, task_id: &str) -> anyhow::Result<Option<TriageResult>> {
        let vikunja_task_id = self.resolve_task_numeric_id(task_id)?;
        let comments = self.get_task_comments(vikunja_task_id)?;

        for comment in comments.iter().rev() {
            if let Some(parsed) = parse_triage_result_markdown_comment(&comment.comment) {
                return Ok(Some(parsed));
            }

            if !comment.comment.contains(TRIAGE_RESULT_MARKER) {
                continue;
            }

            let Some(yaml) = extract_yaml_block(&comment.comment, TRIAGE_RESULT_MARKER) else {
                tracing::warn!(
                    comment_id = comment.id,
                    task_id = vikunja_task_id,
                    "triage result marker found but no YAML block present"
                );
                continue;
            };

            match serde_yaml::from_str::<TriageResult>(&yaml) {
                Ok(parsed) => return Ok(Some(parsed)),
                Err(error) => {
                    tracing::warn!(
                        comment_id = comment.id,
                        task_id = vikunja_task_id,
                        error = ?error,
                        "failed to parse triage result YAML comment"
                    );
                }
            }
        }

        Ok(None)
    }

    fn set_labels(&self, task_id: &str, labels: &TaskLabels) -> anyhow::Result<()> {
        let vikunja_task_id = self.resolve_task_numeric_id(task_id)?;
        let current_labels: Vec<VikunjaLabel> =
            self.api_get(&format!("tasks/{vikunja_task_id}/labels"))?;

        let workflow_prefix = self.namespaced_label("workflow:");
        let step_prefix = self.namespaced_label("step:");
        let phase_prefix = self.namespaced_label("phase:");
        let failed_label = self.namespaced_label("failed");

        let mut desired = current_labels
            .iter()
            .map(|label| label.title.clone())
            .filter(|title| {
                !title.starts_with(&workflow_prefix)
                    && !title.starts_with(&step_prefix)
                    && !title.starts_with(&phase_prefix)
                    && title != &failed_label
            })
            .collect::<Vec<_>>();

        if let Some(workflow) = &labels.workflow {
            desired.push(self.namespaced_label(&format!("workflow:{workflow}")));
        }
        if let Some(step) = &labels.step {
            desired.push(self.namespaced_label(&format!("step:{step}")));
        }
        if let Some(phase) = &labels.phase {
            desired.push(self.namespaced_label(&format!("phase:{phase}")));
        }
        if labels.failed {
            desired.push(failed_label);
        }

        self.set_task_labels(vikunja_task_id, &desired, &current_labels)
    }

    fn set_assignee(&self, task_id: &str, assignee: &TaskAssignee) -> anyhow::Result<()> {
        let vikunja_task_id = self.resolve_task_numeric_id(task_id)?;

        let target_user_id = match assignee {
            TaskAssignee::Bot => Some(self.bot_user_id),
            TaskAssignee::Human(_) => Some(self.operator_user_id),
            TaskAssignee::Unassigned => None,
        };

        if let Some(target_user_id) = target_user_id {
            let task: VikunjaTask = self.api_get(&format!("tasks/{vikunja_task_id}"))?;
            if task.assignees.len() == 1 && task.assignees[0].id == target_user_id {
                return Ok(());
            }
            self.clear_task_assignees(vikunja_task_id)?;
            self.assign_user_to_task(vikunja_task_id, target_user_id)?;
        } else {
            self.clear_task_assignees(vikunja_task_id)?;
        }

        Ok(())
    }

    fn poll_changes(&self) -> anyhow::Result<Vec<TaskChange>> {
        let current_records = self.pull_tasks()?;
        let mut known = self
            .poll_known
            .lock()
            .map_err(|_| anyhow::anyhow!("vikunja poll index lock poisoned"))?;

        let mut current = HashMap::new();
        let mut changes = Vec::new();

        for record in &current_records {
            let task_id = record.external.id.to_string();
            let current_assignee_ids = record
                .external
                .context
                .get("assignee_ids")
                .and_then(serde_json::Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(serde_json::Value::as_i64)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            current.insert(
                task_id.clone(),
                VikunjaPollKnownTask {
                    updated_at: record.external.updated_at,
                    assignee_ids: current_assignee_ids.clone(),
                },
            );

            match known.get(&task_id) {
                None => changes.push(TaskChange {
                    task_id,
                    change_type: TaskChangeType::Created,
                }),
                Some(previous)
                    if record.external.updated_at > previous.updated_at
                        || self.transitioned_human_to_bot(
                            Some(previous.assignee_ids.as_slice()),
                            &current_assignee_ids,
                        ) =>
                {
                    changes.push(TaskChange {
                        task_id,
                        change_type: TaskChangeType::Updated,
                    })
                }
                _ => {}
            }
        }

        for task_id in known.keys() {
            if !current.contains_key(task_id) {
                changes.push(TaskChange {
                    task_id: task_id.clone(),
                    change_type: TaskChangeType::Deleted,
                });
            }
        }

        *known = current;
        Ok(changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint_comment(human_gate: bool) -> String {
        checkpoint_comment_full(
            "wf",
            "review",
            "review",
            human_gate,
            None,
            None,
        )
    }

    fn checkpoint_comment_full(
        workflow_id: &str,
        phase: &str,
        current_step: &str,
        human_gate: bool,
        human_gate_state: Option<&str>,
        triage_status: Option<&str>,
    ) -> String {
        let human_gate_state_line = match human_gate_state {
            Some(s) => format!("human_gate_state: {s}"),
            None => "human_gate_state: null".to_string(),
        };
        let triage_status_line = match triage_status {
            Some(s) => format!("triage_status: {s}"),
            None => "triage_status: null".to_string(),
        };
        format!(
            "{CHECKPOINT_MARKER}\n```yaml\nworkflow_id: {workflow_id}\nphase: {phase}\ncurrent_step: {current_step}\nhuman_gate: {human_gate}\nupdated_at: 2026-01-01T00:00:00Z\n{human_gate_state_line}\n{triage_status_line}\n```"
        )
    }

    #[test]
    fn test_vikunja_task_uuid_is_stable() {
        let first = vikunja_task_uuid("http://localhost:3456", 123);
        let second = vikunja_task_uuid("http://localhost:3456", 123);
        let different = vikunja_task_uuid("http://localhost:3456", 124);

        assert_eq!(first, second);
        assert_ne!(first, different);
    }

    #[test]
    fn test_parse_description_metadata() {
        let input = "Task description\n---\nagent_hub:\n  workflow_id: test_wf\n";
        let (desc, metadata) = parse_description_metadata(input);

        assert_eq!(desc, "Task description");
        let metadata = metadata.expect("metadata should parse");
        assert_eq!(metadata.workflow_id.as_deref(), Some("test_wf"));
    }

    #[test]
    fn test_parse_description_no_metadata() {
        let (desc, metadata) = parse_description_metadata("plain description");
        assert_eq!(desc, "plain description");
        assert!(metadata.is_none());
    }

    #[test]
    fn test_parse_comment_action_approve() {
        let action = parse_comment_action("/ah approve some reason").expect("action expected");
        assert_eq!(action.kind, HumanActionKind::Approve);
        assert_eq!(action.reason.as_deref(), Some("some reason"));
        assert!(action.workflow_id.is_none());
    }

    #[test]
    fn test_parse_comment_action_workflow() {
        let action = parse_comment_action("/ah workflow my_wf").expect("action expected");
        assert_eq!(action.kind, HumanActionKind::ChooseWorkflow);
        assert_eq!(action.workflow_id.as_deref(), Some("my_wf"));
        assert!(action.reason.is_none());
    }

    #[test]
    fn test_parse_comment_action_unknown() {
        assert!(parse_comment_action("/ah unknown").is_none());
    }

    #[test]
    fn test_parse_comment_action_not_first_line() {
        let comment = "first line\n/ah approve looks good";
        let action = parse_comment_action(comment).expect("action expected");
        assert_eq!(action.kind, HumanActionKind::Approve);
        assert_eq!(action.reason.as_deref(), Some("looks good"));
    }

    #[test]
    fn test_parse_description_metadata_uses_last_separator() {
        let input =
            "Task description\n---\nnot: metadata\n---\nagent_hub:\n  workflow_id: test_wf\n";
        let (desc, metadata) = parse_description_metadata(input);

        assert_eq!(desc, "Task description\n---\nnot: metadata");
        let metadata = metadata.expect("metadata should parse");
        assert_eq!(metadata.workflow_id.as_deref(), Some("test_wf"));
    }

    #[test]
    fn test_parse_description_metadata_empty_block() {
        let input = "Task description\n---\n   \n";
        let (desc, metadata) = parse_description_metadata(input);

        assert_eq!(desc, "Task description");
        assert!(metadata.is_none());
    }

    #[test]
    fn test_parse_comment_action_with_whitespace_and_case_insensitive_command() {
        let action =
            parse_comment_action("  /ah    ApPrOvE   looks   good ").expect("action expected");
        assert_eq!(action.kind, HumanActionKind::Approve);
        assert_eq!(action.reason.as_deref(), Some("looks good"));
    }

    #[test]
    fn test_parse_comment_action_html_wrapped_unblock() {
        // Vikunja's WYSIWYG editor emits this when a user types `/ah unblock`.
        let action = parse_comment_action("<p><code>/ah unblock</code>&nbsp;</p>")
            .expect("HTML-wrapped /ah should parse");
        assert_eq!(action.kind, HumanActionKind::Unblock);
        assert!(action.reason.is_none());
    }

    #[test]
    fn test_parse_comment_action_html_wrapped_approve_with_reason() {
        let action = parse_comment_action(
            "<p><code>/ah approve looks good to me</code></p>",
        )
        .expect("HTML-wrapped /ah approve should parse");
        assert_eq!(action.kind, HumanActionKind::Approve);
        assert_eq!(action.reason.as_deref(), Some("looks good to me"));
    }

    #[test]
    fn test_parse_comment_action_html_entity_amp_in_reason() {
        let action = parse_comment_action(
            "<p>/ah request_changes fix &amp; retry</p>",
        )
        .expect("HTML-encoded entity should decode");
        assert_eq!(action.kind, HumanActionKind::RequestChanges);
        assert_eq!(action.reason.as_deref(), Some("fix & retry"));
    }

    #[test]
    fn test_summarize_long_string_value_passes_through() {
        // Real-world summary lengths run a few hundred characters; the legacy
        // 160-char cap was clipping useful content. This covers the common case.
        let long = "Added ipam ContainerPrefix prod-east-ipv4-auto at \
                    resources/compute/ipam/container_prefixes/prod_east_ipv4_auto.yaml \
                    with CIDR 10.56.0.0/13 for orch-1 prod-east. \
                    Includes vlan_id 42, gateway 10.56.0.1, and matching DNS records.";
        let value = serde_json::Value::String(long.to_string());
        let summarized = summarize_json_value(&value);
        assert!(!summarized.ends_with('…'), "should not be truncated");
        assert_eq!(summarized, long.trim());
    }

    #[test]
    fn test_summarize_string_value_hits_hard_cap_char_safely() {
        // Build a string longer than the hard cap that contains multi-byte
        // characters near (but not exactly on) the boundary. Truncation must
        // not panic and the result should end with the ellipsis marker.
        let mut long = String::new();
        for _ in 0..(SUMMARIZE_STRING_HARD_CAP + 50) {
            long.push('é');
        }
        let value = serde_json::Value::String(long);
        let summarized = summarize_json_value(&value);
        assert!(summarized.ends_with('…'));
        assert_eq!(
            summarized.chars().filter(|c| *c == 'é').count(),
            SUMMARIZE_STRING_HARD_CAP,
        );
    }

    #[test]
    fn test_truncate_chars_with_ellipsis_preserves_unicode_boundary() {
        // Boundary character is multi-byte; naive byte slicing would panic.
        let input = "a😀b😀c";
        assert_eq!(truncate_chars_with_ellipsis(input, 2), "a😀…");
        assert_eq!(truncate_chars_with_ellipsis(input, 99), "a😀b😀c");
    }

    #[test]
    fn test_html_to_plain_text_basic() {
        // Tag-stripping + html-escape's entity decoder. Whitespace-splitting
        // is what parse_comment_action ultimately does, and U+00A0 (the
        // result of decoding &nbsp;) counts as whitespace there.
        assert_eq!(
            html_to_plain_text("<p><code>/ah unblock</code>&nbsp;</p>")
                .split_whitespace()
                .collect::<Vec<_>>(),
            vec!["/ah", "unblock"],
        );
        assert_eq!(html_to_plain_text("a &amp; b"), "a & b");
        assert_eq!(html_to_plain_text("plain text"), "plain text");
        // Less-common entities the old hand-rolled table didn't include
        // (e.g. &mdash;, &hellip;) decode correctly via the real entity
        // table — that's the whole point of the dep swap.
        assert_eq!(html_to_plain_text("a &mdash; b"), "a \u{2014} b");
        assert_eq!(html_to_plain_text("done&hellip;"), "done\u{2026}");
        // Invalid / unknown entities are left as-is rather than being
        // silently dropped — safer default for arbitrary user input.
        assert_eq!(html_to_plain_text("&unknown;"), "&unknown;");
        assert_eq!(html_to_plain_text("orphan & sign"), "orphan & sign");
    }

    #[test]
    fn test_latest_checkpoint_from_comments_prefers_latest_parseable() {
        let comments = vec![
            VikunjaComment {
                id: 1,
                comment: checkpoint_comment(false),
                author: VikunjaUser {
                    id: 10,
                    username: "alice".to_string(),
                    email: None,
                },
                created: "2026-01-01T00:00:00Z".to_string(),
                updated: "2026-01-01T00:00:00Z".to_string(),
            },
            VikunjaComment {
                id: 2,
                comment: checkpoint_comment(true),
                author: VikunjaUser {
                    id: 11,
                    username: "bob".to_string(),
                    email: None,
                },
                created: "2026-01-01T00:01:00Z".to_string(),
                updated: "2026-01-01T00:01:00Z".to_string(),
            },
        ];

        let checkpoint = latest_checkpoint_from_comments(&comments).expect("checkpoint expected");
        assert!(checkpoint.human_gate);
    }

    #[test]
    fn test_transitioned_human_to_bot_only_on_human_to_bot_change() {
        let adapter = VikunjaAdapter {
            client: reqwest::blocking::Client::new(),
            base_url: "http://localhost:3456".to_string(),
            auth: VikunjaAuth::StaticToken("token".to_string()),
            project_id: 1,
            view_id: Mutex::new(None),
            label_prefix: "ah".to_string(),
            task_id_cache: Mutex::new(HashMap::new()),
            poll_known: Mutex::new(HashMap::new()),
            assignment_known: Mutex::new(HashMap::new()),
            bot_user_id: 99,
            operator_user_id: 42,
        };

        assert!(adapter.transitioned_human_to_bot(Some(&[42]), &[99]));
        assert!(!adapter.transitioned_human_to_bot(Some(&[99]), &[99]));
        assert!(!adapter.transitioned_human_to_bot(Some(&[]), &[99]));
        assert!(!adapter.transitioned_human_to_bot(None, &[99]));
    }

    fn test_adapter() -> VikunjaAdapter {
        VikunjaAdapter {
            client: reqwest::blocking::Client::new(),
            base_url: "http://localhost:3456".to_string(),
            auth: VikunjaAuth::StaticToken("token".to_string()),
            project_id: 1,
            view_id: Mutex::new(None),
            label_prefix: "ah".to_string(),
            task_id_cache: Mutex::new(HashMap::new()),
            poll_known: Mutex::new(HashMap::new()),
            assignment_known: Mutex::new(HashMap::new()),
            bot_user_id: 99,
            operator_user_id: 42,
        }
    }

    fn test_task() -> VikunjaTask {
        VikunjaTask {
            id: 1,
            title: "task".to_string(),
            description: String::new(),
            done: false,
            priority: 0,
            labels: vec![],
            assignees: vec![],
            created: "2026-01-01T00:00:00Z".to_string(),
            updated: "2026-01-01T00:01:00Z".to_string(),
            project_id: 1,
            index: 1,
            identifier: None,
        }
    }

    fn checkpoint_vikunja_comment(body: &str) -> VikunjaComment {
        VikunjaComment {
            id: 1,
            comment: body.to_string(),
            author: VikunjaUser {
                id: 42,
                username: "nick".to_string(),
                email: None,
            },
            created: "2026-01-01T00:00:00Z".to_string(),
            updated: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn test_record_reflects_post_triage_checkpoint() {
        let adapter = test_adapter();
        let task = test_task();
        let comments = vec![checkpoint_vikunja_comment(&checkpoint_comment_full(
            "__triage__",
            "triage",
            "await_triage_approval",
            true,
            Some("triage_completed"),
            Some("pending_approval"),
        ))];

        let record = adapter.vikunja_task_to_record(&task, &comments, false);

        assert!(record.pending_human_action);
        assert_eq!(record.human_gate_state, None);
        assert!(matches!(
            record.triage_status,
            TaskTriageStatus::NeedsWorkflowSelection
        ));

        let cp = record
            .context
            .get("checkpoint")
            .and_then(|v| v.as_object())
            .expect("checkpoint context should be present");
        assert_eq!(cp.get("human_gate"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(
            cp.get("human_gate_state").and_then(|v| v.as_str()),
            Some("triage_completed"),
        );
        assert_eq!(
            cp.get("triage_status").and_then(|v| v.as_str()),
            Some("pending_approval"),
        );
    }

    #[test]
    fn test_record_without_checkpoint_uses_label_defaults() {
        let adapter = test_adapter();
        let task = test_task();

        let record = adapter.vikunja_task_to_record(&task, &[], false);

        assert!(!record.pending_human_action);
        assert_eq!(record.human_gate_state, None);
        assert!(matches!(
            record.triage_status,
            TaskTriageStatus::NeedsWorkflowSelection
        ));
        assert!(record.context.get("checkpoint").is_none());
    }

    #[test]
    fn test_assignment_to_bot_without_open_gate_does_not_approve() {
        let adapter = test_adapter();
        let task = test_task();
        let comments = vec![checkpoint_vikunja_comment(&checkpoint_comment_full(
            "wf",
            "review",
            "review",
            false,
            None,
            None,
        ))];

        let record = adapter.vikunja_task_to_record(&task, &comments, true);

        assert!(record.requested_action.is_none());
        assert_eq!(record.action_revision, 0);
    }

    #[test]
    fn test_assignment_to_bot_with_open_gate_synthesises_approve() {
        let adapter = test_adapter();
        let task = test_task();
        let comments = vec![checkpoint_vikunja_comment(&checkpoint_comment_full(
            "__triage__",
            "triage",
            "await_triage_approval",
            true,
            Some("triage_completed"),
            Some("pending_approval"),
        ))];

        let record = adapter.vikunja_task_to_record(&task, &comments, true);

        let action = record
            .requested_action
            .as_ref()
            .expect("approve action expected");
        assert_eq!(action.kind, HumanActionKind::Approve);
    }
}
