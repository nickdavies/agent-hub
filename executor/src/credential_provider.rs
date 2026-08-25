use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::{CredentialMaterial, DockerMount, ExecutionCancellationContext};

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
    /// Select a non-refreshable OpenAI OAuth access credential for one job.
    #[serde(rename = "opencode-chatgpt-oauth")]
    OpencodeChatgptOauth {
        name: String,
        host_auth_path: String,
        safety_margin_secs: u64,
    },
}

/// Resolved credentials ready for Docker injection
#[derive(Default)]
pub struct ResolvedCredentials {
    pub env_vars: HashMap<String, String>,
    pub file_mounts: Vec<(String, String, bool)>,
    pub temp_files: Vec<PathBuf>,
    pub redaction_values: Vec<String>,
}

impl std::fmt::Debug for ResolvedCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedCredentials")
            .field("env_var_count", &self.env_vars.len())
            .field("file_mount_count", &self.file_mounts.len())
            .field("temp_file_count", &self.temp_files.len())
            .field("redaction_value_count", &self.redaction_values.len())
            .finish()
    }
}

impl ResolvedCredentials {
    pub fn material(&self) -> anyhow::Result<CredentialMaterial> {
        Ok(CredentialMaterial {
            environment: self.env_vars.clone().into_iter().collect(),
            mounts: self
                .file_mounts
                .iter()
                .map(|(host, container, read_only)| DockerMount::new(host, container, *read_only))
                .collect::<anyhow::Result<_>>()?,
            temporary_files: self.temp_files.clone(),
            redaction_values: self.redaction_values.clone(),
        })
    }
}

impl CredentialProvider {
    pub fn name(&self) -> &str {
        match self {
            Self::File { name, .. } => name,
            Self::Env { name, .. } => name,
            Self::Command { name, .. } => name,
            Self::KubeconfigFilter { name, .. } => name,
            Self::OpencodeChatgptOauth { name, .. } => name,
        }
    }

    pub fn provider_type(&self) -> &'static str {
        match self {
            Self::File { .. } => "file",
            Self::Env { .. } => "env",
            Self::Command { .. } => "command",
            Self::KubeconfigFilter { .. } => "kubeconfig-filter",
            Self::OpencodeChatgptOauth { .. } => "opencode-chatgpt-oauth",
        }
    }

    pub fn targets(&self) -> Vec<String> {
        let mut targets = match self {
            Self::File { container_path, .. } => vec![container_path.clone()],
            Self::Env { vars, .. } => vars.keys().cloned().collect(),
            Self::Command {
                capture_env,
                capture_file,
                ..
            } => capture_env
                .iter()
                .chain(capture_file.iter())
                .cloned()
                .collect(),
            Self::KubeconfigFilter { container_path, .. } => {
                container_path.iter().cloned().collect()
            }
            Self::OpencodeChatgptOauth { .. } => {
                vec!["/opencode/data/opencode/auth.json".to_string()]
            }
        };
        targets.sort();
        targets.dedup();
        targets
    }

    async fn resolve_for_job(
        &self,
        valid_through: Option<SystemTime>,
        credential_dir: Option<&Path>,
        cancellation: &ExecutionCancellationContext,
    ) -> anyhow::Result<ResolvedCredentials> {
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
                let operation = cancellation.begin_external_operation(
                    "credential_command",
                    &format!("provider:{}", self.name()),
                )?;
                let mut child_command = tokio::process::Command::new(command);
                child_command.args(args).kill_on_drop(true);
                #[cfg(unix)]
                child_command.as_std_mut().process_group(0);
                let child = child_command.spawn()?;
                let pid = child
                    .id()
                    .and_then(|pid| i32::try_from(pid).ok())
                    .context("credential helper has no valid PID")?;
                let identity = super::read_process_identity(pid)?;
                let helper = cancellation.begin_external_operation(
                    "credential_helper_process",
                    &format!(
                        "pid={};pgid={};boot={};start={}",
                        identity.pid,
                        identity.pgid,
                        identity.linux_boot_id,
                        identity.proc_start_time_ticks
                    ),
                )?;
                let output = child.wait_with_output().await?;
                super::verify_process_group_absent(&identity).await?;
                helper.reconciled()?;
                if !output.status.success() {
                    operation.completed()?;
                    anyhow::bail!(
                        "credential command failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                operation.completed()?;
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let mut creds = ResolvedCredentials::default();
                if let Some(env_var) = capture_env {
                    creds.env_vars.insert(env_var.clone(), stdout.clone());
                }
                if let Some(file_path) = capture_file {
                    let tmp = std::env::temp_dir()
                        .join(format!("agent-hub-cred-{}", uuid::Uuid::new_v4()));
                    let materialize = cancellation.begin_external_operation(
                        "credential_materialize",
                        &tmp.to_string_lossy(),
                    )?;
                    tokio::fs::write(&tmp, stdout.as_bytes()).await?;
                    materialize.completed()?;
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
            Self::OpencodeChatgptOauth {
                host_auth_path,
                safety_margin_secs,
                ..
            } => resolve_opencode_chatgpt_oauth(
                host_auth_path,
                *safety_margin_secs,
                valid_through,
                credential_dir,
                cancellation,
            ),
        }
    }
}

pub async fn resolve_all_for_job(
    providers: &[CredentialProvider],
    valid_through: Option<SystemTime>,
    credential_dir: Option<&Path>,
    cancellation: &ExecutionCancellationContext,
) -> anyhow::Result<ResolvedCredentials> {
    cancellation.check()?;
    let mut merged = ResolvedCredentials::default();
    for provider in providers {
        cancellation.check()?;
        let creds = provider
            .resolve_for_job(valid_through, credential_dir, cancellation)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "credential provider '{}' failed: {error:#}",
                    provider.name()
                )
            })?;
        merged.env_vars.extend(creds.env_vars);
        merged.file_mounts.extend(creds.file_mounts);
        merged.temp_files.extend(creds.temp_files);
        merged.redaction_values.extend(creds.redaction_values);
    }
    cancellation.check()?;
    Ok(merged)
}

fn resolve_opencode_chatgpt_oauth(
    host_auth_path: &str,
    safety_margin_secs: u64,
    valid_through: Option<SystemTime>,
    credential_dir: Option<&Path>,
    cancellation: &ExecutionCancellationContext,
) -> anyhow::Result<ResolvedCredentials> {
    let valid_through =
        valid_through.context("authorized job deadline is required for ChatGPT OAuth")?;
    if safety_margin_secs == 0 {
        anyhow::bail!("ChatGPT OAuth safety margin must be greater than zero");
    }
    let credential_dir =
        credential_dir.context("job credential directory is required for ChatGPT OAuth")?;
    let source = shellexpand::tilde(host_auth_path).to_string();
    let bytes = std::fs::read(&source).context("read configured ChatGPT OAuth source")?;
    let entries = parse_auth_entries(&bytes)?;
    let mut openai = entries
        .into_iter()
        .filter(|(provider, _)| provider == "openai")
        .map(|(_, value)| value);
    let record = openai
        .next()
        .context("configured auth source has no openai record")?;
    if openai.next().is_some() {
        anyhow::bail!("configured auth source contains duplicate openai records");
    }
    let record = record
        .as_object()
        .context("configured openai auth record must be an object")?;
    if record.get("type").and_then(serde_json::Value::as_str) != Some("oauth") {
        anyhow::bail!("configured openai auth record must have OAuth type");
    }
    let access = required_nonempty_string(record, "access")?;
    if access.chars().any(char::is_control) {
        anyhow::bail!("configured openai OAuth access token contains control characters");
    }
    let account_id = required_nonempty_string(record, "accountId")?;
    let expires = record
        .get("expires")
        .and_then(serde_json::Value::as_u64)
        .context("configured openai OAuth expiry must be an unsigned millisecond timestamp")?;
    let required = oauth_required_expiry_millis(valid_through, safety_margin_secs)?;
    if u128::from(expires) <= required {
        anyhow::bail!(
            "openai OAuth access token expires before the job deadline and safety margin"
        );
    }

    let auth_dir = credential_dir.join("opencode");
    let directory = cancellation
        .begin_external_operation("credential_directory_create", &auth_dir.to_string_lossy())?;
    std::fs::create_dir_all(&auth_dir).context("create job OpenCode data directory")?;
    directory.completed()?;
    let auth_path = auth_dir.join("auth.json");
    let output = serde_json::to_vec(&serde_json::json!({
        "openai": {
            "type": "oauth",
            "access": access,
            "refresh": "",
            "expires": expires,
            "accountId": account_id,
        }
    }))?;
    write_secret_file(&auth_path, &output, cancellation)?;
    Ok(ResolvedCredentials {
        file_mounts: Vec::new(),
        env_vars: HashMap::new(),
        temp_files: vec![auth_path],
        redaction_values: vec![access.to_string()],
    })
}

fn oauth_required_expiry_millis(
    valid_through: SystemTime,
    safety_margin_secs: u64,
) -> anyhow::Result<u128> {
    Ok(valid_through
        .checked_add(Duration::from_secs(safety_margin_secs))
        .context("OAuth validity deadline overflow")?
        .duration_since(UNIX_EPOCH)
        .context("OAuth validity deadline precedes Unix epoch")?
        .as_millis())
}

fn required_nonempty_string<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> anyhow::Result<&'a str> {
    object
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("configured openai OAuth {key} must be a non-empty string"))
}

fn parse_auth_entries(bytes: &[u8]) -> anyhow::Result<Vec<(String, serde_json::Value)>> {
    use serde::de::{MapAccess, Visitor};

    struct EntriesVisitor;
    impl<'de> Visitor<'de> for EntriesVisitor {
        type Value = Vec<(String, serde_json::Value)>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an OpenCode auth object")
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut entries = Vec::new();
            while let Some(entry) = map.next_entry()? {
                entries.push(entry);
            }
            Ok(entries)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let entries = serde::de::Deserializer::deserialize_map(&mut deserializer, EntriesVisitor)
        .context("parse configured ChatGPT OAuth source")?;
    deserializer
        .end()
        .context("parse configured ChatGPT OAuth source")?;
    Ok(entries)
}

fn write_secret_file(
    path: &Path,
    bytes: &[u8],
    cancellation: &ExecutionCancellationContext,
) -> anyhow::Result<()> {
    let operation = cancellation
        .begin_external_operation("credential_secret_write", &path.to_string_lossy())?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .context("create job OpenCode auth file")?;
        file.write_all(bytes)
            .context("write job OpenCode auth file")?;
        operation.completed()
    }
    #[cfg(not(unix))]
    {
        let _ = (path, bytes);
        anyhow::bail!("secure OpenCode auth materialization requires Unix file modes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn cancellation(root: &Path) -> super::super::ExecutionCancellationContext {
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        super::super::ExecutionCancellationContext::for_test(root, stop_rx).unwrap()
    }

    #[test]
    fn auth_parser_preserves_duplicate_provider_names() {
        let entries = parse_auth_entries(br#"{"openai":{},"openai":{}}"#).unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn chatgpt_oauth_materializes_only_non_refreshable_openai_access() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-oauth-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.json");
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .checked_add(Duration::from_secs(3600))
            .unwrap()
            .as_millis() as u64;
        std::fs::write(
            &source,
            serde_json::to_vec(&serde_json::json!({
                "openai": {
                    "type": "oauth",
                    "access": "synthetic-access",
                    "refresh": "synthetic-refresh-must-not-copy",
                    "expires": expires,
                    "accountId": "synthetic-account"
                },
                "unrelated": {"type":"api", "key":"synthetic-other-secret"}
            }))
            .unwrap(),
        )
        .unwrap();
        let data = root.join("job-data");
        let provider = CredentialProvider::OpencodeChatgptOauth {
            name: "openai:chatgpt-oauth".into(),
            host_auth_path: source.to_string_lossy().into_owned(),
            safety_margin_secs: 60,
        };
        let cancellation = cancellation(&root);
        let resolved = provider
            .resolve_for_job(
                SystemTime::now().checked_add(Duration::from_secs(120)),
                Some(&data),
                &cancellation,
            )
            .await
            .unwrap();
        let auth_path = data.join("opencode/auth.json");
        let bytes = std::fs::read(&auth_path).unwrap();
        let output: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(output["openai"]["access"], "synthetic-access");
        assert_eq!(output["openai"]["accountId"], "synthetic-account");
        assert_eq!(output["openai"]["refresh"], "");
        assert!(output.get("unrelated").is_none());
        assert!(
            !String::from_utf8(bytes)
                .unwrap()
                .contains("synthetic-refresh-must-not-copy")
        );
        assert_eq!(
            std::fs::metadata(&auth_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(resolved.env_vars.is_empty());
        assert!(resolved.file_mounts.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn chatgpt_oauth_rejects_insufficient_validity_before_materializing() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-oauth-expiry-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.json");
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .checked_add(Duration::from_secs(30))
            .unwrap()
            .as_millis() as u64;
        std::fs::write(
            &source,
            serde_json::to_vec(&serde_json::json!({
                "openai": {
                    "type": "oauth",
                    "access": "synthetic-access",
                    "expires": expires,
                    "accountId": "synthetic-account"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let data = root.join("job-data");
        let provider = CredentialProvider::OpencodeChatgptOauth {
            name: "openai:chatgpt-oauth".into(),
            host_auth_path: source.to_string_lossy().into_owned(),
            safety_margin_secs: 60,
        };
        let cancellation = cancellation(&root);
        assert!(
            provider
                .resolve_for_job(
                    SystemTime::now().checked_add(Duration::from_secs(120)),
                    Some(&data),
                    &cancellation,
                )
                .await
                .is_err()
        );
        assert!(!data.join("opencode/auth.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn chatgpt_oauth_rejects_missing_wrong_type_and_duplicate_records() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-oauth-shape-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.json");
        let data = root.join("job-data");
        let provider = CredentialProvider::OpencodeChatgptOauth {
            name: "openai:chatgpt-oauth".into(),
            host_auth_path: source.to_string_lossy().into_owned(),
            safety_margin_secs: 60,
        };
        let cancellation = cancellation(&root);
        for invalid in [
            r#"{}"#,
            r#"{"openai":{"type":"api","access":"synthetic","expires":9999999999999,"accountId":"synthetic"}}"#,
            r#"{"openai":{"type":"oauth","access":"synthetic","expires":9999999999999,"accountId":"synthetic"},"openai":{"type":"oauth","access":"synthetic","expires":9999999999999,"accountId":"synthetic"}}"#,
        ] {
            std::fs::write(&source, invalid).unwrap();
            assert!(
                provider
                    .resolve_for_job(
                        SystemTime::now().checked_add(Duration::from_secs(120)),
                        Some(&data),
                        &cancellation,
                    )
                    .await
                    .is_err()
            );
            assert!(!data.join("opencode/auth.json").exists());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn chatgpt_oauth_rejects_expiry_exactly_at_deadline_margin_boundary() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-oauth-boundary-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.json");
        let valid_through = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let required = oauth_required_expiry_millis(valid_through, 60).unwrap() as u64;
        std::fs::write(
            &source,
            serde_json::to_vec(&serde_json::json!({
                "openai": {
                    "type": "oauth",
                    "access": "synthetic-access",
                    "refresh": "synthetic-refresh-must-not-run",
                    "expires": required,
                    "accountId": "synthetic-account"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let provider = CredentialProvider::OpencodeChatgptOauth {
            name: "openai:chatgpt-oauth".into(),
            host_auth_path: source.to_string_lossy().into_owned(),
            safety_margin_secs: 60,
        };
        let cancellation = cancellation(&root);
        assert!(
            provider
                .resolve_for_job(Some(valid_through), Some(&root.join("data")), &cancellation,)
                .await
                .is_err()
        );
        assert!(!root.join("data/opencode/auth.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
