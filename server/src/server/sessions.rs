use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tracing::info;

// Re-export protocol types so existing `use super::sessions::X` imports work.
pub use protocol::{
    EffectiveSessionStatus, Provider, SessionApprovalMode, SessionConfigUpdate,
    SessionNotifyConfig, SessionStatus, SessionView,
};

use super::storage::PersistedSession;

pub struct SessionInner {
    pub project: String,
    pub last_seen: Instant,
    pub config: SessionNotifyConfig,
    pub editor_type: Provider,
    pub status: SessionStatus,
    pub waiting_reason: Option<String>,
    pub display_name: Option<String>,
    pub ended_at: Option<Instant>,
}

/// Raw session data for internal use (before effective status resolution).
#[derive(Clone, serde::Serialize)]
pub struct RawSessionView {
    pub session_id: String,
    pub project: String,
    pub config: SessionNotifyConfig,
    pub editor_type: Provider,
    pub stored_status: SessionStatus,
    pub waiting_reason: Option<String>,
    pub display_name: Option<String>,
}

pub struct SessionRegistry {
    sessions: RwLock<HashMap<String, SessionInner>>,
    ttl: Duration,
    default_approval_mode: SessionApprovalMode,
}

impl SessionRegistry {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
            default_approval_mode: SessionApprovalMode::default(),
        }
    }

    pub fn with_default_approval_mode(mut self, mode: SessionApprovalMode) -> Self {
        self.default_approval_mode = mode;
        self
    }

    /// Upserts a session. Returns the project name extracted from cwd.
    /// `editor_type` is set on first registration and not overwritten on subsequent calls.
    pub async fn get_or_register(
        &self,
        session_id: &str,
        cwd: &str,
        editor_type: Option<Provider>,
    ) -> String {
        let project = extract_project_name(cwd);
        let mut sessions = self.sessions.write().await;
        let now = Instant::now();
        let default_mode = self.default_approval_mode;
        sessions
            .entry(session_id.to_owned())
            .and_modify(|s| s.last_seen = now)
            .or_insert_with(|| {
                info!(session_id, project = project, "session registered");
                SessionInner {
                    project: project.clone(),
                    last_seen: now,
                    config: SessionNotifyConfig::with_default_approval_mode(default_mode),
                    editor_type: editor_type.unwrap_or_default(),
                    status: SessionStatus::default(),
                    waiting_reason: None,
                    display_name: None,
                    ended_at: None,
                }
            });
        project
    }

    pub async fn get_config(&self, session_id: &str) -> Option<SessionNotifyConfig> {
        let sessions = self.sessions.read().await;
        sessions.get(session_id).map(|s| s.config.clone())
    }

    pub async fn update_config(
        &self,
        session_id: &str,
        update: &SessionConfigUpdate,
    ) -> Option<SessionNotifyConfig> {
        let mut sessions = self.sessions.write().await;
        sessions.get_mut(session_id).map(|s| {
            s.config.apply(update);
            s.config.clone()
        })
    }

    /// Update the stored status for a session. Returns true if the session exists.
    pub async fn set_status(
        &self,
        session_id: &str,
        status: SessionStatus,
        waiting_reason: Option<String>,
        display_name: Option<String>,
    ) -> bool {
        let mut sessions = self.sessions.write().await;
        if let Some(s) = sessions.get_mut(session_id) {
            s.status = status;
            s.last_seen = Instant::now();
            // Clear waiting_reason unless we're setting Waiting
            s.waiting_reason = if status == SessionStatus::Waiting {
                waiting_reason
            } else {
                None
            };
            if let Some(name) = display_name {
                s.display_name = Some(name);
            }
            if status == SessionStatus::Ended {
                s.ended_at = Some(Instant::now());
            }
            info!(session_id, status = ?status, "session status updated");
            true
        } else {
            false
        }
    }

    /// Update the display name for a session (e.g. from an approval request).
    pub async fn set_display_name(&self, session_id: &str, display_name: String) {
        let mut sessions = self.sessions.write().await;
        if let Some(s) = sessions.get_mut(session_id) {
            s.display_name = Some(display_name);
        }
    }

    /// List sessions as raw data (without effective status computation).
    /// The caller is responsible for resolving effective status.
    pub async fn list(&self) -> Vec<RawSessionView> {
        let sessions = self.sessions.read().await;
        sessions
            .iter()
            .map(|(id, s)| RawSessionView {
                session_id: id.clone(),
                project: s.project.clone(),
                config: s.config.clone(),
                editor_type: s.editor_type,
                stored_status: s.status,
                waiting_reason: s.waiting_reason.clone(),
                display_name: s.display_name.clone(),
            })
            .collect()
    }

    pub async fn find_by_project(&self, name: &str) -> Vec<RawSessionView> {
        let sessions = self.sessions.read().await;
        sessions
            .iter()
            .filter(|(_, s)| s.project == name)
            .map(|(id, s)| RawSessionView {
                session_id: id.clone(),
                project: s.project.clone(),
                config: s.config.clone(),
                editor_type: s.editor_type,
                stored_status: s.status,
                waiting_reason: s.waiting_reason.clone(),
                display_name: s.display_name.clone(),
            })
            .collect()
    }

    pub async fn count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Export all sessions for persistence.
    pub async fn snapshot(&self) -> HashMap<String, PersistedSession> {
        let sessions = self.sessions.read().await;
        sessions
            .iter()
            .map(|(id, s)| {
                (
                    id.clone(),
                    PersistedSession {
                        project: s.project.clone(),
                        config: s.config.clone(),
                        editor_type: s.editor_type,
                        status: s.status,
                        display_name: s.display_name.clone(),
                    },
                )
            })
            .collect()
    }

    /// Restore sessions from persisted state. Sets last_seen to now for fresh TTL.
    pub async fn restore(&self, sessions: HashMap<String, PersistedSession>) {
        let mut map = self.sessions.write().await;
        let now = Instant::now();
        for (id, persisted) in sessions {
            info!(
                session_id = id,
                project = persisted.project,
                "session restored"
            );
            let ended_at = if persisted.status == SessionStatus::Ended {
                Some(now) // Start the 30-min eviction window from restart
            } else {
                None
            };
            map.insert(
                id,
                SessionInner {
                    project: persisted.project,
                    last_seen: now,
                    config: persisted.config,
                    editor_type: persisted.editor_type,
                    status: persisted.status,
                    waiting_reason: None,
                    display_name: persisted.display_name,
                    ended_at,
                },
            );
        }
    }

    /// Evict sessions past their TTL.
    /// Active/idle/waiting sessions use the default TTL; ended sessions use `ended_ttl`.
    /// Returns the IDs of evicted sessions.
    pub async fn evict_stale(&self, ended_ttl: Duration) -> Vec<String> {
        let mut sessions = self.sessions.write().await;
        let mut evicted_ids = Vec::new();
        sessions.retain(|id, s| {
            let alive = match s.status {
                SessionStatus::Ended => s.ended_at.is_some_and(|t| t.elapsed() < ended_ttl),
                _ => s.last_seen.elapsed() < self.ttl,
            };
            if !alive {
                info!(session_id = id, status = ?s.status, "session evicted (stale)");
                evicted_ids.push(id.clone());
            }
            alive
        });
        if !evicted_ids.is_empty() {
            info!(
                evicted = evicted_ids.len(),
                remaining = sessions.len(),
                "session eviction complete"
            );
        }
        evicted_ids
    }
}

/// Extract project name from cwd (last path component).
fn extract_project_name(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_project_name() {
        assert_eq!(extract_project_name("/home/nick/workspaces/myapp"), "myapp");
        assert_eq!(extract_project_name("/"), "/");
        assert_eq!(extract_project_name("relative/path"), "path");
    }

    // ---------------------------------------------------------------
    // Serde deserialization tests — ensure every editor_type value
    // that providers actually send is accepted by the server.
    // ---------------------------------------------------------------

    #[test]
    fn deserialize_editor_type_all_variants() {
        // These are the values each provider plugin sends as editor_type.
        let cases = [
            ("\"claude\"", Provider::Claude),
            ("\"cursor\"", Provider::Cursor),
            ("\"opencode\"", Provider::Opencode),
            ("\"unknown\"", Provider::Unknown),
        ];
        for (json, expected) in cases {
            let got: Provider =
                serde_json::from_str(json).unwrap_or_else(|e| panic!("{json} => {e}"));
            assert_eq!(got, expected, "deserialized {json}");
        }
    }

    #[test]
    fn deserialize_editor_type_rejects_unknown_variant() {
        // A typo or new provider that hasn't been added should fail
        // at deserialization time rather than silently being accepted.
        let result = serde_json::from_str::<Provider>("\"vscode\"");
        assert!(result.is_err(), "unknown editor_type should fail");
    }

    #[test]
    fn deserialize_session_status_all_variants() {
        let cases = [
            ("\"active\"", SessionStatus::Active),
            ("\"idle\"", SessionStatus::Idle),
            ("\"waiting\"", SessionStatus::Waiting),
            ("\"ended\"", SessionStatus::Ended),
        ];
        for (json, expected) in cases {
            let got: SessionStatus =
                serde_json::from_str(json).unwrap_or_else(|e| panic!("{json} => {e}"));
            assert_eq!(got, expected, "deserialized {json}");
        }
    }

    #[test]
    fn serialize_effective_status_tagged() {
        // Verify the JSON shape the web UI expects (internally-tagged enum).
        let active = serde_json::to_value(EffectiveSessionStatus::Active).unwrap();
        assert_eq!(active["status"], "active");

        let idle = serde_json::to_value(EffectiveSessionStatus::Idle).unwrap();
        assert_eq!(idle["status"], "idle");

        let waiting = serde_json::to_value(EffectiveSessionStatus::Waiting {
            reason: Some("test".to_string()),
        })
        .unwrap();
        assert_eq!(waiting["status"], "waiting");
        assert_eq!(waiting["reason"], "test");

        let ended = serde_json::to_value(EffectiveSessionStatus::Ended).unwrap();
        assert_eq!(ended["status"], "ended");
    }

    #[tokio::test]
    async fn register_and_list() {
        let reg = SessionRegistry::new(7200);
        reg.get_or_register("s1", "/home/nick/project-a", None)
            .await;
        reg.get_or_register("s2", "/home/nick/project-b", None)
            .await;
        let sessions = reg.list().await;
        assert_eq!(sessions.len(), 2);
        // Default status should be Active
        assert!(
            sessions
                .iter()
                .all(|s| s.stored_status == SessionStatus::Active)
        );
    }

    #[tokio::test]
    async fn find_by_project() {
        let reg = SessionRegistry::new(7200);
        reg.get_or_register("s1", "/home/nick/myapp", None).await;
        reg.get_or_register("s2", "/home/nick/other", None).await;
        let found = reg.find_by_project("myapp").await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "s1");
    }

    #[tokio::test]
    async fn update_session_config() {
        let reg = SessionRegistry::new(7200);
        reg.get_or_register("s1", "/home/nick/proj", None).await;
        let cfg = reg
            .update_config(
                "s1",
                &SessionConfigUpdate {
                    stop_enabled: Some(false),
                    permission_enabled: None,
                    approval_mode: None,
                },
            )
            .await
            .unwrap();
        assert!(!cfg.stop_enabled);
        assert!(cfg.permission_enabled);
    }

    #[tokio::test]
    async fn set_status() {
        let reg = SessionRegistry::new(7200);
        reg.get_or_register("s1", "/home/nick/proj", None).await;

        // Set to Idle
        assert!(reg.set_status("s1", SessionStatus::Idle, None, None).await);
        let sessions = reg.list().await;
        assert_eq!(sessions[0].stored_status, SessionStatus::Idle);
        assert!(sessions[0].waiting_reason.is_none());

        // Set to Waiting with reason
        assert!(
            reg.set_status(
                "s1",
                SessionStatus::Waiting,
                Some("plan question".to_string()),
                None
            )
            .await
        );
        let sessions = reg.list().await;
        assert_eq!(sessions[0].stored_status, SessionStatus::Waiting);
        assert_eq!(sessions[0].waiting_reason.as_deref(), Some("plan question"));

        // Set to Active clears waiting_reason
        assert!(
            reg.set_status("s1", SessionStatus::Active, None, None)
                .await
        );
        let sessions = reg.list().await;
        assert_eq!(sessions[0].stored_status, SessionStatus::Active);
        assert!(sessions[0].waiting_reason.is_none());

        // Non-existent session returns false
        assert!(
            !reg.set_status("nope", SessionStatus::Idle, None, None)
                .await
        );
    }

    #[tokio::test]
    async fn set_status_ended_sets_ended_at() {
        let reg = SessionRegistry::new(7200);
        reg.get_or_register("s1", "/home/nick/proj", None).await;
        reg.set_status("s1", SessionStatus::Ended, None, None).await;
        let sessions = reg.list().await;
        assert_eq!(sessions[0].stored_status, SessionStatus::Ended);
    }

    #[tokio::test]
    async fn display_name() {
        let reg = SessionRegistry::new(7200);
        reg.get_or_register("s1", "/home/nick/proj", None).await;

        // Set display name via set_display_name
        reg.set_display_name("s1", "Fix auth bug".to_string()).await;
        let sessions = reg.list().await;
        assert_eq!(sessions[0].display_name.as_deref(), Some("Fix auth bug"));

        // Set display name via set_status
        reg.set_status(
            "s1",
            SessionStatus::Active,
            None,
            Some("New task name".to_string()),
        )
        .await;
        let sessions = reg.list().await;
        assert_eq!(sessions[0].display_name.as_deref(), Some("New task name"));
    }
}
