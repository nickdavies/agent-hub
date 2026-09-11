use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_hub_approval_client::{ApprovalClient, ClientOptions, ErrorKind, validate_endpoint};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::any;
use protocol::{
    ApprovalDecision, ApprovalResolveRequest, QuestionInfo, QuestionOption, QuestionProxyRequest,
    Secret, SessionId,
};
use tokio::net::TcpListener;

#[derive(Clone)]
struct ServerState {
    status: StatusCode,
    body: Arc<Vec<u8>>,
    authorization: Arc<Mutex<Option<String>>>,
    requests: Arc<AtomicUsize>,
    delay: Duration,
}

async fn response(State(state): State<ServerState>, headers: HeaderMap) -> Response<Body> {
    state.requests.fetch_add(1, Ordering::Relaxed);
    *state.authorization.lock().unwrap() = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    tokio::time::sleep(state.delay).await;
    Response::builder()
        .status(state.status)
        .body(Body::from(state.body.as_ref().clone()))
        .unwrap()
}

async fn server(
    status: StatusCode,
    body: impl Into<Vec<u8>>,
    delay: Duration,
) -> (String, Arc<Mutex<Option<String>>>, Arc<AtomicUsize>) {
    let authorization = Arc::new(Mutex::new(None));
    let requests = Arc::new(AtomicUsize::new(0));
    let state = ServerState {
        status,
        body: Arc::new(body.into()),
        authorization: Arc::clone(&authorization),
        requests: Arc::clone(&requests),
        delay,
    };
    let app = Router::new().fallback(any(response)).with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), authorization, requests)
}

#[test]
fn endpoint_rules_require_secure_authenticated_remote_or_literal_loopback() {
    for endpoint in [
        "file:///server",
        "http://localhost:8080",
        "http://example.test",
        "https://example.test",
        "https://user@example.test",
        "https://example.test?query=yes",
        "https://example.test#fragment",
    ] {
        assert!(validate_endpoint(endpoint, false).is_err(), "{endpoint}");
    }
    validate_endpoint("http://127.0.0.1:8080/base", false).unwrap();
    validate_endpoint("http://[::1]:8080", false).unwrap();
    validate_endpoint("https://127.0.0.1:8080", false).unwrap();
    validate_endpoint("https://example.test/base", true).unwrap();
}

#[tokio::test]
async fn success_statuses_and_bearer_auth_are_compatible() {
    for status in [StatusCode::OK, StatusCode::ACCEPTED, StatusCode::NO_CONTENT] {
        let (endpoint, authorization, _) = server(status, Vec::new(), Duration::ZERO).await;
        let client = ApprovalClient::new(&endpoint, Some(Secret::new("secret"))).unwrap();
        assert_eq!(client.health().await.unwrap(), status.as_u16());
        assert_eq!(authorization.lock().unwrap().as_deref(), None);
        let status = client.session_status().await.unwrap();
        assert!((200..300).contains(&status));
        let status = client
            .resolve_approval_status(
                uuid::Uuid::nil(),
                &ApprovalResolveRequest {
                    decision: ApprovalDecision::Approve,
                    message: None,
                },
            )
            .await
            .unwrap();
        assert!((200..300).contains(&status));
        assert_eq!(
            authorization.lock().unwrap().as_deref(),
            Some("Bearer secret")
        );
    }
}

#[tokio::test]
async fn classifies_client_server_conflict_and_payload_failures() {
    for (status, kind) in [
        (StatusCode::BAD_REQUEST, ErrorKind::ClientResponse),
        (StatusCode::CONFLICT, ErrorKind::Conflict),
        (StatusCode::PAYLOAD_TOO_LARGE, ErrorKind::PayloadTooLarge),
        (StatusCode::INTERNAL_SERVER_ERROR, ErrorKind::ServerResponse),
    ] {
        let (endpoint, _, _) = server(status, Vec::new(), Duration::ZERO).await;
        let error = ApprovalClient::new(&endpoint, None)
            .unwrap()
            .health()
            .await
            .unwrap_err();
        assert_eq!(error.kind(), kind);
        assert_eq!(error.status(), Some(status.as_u16()));
    }
}

#[tokio::test]
async fn rejects_redirects_without_following_them() {
    let (endpoint, _, _) = server(StatusCode::FOUND, Vec::new(), Duration::ZERO).await;
    let error = ApprovalClient::new(&endpoint, None)
        .unwrap()
        .health()
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UnexpectedStatus);
    assert_eq!(error.status(), Some(302));
}

#[tokio::test]
async fn bounds_operation_time_and_response_bodies() {
    let options = ClientOptions {
        operation_timeout: Duration::from_millis(20),
        response_limit: 8,
    };
    let (endpoint, _, _) = server(StatusCode::OK, b"[]".to_vec(), Duration::from_secs(1)).await;
    let error = ApprovalClient::with_options(&endpoint, None, options)
        .unwrap()
        .list_pending_approvals()
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Timeout);

    let (endpoint, _, _) = server(StatusCode::OK, vec![b' '; 9], Duration::ZERO).await;
    let error = ApprovalClient::with_options(&endpoint, None, options)
        .unwrap()
        .list_pending_approvals()
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResponseTooLarge);
}

fn question_request() -> QuestionProxyRequest {
    QuestionProxyRequest {
        id: "request-1".into(),
        session_id: SessionId::new("session-1"),
        session_display_name: "Session".into(),
        cwd: "/workspace".into(),
        question_request_id: "question-1".into(),
        questions: vec![QuestionInfo::new("Continue?", "Decision", vec![], None, None).unwrap()],
        provider: "opencode".into(),
    }
}

#[tokio::test]
async fn question_registration_validates_before_sending() {
    let response = serde_json::json!({
        "id": uuid::Uuid::nil(),
        "type": "pending"
    });
    let (endpoint, _, requests) =
        server(StatusCode::OK, response.to_string(), Duration::ZERO).await;
    let client = ApprovalClient::new(&endpoint, None).unwrap();
    let valid = QuestionProxyRequest {
        id: "🦀".repeat(protocol::QUESTION_PROXY_ID_MAX_CHARS),
        session_id: SessionId::new("界".repeat(protocol::QUESTION_SESSION_ID_MAX_CHARS)),
        session_display_name: "é".repeat(protocol::QUESTION_SESSION_DISPLAY_NAME_MAX_CHARS),
        cwd: "🙂".repeat(protocol::QUESTION_CWD_MAX_CHARS),
        question_request_id: "🦀".repeat(protocol::QUESTION_REQUEST_ID_MAX_CHARS),
        questions: vec![
            QuestionInfo::new(
                "界".repeat(protocol::QUESTION_TEXT_MAX_CHARS),
                "é".repeat(protocol::QUESTION_HEADER_MAX_CHARS),
                vec![QuestionOption::new("Yes", "Continue").unwrap()],
                None,
                None,
            )
            .unwrap();
            protocol::QUESTION_MAX_COUNT
        ],
        provider: "界".repeat(protocol::QUESTION_PROVIDER_MAX_CHARS),
    };
    client.register_question(&valid).await.unwrap();
    assert_eq!(requests.load(Ordering::Relaxed), 1);

    for (field, max_chars) in [
        ("id", protocol::QUESTION_PROXY_ID_MAX_CHARS),
        ("session_id", protocol::QUESTION_SESSION_ID_MAX_CHARS),
        (
            "session_display_name",
            protocol::QUESTION_SESSION_DISPLAY_NAME_MAX_CHARS,
        ),
        ("cwd", protocol::QUESTION_CWD_MAX_CHARS),
        (
            "question_request_id",
            protocol::QUESTION_REQUEST_ID_MAX_CHARS,
        ),
        ("provider", protocol::QUESTION_PROVIDER_MAX_CHARS),
    ] {
        for value in [String::new(), "界".repeat(max_chars + 1)] {
            let mut invalid = question_request();
            match field {
                "id" => invalid.id = value,
                "session_id" => invalid.session_id = SessionId::new(value),
                "session_display_name" => invalid.session_display_name = value,
                "cwd" => invalid.cwd = value,
                "question_request_id" => invalid.question_request_id = value,
                "provider" => invalid.provider = value,
                _ => unreachable!(),
            }
            let error = client.register_question(&invalid).await.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ClientResponse, "{field}");
            assert_eq!(error.status(), None, "{field}");
        }
    }

    for questions in [
        vec![],
        vec![question_request().questions[0].clone(); protocol::QUESTION_MAX_COUNT + 1],
    ] {
        let mut invalid = question_request();
        invalid.questions = questions;
        let error = client.register_question(&invalid).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ClientResponse);
        assert_eq!(error.status(), None);
    }

    let mut invalid = question_request();
    invalid.questions[0].question.clear();
    let error = client.register_question(&invalid).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ClientResponse);
    assert_eq!(error.status(), None);
    assert_eq!(requests.load(Ordering::Relaxed), 1);
}
