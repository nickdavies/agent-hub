use std::fmt;
use std::time::Duration;

use protocol::{
    Approval, ApprovalRequest, ApprovalResolveRequest, ApprovalResponse, ApprovalWaitResponse,
    ApprovalWaiterCancelRequest, ConfigResponse, NotifyConfig, NotifyConfigUpdate, PendingQuestion,
    QuestionProxyRequest, QuestionProxyResponse, QuestionResolveRequest, QuestionWaitResponse,
    Secret, SessionConfigUpdate, SessionId, SessionNotifyConfig, SessionView, StatusReport,
};
use reqwest::{RequestBuilder, Response, Url, redirect::Policy};
use serde::Serialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

pub const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_RESPONSE_LIMIT: usize = protocol::ATTENDED_GATEWAY_MAX_FRAME_BYTES;

#[derive(Clone, Copy, Debug)]
pub struct ClientOptions {
    pub operation_timeout: Duration,
    pub response_limit: usize,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
            response_limit: DEFAULT_RESPONSE_LIMIT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidEndpoint,
    Timeout,
    Transport,
    ClientResponse,
    Conflict,
    PayloadTooLarge,
    ServerResponse,
    UnexpectedStatus,
    ResponseTooLarge,
    InvalidResponse,
}

#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    operation: &'static str,
    status: Option<u16>,
    detail: String,
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn status(&self) -> Option<u16> {
        self.status
    }

    pub fn operation(&self) -> &'static str {
        self.operation
    }

    pub fn is_client_response(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::ClientResponse | ErrorKind::Conflict | ErrorKind::PayloadTooLarge
        )
    }

    pub fn is_server_response(&self) -> bool {
        self.kind == ErrorKind::ServerResponse
    }

    fn endpoint(detail: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::InvalidEndpoint,
            operation: "endpoint validation",
            status: None,
            detail: detail.into(),
        }
    }

    fn response(operation: &'static str, status: u16) -> Self {
        let kind = match status {
            409 => ErrorKind::Conflict,
            413 => ErrorKind::PayloadTooLarge,
            400..=499 => ErrorKind::ClientResponse,
            500..=599 => ErrorKind::ServerResponse,
            _ => ErrorKind::UnexpectedStatus,
        };
        let display = reqwest::StatusCode::from_u16(status)
            .map(|status| status.to_string())
            .unwrap_or_else(|_| status.to_string());
        Self {
            kind,
            operation,
            status: Some(status),
            detail: format!("server returned {display}"),
        }
    }

    fn request(operation: &'static str, error: reqwest::Error) -> Self {
        Self {
            kind: if error.is_timeout() {
                ErrorKind::Timeout
            } else {
                ErrorKind::Transport
            },
            operation,
            status: error.status().map(|status| status.as_u16()),
            detail: error.to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.operation, self.detail)
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Debug)]
pub struct ApiResponse<T> {
    pub status: u16,
    pub value: T,
}

#[derive(Clone)]
pub struct ApprovalClient {
    http: reqwest::Client,
    endpoint: Url,
    token: Option<Secret>,
    options: ClientOptions,
}

impl ApprovalClient {
    pub fn new(endpoint: &str, token: Option<Secret>) -> Result<Self, Error> {
        Self::with_options(endpoint, token, ClientOptions::default())
    }

    pub fn with_options(
        endpoint: &str,
        token: Option<Secret>,
        options: ClientOptions,
    ) -> Result<Self, Error> {
        if options.operation_timeout.is_zero() {
            return Err(Error::endpoint("HTTP operation timeout must be positive"));
        }
        if options.response_limit == 0 {
            return Err(Error::endpoint("HTTP response limit must be positive"));
        }
        let endpoint = validate_endpoint(endpoint, token.is_some())?;
        let http = reqwest::Client::builder()
            .redirect(Policy::none())
            .connect_timeout(options.operation_timeout)
            .timeout(options.operation_timeout)
            .build()
            .map_err(|error| Error::request("HTTP client construction", error))?;
        Ok(Self {
            http,
            endpoint,
            token,
            options,
        })
    }

    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    pub async fn register_approval(
        &self,
        request: &ApprovalRequest,
        waiter_id: Option<Uuid>,
        timeout: Option<Duration>,
    ) -> Result<ApiResponse<ApprovalResponse>, Error> {
        let mut builder = self.authenticated(self.http.post(self.url("api/v1/hooks/approval")?));
        if let Some(waiter_id) = waiter_id {
            builder = builder.header(protocol::APPROVAL_WAITER_HEADER, waiter_id.to_string());
        }
        self.send_json(builder.json(request), timeout, "approval registration")
            .await
    }

    pub async fn wait_approval(
        &self,
        id: Uuid,
        waiter_id: Option<Uuid>,
        timeout: Duration,
    ) -> Result<ApiResponse<ApprovalWaitResponse>, Error> {
        let mut url = self.url(&format!("api/v1/approvals/{id}/wait"))?;
        if let Some(waiter_id) = waiter_id {
            url.query_pairs_mut()
                .append_pair("waiter_id", &waiter_id.to_string());
        }
        self.send_json(
            self.authenticated(self.http.get(url)),
            Some(timeout),
            "approval wait",
        )
        .await
    }

    pub async fn cancel_approval_waiter(
        &self,
        approval_id: Uuid,
        waiter_id: Uuid,
    ) -> Result<ApiResponse<ApprovalResponse>, Error> {
        let request = ApprovalWaiterCancelRequest {
            waiter_id: waiter_id.to_string(),
        };
        self.send_json(
            self.authenticated(
                self.http
                    .post(self.url(&format!("api/v1/approvals/{approval_id}/waiters/cancel"))?),
            )
            .json(&request),
            None,
            "approval waiter cancellation",
        )
        .await
    }

    pub async fn get_approval(&self, id: Uuid) -> Result<ApiResponse<Approval>, Error> {
        self.get_json(&format!("api/v1/approvals/{id}"), "approval lookup")
            .await
    }

    pub async fn list_pending_approvals(&self) -> Result<ApiResponse<Vec<Approval>>, Error> {
        self.get_json("api/v1/approvals/pending", "pending approvals")
            .await
    }

    pub async fn resolve_approval(
        &self,
        id: Uuid,
        request: &ApprovalResolveRequest,
    ) -> Result<ApiResponse<Approval>, Error> {
        self.resolve_approval_with_timeout(id, request, None).await
    }

    pub async fn resolve_approval_with_timeout(
        &self,
        id: Uuid,
        request: &ApprovalResolveRequest,
        timeout: Option<Duration>,
    ) -> Result<ApiResponse<Approval>, Error> {
        self.send_json(
            self.authenticated(
                self.http
                    .post(self.url(&format!("api/v1/approvals/{id}/resolve"))?),
            )
            .json(request),
            timeout,
            "approval resolution",
        )
        .await
    }

    pub async fn resolve_approval_status(
        &self,
        id: Uuid,
        request: &ApprovalResolveRequest,
    ) -> Result<u16, Error> {
        self.send_status(
            self.authenticated(
                self.http
                    .post(self.url(&format!("api/v1/approvals/{id}/resolve"))?),
            )
            .json(request),
            None,
            "approval resolution",
            true,
        )
        .await
    }

    pub async fn resolve_question_status(
        &self,
        id: Uuid,
        request: &QuestionResolveRequest,
    ) -> Result<u16, Error> {
        self.send_status(
            self.authenticated(
                self.http
                    .post(self.url(&format!("api/v1/questions/{id}/resolve"))?),
            )
            .json(request),
            None,
            "question resolution",
            true,
        )
        .await
    }

    pub async fn resolve_question_with_timeout(
        &self,
        id: Uuid,
        request: &QuestionResolveRequest,
        timeout: Option<Duration>,
    ) -> Result<ApiResponse<PendingQuestion>, Error> {
        self.send_json(
            self.authenticated(
                self.http
                    .post(self.url(&format!("api/v1/questions/{id}/resolve"))?),
            )
            .json(request),
            timeout,
            "question resolution",
        )
        .await
    }

    pub async fn register_question(
        &self,
        request: &QuestionProxyRequest,
    ) -> Result<ApiResponse<QuestionProxyResponse>, Error> {
        self.post_json("api/v1/hooks/question", request, "question registration")
            .await
    }

    pub async fn wait_question(
        &self,
        id: Uuid,
        timeout: Duration,
    ) -> Result<ApiResponse<QuestionWaitResponse>, Error> {
        self.send_json(
            self.authenticated(
                self.http
                    .get(self.url(&format!("api/v1/questions/{id}/wait"))?),
            ),
            Some(timeout),
            "question wait",
        )
        .await
    }

    pub async fn get_question(&self, id: Uuid) -> Result<ApiResponse<PendingQuestion>, Error> {
        self.get_json(&format!("api/v1/questions/{id}"), "question lookup")
            .await
    }

    pub async fn list_pending_questions(&self) -> Result<ApiResponse<Vec<PendingQuestion>>, Error> {
        self.get_json("api/v1/questions/pending", "pending questions")
            .await
    }

    pub async fn resolve_question(
        &self,
        id: Uuid,
        request: &QuestionResolveRequest,
    ) -> Result<ApiResponse<PendingQuestion>, Error> {
        self.resolve_question_with_timeout(id, request, None).await
    }

    pub async fn list_sessions(&self) -> Result<ApiResponse<Vec<SessionView>>, Error> {
        self.get_json("api/v1/sessions", "session list").await
    }

    pub async fn session_status(&self) -> Result<u16, Error> {
        self.send_status(
            self.authenticated(self.http.get(self.url("api/v1/sessions")?)),
            None,
            "session status",
            false,
        )
        .await
    }

    pub async fn update_session(
        &self,
        id: &SessionId,
        update: &SessionConfigUpdate,
    ) -> Result<ApiResponse<SessionNotifyConfig>, Error> {
        self.put_json(&format!("api/v1/sessions/{id}"), update, "session update")
            .await
    }

    pub async fn get_config(&self) -> Result<ApiResponse<ConfigResponse>, Error> {
        self.get_json("api/v1/config", "config lookup").await
    }

    pub async fn update_config(
        &self,
        update: &NotifyConfigUpdate,
    ) -> Result<ApiResponse<NotifyConfig>, Error> {
        self.put_json("api/v1/config", update, "config update")
            .await
    }

    pub async fn report_status(&self, report: &StatusReport) -> Result<u16, Error> {
        self.send_status(
            self.authenticated(self.http.post(self.url("api/v1/hooks/status")?))
                .json(report),
            None,
            "status report",
            true,
        )
        .await
    }

    pub async fn health(&self) -> Result<u16, Error> {
        self.send_status(
            self.http.get(self.url("health")?),
            None,
            "health check",
            true,
        )
        .await
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        operation: &'static str,
    ) -> Result<ApiResponse<T>, Error> {
        self.send_json(
            self.authenticated(self.http.get(self.url(path)?)),
            None,
            operation,
        )
        .await
    }

    async fn post_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        operation: &'static str,
    ) -> Result<ApiResponse<T>, Error> {
        self.send_json(
            self.authenticated(self.http.post(self.url(path)?))
                .json(body),
            None,
            operation,
        )
        .await
    }

    async fn put_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        operation: &'static str,
    ) -> Result<ApiResponse<T>, Error> {
        self.send_json(
            self.authenticated(self.http.put(self.url(path)?))
                .json(body),
            None,
            operation,
        )
        .await
    }

    async fn send_json<T: DeserializeOwned>(
        &self,
        request: RequestBuilder,
        timeout: Option<Duration>,
        operation: &'static str,
    ) -> Result<ApiResponse<T>, Error> {
        let response = self.send(request, timeout, operation).await?;
        let status = response.status().as_u16();
        let bytes = self.read_body(response, timeout, operation).await?;
        let value = serde_json::from_slice(&bytes).map_err(|error| Error {
            kind: ErrorKind::InvalidResponse,
            operation,
            status: Some(status),
            detail: error.to_string(),
        })?;
        Ok(ApiResponse { status, value })
    }

    async fn send_status(
        &self,
        request: RequestBuilder,
        timeout: Option<Duration>,
        operation: &'static str,
        require_success: bool,
    ) -> Result<u16, Error> {
        let response = request
            .timeout(timeout.unwrap_or(self.options.operation_timeout))
            .send()
            .await
            .map_err(|error| Error::request(operation, error))?;
        let status = response.status().as_u16();
        if require_success && !response.status().is_success() {
            return Err(Error::response(operation, status));
        }
        Ok(status)
    }

    async fn send(
        &self,
        request: RequestBuilder,
        timeout: Option<Duration>,
        operation: &'static str,
    ) -> Result<Response, Error> {
        let response = request
            .timeout(timeout.unwrap_or(self.options.operation_timeout))
            .send()
            .await
            .map_err(|error| Error::request(operation, error))?;
        if !response.status().is_success() {
            return Err(Error::response(operation, response.status().as_u16()));
        }
        Ok(response)
    }

    async fn read_body(
        &self,
        mut response: Response,
        timeout: Option<Duration>,
        operation: &'static str,
    ) -> Result<Vec<u8>, Error> {
        let read = async {
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| Error::request(operation, error))?
            {
                if body.len().saturating_add(chunk.len()) > self.options.response_limit {
                    return Err(Error {
                        kind: ErrorKind::ResponseTooLarge,
                        operation,
                        status: Some(response.status().as_u16()),
                        detail: "response exceeds limit".into(),
                    });
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        };
        tokio::time::timeout(timeout.unwrap_or(self.options.operation_timeout), read)
            .await
            .map_err(|_| Error {
                kind: ErrorKind::Timeout,
                operation,
                status: None,
                detail: "response body timed out".into(),
            })?
    }

    fn authenticated(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.token {
            Some(token) => request.bearer_auth(token.expose()),
            None => request,
        }
    }

    fn url(&self, path: &str) -> Result<Url, Error> {
        self.endpoint
            .join(path)
            .map_err(|error| Error::endpoint(format!("cannot build server URL: {error}")))
    }
}

pub fn validate_endpoint(value: &str, has_token: bool) -> Result<Url, Error> {
    let mut url = Url::parse(value)
        .map_err(|error| Error::endpoint(format!("server URL is invalid: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::endpoint("server URL must use HTTP or HTTPS"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::endpoint("server URL must not contain credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::endpoint(
            "server URL must not contain query or fragment",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| Error::endpoint("server URL has no host"))?
        .trim_matches(['[', ']']);
    let loopback = host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback());
    if url.scheme() == "http" && !loopback {
        return Err(Error::endpoint(
            "HTTP is permitted only for a literal loopback address",
        ));
    }
    if !loopback && !has_token {
        return Err(Error::endpoint(
            "non-loopback approval endpoints require HTTPS plus a bearer token",
        ));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}
