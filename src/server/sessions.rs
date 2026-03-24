use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::info;

use super::storage::PersistedSession;

pub struct SessionInner {
    pub project: String,
    pub last_seen: Instant,
    pub config: SessionNotifyConfig,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SessionNotifyConfig {
    pub stop_enabled: bool,
    pub permission_enabled: bool,
}

impl Default for SessionNotifyConfig {
    fn default() -> Self {
        Self {
            stop_enabled: true,
            permission_enabled: true,
        }
    }
}

/// Partial update for per-session config.
#[derive(Deserialize)]
pub struct SessionConfigUpdate {
    pub stop_enabled: Option<bool>,
    pub permission_enabled: Option<bool>,
}

impl SessionNotifyConfig {
    pub fn apply(&mut self, update: &SessionConfigUpdate) {
        if let Some(v) = update.stop_enabled {
            self.stop_enabled = v;
        }
        if let Some(v) = update.permission_enabled {
            self.permission_enabled = v;
        }
    }
}

/// API response for a session.
#[derive(Serialize)]
pub struct SessionView {
    pub session_id: String,
    pub project: String,
    pub config: SessionNotifyConfig,
}

pub struct SessionRegistry {
    sessions: RwLock<HashMap<String, SessionInner>>,
    ttl: Duration,
}

impl SessionRegistry {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Upserts a session. Returns the project name extracted from cwd.
    pub async fn get_or_register(&self, session_id: &str, cwd: &str) -> String {
        let project = extract_project_name(cwd);
        let mut sessions = self.sessions.write().await;
        let now = Instant::now();
        sessions
            .entry(session_id.to_owned())
            .and_modify(|s| s.last_seen = now)
            .or_insert_with(|| {
                info!(session_id, project = project, "session registered");
                SessionInner {
                    project: project.clone(),
                    last_seen: now,
                    config: SessionNotifyConfig::default(),
                }
            });
        project
    }

    pub async fn deregister(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        if sessions.remove(session_id).is_some() {
            info!(session_id, "session deregistered");
        }
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

    pub async fn list(&self) -> Vec<SessionView> {
        let sessions = self.sessions.read().await;
        sessions
            .iter()
            .map(|(id, s)| SessionView {
                session_id: id.clone(),
                project: s.project.clone(),
                config: s.config.clone(),
            })
            .collect()
    }

    pub async fn find_by_project(&self, name: &str) -> Vec<SessionView> {
        let sessions = self.sessions.read().await;
        sessions
            .iter()
            .filter(|(_, s)| s.project == name)
            .map(|(id, s)| SessionView {
                session_id: id.clone(),
                project: s.project.clone(),
                config: s.config.clone(),
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
            map.insert(
                id,
                SessionInner {
                    project: persisted.project,
                    last_seen: now,
                    config: persisted.config,
                },
            );
        }
    }

    /// Evict sessions that haven't been seen within the TTL.
    pub async fn evict_stale(&self) {
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|id, s| {
            let alive = s.last_seen.elapsed() < self.ttl;
            if !alive {
                info!(session_id = id, "session evicted (stale)");
            }
            alive
        });
        let evicted = before - sessions.len();
        if evicted > 0 {
            info!(
                evicted,
                remaining = sessions.len(),
                "session eviction complete"
            );
        }
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

    #[tokio::test]
    async fn register_and_list() {
        let reg = SessionRegistry::new(7200);
        reg.get_or_register("s1", "/home/nick/project-a").await;
        reg.get_or_register("s2", "/home/nick/project-b").await;
        let sessions = reg.list().await;
        assert_eq!(sessions.len(), 2);
    }

    #[tokio::test]
    async fn deregister() {
        let reg = SessionRegistry::new(7200);
        reg.get_or_register("s1", "/home/nick/project-a").await;
        reg.deregister("s1").await;
        assert_eq!(reg.count().await, 0);
    }

    #[tokio::test]
    async fn find_by_project() {
        let reg = SessionRegistry::new(7200);
        reg.get_or_register("s1", "/home/nick/myapp").await;
        reg.get_or_register("s2", "/home/nick/other").await;
        let found = reg.find_by_project("myapp").await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "s1");
    }

    #[tokio::test]
    async fn update_session_config() {
        let reg = SessionRegistry::new(7200);
        reg.get_or_register("s1", "/home/nick/proj").await;
        let cfg = reg
            .update_config(
                "s1",
                &SessionConfigUpdate {
                    stop_enabled: Some(false),
                    permission_enabled: None,
                },
            )
            .await
            .unwrap();
        assert!(!cfg.stop_enabled);
        assert!(cfg.permission_enabled);
    }
}
