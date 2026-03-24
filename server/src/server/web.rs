use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use tower_sessions::Session;
use uuid::Uuid;

use super::approvals::{Approval, ApprovalStatus};
use super::config::ApprovalFeatureMode;
use super::notifier::Notifier;
use super::oauth;
use super::sessions::SessionView;
use super::AppState;

// --- Templates ---

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    providers: Vec<String>,
    has_basic_auth: bool,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    email: String,
    sessions: Vec<SessionView>,
    pending_approvals: Vec<Approval>,
    readwrite: bool,
}

#[derive(Template)]
#[template(path = "approval_detail.html")]
struct ApprovalDetailTemplate {
    email: String,
    approval: Approval,
    tool_input_pretty: String,
    readwrite: bool,
}

// --- Handlers ---

/// GET /auth/login
pub async fn login_page<N: Notifier>(State(state): State<AppState<N>>) -> Response {
    let (providers, has_basic_auth) = match &*state.oauth {
        Some(mgr) => (
            mgr.provider_names().into_iter().map(String::from).collect(),
            mgr.has_basic_auth(),
        ),
        None => (vec![], false),
    };
    into_html_response(LoginTemplate {
        providers,
        has_basic_auth,
    })
}

#[derive(Deserialize)]
pub struct BasicAuthForm {
    pub username: String,
    pub password: String,
}

/// POST /auth/login/basic — basic auth form submission.
pub async fn basic_auth_login<N: Notifier>(
    State(state): State<AppState<N>>,
    session: Session,
    Form(form): Form<BasicAuthForm>,
) -> Response {
    let oauth = match &*state.oauth {
        Some(mgr) => mgr,
        None => return (StatusCode::NOT_FOUND, "Auth not configured").into_response(),
    };

    if !oauth.check_basic_auth(&form.username, &form.password) {
        return Redirect::to("/auth/login?error=invalid").into_response();
    }

    // Use the username as the "email" for session identity
    if let Err(e) = oauth::set_session_email(&session, &form.username).await {
        tracing::error!("failed to store session: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Session error").into_response();
    }

    Redirect::to("/approvals").into_response()
}

/// GET /approvals — dashboard (auth enforced by middleware)
pub async fn dashboard<N: Notifier>(
    State(state): State<AppState<N>>,
    session: Session,
) -> Response {
    let email = session_email(&session).await;
    let sessions = state.sessions.list().await;
    let pending_approvals = state.approvals.list_pending().await;
    let readwrite = state.config.approval_mode == ApprovalFeatureMode::Readwrite;

    into_html_response(DashboardTemplate {
        email,
        sessions,
        pending_approvals,
        readwrite,
    })
}

/// GET /approvals/{id} (auth enforced by middleware)
pub async fn approval_detail<N: Notifier>(
    State(state): State<AppState<N>>,
    session: Session,
    Path(id): Path<Uuid>,
) -> Response {
    let email = session_email(&session).await;

    let approval = match state.approvals.get(id).await {
        Some(a) => a,
        None => return (StatusCode::NOT_FOUND, "Approval not found").into_response(),
    };

    let tool_input_pretty =
        serde_json::to_string_pretty(&approval.tool_input).unwrap_or_else(|_| "{}".to_string());
    let readwrite = state.config.approval_mode == ApprovalFeatureMode::Readwrite;

    into_html_response(ApprovalDetailTemplate {
        email,
        approval,
        tool_input_pretty,
        readwrite,
    })
}

#[derive(Deserialize)]
pub struct ResolveForm {
    pub decision: String,
    pub message: Option<String>,
}

/// POST /approvals/{id}/resolve — form submission (auth enforced by middleware)
pub async fn resolve_approval<N: Notifier>(
    State(state): State<AppState<N>>,
    Path(id): Path<Uuid>,
    Form(form): Form<ResolveForm>,
) -> Response {
    if state.config.approval_mode != ApprovalFeatureMode::Readwrite {
        return (StatusCode::FORBIDDEN, "Read-only mode").into_response();
    }

    let status = match form.decision.as_str() {
        "approve" => ApprovalStatus::Approved {
            message: form.message.filter(|m| !m.is_empty()),
        },
        "deny" => ApprovalStatus::Denied {
            reason: form.message.unwrap_or_default(),
        },
        _ => return (StatusCode::BAD_REQUEST, "Invalid decision").into_response(),
    };

    match state.approvals.resolve(id, status).await {
        Some(_) => Redirect::to("/approvals").into_response(),
        None => (StatusCode::NOT_FOUND, "Approval not found").into_response(),
    }
}

#[derive(Deserialize)]
pub struct ToggleModeForm {
    pub mode: String,
}

/// POST /approvals/toggle-mode/{session_id} (auth enforced by middleware)
pub async fn toggle_approval_mode<N: Notifier>(
    State(state): State<AppState<N>>,
    Path(session_id): Path<String>,
    Form(form): Form<ToggleModeForm>,
) -> Response {
    if state.config.approval_mode != ApprovalFeatureMode::Readwrite {
        return (StatusCode::FORBIDDEN, "Read-only mode").into_response();
    }

    use super::sessions::{SessionApprovalMode, SessionConfigUpdate};
    let mode = match form.mode.as_str() {
        "remote" => SessionApprovalMode::Remote,
        "terminal" => SessionApprovalMode::Terminal,
        _ => return (StatusCode::BAD_REQUEST, "Invalid mode").into_response(),
    };

    let update = SessionConfigUpdate {
        stop_enabled: None,
        permission_enabled: None,
        approval_mode: Some(mode),
    };

    state.sessions.update_config(&session_id, &update).await;
    Redirect::to("/approvals").into_response()
}

/// Extract email from session. Middleware guarantees this exists on authed routes.
async fn session_email(session: &Session) -> String {
    oauth::get_session_email(session)
        .await
        .unwrap_or_default()
}

fn into_html_response<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("template render error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}
