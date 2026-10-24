//! Host HTTP contract. Only disposable policy fixtures and loopback servers are
//! used; child-process environment overrides never mutate the test runner.
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

#[derive(Clone, Default)]
struct HttpState {
    calls: Arc<Mutex<Vec<(String, Option<String>, Value)>>>,
    unauthorized: bool,
}

struct Server {
    url: String,
    state: HttpState,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(unauthorized: bool) -> Self {
        let state = HttpState {
            unauthorized,
            ..Default::default()
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(handle).with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, state, task }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle(
    State(state): State<HttpState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    let payload = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    state.calls.lock().unwrap().push((
        uri.path().into(),
        headers
            .get("authorization")
            .map(|h| h.to_str().unwrap().into()),
        payload,
    ));
    if state.unauthorized {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"invalid credentials"})),
        );
    }
    let response = match uri.path() {
        "/api/v1/hooks/approval" | "/api/v1/hooks/question" => {
            json!({"id":Uuid::from_u128(1), "type":"pending"})
        }
        path if path.starts_with("/api/v1/approvals/") => {
            json!({"type":"approved", "message":null})
        }
        path if path.starts_with("/api/v1/questions/") => {
            json!({"type":"answered", "answers":[["Yes"]]})
        }
        "/api/v1/hooks/status" => json!({}),
        path => panic!("unexpected HTTP request {path}"),
    };
    (StatusCode::OK, Json(response))
}

struct Policy(PathBuf);

impl Policy {
    fn new(action: &str) -> Self {
        let root = std::env::temp_dir().join(format!("gateway-host-http-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let policy = Self(root);
        policy.write(json!({"version":1, "default":action, "rules":[]}));
        policy
    }

    fn write(&self, value: Value) {
        std::fs::write(self.0.join("permissions.json"), value.to_string()).unwrap();
    }

    async fn run(
        &self,
        command: &str,
        server: Option<&str>,
        token: Option<&str>,
        input: Value,
    ) -> Output {
        let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-hub-gateway"));
        process
            .arg(command)
            .env_remove("AGENT_HUB_SERVER")
            .env_remove("AGENT_HUB_TOKEN")
            .env_remove("AGENT_HUB_CONFIG");
        if command != "status-report" {
            process.args(["--opencode", "--timeout", "2"]);
        }
        if command == "approval" {
            process.arg("--config").arg(self.0.join("permissions.json"));
        }
        if let Some(server) = server {
            process.args(["--server", server]);
        }
        if let Some(token) = token {
            process.args(["--token", token]);
        }
        let mut child = process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(8), child.wait_with_output())
            .await
            .unwrap()
            .unwrap()
    }
}

impl Drop for Policy {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn approval_input() -> Value {
    json!({"session_id":"host-http", "tool_name":"bash", "tool_input":{"command":"pwd"}, "cwd":"/workspace", "workspace_roots":["/workspace"], "hook_event_name":"tool.execute.before"})
}

fn question_input() -> Value {
    json!({"id":"host-question", "session_id":"host-http", "session_display_name":"Host HTTP", "cwd":"/workspace", "question_request_id":"native-question", "provider":"opencode", "questions":[{"question":"Continue?", "header":"Decision", "options":[{"label":"Yes", "description":"Continue"}], "multiple":false, "custom":true}]})
}

fn status_input() -> Value {
    json!({"session_id":"host-http", "cwd":"/workspace", "status":"idle"})
}

#[tokio::test]
async fn delegate_receives_claude_pre_tool_use_event_with_original_input() {
    let policy = Policy::new("deny");
    let script = policy.0.join("delegate.sh");
    let capture = policy.0.join("delegate-input.json");
    std::fs::write(
        &script,
        "cat > \"$1\"\nprintf '%s' '{\"hookSpecificOutput\":{\"permissionDecision\":\"allow\"}}'\n",
    )
    .unwrap();
    policy.write(json!({"version":1,"default":"deny","rules":[{"tools":["Bash"],"action":"delegate","command":format!("sh {} {}", script.display(), capture.display())}]}));
    let raw = approval_input();
    let result = policy.run("approval", None, None, raw.clone()).await;
    assert!(result.status.success());
    let delegated: Value = serde_json::from_slice(&std::fs::read(capture).unwrap()).unwrap();
    assert_eq!(delegated["hook_event_name"], "PreToolUse");
    assert_eq!(delegated["tool_name"], "Bash");
    assert_eq!(delegated["tool_input"], raw["tool_input"]);
}

#[tokio::test]
async fn pure_local_routing_needs_neither_server_nor_token_but_ask_fails_closed_without_server() {
    let policy = Policy::new("allow");
    let output = policy.run("approval", None, None, approval_input()).await;
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["allowed"],
        true
    );
    policy.write(json!({"version":1, "default":"ask", "rules":[]}));
    let output = policy.run("approval", None, None, approval_input()).await;
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["allowed"],
        false
    );
}

#[tokio::test]
async fn host_http_sends_no_auth_without_token_and_exact_bearer_when_present() {
    for token in [None, Some("test-token")] {
        let server = Server::start(false).await;
        let policy = Policy::new("ask");
        for (command, input) in [
            ("approval", approval_input()),
            ("question", question_input()),
            ("status-report", status_input()),
        ] {
            let output = policy.run(command, Some(&server.url), token, input).await;
            assert!(
                output.status.success(),
                "{command}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let calls = server.state.calls.lock().unwrap();
        assert_eq!(calls.len(), 5);
        for (_, authorization, _) in calls.iter() {
            assert_eq!(authorization.as_deref(), token.map(|_| "Bearer test-token"));
        }
    }
}

#[tokio::test]
async fn rejected_bearer_credentials_are_never_retried_without_authentication() {
    let server = Server::start(true).await;
    let policy = Policy::new("ask");
    let output = policy
        .run(
            "approval",
            Some(&server.url),
            Some("bad-token"),
            approval_input(),
        )
        .await;
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["allowed"],
        false
    );
    for (command, input, code) in [
        ("question", question_input(), 4),
        ("status-report", status_input(), 1),
    ] {
        assert_eq!(
            policy
                .run(command, Some(&server.url), Some("bad-token"), input)
                .await
                .status
                .code(),
            Some(code)
        );
    }
    let calls = server.state.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert!(
        calls
            .iter()
            .all(|(_, auth, _)| auth.as_deref() == Some("Bearer bad-token"))
    );
}
