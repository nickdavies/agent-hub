use axum::extract::{Path, Query, State};
use axum::response::Response;
use tower_sessions::Session;

use super::{AppState, DEFAULT_WEB_HOME};

pub use agent_hub_web_auth::{OAuthManager, get_session_email, logout, set_session_email};

pub async fn start_auth(
    State(state): State<AppState>,
    session: Session,
    Path(provider_name): Path<String>,
) -> Response {
    agent_hub_web_auth::start_auth(state.oauth.as_ref().as_ref(), session, &provider_name).await
}

pub async fn callback(
    State(state): State<AppState>,
    session: Session,
    Path(provider_name): Path<String>,
    Query(params): Query<agent_hub_web_auth::CallbackParams>,
) -> Response {
    agent_hub_web_auth::callback(
        state.oauth.as_ref().as_ref(),
        session,
        &provider_name,
        params,
        DEFAULT_WEB_HOME,
    )
    .await
}
