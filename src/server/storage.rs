use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::config::NotifyConfig;
use super::presence::PresenceState;
use super::sessions::SessionNotifyConfig;

/// Serializable snapshot of server state that survives restarts.
#[derive(Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub sessions: HashMap<String, PersistedSession>,
    pub notify_config: Option<NotifyConfig>,
    pub presence: Option<PresenceState>,
}

/// A single session's persisted data (excludes last_seen which is runtime-only).
#[derive(Serialize, Deserialize)]
pub struct PersistedSession {
    pub project: String,
    pub config: SessionNotifyConfig,
}

/// Encapsulates all storage operations for server state persistence.
pub trait Storage: Send + Sync {
    fn load(&self) -> impl Future<Output = anyhow::Result<Option<PersistedState>>> + Send;
    fn save(&self, state: &PersistedState) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// No-op storage backend. State is not persisted.
pub struct NullStorage;

impl Storage for NullStorage {
    async fn load(&self) -> anyhow::Result<Option<PersistedState>> {
        Ok(None)
    }

    async fn save(&self, _state: &PersistedState) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Persists state to a local JSON file using atomic write (temp file + rename).
pub struct LocalFileStorage {
    path: PathBuf,
}

impl LocalFileStorage {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Storage for LocalFileStorage {
    async fn load(&self) -> anyhow::Result<Option<PersistedState>> {
        match tokio::fs::read(&self.path).await {
            Ok(data) => {
                let state: PersistedState = serde_json::from_slice(&data)?;
                Ok(Some(state))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn save(&self, state: &PersistedState) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let data = serde_json::to_vec_pretty(state)?;
        let tmp = self.path.with_extension("tmp");
        tokio::fs::write(&tmp, &data).await?;
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }
}
