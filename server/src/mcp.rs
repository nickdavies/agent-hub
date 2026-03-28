use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::tool;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::{ServerHandler, tool_handler, tool_router};
use serde::Serialize;

use crate::server::config::SharedNotifyConfig;
use crate::server::presence::{Presence, PresenceState};
use crate::server::sessions::{SessionConfigUpdate, SessionRegistry};

#[derive(Clone)]
pub struct NotifyMcp {
    sessions: Arc<SessionRegistry>,
    notify_config: SharedNotifyConfig,
    presence: Arc<Presence>,
    tool_router: ToolRouter<Self>,
}

pub fn service(
    sessions: Arc<SessionRegistry>,
    notify_config: SharedNotifyConfig,
    presence: Arc<Presence>,
) -> StreamableHttpService<NotifyMcp> {
    StreamableHttpService::new(
        move || {
            Ok(NotifyMcp::new(
                Arc::clone(&sessions),
                Arc::clone(&notify_config),
                Arc::clone(&presence),
            ))
        },
        LocalSessionManager::default().into(),
        Default::default(),
    )
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ConfigureSessionParams {
    #[schemars(description = "Project directory name (last component of cwd)")]
    project: Option<String>,
    #[schemars(description = "Exact session ID")]
    session_id: Option<String>,
    #[schemars(description = "Enable stop notifications for this session")]
    stop_enabled: Option<bool>,
    #[schemars(description = "Enable permission prompt notifications for this session")]
    permission_enabled: Option<bool>,
}

#[tool_router]
impl NotifyMcp {
    fn new(
        sessions: Arc<SessionRegistry>,
        notify_config: SharedNotifyConfig,
        presence: Arc<Presence>,
    ) -> Self {
        Self {
            sessions,
            notify_config,
            presence,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Configure notification preferences for a Claude session. Pass project (your cwd directory name) or session_id. Set stop_enabled and/or permission_enabled to toggle notification types."
    )]
    async fn configure_session(
        &self,
        Parameters(params): Parameters<ConfigureSessionParams>,
    ) -> String {
        let target_id =
            match resolve_session(&self.sessions, params.session_id, params.project).await {
                Ok(id) => id,
                Err(msg) => return msg,
            };

        let update = SessionConfigUpdate {
            stop_enabled: params.stop_enabled,
            permission_enabled: params.permission_enabled,
            approval_mode: None,
        };

        match self.sessions.update_config(&target_id, &update).await {
            Some(cfg) => serde_json::to_string_pretty(&cfg).unwrap_or_else(|e| e.to_string()),
            None => format!("Session '{target_id}' not found"),
        }
    }

    #[tool(description = "List all active Claude sessions with their notification configuration.")]
    async fn list_sessions(&self) -> String {
        let sessions = self.sessions.list().await;
        serde_json::to_string_pretty(&sessions).unwrap_or_else(|e| e.to_string())
    }

    #[tool(
        description = "Get current notification server status including presence state, global notification config, and active session count."
    )]
    async fn get_status(&self) -> String {
        let presence = self.presence.get().await;
        let config = self.notify_config.read().await.clone();
        let session_count = self.sessions.count().await;

        let status = StatusResponse {
            presence,
            stop_enabled: config.stop_enabled,
            permission_enabled: config.permission_enabled,
            active_sessions: session_count,
        };

        serde_json::to_string_pretty(&status).unwrap_or_else(|e| e.to_string())
    }
}

#[tool_handler]
impl ServerHandler for NotifyMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "agent-hub-server",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Notification server for Claude Code. Use configure_session to toggle \
                 notifications for your current session, list_sessions to see all active \
                 sessions, or get_status to check presence and global config.",
            )
    }
}

#[derive(Serialize)]
struct StatusResponse {
    presence: PresenceState,
    stop_enabled: bool,
    permission_enabled: bool,
    active_sessions: usize,
}

async fn resolve_session(
    sessions: &SessionRegistry,
    session_id: Option<String>,
    project: Option<String>,
) -> Result<String, String> {
    if let Some(id) = session_id {
        return Ok(id);
    }

    let project = match project {
        Some(p) => p,
        None => return Err("Either 'project' or 'session_id' is required".into()),
    };

    let matches = sessions.find_by_project(&project).await;
    match matches.len() {
        0 => Err(format!("No sessions found for project '{project}'")),
        1 => Ok(matches[0].session_id.clone()),
        n => {
            let ids: Vec<_> = matches.iter().map(|s| s.session_id.as_str()).collect();
            Err(format!(
                "Ambiguous: {n} sessions for project '{project}': {ids:?}. Pass session_id to disambiguate."
            ))
        }
    }
}
