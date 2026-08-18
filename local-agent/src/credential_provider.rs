use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// A credential provider definition from config
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CredentialProvider {
    /// Mount a host file/directory read-only into the container
    #[serde(rename = "file")]
    File {
        name: String,
        host_path: String,
        container_path: String,
        #[serde(default = "default_true")]
        read_only: bool,
    },
    /// Inject environment variables
    #[serde(rename = "env")]
    Env {
        name: String,
        vars: HashMap<String, String>,
    },
    /// Run a host command and capture output as env vars or files
    #[serde(rename = "command")]
    Command {
        name: String,
        command: String,
        args: Vec<String>,
        /// Map command output (stdout) to this env var
        capture_env: Option<String>,
        /// Write command output to this container path
        capture_file: Option<String>,
    },
    /// Filter host kubeconfig and mount into container
    #[serde(rename = "kubeconfig-filter")]
    KubeconfigFilter {
        name: String,
        /// Source kubeconfig (default: ~/.kube/config)
        source_path: Option<String>,
        /// Allowed context patterns (glob)
        allowed_contexts: Vec<String>,
        /// Container path for filtered kubeconfig
        container_path: Option<String>,
    },
}

/// Resolved credentials ready for Docker injection
#[derive(Debug, Default)]
pub struct ResolvedCredentials {
    pub env_vars: HashMap<String, String>,
    pub file_mounts: Vec<(String, String, bool)>,
    pub temp_files: Vec<PathBuf>,
}

impl CredentialProvider {
    pub fn name(&self) -> &str {
        match self {
            Self::File { name, .. } => name,
            Self::Env { name, .. } => name,
            Self::Command { name, .. } => name,
            Self::KubeconfigFilter { name, .. } => name,
        }
    }

    /// Resolve this provider into concrete credentials
    pub async fn resolve(&self) -> anyhow::Result<ResolvedCredentials> {
        match self {
            Self::File {
                host_path,
                container_path,
                read_only,
                ..
            } => {
                let resolved_path = shellexpand::tilde(host_path).to_string();
                Ok(ResolvedCredentials {
                    file_mounts: vec![(resolved_path, container_path.clone(), *read_only)],
                    ..Default::default()
                })
            }
            Self::Env { vars, .. } => Ok(ResolvedCredentials {
                env_vars: vars.clone(),
                ..Default::default()
            }),
            Self::Command {
                command,
                args,
                capture_env,
                capture_file,
                ..
            } => {
                let output = tokio::process::Command::new(command)
                    .args(args)
                    .output()
                    .await?;
                if !output.status.success() {
                    anyhow::bail!(
                        "credential command failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let mut creds = ResolvedCredentials::default();
                if let Some(env_var) = capture_env {
                    creds.env_vars.insert(env_var.clone(), stdout.clone());
                }
                if let Some(file_path) = capture_file {
                    let tmp = std::env::temp_dir()
                        .join(format!("agent-hub-cred-{}", uuid::Uuid::new_v4()));
                    tokio::fs::write(&tmp, stdout.as_bytes()).await?;
                    creds.file_mounts.push((
                        tmp.to_string_lossy().to_string(),
                        file_path.clone(),
                        true,
                    ));
                    creds.temp_files.push(tmp);
                }
                Ok(creds)
            }
            Self::KubeconfigFilter {
                source_path,
                allowed_contexts: _,
                container_path,
                ..
            } => {
                // For MVP: just mount the whole kubeconfig read-only.
                // TODO: implement actual context filtering.
                let source = source_path.as_deref().unwrap_or("~/.kube/config");
                let resolved_source = shellexpand::tilde(source).to_string();
                let target = container_path
                    .as_deref()
                    .unwrap_or("/home/agent/.kube/config")
                    .to_string();

                Ok(ResolvedCredentials {
                    file_mounts: vec![(resolved_source, target.clone(), true)],
                    env_vars: HashMap::from([("KUBECONFIG".to_string(), target)]),
                    ..Default::default()
                })
            }
        }
    }
}

/// Resolve multiple providers and merge results
pub async fn resolve_all(providers: &[CredentialProvider]) -> anyhow::Result<ResolvedCredentials> {
    let mut merged = ResolvedCredentials::default();
    for provider in providers {
        let creds = provider.resolve().await.map_err(|error| {
            anyhow::anyhow!(
                "credential provider '{}' failed: {error:#}",
                provider.name()
            )
        })?;
        merged.env_vars.extend(creds.env_vars);
        merged.file_mounts.extend(creds.file_mounts);
        merged.temp_files.extend(creds.temp_files);
    }
    Ok(merged)
}
