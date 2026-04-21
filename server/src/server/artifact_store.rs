use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use async_trait::async_trait;
use tokio::fs;

use crate::error::AppError;

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Store artifact bytes at a relative key path
    #[allow(dead_code)]
    async fn store(&self, key: &str, data: &[u8]) -> anyhow::Result<()>;

    /// Retrieve artifact bytes by relative key path
    async fn retrieve(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>>;

    /// List artifact keys under a prefix
    #[allow(dead_code)]
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>>;

    /// Check if an artifact exists
    #[allow(dead_code)]
    async fn exists(&self, key: &str) -> anyhow::Result<bool>;
}

/// Filesystem-based artifact store (shared directory between server and worker)
pub struct DirectoryStore {
    root: PathBuf,
}

impl DirectoryStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn resolve_key_path(&self, key: &str) -> anyhow::Result<PathBuf> {
        Ok(self.root.join(normalize_store_key_path(key)?))
    }
}

#[async_trait]
impl ArtifactStore for DirectoryStore {
    async fn store(&self, key: &str, data: &[u8]) -> anyhow::Result<()> {
        let path = self.resolve_key_path(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating artifact parent dir: {}", parent.display()))?;
        }
        fs::write(&path, data)
            .await
            .with_context(|| format!("writing artifact: {}", path.display()))
    }

    async fn retrieve(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let path = self.resolve_key_path(key)?;
        match fs::read(&path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("reading artifact: {}", path.display()))
            }
        }
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let base = self.resolve_key_path(prefix)?;
        let metadata = match fs::metadata(&base).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading artifact metadata: {}", base.display()));
            }
        };

        if metadata.is_file() {
            let key = path_to_store_key(self.root.as_path(), &base)?;
            return Ok(vec![key]);
        }

        let mut out = Vec::new();
        let mut queue = vec![base];
        while let Some(dir) = queue.pop() {
            let mut entries = fs::read_dir(&dir)
                .await
                .with_context(|| format!("listing artifact dir: {}", dir.display()))?;
            while let Some(entry) = entries.next_entry().await? {
                let entry_path = entry.path();
                let file_type = entry.file_type().await?;
                if file_type.is_dir() {
                    queue.push(entry_path);
                } else if file_type.is_file() {
                    out.push(path_to_store_key(self.root.as_path(), &entry_path)?);
                }
            }
        }
        out.sort();
        Ok(out)
    }

    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        let path = self.resolve_key_path(key)?;
        match fs::metadata(&path).await {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => {
                Err(error).with_context(|| format!("reading artifact metadata: {}", path.display()))
            }
        }
    }
}

fn normalize_store_key_path(key: &str) -> anyhow::Result<PathBuf> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Ok(PathBuf::new());
    }
    let rel = Path::new(trimmed);
    if rel.is_absolute() {
        anyhow::bail!("artifact key must be relative");
    }
    if rel
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        anyhow::bail!("artifact key must not contain parent directory segments");
    }
    Ok(rel.to_path_buf())
}

#[allow(dead_code)]
fn path_to_store_key(root: &Path, path: &Path) -> anyhow::Result<String> {
    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("artifact path outside store root: {}", path.display()))?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

#[allow(dead_code)]
pub(crate) fn resolve_in_store(
    artifact_store_root: &str,
    stored_path: &str,
) -> Result<PathBuf, String> {
    normalize_store_key_path(stored_path)
        .map(|rel| Path::new(artifact_store_root).join(rel))
        .map_err(|error| error.to_string())
}

pub(crate) fn ensure_store_root_exists(artifact_store_root: &str) -> Result<(), AppError> {
    std::fs::create_dir_all(artifact_store_root)
        .map_err(|error| AppError::Config(format!("failed to create artifact store root: {error}")))
}
