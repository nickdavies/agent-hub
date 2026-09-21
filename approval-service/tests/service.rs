use agent_hub_approval_service::config::{ApprovalFeatureMode, AuthMode, ServerConfig};
use agent_hub_approval_service::notifier::NullNotifier;
use agent_hub_approval_service::sessions::SessionApprovalMode;
use agent_hub_approval_service::{AppState, router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use protocol::{ApprovalContext, ApprovalRequest, RequestType, SessionId};
use tower::ServiceExt;
use uuid::Uuid;

fn state(mode: ApprovalFeatureMode) -> AppState<NullNotifier> {
    AppState::new(
        ServerConfig {
            auth_mode: AuthMode::None,
            tokens: vec![],
            listen_addr: "127.0.0.1:0".into(),
            presence_ttl_secs: 120,
            session_ttl_secs: 7200,
            notification_delay_secs: 0,
            approval_mode: mode,
            base_url: Some("http://localhost:8080".into()),
            default_approval_mode: SessionApprovalMode::Remote,
        },
        NullNotifier,
        None,
    )
}

fn approval_request(id: &str) -> String {
    serde_json::to_string(&ApprovalRequest {
        id: id.into(),
        request_digest: None,
        session_id: SessionId::new("session"),
        session_display_name: "Service test".into(),
        cwd: "/workspace/project".into(),
        tool: serde_json::from_value(serde_json::json!("Bash")).unwrap(),
        tool_input: serde_json::json!({"command": "cargo test"}),
        provider: "opencode".into(),
        request_type: RequestType::ToolUse,
        context: ApprovalContext {
            workspace_roots: vec!["/workspace/project".into()],
            hook_event_name: "permission.ask".to_string().into(),
            extra: None,
            attended: None,
        },
    })
    .unwrap()
}

fn launch_request(_id: &str) -> String {
    let canonical: protocol::AttendedAuthorizationRequestV3 = serde_json::from_slice(
        include_bytes!("../../protocol/tests/fixtures/attended_authorization_v3.json"),
    )
    .unwrap();
    let canonical = protocol::AttendedAuthorizationRequestV4::from(canonical);
    let resolved = protocol::AgentHubRunResolvedV1::new(
        "018f0f21-76c0-7b37-a446-31f82a5e7602".into(),
        canonical.canonical_artifact().unwrap(),
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2099-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let event = resolved.gateway_event().unwrap();
    let request = ApprovalRequest {
        id: resolved.invocation_id.clone(),
        request_digest: Some(resolved.permission_scope_digest().unwrap()),
        session_id: event.session_id,
        session_display_name: event.session_title.unwrap(),
        cwd: event.cwd,
        tool: event.tool.into(),
        tool_input: event.tool_input,
        provider: "agent-hub-host-launch".into(),
        request_type: RequestType::ToolUse,
        context: ApprovalContext {
            workspace_roots: event.workspace_roots.unwrap(),
            hook_event_name: event.hook_event_name.into(),
            extra: None,
            attended: None,
        },
    };
    serde_json::to_string(&request).unwrap()
}

async fn request(app: &Router, method: &str, path: &str, body: String) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn api_is_idempotent_and_waiter_cancellation_is_per_waiter() {
    let app = router(state(ApprovalFeatureMode::Readwrite));
    let first_waiter = Uuid::new_v4();
    let second_waiter = Uuid::new_v4();
    let body = approval_request("idempotent");
    let create = |waiter: Uuid| {
        let app = app.clone();
        let body = body.clone();
        async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/hooks/approval")
                        .header("content-type", "application/json")
                        .header(protocol::APPROVAL_WAITER_HEADER, waiter.to_string())
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        }
    };
    let first = create(first_waiter).await;
    let second = create(second_waiter).await;
    assert_eq!(first["id"], second["id"]);
    let approval_id = first["id"].as_str().unwrap();

    let (_, first_cancel) = request(
        &app,
        "POST",
        &format!("/api/v1/approvals/{approval_id}/waiters/cancel"),
        serde_json::json!({"waiter_id": first_waiter}).to_string(),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&first_cancel).unwrap()["type"],
        "pending"
    );
    let (_, last_cancel) = request(
        &app,
        "POST",
        &format!("/api/v1/approvals/{approval_id}/waiters/cancel"),
        serde_json::json!({"waiter_id": second_waiter}).to_string(),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&last_cancel).unwrap()["type"],
        "cancelled"
    );
}

#[tokio::test]
async fn state_round_trip_preserves_legacy_shape_and_strict_retry_identity() {
    let original = state(ApprovalFeatureMode::Readwrite);
    let app = router(original.clone());
    let (_, created) = request(
        &app,
        "POST",
        "/api/v1/hooks/approval",
        approval_request("persisted"),
    )
    .await;
    let created: serde_json::Value = serde_json::from_str(&created).unwrap();
    let snapshot = original.snapshot().await;
    let encoded = serde_json::to_value(&snapshot).unwrap();
    assert!(encoded["pending_approvals"][0]["canonical_request"].is_string());

    let restored = state(ApprovalFeatureMode::Readwrite);
    restored.restore(snapshot).await.unwrap();
    let restored_app = router(restored);
    let (_, retry) = request(
        &restored_app,
        "POST",
        "/api/v1/hooks/approval",
        approval_request("persisted"),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&retry).unwrap()["id"],
        created["id"]
    );

    let mut legacy = encoded;
    legacy["pending_approvals"][0] = legacy["pending_approvals"][0]["approval"].take();
    let _: agent_hub_approval_service::storage::PersistedState =
        serde_json::from_value(legacy).unwrap();
}

#[tokio::test]
async fn ui_exposes_desktop_detail_and_mobile_queue() {
    let app = router(state(ApprovalFeatureMode::Readwrite));
    let (_, created) = request(
        &app,
        "POST",
        "/api/v1/hooks/approval",
        approval_request("ui"),
    )
    .await;
    let id = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (queue_status, queue) = request(&app, "GET", "/approvals/queue", String::new()).await;
    assert_eq!(queue_status, StatusCode::OK);
    assert!(queue.contains("id=\"queue-actions\""));
    assert!(queue.contains("/api/v1/approvals/pending"));
    let (detail_status, detail) =
        request(&app, "GET", &format!("/approvals/{id}"), String::new()).await;
    assert_eq!(detail_status, StatusCode::OK);
    assert!(detail.contains("cargo test"));
}

#[tokio::test]
async fn strict_one_step_review_accepts_canonical_payload_and_rejects_unknown_fields() {
    let app = router(state(ApprovalFeatureMode::Readwrite));
    let (status, created) = request(
        &app,
        "POST",
        "/api/v1/hooks/approval",
        launch_request("launch"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_, detail) = request(&app, "GET", &format!("/approvals/{id}"), String::new()).await;
    assert!(detail.contains("id=\"agent-hub-run-resolved\""));
    assert!(detail.contains("data-section=\"policy\""));

    let mut invalid: serde_json::Value = serde_json::from_str(&launch_request("invalid")).unwrap();
    invalid["tool_input"]["unexpected"] = true.into();
    let (status, _) = request(&app, "POST", "/api/v1/hooks/approval", invalid.to_string()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let mut legacy_digest: serde_json::Value =
        serde_json::from_str(&launch_request("legacy-digest")).unwrap();
    legacy_digest["request_digest"] = legacy_digest["tool_input"]["canonical_digest"].clone();
    let (status, _) = request(
        &app,
        "POST",
        "/api/v1/hooks/approval",
        legacy_digest.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    for (field, value) in [
        ("id", serde_json::json!(Uuid::new_v4())),
        ("session_id", serde_json::json!("wrong-session")),
        ("session_display_name", serde_json::json!("wrong title")),
        ("cwd", serde_json::json!("/wrong/cwd")),
        ("provider", serde_json::json!("opencode")),
        ("request_type", serde_json::json!("plan_question")),
    ] {
        let mut invalid: serde_json::Value =
            serde_json::from_str(&launch_request("invalid-envelope")).unwrap();
        invalid[field] = value;
        let (status, _) =
            request(&app, "POST", "/api/v1/hooks/approval", invalid.to_string()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "field {field}");
    }

    let mut invalid_context: serde_json::Value =
        serde_json::from_str(&launch_request("invalid-context")).unwrap();
    invalid_context["context"]["hook_event_name"] = "permission.ask".into();
    let (status, _) = request(
        &app,
        "POST",
        "/api/v1/hooks/approval",
        invalid_context.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let mut invalid_roots: serde_json::Value =
        serde_json::from_str(&launch_request("invalid-roots")).unwrap();
    invalid_roots["context"]["workspace_roots"] = serde_json::json!(["/wrong/root"]);
    let (status, _) = request(
        &app,
        "POST",
        "/api/v1/hooks/approval",
        invalid_roots.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let mut invalid_extra: serde_json::Value =
        serde_json::from_str(&launch_request("invalid-extra")).unwrap();
    invalid_extra["context"]["extra"] = serde_json::json!({"dippy_reason": "not canonical"});
    let (status, _) = request(
        &app,
        "POST",
        "/api/v1/hooks/approval",
        invalid_extra.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let mut generic: serde_json::Value = serde_json::from_str(&approval_request("opaque")).unwrap();
    generic["request_digest"] = "opaque-producer-value".into();
    let (status, body) = request(&app, "POST", "/api/v1/hooks/approval", generic.to_string()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
