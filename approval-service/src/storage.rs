use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use protocol::{
    NotifyConfig, PresenceState, Provider, SessionId, SessionNotifyConfig, SessionStatus,
};

use super::approvals::ApprovalSnapshot;
use super::questions::QuestionSnapshot;

/// Serializable snapshot of server state that survives restarts.
#[derive(Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub sessions: HashMap<SessionId, PersistedSession>,
    pub notify_config: Option<NotifyConfig>,
    pub presence: Option<PresenceState>,
    #[serde(default)]
    pub pending_approvals: Vec<ApprovalSnapshot>,
    #[serde(default)]
    pub pending_questions: Vec<QuestionSnapshot>,
}

/// A single session's persisted data (excludes last_seen which is runtime-only).
#[derive(Serialize, Deserialize)]
pub struct PersistedSession {
    pub project: String,
    pub config: SessionNotifyConfig,
    #[serde(default)]
    pub editor_type: Provider,
    #[serde(default)]
    pub status: SessionStatus,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Encapsulates all storage operations for server state persistence.
pub trait Storage: Send + Sync {
    fn load(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<PersistedState>>> + Send + '_>>;
    fn save<'a>(
        &'a self,
        state: &'a PersistedState,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
}

/// No-op storage backend. State is not persisted.
pub struct NullStorage;

impl Storage for NullStorage {
    fn load(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<PersistedState>>> + Send + '_>> {
        Box::pin(async { Ok(None) })
    }

    fn save<'a>(
        &'a self,
        _state: &'a PersistedState,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
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
    fn load(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<PersistedState>>> + Send + '_>> {
        Box::pin(async move {
            match tokio::fs::read(&self.path).await {
                Ok(data) => {
                    let state: PersistedState = serde_json::from_slice(&data)?;
                    Ok(Some(state))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }

    fn save<'a>(
        &'a self,
        state: &'a PersistedState,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            use tokio::io::AsyncWriteExt;

            let parent = self
                .path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            tokio::fs::create_dir_all(parent).await?;
            let data = serde_json::to_vec_pretty(state)?;
            let tmp = self.path.with_extension("tmp");
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)
                .await?;
            file.write_all(&data).await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&tmp, &self.path).await?;
            let parent = parent.to_path_buf();
            tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all()).await??;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_without_questions_remains_readable() {
        let state: PersistedState = serde_json::from_str(
            r#"{"sessions":{},"notify_config":null,"presence":null,"pending_approvals":[]}"#,
        )
        .unwrap();
        assert!(state.pending_questions.is_empty());
    }
}
