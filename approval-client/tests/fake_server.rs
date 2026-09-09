use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_hub_approval_client::{ApprovalClient, ClientOptions, ErrorKind, validate_endpoint};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::any;
use protocol::{ApprovalDecision, ApprovalResolveRequest, Secret};
use tokio::net::TcpListener;

#[derive(Clone)]
struct ServerState {
    status: StatusCode,
    body: Arc<Vec<u8>>,
    authorization: Arc<Mutex<Option<String>>>,
    delay: Duration,
}

async fn response(State(state): State<ServerState>, headers: HeaderMap) -> Response<Body> {
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
) -> (String, Arc<Mutex<Option<String>>>) {
    let authorization = Arc::new(Mutex::new(None));
    let state = ServerState {
        status,
        body: Arc::new(body.into()),
        authorization: Arc::clone(&authorization),
        delay,
    };
    let app = Router::new().fallback(any(response)).with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), authorization)
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
        let (endpoint, authorization) = server(status, Vec::new(), Duration::ZERO).await;
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
        let (endpoint, _) = server(status, Vec::new(), Duration::ZERO).await;
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
    let (endpoint, _) = server(StatusCode::FOUND, Vec::new(), Duration::ZERO).await;
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
    let (endpoint, _) = server(StatusCode::OK, b"[]".to_vec(), Duration::from_secs(1)).await;
    let error = ApprovalClient::with_options(&endpoint, None, options)
        .unwrap()
        .list_pending_approvals()
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Timeout);

    let (endpoint, _) = server(StatusCode::OK, vec![b' '; 9], Duration::ZERO).await;
    let error = ApprovalClient::with_options(&endpoint, None, options)
        .unwrap()
        .list_pending_approvals()
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResponseTooLarge);
}
