use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::{Deserialize, Serialize};
use tower_sessions::Session;
use uuid::Uuid;

use protocol::{Approval, SessionView};

use super::AppState;
use super::config::ApprovalFeatureMode;
use super::notifier::Notifier;
use super::oauth;

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
    has_auth: bool,
}

#[derive(Template)]
#[template(path = "approval_detail.html")]
struct ApprovalDetailTemplate {
    email: String,
    approval: Approval,
    tool_input_pretty: String,
    approval_json: String,
    readwrite: bool,
    has_auth: bool,
}

#[derive(Template)]
#[template(path = "approval_queue.html")]
struct ApprovalQueueTemplate {
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
    let sessions = super::build_session_views(&state).await;
    let pending_approvals = state.approvals.list_pending().await;
    let readwrite = state.config.approval_mode == ApprovalFeatureMode::Readwrite;
    let has_auth = state.config.auth_mode != crate::server::config::AuthMode::None;

    into_html_response(DashboardTemplate {
        email,
        sessions,
        pending_approvals,
        readwrite,
        has_auth,
    })
}

/// GET /approvals/queue — mobile approval queue (auth enforced by middleware)
pub async fn approval_queue<N: Notifier>(State(state): State<AppState<N>>) -> Response {
    into_html_response(ApprovalQueueTemplate {
        readwrite: state.config.approval_mode == ApprovalFeatureMode::Readwrite,
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
    let approval_json = match json_for_script(&approval) {
        Ok(json) => json,
        Err(error) => {
            tracing::error!(%error, "failed to serialize approval detail data");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Serialization error").into_response();
        }
    };
    let readwrite = state.config.approval_mode == ApprovalFeatureMode::Readwrite;
    let has_auth = state.config.auth_mode != crate::server::config::AuthMode::None;

    into_html_response(ApprovalDetailTemplate {
        email,
        approval,
        tool_input_pretty,
        approval_json,
        readwrite,
        has_auth,
    })
}

fn json_for_script<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(value).map(|json| {
        json.replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029")
    })
}

/// Extract email from session. Middleware guarantees this exists on authed routes.
async fn session_email(session: &Session) -> String {
    oauth::get_session_email(session).await.unwrap_or_default()
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
