use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::LocalAgentEgressTier;
use crate::egress_config::{LocalAgentConfig, ProxyConfig, canonical_hosts};

const EGRESS_WORKER_LABEL: &str = "agent-hub.local-agent.worker_id";
const EGRESS_MANAGED_LABEL: &str = "agent-hub.local-agent.egress.managed";
const EGRESS_KIND_LABEL: &str = "agent-hub.local-agent.egress.kind";
const EGRESS_TIER_LABEL: &str = "agent-hub.local-agent.egress.tier";
const EGRESS_FINGERPRINT_LABEL: &str = "agent-hub.local-agent.egress.fingerprint";
const PROXY_READINESS_RETRIES: usize = 10;
const PROXY_READINESS_DELAY_MS: u64 = 500;
const PROXY_READINESS_CONNECT_TIMEOUT_SECS: u64 = 1;
const PROXY_DIAGNOSTIC_LIMIT: usize = 8192;
const PROXY_LOG_TAIL_LINES: usize = 40;
const PROXY_CREATE_RACE_RETRIES: usize = 10;
const PROXY_CREATE_RACE_DELAY_MS: u64 = 100;
const PROXY_COMPATIBILITY_DIAGNOSTIC_LIMIT: usize = 8192;
const PROXY_COMPATIBILITY_VALUE_LIMIT: usize = 96;

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
struct ProxyCompatibilityMismatch {
    field: String,
    expected: String,
    actual: String,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
struct ProxyCompatibilityReport {
    owned: bool,
    mismatches: Vec<ProxyCompatibilityMismatch>,
}

impl ProxyCompatibilityReport {
    fn compatible(&self) -> bool {
        self.mismatches.is_empty()
    }

    fn push(&mut self, field: &str, expected: impl ToString, actual: impl ToString) {
        self.mismatches.push(ProxyCompatibilityMismatch {
            field: bound_text(field, PROXY_COMPATIBILITY_VALUE_LIMIT),
            expected: bound_text(&expected.to_string(), PROXY_COMPATIBILITY_VALUE_LIMIT),
            actual: bound_text(&actual.to_string(), PROXY_COMPATIBILITY_VALUE_LIMIT),
        });
    }

    fn diagnostic(&self) -> String {
        let serialized = serde_json::to_string(self).expect("compatibility report serializes");
        if serialized.len() <= PROXY_COMPATIBILITY_DIAGNOSTIC_LIMIT {
            serialized
        } else {
            serde_json::json!({
                "owned": self.owned,
                "mismatch_fields": self.mismatches.iter().map(|mismatch| &mismatch.field).collect::<Vec<_>>(),
                "details_truncated": true,
            })
            .to_string()
        }
    }
}

#[derive(Debug, Clone)]
pub struct RestrictedEgressRuntime {
    pub network_name: String,
    pub proxy_url: String,
    pub no_proxy: String,
    pub tier_hosts: Vec<String>,
    pub proxy_host: String,
    pub proxy_ip: String,
}

impl RestrictedEgressRuntime {
    pub fn apply_container_network_controls(
        &self,
        args: &mut Vec<String>,
        env: &mut HashMap<String, String>,
    ) {
        args.push(format!("--network={}", self.network_name));
        args.push("--dns=127.0.0.1".to_string());
        args.push(format!("--add-host={}:{}", self.proxy_host, self.proxy_ip));

        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            env.remove(key);
        }
        env.insert("HTTP_PROXY".to_string(), self.proxy_url.clone());
        env.insert("HTTPS_PROXY".to_string(), self.proxy_url.clone());
        env.insert("NO_PROXY".to_string(), self.no_proxy.clone());
        env.insert("http_proxy".to_string(), self.proxy_url.clone());
        env.insert("https_proxy".to_string(), self.proxy_url.clone());
        env.insert("no_proxy".to_string(), self.no_proxy.clone());
        env.insert(
            "AGENT_RESTRICTED_EGRESS_HOSTS".to_string(),
            self.tier_hosts.join(","),
        );
    }
}

#[derive(Debug, Clone)]
pub struct EgressManager {
    worker_id: String,
    egress_dir: PathBuf,
    config: LocalAgentConfig,
}

impl EgressManager {
    pub fn new(worker_id: &str, data_dir: &Path, config: LocalAgentConfig) -> anyhow::Result<Self> {
        let data_dir = if data_dir.is_absolute() {
            data_dir.to_path_buf()
        } else {
            std::env::current_dir()
                .context("resolve current directory")?
                .join(data_dir)
        };
        let egress_dir = data_dir.join("egress");
        ensure_private_directory(&egress_dir)?;
        Ok(Self {
            worker_id: sanitize_name_component(worker_id),
            egress_dir,
            config,
        })
    }

    pub async fn prepare_restricted_tier(
        &self,
        tier: LocalAgentEgressTier,
        llm_provider: Option<&str>,
    ) -> anyhow::Result<RestrictedEgressRuntime> {
        if !matches!(
            tier,
            LocalAgentEgressTier::Llm | LocalAgentEgressTier::Build
        ) {
            anyhow::bail!("egress manager can only prepare restricted llm/build tiers");
        }

        let tier_hosts = self.resolve_tier_hosts(tier, llm_provider)?;

        let external_network = self.network_name("external");
        let internal_network = self.network_name(tier.as_str());

        self.ensure_network(&external_network, false).await?;
        self.ensure_network(&internal_network, true).await?;

        let squid_config = self.render_squid_config(&tier_hosts);
        let config_path = self
            .egress_dir
            .join(format!("squid-{}.conf", tier.as_str()));
        write_generated_config(&config_path, squid_config.as_bytes())?;

        let fingerprint = proxy_fingerprint(squid_config.as_bytes(), &self.config.egress.proxy);
        let container_name = self.container_name(tier);
        self.ensure_proxy_container(
            tier,
            &container_name,
            &config_path,
            &fingerprint,
            &internal_network,
            &external_network,
        )
        .await?;
        let proxy_ip = self
            .container_network_ip(&container_name, &internal_network)
            .await?
            .ok_or_else(|| anyhow::anyhow!("restricted proxy has no internal network address"))?;

        self.write_state_file(tier, &fingerprint, &tier_hosts)?;

        Ok(RestrictedEgressRuntime {
            network_name: internal_network,
            proxy_url: format!(
                "http://{}:{}",
                container_name, self.config.egress.proxy.listen_port
            ),
            no_proxy: "localhost,127.0.0.1,::1,.local".to_string(),
            tier_hosts,
            proxy_host: container_name,
            proxy_ip,
        })
    }

    pub fn config(&self) -> &LocalAgentConfig {
        &self.config
    }

    pub async fn preflight_llm_only(&self) -> anyhow::Result<()> {
        let runtime = self
            .prepare_restricted_tier(LocalAgentEgressTier::Llm, None)
            .await?;
        let allowed = runtime
            .tier_hosts
            .first()
            .ok_or_else(|| anyhow::anyhow!("llm-only requires a model host"))?;
        let image = self
            .config
            .egress
            .preflight_image
            .as_deref()
            .map(str::trim)
            .filter(|image| !image.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "egress.preflight_image is required and must provide sh, curl, and getent"
                )
            })?;
        let args = self.preflight_container_invocation(&runtime, allowed, image);
        let mut command = tokio::process::Command::new("docker");
        command.kill_on_drop(true);
        command.args(&args);
        let output = command
            .output()
            .await
            .context("launch restricted egress preflight container")?;
        if !output.status.success() {
            anyhow::bail!(
                "restricted egress preflight failed: status={} stdout={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn preflight_container_invocation(
        &self,
        runtime: &RestrictedEgressRuntime,
        allowed_host: &str,
        image: &str,
    ) -> Vec<String> {
        let mut args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "--read-only".to_string(),
            "--security-opt=no-new-privileges".to_string(),
            "--cap-drop=ALL".to_string(),
            "--tmpfs=/tmp:rw,size=64m".to_string(),
        ];
        let mut env = HashMap::new();
        runtime.apply_container_network_controls(&mut args, &mut env);
        let mut env = env.into_iter().collect::<Vec<_>>();
        env.sort_by(|left, right| left.0.cmp(&right.0));
        for (key, value) in env {
            args.push("-e".to_string());
            args.push(format!("{key}={value}"));
        }
        args.extend([
            "--entrypoint".to_string(),
            "sh".to_string(),
            image.to_string(),
            "-ceu".to_string(),
            r#"
model_host="$1"
command -v curl >/dev/null
command -v getent >/dev/null
curl -sS --connect-timeout 10 --max-time 20 -o /dev/null "https://${model_host}/"
if getent hosts preflight-arbitrary.invalid >/dev/null 2>&1; then exit 21; fi
if env -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u http_proxy -u https_proxy -u all_proxy curl -ksS --connect-timeout 3 --max-time 5 -o /dev/null https://1.1.1.1/; then exit 22; fi
if NO_PROXY="$model_host" no_proxy="$model_host" curl -sS --connect-timeout 3 --max-time 5 -o /dev/null "https://${model_host}/"; then exit 23; fi
if curl --fail -sS --connect-timeout 5 --max-time 10 -o /dev/null https://example.com/; then exit 24; fi
"#
            .to_string(),
            "egress-preflight".to_string(),
            allowed_host.to_string(),
        ]);
        args
    }

    fn resolve_tier_hosts(
        &self,
        tier: LocalAgentEgressTier,
        llm_provider: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let llm_hosts = match tier {
            LocalAgentEgressTier::Llm | LocalAgentEgressTier::Build => {
                self.resolve_llm_provider_hosts(llm_provider)?
            }
            LocalAgentEgressTier::Github | LocalAgentEgressTier::Full => Vec::new(),
        };
        let network_policy_name = match tier {
            LocalAgentEgressTier::Llm => Some("llm-only"),
            LocalAgentEgressTier::Github => Some("github-only"),
            LocalAgentEgressTier::Build => Some("build"),
            LocalAgentEgressTier::Full => None,
        };

        if let Some(policy_name) = network_policy_name
            && let Some(policy) = self.config.network_policy(policy_name)
        {
            let mut hosts = policy.hosts.clone();
            if policy.uses_llm_provider_hosts {
                hosts.extend(llm_hosts.iter().cloned());
            }
            let hosts = canonical_hosts(&hosts);
            if hosts.is_empty() {
                anyhow::bail!(
                    "restricted {} tier requested but '{}' policy has no hosts",
                    tier.as_str(),
                    policy_name
                );
            }
            if policy.unrestricted || policy.docker_network.is_some() {
                anyhow::bail!(
                    "restricted '{policy_name}' policy must not be unrestricted or select an external Docker network"
                );
            }
            if matches!(
                tier,
                LocalAgentEgressTier::Llm | LocalAgentEgressTier::Build
            ) {
                let expected = match tier {
                    LocalAgentEgressTier::Llm => llm_hosts.clone(),
                    LocalAgentEgressTier::Build => self.build_hosts(&llm_hosts),
                    _ => unreachable!(),
                };
                if hosts != expected {
                    anyhow::bail!(
                        "restricted {policy_name} hosts must exactly match configured hosts: policy={hosts:?} configured={expected:?}"
                    );
                }
            }
            return Ok(hosts);
        }

        match tier {
            LocalAgentEgressTier::Llm => Ok(llm_hosts),
            LocalAgentEgressTier::Build => Ok(self.build_hosts(&llm_hosts)),
            LocalAgentEgressTier::Github => {
                anyhow::bail!("github-only policy must be loaded from CONFIG_ROOT")
            }
            LocalAgentEgressTier::Full => {
                anyhow::bail!("full tier does not use restricted egress proxy")
            }
        }
    }

    fn resolve_llm_provider_hosts(&self, provider: Option<&str>) -> anyhow::Result<Vec<String>> {
        let (provider, local_hosts) = self.config.egress.llm.resolve_provider_hosts(provider)?;
        validate_literal_model_hosts(&local_hosts)?;
        let Some(config_tree) = self.config.config_tree() else {
            return Ok(local_hosts);
        };
        let system_hosts = config_tree
            .llm_provider_hosts
            .get(&provider)
            .ok_or_else(|| anyhow::anyhow!("unknown system LLM provider '{provider}'"))?;
        if system_hosts != &local_hosts {
            anyhow::bail!(
                "LLM provider '{provider}' hosts differ between system and local-agent config: system={system_hosts:?} local={local_hosts:?}"
            );
        }
        Ok(system_hosts.clone())
    }

    fn build_hosts(&self, llm_hosts: &[String]) -> Vec<String> {
        let mut hosts = llm_hosts.to_vec();
        hosts.extend(self.config.egress.build.github_hosts.clone());
        hosts.extend(self.config.egress.build.go_registry_hosts.clone());
        hosts.extend(self.config.egress.build.crates_registry_hosts.clone());
        canonical_hosts(&hosts)
    }

    fn render_squid_config(&self, allowed_hosts: &[String]) -> String {
        let (apex_hosts, subdomain_hosts) = squid_acl_host_groups(allowed_hosts);
        let mut out = String::new();
        out.push_str(&format!(
            "http_port {}\n",
            self.config.egress.proxy.listen_port
        ));
        // Residual risk note: container DNS resolution itself is still required.
        // ACLs enforce destination host allow-lists, but DNS lookups may leak queried names.
        out.push_str("visible_hostname agent-hub-egress\n");
        out.push_str("acl SSL_ports port 443\n");
        out.push_str("acl Safe_ports port 80\n");
        out.push_str("acl Safe_ports port 443\n");
        out.push_str("acl CONNECT method CONNECT\n");
        out.push_str("http_access deny !Safe_ports\n");
        out.push_str("http_access deny CONNECT !SSL_ports\n");
        out.push_str("acl allowed_exact dstdomain");
        for host in &apex_hosts {
            out.push(' ');
            out.push_str(host);
        }
        out.push('\n');
        out.push_str("acl allowed_subdomains dstdomain");
        for host in &subdomain_hosts {
            out.push(' ');
            out.push_str(host);
        }
        out.push('\n');
        out.push_str("http_access allow allowed_exact\n");
        out.push_str("http_access allow allowed_subdomains\n");
        out.push_str("http_access deny all\n");
        out.push_str("cache deny all\n");
        out.push_str("cache_log stdio:/dev/stderr\n");
        out.push_str("access_log daemon:/var/log/squid/access.log\n");
        out
    }

    async fn ensure_network(&self, name: &str, internal: bool) -> anyhow::Result<()> {
        if let Some(inspect) = docker_network_inspect(name).await? {
            let (owned, compatible) = self.network_ownership_and_compatibility(&inspect, internal);
            if compatible {
                return Ok(());
            }
            if !owned {
                anyhow::bail!(
                    "Docker network '{name}' conflicts with restricted egress and is not owned by this worker"
                );
            }
            let mut rm = tokio::process::Command::new("docker");
            rm.kill_on_drop(true);
            rm.arg("network").arg("rm").arg(name);
            run_status(
                &mut rm,
                "docker network rm incompatible owned network failed",
            )
            .await?;
        }

        let mut cmd = tokio::process::Command::new("docker");
        cmd.kill_on_drop(true);
        cmd.arg("network")
            .arg("create")
            .arg("--driver")
            .arg("bridge")
            .arg("--label")
            .arg(format!("{EGRESS_MANAGED_LABEL}=true"))
            .arg("--label")
            .arg(format!("{EGRESS_WORKER_LABEL}={}", self.worker_id))
            .arg("--label")
            .arg(format!("{EGRESS_KIND_LABEL}=network"));
        if internal {
            cmd.arg("--internal");
        }
        cmd.arg(name);

        match run_status(&mut cmd, "docker network create failed").await {
            Ok(()) => Ok(()),
            Err(error) => {
                let Some(inspect) = docker_network_inspect(name).await? else {
                    return Err(error);
                };
                let (_, compatible) = self.network_ownership_and_compatibility(&inspect, internal);
                if !compatible {
                    anyhow::bail!(
                        "Docker network '{name}' appeared during create race but failed restricted egress ownership/internal/driver/scope checks"
                    );
                }
                Ok(())
            }
        }
    }

    fn network_ownership_and_compatibility(
        &self,
        inspect: &serde_json::Value,
        internal: bool,
    ) -> (bool, bool) {
        let labels = inspect.get("Labels").and_then(serde_json::Value::as_object);
        let owned = labels
            .and_then(|labels| labels.get(EGRESS_MANAGED_LABEL))
            .and_then(serde_json::Value::as_str)
            == Some("true")
            && labels
                .and_then(|labels| labels.get(EGRESS_WORKER_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some(self.worker_id.as_str());
        let compatible = owned
            && inspect.get("Internal").and_then(serde_json::Value::as_bool) == Some(internal)
            && inspect.get("Driver").and_then(serde_json::Value::as_str) == Some("bridge")
            && inspect.get("Scope").and_then(serde_json::Value::as_str) == Some("local")
            && labels
                .and_then(|labels| labels.get(EGRESS_KIND_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some("network");
        (owned, compatible)
    }

    async fn ensure_proxy_container(
        &self,
        tier: LocalAgentEgressTier,
        container_name: &str,
        config_path: &Path,
        fingerprint: &str,
        internal_network: &str,
        external_network: &str,
    ) -> anyhow::Result<()> {
        let inspect = docker_container_inspect(container_name).await?;
        let compatibility = inspect.as_ref().map(|json| {
            self.proxy_compatibility_report(
                json,
                tier,
                config_path,
                fingerprint,
                internal_network,
                external_network,
            )
        });
        let owned = compatibility.as_ref().is_some_and(|report| report.owned);
        let compatible = compatibility
            .as_ref()
            .is_some_and(ProxyCompatibilityReport::compatible);
        let should_recreate = !compatible;

        if should_recreate && inspect.is_some() {
            if let Some(report) = &compatibility {
                eprintln!(
                    "info: existing restricted proxy is incompatible: {}",
                    report.diagnostic()
                );
            }
            if !owned {
                anyhow::bail!(
                    "Docker container '{container_name}' conflicts with restricted egress and is not owned by this worker: {}",
                    compatibility
                        .as_ref()
                        .map(ProxyCompatibilityReport::diagnostic)
                        .unwrap_or_default()
                );
            }
            let mut rm = tokio::process::Command::new("docker");
            rm.kill_on_drop(true);
            rm.args(proxy_remove_args(container_name));
            run_status(&mut rm, "docker rm proxy container failed").await?;
        }

        if should_recreate {
            let mut run = tokio::process::Command::new("docker");
            run.kill_on_drop(true);
            run.args(proxy_container_run_args(
                &self.config.egress.proxy,
                container_name,
                internal_network,
                config_path,
                fingerprint,
                tier,
                &self.worker_id,
            ));
            if let Err(error) = run_status(&mut run, "docker run squid proxy failed").await {
                let compatible_winner = wait_for_compatible_proxy_winner(
                    || docker_container_inspect(container_name),
                    |json| {
                        let report = self.proxy_compatibility_report(
                            json,
                            tier,
                            config_path,
                            fingerprint,
                            internal_network,
                            external_network,
                        );
                        report.owned && report.compatible()
                    },
                    PROXY_CREATE_RACE_RETRIES,
                    Duration::from_millis(PROXY_CREATE_RACE_DELAY_MS),
                )
                .await;
                require_compatible_create_winner(compatible_winner, error)?;
                self.wait_for_proxy_ready(container_name, internal_network)
                    .await?;
                return Ok(());
            }

            let mut connect = tokio::process::Command::new("docker");
            connect.kill_on_drop(true);
            connect
                .arg("network")
                .arg("connect")
                .arg(external_network)
                .arg(container_name);
            self.connect_container_network(container_name, external_network, &mut connect)
                .await?;
            self.wait_for_proxy_ready(container_name, internal_network)
                .await?;
            return Ok(());
        }

        for action in compatible_proxy_actions(internal_network, external_network) {
            match action {
                CompatibleProxyAction::EnsureRunning => {
                    self.ensure_container_running(container_name).await?
                }
                CompatibleProxyAction::EnsureConnected(network) => {
                    self.ensure_container_connected(container_name, &network)
                        .await?
                }
                CompatibleProxyAction::WaitReady(network) => {
                    self.wait_for_proxy_ready(container_name, &network).await?
                }
            }
        }
        Ok(())
    }

    fn proxy_compatibility_report(
        &self,
        json: &serde_json::Value,
        tier: LocalAgentEgressTier,
        config_path: &Path,
        fingerprint: &str,
        internal_network: &str,
        external_network: &str,
    ) -> ProxyCompatibilityReport {
        let labels = json
            .pointer("/Config/Labels")
            .and_then(serde_json::Value::as_object);
        let owned = labels
            .and_then(|labels| labels.get(EGRESS_MANAGED_LABEL))
            .and_then(serde_json::Value::as_str)
            == Some("true")
            && labels
                .and_then(|labels| labels.get(EGRESS_WORKER_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some(self.worker_id.as_str());
        let mut report = ProxyCompatibilityReport {
            owned,
            mismatches: Vec::new(),
        };
        let expected_tmpfs = proxy_tmpfs(&self.config.egress.proxy);
        let expected_bind = format!("{}:/etc/squid/squid.conf:ro", config_path.to_string_lossy());
        let tmpfs = json
            .pointer("/HostConfig/Tmpfs")
            .and_then(serde_json::Value::as_object);
        check_json_string(
            &mut report,
            "label.managed",
            labels.and_then(|labels| labels.get(EGRESS_MANAGED_LABEL)),
            "true",
        );
        check_json_string(
            &mut report,
            "label.worker_id",
            labels.and_then(|labels| labels.get(EGRESS_WORKER_LABEL)),
            &self.worker_id,
        );
        check_json_string(
            &mut report,
            "label.kind",
            labels.and_then(|labels| labels.get(EGRESS_KIND_LABEL)),
            "proxy",
        );
        check_json_string(
            &mut report,
            "label.tier",
            labels.and_then(|labels| labels.get(EGRESS_TIER_LABEL)),
            tier.as_str(),
        );
        check_json_string(
            &mut report,
            "label.fingerprint",
            labels.and_then(|labels| labels.get(EGRESS_FINGERPRINT_LABEL)),
            fingerprint,
        );
        check_json_string(
            &mut report,
            "image",
            json.pointer("/Config/Image"),
            &self.config.egress.proxy.image,
        );
        let actual_user = json
            .pointer("/Config/User")
            .and_then(serde_json::Value::as_str);
        let expected_uid = self.config.egress.proxy.runtime_uid.to_string();
        let expected_gid = self.config.egress.proxy.runtime_gid.to_string();
        let (actual_uid, actual_gid) = actual_user
            .and_then(|user| user.split_once(':'))
            .map_or((actual_user, actual_user), |(uid, gid)| {
                (Some(uid), Some(gid))
            });
        if actual_uid != Some(expected_uid.as_str()) {
            report.push(
                "runtime.uid",
                self.config.egress.proxy.runtime_uid,
                actual_uid.unwrap_or("<missing>"),
            );
        }
        if actual_gid != Some(expected_gid.as_str()) {
            report.push(
                "runtime.gid",
                self.config.egress.proxy.runtime_gid,
                actual_gid.unwrap_or("<missing>"),
            );
        }
        check_json_bool(
            &mut report,
            "read_only_root",
            json.pointer("/HostConfig/ReadonlyRootfs"),
            true,
        );
        check_json_string(
            &mut report,
            "restart_policy",
            json.pointer("/HostConfig/RestartPolicy/Name"),
            "unless-stopped",
        );
        check_json_string(
            &mut report,
            "network.mode",
            json.pointer("/HostConfig/NetworkMode"),
            internal_network,
        );

        let cap_drop = json.pointer("/HostConfig/CapDrop");
        if !cap_drop
            .and_then(serde_json::Value::as_array)
            .is_some_and(|caps| {
                caps.len() == 1
                    && caps[0]
                        .as_str()
                        .is_some_and(|cap| cap.eq_ignore_ascii_case("all"))
            })
        {
            report.push("capabilities.drop", "[ALL]", summarize_json(cap_drop));
        }
        let cap_add = json.pointer("/HostConfig/CapAdd");
        if !cap_add.is_none_or(serde_json::Value::is_null) {
            report.push("capabilities.add", "null", summarize_json(cap_add));
        }

        let binds = json.pointer("/HostConfig/Binds");
        if !binds
            .and_then(serde_json::Value::as_array)
            .is_some_and(|binds| {
                binds.len() == 1 && binds[0].as_str() == Some(expected_bind.as_str())
            })
        {
            report.push(
                "mount.config_bind.declaration",
                &expected_bind,
                summarize_json(binds),
            );
        }

        for (path, options) in &expected_tmpfs {
            let actual = tmpfs.and_then(|tmpfs| tmpfs.get(path));
            if actual.and_then(serde_json::Value::as_str) != Some(options.as_str()) {
                report.push(&format!("tmpfs.{path}"), options, summarize_json(actual));
            }
        }
        let expected_mode = format!("mode={}", self.config.egress.proxy.writable_tmpfs_mode);
        let actual_modes = expected_tmpfs
            .iter()
            .map(|(path, _)| {
                tmpfs
                    .and_then(|tmpfs| tmpfs.get(path))
                    .and_then(serde_json::Value::as_str)
                    .and_then(|options| {
                        options
                            .split(',')
                            .find(|option| option.starts_with("mode="))
                    })
                    .unwrap_or("<missing>")
            })
            .collect::<Vec<_>>();
        if actual_modes
            .iter()
            .any(|mode| *mode != expected_mode.as_str())
        {
            report.push(
                "runtime.writable_tmpfs_mode",
                &expected_mode,
                format!("{actual_modes:?}"),
            );
        }
        let unexpected_tmpfs = tmpfs
            .map(|tmpfs| {
                tmpfs
                    .keys()
                    .filter(|path| !expected_tmpfs.iter().any(|(expected, _)| expected == *path))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !unexpected_tmpfs.is_empty() {
            report.push("tmpfs.unexpected", "[]", format!("{unexpected_tmpfs:?}"));
        }
        if tmpfs.is_none() {
            report.push(
                "tmpfs.map",
                "object",
                summarize_json(json.pointer("/HostConfig/Tmpfs")),
            );
        }

        append_mount_topology_mismatches(
            &mut report,
            json,
            config_path,
            &self.config.egress.proxy.declared_volumes,
        );
        append_network_attachment_mismatches(&mut report, json, internal_network, external_network);
        report
    }

    async fn ensure_container_running(&self, container_name: &str) -> anyhow::Result<()> {
        let mut inspect = tokio::process::Command::new("docker");
        inspect.kill_on_drop(true);
        inspect
            .arg("inspect")
            .arg("--format")
            .arg("{{.State.Running}}")
            .arg(container_name);
        let output = inspect.output().await?;
        let running =
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true";
        if running {
            return Ok(());
        }

        let mut start = tokio::process::Command::new("docker");
        start.kill_on_drop(true);
        start.arg("start").arg(container_name);
        run_status(&mut start, "docker start proxy container failed").await?;
        Ok(())
    }

    async fn ensure_container_connected(
        &self,
        container_name: &str,
        network_name: &str,
    ) -> anyhow::Result<()> {
        let mut inspect = tokio::process::Command::new("docker");
        inspect.kill_on_drop(true);
        inspect
            .arg("inspect")
            .arg("--format")
            .arg(format!(
                "{{{{index .NetworkSettings.Networks \"{network_name}\"}}}}"
            ))
            .arg(container_name);
        let output = inspect.output().await?;
        let connected = output.status.success()
            && String::from_utf8_lossy(&output.stdout).trim() != "<no value>";
        if connected {
            return Ok(());
        }

        let mut connect = tokio::process::Command::new("docker");
        connect.kill_on_drop(true);
        connect
            .arg("network")
            .arg("connect")
            .arg(network_name)
            .arg(container_name);
        self.connect_container_network(container_name, network_name, &mut connect)
            .await
    }

    async fn connect_container_network(
        &self,
        container_name: &str,
        network_name: &str,
        connect_cmd: &mut tokio::process::Command,
    ) -> anyhow::Result<()> {
        match run_status(connect_cmd, "docker network connect failed").await {
            Ok(()) => Ok(()),
            Err(error) => {
                if self
                    .container_connected_to_network(container_name, network_name)
                    .await?
                {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn wait_for_proxy_ready(
        &self,
        container_name: &str,
        network_name: &str,
    ) -> anyhow::Result<()> {
        eprintln!(
            "info: waiting for squid proxy readiness on container={container_name} network={network_name}"
        );

        for attempt in 1..=PROXY_READINESS_RETRIES {
            if !container_running(container_name).await? {
                return self
                    .proxy_readiness_failure(container_name, "container is not running")
                    .await;
            }

            if let Some(ip) = self
                .container_network_ip(container_name, network_name)
                .await?
            {
                let addr = format!("{}:{}", ip, self.config.egress.proxy.listen_port);
                let connect_result = timeout(
                    Duration::from_secs(PROXY_READINESS_CONNECT_TIMEOUT_SECS),
                    TcpStream::connect(addr.as_str()),
                )
                .await;

                if matches!(connect_result, Ok(Ok(_))) {
                    eprintln!(
                        "debug: squid proxy is accepting connections on {addr} (attempt {attempt})"
                    );
                    return Ok(());
                }

                eprintln!(
                    "debug: squid proxy not ready yet on {addr} (attempt {attempt}/{PROXY_READINESS_RETRIES})"
                );
            } else {
                eprintln!(
                    "debug: proxy container has no network IP yet (attempt {attempt}/{PROXY_READINESS_RETRIES})"
                );
            }

            if attempt < PROXY_READINESS_RETRIES {
                tokio::time::sleep(Duration::from_millis(PROXY_READINESS_DELAY_MS)).await;
            }
        }

        self.proxy_readiness_failure(
            container_name,
            &format!(
                "did not accept TCP connections on port {} after {} retries",
                self.config.egress.proxy.listen_port, PROXY_READINESS_RETRIES
            ),
        )
        .await
    }

    async fn proxy_readiness_failure<T>(
        &self,
        container_name: &str,
        reason: &str,
    ) -> anyhow::Result<T> {
        let state = docker_bounded_output(
            ["inspect", "--format", "{{json .State}}", container_name],
            PROXY_DIAGNOSTIC_LIMIT / 2,
        )
        .await;
        let tail_lines = PROXY_LOG_TAIL_LINES.to_string();
        let logs = docker_bounded_output(
            ["logs", "--tail", tail_lines.as_str(), container_name],
            PROXY_DIAGNOSTIC_LIMIT / 2,
        )
        .await;
        anyhow::bail!(
            "proxy container '{container_name}' readiness failed: {reason}; {}",
            bounded_proxy_diagnostics(&state, &logs)
        )
    }

    async fn container_connected_to_network(
        &self,
        container_name: &str,
        network_name: &str,
    ) -> anyhow::Result<bool> {
        let mut inspect = tokio::process::Command::new("docker");
        inspect.kill_on_drop(true);
        inspect
            .arg("inspect")
            .arg("--format")
            .arg(format!(
                "{{{{index .NetworkSettings.Networks \"{network_name}\"}}}}"
            ))
            .arg(container_name);
        let output = inspect.output().await?;
        Ok(output.status.success()
            && String::from_utf8_lossy(&output.stdout).trim() != "<no value>")
    }

    async fn container_network_ip(
        &self,
        container_name: &str,
        network_name: &str,
    ) -> anyhow::Result<Option<String>> {
        let mut inspect = tokio::process::Command::new("docker");
        inspect.kill_on_drop(true);
        inspect
            .arg("inspect")
            .arg("--format")
            .arg(format!(
                "{{{{(index .NetworkSettings.Networks \"{network_name}\").IPAddress}}}}"
            ))
            .arg(container_name);
        let output = inspect.output().await?;
        if !output.status.success() {
            return Ok(None);
        }

        let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if ip.is_empty() || ip == "<no value>" {
            Ok(None)
        } else {
            Ok(Some(ip))
        }
    }

    fn network_name(&self, suffix: &str) -> String {
        format!("agent-hub-{}-egress-{suffix}", self.worker_id)
    }

    fn container_name(&self, tier: LocalAgentEgressTier) -> String {
        format!(
            "agent-hub-{}-egress-proxy-{}",
            self.worker_id,
            tier.as_str()
        )
    }

    fn write_state_file(
        &self,
        tier: LocalAgentEgressTier,
        fingerprint: &str,
        hosts: &[String],
    ) -> anyhow::Result<()> {
        let path = self
            .egress_dir
            .join(format!("state-{}.json", tier.as_str()));
        let payload = serde_json::json!({
            "worker_id": self.worker_id,
            "tier": tier.as_str(),
            "fingerprint": fingerprint,
            "hosts": hosts,
            "proxy_image": self.config.egress.proxy.image,
            "proxy_runtime_uid": self.config.egress.proxy.runtime_uid,
            "proxy_runtime_gid": self.config.egress.proxy.runtime_gid,
            "proxy_writable_tmpfs_mode": self.config.egress.proxy.writable_tmpfs_mode,
            "proxy_declared_volumes": self.config.egress.proxy.declared_volumes,
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&payload)?)
            .with_context(|| format!("write {}", path.display()))
    }
}

fn ensure_private_directory(path: &Path) -> anyhow::Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            anyhow::bail!("private data path '{}' is not a directory", path.display());
        }
    } else {
        std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    }
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect private data path {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("private data path '{}' is not a directory", path.display());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("set private permissions on {}", path.display()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtomicWriteStep {
    ParentOpened,
    TemporaryCreated,
    DestinationValidated,
    ModeSet,
    ContentsWritten,
    FileSynced,
    Renamed,
    DirectorySynced,
}

const O_RDONLY: i32 = 0;
const O_WRONLY: i32 = 1;
const O_CREAT: i32 = 0o100;
const O_EXCL: i32 = 0o200;
const O_CLOEXEC: i32 = 0o2000000;
const O_DIRECTORY: i32 = 0o200000;
const O_NOFOLLOW: i32 = 0o400000;
const O_PATH: i32 = 0o10000000;

unsafe extern "C" {
    fn openat(dirfd: i32, pathname: *const i8, flags: i32, mode: u32) -> i32;
    fn renameat(olddirfd: i32, oldpath: *const i8, newdirfd: i32, newpath: *const i8) -> i32;
    fn unlinkat(dirfd: i32, pathname: *const i8, flags: i32) -> i32;
}

fn path_component(value: &std::ffi::OsStr, label: &str) -> anyhow::Result<CString> {
    CString::new(value.as_bytes()).with_context(|| format!("{label} contains an embedded NUL"))
}

fn openat_file(dirfd: RawFd, name: &CStr, flags: i32, mode: u32) -> std::io::Result<File> {
    // SAFETY: name is NUL-terminated, dirfd remains open for the call, and a successful fd is
    // transferred exactly once into File.
    let fd = unsafe { openat(dirfd, name.as_ptr(), flags, mode) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn renameat_file(dirfd: RawFd, old: &CStr, new: &CStr) -> std::io::Result<()> {
    // SAFETY: both names are NUL-terminated and dirfd remains open for the call.
    if unsafe { renameat(dirfd, old.as_ptr(), dirfd, new.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn unlinkat_file(dirfd: RawFd, name: &CStr) {
    // SAFETY: name is NUL-terminated and dirfd remains open for the call. Cleanup is best effort.
    unsafe {
        unlinkat(dirfd, name.as_ptr(), 0);
    }
}

fn verify_parent_anchor(
    grandparent: &File,
    parent_name: &CStr,
    expected: &std::fs::Metadata,
) -> anyhow::Result<()> {
    let current = openat_file(
        grandparent.as_raw_fd(),
        parent_name,
        O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC,
        0,
    )
    .context("reopen generated config parent relative to anchored grandparent")?;
    let current = current
        .metadata()
        .context("inspect generated config parent")?;
    if current.dev() != expected.dev() || current.ino() != expected.ino() {
        anyhow::bail!("generated config parent changed during atomic write");
    }
    Ok(())
}

fn validate_destination(dirfd: RawFd, destination: &CStr) -> anyhow::Result<()> {
    match openat_file(dirfd, destination, O_PATH | O_NOFOLLOW | O_CLOEXEC, 0) {
        Ok(file) => {
            if !file.metadata()?.is_file() {
                anyhow::bail!("generated config destination is not a regular file");
            }
            Ok(())
        }
        Err(error) if error.raw_os_error() == Some(2) => Ok(()),
        Err(error) => Err(error).context("lstat generated config destination"),
    }
}

fn write_generated_config(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    write_generated_config_with_hook(path, contents, |_| {})
}

fn write_generated_config_with_hook(
    path: &Path,
    contents: &[u8],
    mut hook: impl FnMut(AtomicWriteStep),
) -> anyhow::Result<()> {
    let parent_path = path
        .parent()
        .context("generated config path has no parent directory")?;
    let grandparent_path = parent_path
        .parent()
        .context("generated config parent has no parent directory")?;
    let parent_name = path_component(
        parent_path
            .file_name()
            .context("generated config parent has no file name")?,
        "generated config parent",
    )?;
    let destination = path_component(
        path.file_name()
            .context("generated config destination has no file name")?,
        "generated config destination",
    )?;
    let temporary = CString::new(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        uuid::Uuid::new_v4()
    ))?;

    let grandparent = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        .open(grandparent_path)
        .with_context(|| {
            format!(
                "open generated config grandparent {} without following symlinks",
                grandparent_path.display()
            )
        })?;
    let parent = openat_file(
        grandparent.as_raw_fd(),
        &parent_name,
        O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC,
        0,
    )
    .with_context(|| {
        format!(
            "open generated config parent {} without following symlinks",
            parent_path.display()
        )
    })?;
    let parent_metadata = parent.metadata()?;

    let verify = |step, hook: &mut dyn FnMut(AtomicWriteStep)| -> anyhow::Result<()> {
        hook(step);
        verify_parent_anchor(&grandparent, &parent_name, &parent_metadata)?;
        validate_destination(parent.as_raw_fd(), &destination)
    };
    verify(AtomicWriteStep::ParentOpened, &mut hook)?;
    parent
        .set_permissions(std::fs::Permissions::from_mode(0o700))
        .context("set private generated config parent permissions")?;

    let result = (|| -> anyhow::Result<()> {
        let mut file = openat_file(
            parent.as_raw_fd(),
            &temporary,
            O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC,
            0o600,
        )
        .context("create temporary generated config relative to parent descriptor")?;
        verify(AtomicWriteStep::TemporaryCreated, &mut hook)?;
        validate_destination(parent.as_raw_fd(), &destination)?;
        verify(AtomicWriteStep::DestinationValidated, &mut hook)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .context("set generated config mode")?;
        verify(AtomicWriteStep::ModeSet, &mut hook)?;
        file.write_all(contents).context("write generated config")?;
        file.flush().context("flush generated config")?;
        verify(AtomicWriteStep::ContentsWritten, &mut hook)?;
        file.sync_all().context("sync generated config")?;
        verify(AtomicWriteStep::FileSynced, &mut hook)?;
        renameat_file(parent.as_raw_fd(), &temporary, &destination)
            .context("atomically rename generated config relative to parent descriptor")?;
        verify(AtomicWriteStep::Renamed, &mut hook)?;
        parent
            .sync_all()
            .context("sync generated config directory")?;
        verify(AtomicWriteStep::DirectorySynced, &mut hook)?;
        Ok(())
    })();
    if result.is_err() {
        unlinkat_file(parent.as_raw_fd(), &temporary);
    }
    result
}

fn proxy_tmpfs(proxy: &ProxyConfig) -> Vec<(String, String)> {
    let ownership = format!(
        "uid={},gid={},mode={}",
        proxy.runtime_uid, proxy.runtime_gid, proxy.writable_tmpfs_mode
    );
    [
        ("/tmp", "size=256m"),
        ("/run", "size=64m"),
        ("/var/spool/squid", "size=1g"),
        ("/var/cache/squid", "size=1g"),
    ]
    .into_iter()
    .map(|(path, size)| (path.to_string(), format!("rw,{size},{ownership}")))
    .collect()
}

fn proxy_container_run_args(
    proxy: &ProxyConfig,
    container_name: &str,
    internal_network: &str,
    config_path: &Path,
    fingerprint: &str,
    tier: LocalAgentEgressTier,
    worker_id: &str,
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--restart".to_string(),
        "unless-stopped".to_string(),
        "--cap-drop=ALL".to_string(),
        "--read-only".to_string(),
        "--user".to_string(),
        format!("{}:{}", proxy.runtime_uid, proxy.runtime_gid),
    ];
    for (path, options) in proxy_tmpfs(proxy) {
        args.push("--tmpfs".to_string());
        args.push(format!("{path}:{options}"));
    }
    args.extend([
        "--name".to_string(),
        container_name.to_string(),
        "--network".to_string(),
        internal_network.to_string(),
        "--label".to_string(),
        format!("{EGRESS_MANAGED_LABEL}=true"),
        "--label".to_string(),
        format!("{EGRESS_WORKER_LABEL}={worker_id}"),
        "--label".to_string(),
        format!("{EGRESS_KIND_LABEL}=proxy"),
        "--label".to_string(),
        format!("{EGRESS_TIER_LABEL}={}", tier.as_str()),
        "--label".to_string(),
        format!("{EGRESS_FINGERPRINT_LABEL}={fingerprint}"),
        "-v".to_string(),
        format!("{}:/etc/squid/squid.conf:ro", config_path.to_string_lossy()),
        proxy.image.clone(),
    ]);
    args
}

fn proxy_remove_args(container_name: &str) -> Vec<String> {
    vec![
        "rm".to_string(),
        "-f".to_string(),
        "-v".to_string(),
        container_name.to_string(),
    ]
}

fn summarize_json(value: Option<&serde_json::Value>) -> String {
    value.map_or_else(|| "<missing>".to_string(), serde_json::Value::to_string)
}

fn check_json_string(
    report: &mut ProxyCompatibilityReport,
    field: &str,
    actual: Option<&serde_json::Value>,
    expected: &str,
) {
    if actual.and_then(serde_json::Value::as_str) != Some(expected) {
        report.push(field, expected, summarize_json(actual));
    }
}

fn check_json_bool(
    report: &mut ProxyCompatibilityReport,
    field: &str,
    actual: Option<&serde_json::Value>,
    expected: bool,
) {
    if actual.and_then(serde_json::Value::as_bool) != Some(expected) {
        report.push(field, expected, summarize_json(actual));
    }
}

fn append_mount_topology_mismatches(
    report: &mut ProxyCompatibilityReport,
    json: &serde_json::Value,
    config_path: &Path,
    declared_volumes: &[String],
) {
    let mounts = match json.get("Mounts").and_then(serde_json::Value::as_array) {
        Some(mounts) => mounts.as_slice(),
        None => {
            report.push("mounts", "array", summarize_json(json.get("Mounts")));
            &[]
        }
    };
    let volume_or_bind = mounts
        .iter()
        .filter(|mount| {
            matches!(
                mount.get("Type").and_then(serde_json::Value::as_str),
                Some("volume" | "bind")
            )
        })
        .collect::<Vec<_>>();
    let volumes = volume_or_bind
        .iter()
        .filter(|mount| mount.get("Type").and_then(serde_json::Value::as_str) == Some("volume"))
        .copied()
        .collect::<Vec<_>>();
    let binds = volume_or_bind
        .iter()
        .filter(|mount| mount.get("Type").and_then(serde_json::Value::as_str) == Some("bind"))
        .copied()
        .collect::<Vec<_>>();
    if volumes.len() != 1 {
        report.push("mount.volume.count", 1, volumes.len());
    }
    if binds.len() != 1 {
        report.push("mount.bind.count", 1, binds.len());
    }
    if volume_or_bind.len() != 2 {
        report.push("mount.volume_or_bind.count", 2, volume_or_bind.len());
    }
    let unexpected_volumes = volumes
        .iter()
        .filter_map(|mount| {
            let destination = mount
                .get("Destination")
                .and_then(serde_json::Value::as_str)?;
            (destination != "/var/log/squid").then_some(destination)
        })
        .collect::<Vec<_>>();
    if !unexpected_volumes.is_empty() {
        report.push(
            "mount.volume.unexpected",
            "[]",
            format!("{unexpected_volumes:?}"),
        );
    }
    let runtime_spool_mounts = volume_or_bind
        .iter()
        .filter(|mount| {
            mount.get("Destination").and_then(serde_json::Value::as_str) == Some("/var/spool/squid")
        })
        .map(|mount| summarize_json(Some(mount)))
        .collect::<Vec<_>>();
    if !runtime_spool_mounts.is_empty() {
        report.push(
            "mount.spool.runtime",
            "shadowed only by owned tmpfs",
            format!("{runtime_spool_mounts:?}"),
        );
    }
    let unexpected_binds = binds
        .iter()
        .filter_map(|mount| {
            let destination = mount
                .get("Destination")
                .and_then(serde_json::Value::as_str)?;
            (destination != "/etc/squid/squid.conf").then_some(destination)
        })
        .collect::<Vec<_>>();
    if !unexpected_binds.is_empty() {
        report.push(
            "mount.bind.unexpected",
            "[]",
            format!("{unexpected_binds:?}"),
        );
    }

    let log_mount = mounts
        .iter()
        .find(|mount| {
            mount.get("Destination").and_then(serde_json::Value::as_str) == Some("/var/log/squid")
        })
        .or_else(|| volumes.first().copied());
    match log_mount {
        Some(mount) => {
            check_json_string(report, "mount.log.type", mount.get("Type"), "volume");
            check_json_string(
                report,
                "mount.log.destination",
                mount.get("Destination"),
                "/var/log/squid",
            );
            check_json_bool(report, "mount.log.rw", mount.get("RW"), true);
            let name = mount
                .get("Name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty());
            if name.is_none() {
                report.push(
                    "mount.log.name",
                    "non-empty anonymous volume name",
                    summarize_json(mount.get("Name")),
                );
            }
            let source = mount
                .get("Source")
                .and_then(serde_json::Value::as_str)
                .filter(|source| !source.is_empty());
            if !name
                .zip(source)
                .is_some_and(|(name, source)| source.ends_with(&format!("/volumes/{name}/_data")))
            {
                report.push(
                    "mount.log.source",
                    ".../volumes/<Name>/_data",
                    summarize_json(mount.get("Source")),
                );
            }
        }
        None => report.push(
            "mount.log",
            "anonymous writable /var/log/squid volume",
            "<missing>",
        ),
    }

    let config_mount = mounts
        .iter()
        .find(|mount| {
            mount.get("Destination").and_then(serde_json::Value::as_str)
                == Some("/etc/squid/squid.conf")
        })
        .or_else(|| binds.first().copied());
    match config_mount {
        Some(mount) => {
            check_json_string(report, "mount.config.type", mount.get("Type"), "bind");
            check_json_string(
                report,
                "mount.config.source",
                mount.get("Source"),
                config_path.to_string_lossy().as_ref(),
            );
            check_json_string(
                report,
                "mount.config.destination",
                mount.get("Destination"),
                "/etc/squid/squid.conf",
            );
            check_json_bool(report, "mount.config.rw", mount.get("RW"), false);
        }
        None => report.push("mount.config", "read-only Squid config bind", "<missing>"),
    }

    let image_volumes = json.pointer("/Config/Volumes");
    let mut actual_declared_volumes = image_volumes
        .and_then(serde_json::Value::as_object)
        .map(|volumes| volumes.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    actual_declared_volumes.sort();
    let mut expected_declared_volumes = declared_volumes.to_vec();
    expected_declared_volumes.sort();
    if actual_declared_volumes != expected_declared_volumes {
        report.push(
            "image.declared_volumes",
            format!("{expected_declared_volumes:?}"),
            format!("{actual_declared_volumes:?}"),
        );
    }
    let explicit_mounts = json.pointer("/HostConfig/Mounts");
    if !explicit_mounts
        .is_none_or(|mounts| mounts.is_null() || mounts.as_array().is_some_and(Vec::is_empty))
    {
        report.push(
            "mount.unexpected_explicit",
            "null or []",
            summarize_json(explicit_mounts),
        );
    }
}

fn append_network_attachment_mismatches(
    report: &mut ProxyCompatibilityReport,
    json: &serde_json::Value,
    internal_network: &str,
    external_network: &str,
) {
    let networks = json
        .pointer("/NetworkSettings/Networks")
        .and_then(serde_json::Value::as_object);
    for expected in [internal_network, external_network] {
        if !networks.is_some_and(|networks| networks.contains_key(expected)) {
            report.push(
                &format!("network.attachment.{expected}"),
                "present",
                "missing",
            );
        }
    }
    let unexpected = networks
        .map(|networks| {
            networks
                .keys()
                .filter(|network| *network != internal_network && *network != external_network)
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !unexpected.is_empty() {
        report.push(
            "network.attachments.unexpected",
            "[]",
            format!("{unexpected:?}"),
        );
    }
    if networks.is_none() {
        report.push("network.attachments", "object", "<missing>");
    }
}

async fn wait_for_compatible_proxy_winner<Inspect, InspectFuture, Compatible>(
    mut inspect: Inspect,
    mut compatible: Compatible,
    retries: usize,
    delay: Duration,
) -> bool
where
    Inspect: FnMut() -> InspectFuture,
    InspectFuture: std::future::Future<Output = anyhow::Result<Option<serde_json::Value>>>,
    Compatible: FnMut(&serde_json::Value) -> bool,
{
    for attempt in 1..=retries {
        if let Ok(Some(json)) = inspect().await
            && compatible(&json)
        {
            return true;
        }
        if attempt < retries && !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }
    false
}

fn require_compatible_create_winner(
    compatible_winner: bool,
    original_error: anyhow::Error,
) -> anyhow::Result<()> {
    if compatible_winner {
        Ok(())
    } else {
        Err(original_error)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CompatibleProxyAction {
    EnsureRunning,
    EnsureConnected(String),
    WaitReady(String),
}

fn compatible_proxy_actions(
    internal_network: &str,
    external_network: &str,
) -> Vec<CompatibleProxyAction> {
    vec![
        CompatibleProxyAction::EnsureRunning,
        CompatibleProxyAction::EnsureConnected(internal_network.to_string()),
        CompatibleProxyAction::EnsureConnected(external_network.to_string()),
        CompatibleProxyAction::WaitReady(internal_network.to_string()),
    ]
}

fn proxy_fingerprint(config: &[u8], proxy: &ProxyConfig) -> String {
    let runtime_contract = serde_json::json!({
        "image": proxy.image,
        "listen_port": proxy.listen_port,
        "runtime_uid": proxy.runtime_uid,
        "runtime_gid": proxy.runtime_gid,
        "writable_tmpfs_mode": proxy.writable_tmpfs_mode,
        "declared_volumes": proxy.declared_volumes,
    });
    let mut bytes = config.to_vec();
    bytes.push(0);
    bytes.extend(serde_json::to_vec(&runtime_contract).expect("proxy contract serializes"));
    format!("{:016x}", fnv1a64(&bytes))
}

async fn docker_bounded_output<const N: usize>(args: [&str; N], limit: usize) -> String {
    let mut command = tokio::process::Command::new("docker");
    command
        .kill_on_drop(true)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return bound_text(&format!("docker diagnostic unavailable: {error}"), limit);
        }
    };
    let stdout = child.stdout.take().expect("piped stdout is available");
    let stderr = child.stderr.take().expect("piped stderr is available");
    let stdout_limit = (limit / 2).saturating_add(1) as u64;
    let stderr_limit = limit.saturating_sub(limit / 2).saturating_add(1) as u64;
    let stdout_read = async move {
        let mut bytes = Vec::new();
        stdout
            .take(stdout_limit)
            .read_to_end(&mut bytes)
            .await
            .map(|_| bytes)
    };
    let stderr_read = async move {
        let mut bytes = Vec::new();
        stderr
            .take(stderr_limit)
            .read_to_end(&mut bytes)
            .await
            .map(|_| bytes)
    };
    let (stdout, stderr) = tokio::join!(stdout_read, stderr_read);
    let status = child.wait().await;
    let mut combined = String::new();
    match stdout {
        Ok(stdout) => combined.push_str(&String::from_utf8_lossy(&stdout)),
        Err(error) => combined.push_str(&format!("stdout unavailable: {error}")),
    }
    match stderr {
        Ok(stderr) => combined.push_str(&String::from_utf8_lossy(&stderr)),
        Err(error) => combined.push_str(&format!(" stderr unavailable: {error}")),
    }
    if let Err(error) = status {
        combined.push_str(&format!(" wait unavailable: {error}"));
    }
    bound_text(&combined, limit)
}

fn bounded_proxy_diagnostics(state: &str, logs: &str) -> String {
    let redacted = redact_proxy_diagnostic(&format!("state={} logs={}", state, logs));
    bound_text(&redacted, PROXY_DIAGNOSTIC_LIMIT)
}

fn bound_text(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn redact_proxy_diagnostic(value: &str) -> String {
    let mut output = value
        .lines()
        .map(|line| {
            if line.to_ascii_lowercase().contains("authorization:") {
                "Authorization: [REDACTED]".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    for marker in [
        "token=",
        "access_token=",
        "api_key=",
        "apikey=",
        "password=",
        "secret=",
        "bearer ",
    ] {
        let mut search_from = 0;
        loop {
            let lowercase = output.to_ascii_lowercase();
            let Some(relative_start) = lowercase[search_from..].find(marker) else {
                break;
            };
            let value_start = search_from + relative_start + marker.len();
            let value_end = output[value_start..]
                .find(|character: char| {
                    character.is_whitespace() || matches!(character, '&' | '"' | '\'')
                })
                .map_or(output.len(), |offset| value_start + offset);
            output.replace_range(value_start..value_end, "[REDACTED]");
            search_from = value_start + "[REDACTED]".len();
        }
    }
    let mut search_from = 0;
    while let Some(relative_scheme) = output[search_from..].find("://") {
        let authority_start = search_from + relative_scheme + 3;
        let authority_end = output[authority_start..]
            .find(['/', ' ', '\n'])
            .map_or(output.len(), |offset| authority_start + offset);
        if let Some(at) = output[authority_start..authority_end].find('@') {
            output.replace_range(authority_start..authority_start + at, "[REDACTED]");
        }
        search_from = output[authority_start..]
            .find(['/', ' ', '\n'])
            .map_or(output.len(), |offset| authority_start + offset);
    }
    let mut search_from = 0;
    while let Some(relative_scheme) = output[search_from..].find("://") {
        let url_start = search_from + relative_scheme + 3;
        let url_end = output[url_start..]
            .find(char::is_whitespace)
            .map_or(output.len(), |offset| url_start + offset);
        if let Some(query) = output[url_start..url_end].find('?') {
            let query_start = url_start + query + 1;
            output.replace_range(query_start..url_end, "[REDACTED]");
            search_from = query_start + "[REDACTED]".len();
        } else {
            search_from = url_end;
        }
    }
    output
}

async fn docker_network_inspect(name: &str) -> anyhow::Result<Option<serde_json::Value>> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.kill_on_drop(true);
    cmd.arg("network").arg("inspect").arg(name);
    let output = cmd.output().await?;
    if !output.status.success() {
        return Ok(None);
    }
    let parsed: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).context("parse docker network inspect output")?;
    Ok(parsed.into_iter().next())
}

async fn docker_container_inspect(name: &str) -> anyhow::Result<Option<serde_json::Value>> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.kill_on_drop(true);
    cmd.arg("inspect").arg(name);
    let output = cmd.output().await?;
    if !output.status.success() {
        return Ok(None);
    }

    let parsed: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).context("parse docker inspect output")?;
    Ok(parsed.into_iter().next())
}

async fn run_status(cmd: &mut tokio::process::Command, context: &str) -> anyhow::Result<()> {
    let status = cmd.status().await?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("{context}: {status}")
    }
}

async fn container_running(container_name: &str) -> anyhow::Result<bool> {
    let mut inspect = tokio::process::Command::new("docker");
    inspect.kill_on_drop(true);
    inspect
        .arg("inspect")
        .arg("--format")
        .arg("{{.State.Running}}")
        .arg(container_name);
    let output = inspect.output().await?;
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
}

fn sanitize_name_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn squid_acl_host_groups(hosts: &[String]) -> (Vec<String>, Vec<String>) {
    let mut apex_hosts: Vec<String> = hosts
        .iter()
        .filter(|host| !host.starts_with("*."))
        .cloned()
        .collect();
    apex_hosts.sort();
    apex_hosts.dedup();

    let mut subdomain_hosts = Vec::new();
    for host in hosts.iter().filter_map(|host| host.strip_prefix("*.")) {
        subdomain_hosts.push(format!(".{host}"));
    }
    subdomain_hosts.sort();
    subdomain_hosts.dedup();

    (apex_hosts, subdomain_hosts)
}

fn validate_literal_model_hosts(hosts: &[String]) -> anyhow::Result<()> {
    if hosts.iter().any(|host| host.contains('*')) {
        anyhow::bail!(
            "llm-only model hosts must be literal normalized hosts; wildcards are forbidden"
        );
    }
    Ok(())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn llm_provider_from_env(env: &HashMap<String, String>) -> Option<String> {
    env.get("AGENT_LLM_PROVIDER")
        .or_else(|| env.get("LLM_PROVIDER"))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{
        AtomicWriteStep, CompatibleProxyAction, EGRESS_FINGERPRINT_LABEL, EGRESS_KIND_LABEL,
        EGRESS_MANAGED_LABEL, EGRESS_TIER_LABEL, EGRESS_WORKER_LABEL, EgressManager,
        LocalAgentEgressTier, RestrictedEgressRuntime, bounded_proxy_diagnostics,
        compatible_proxy_actions, proxy_container_run_args, proxy_fingerprint, proxy_remove_args,
        proxy_tmpfs, require_compatible_create_winner, squid_acl_host_groups,
        validate_literal_model_hosts, wait_for_compatible_proxy_winner, write_generated_config,
        write_generated_config_with_hook,
    };
    use crate::egress_config::{ConfigTreeSnapshot, ConfiguredNetworkPolicy, LocalAgentConfig};
    use std::collections::HashMap;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use std::time::Duration;

    unsafe extern "C" {
        fn umask(mask: u32) -> u32;
    }

    struct UmaskGuard(u32);

    impl UmaskGuard {
        fn restrictive() -> Self {
            Self(unsafe { umask(0o077) })
        }
    }

    impl Drop for UmaskGuard {
        fn drop(&mut self) {
            unsafe {
                umask(self.0);
            }
        }
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("agent-hub-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct ContainerRestartState {
        running: bool,
        pid: u32,
        restart_count: u64,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ProcessTreeEntry {
        pid: u32,
        ppid: u32,
        exe: String,
        comm: String,
        args: String,
    }

    fn container_restart_state_args(container_name: &str) -> Vec<String> {
        vec![
            "inspect".to_string(),
            "--format".to_string(),
            "{{.State.Running}}\t{{.State.Pid}}\t{{.RestartCount}}".to_string(),
            container_name.to_string(),
        ]
    }

    fn squid_image_contract_args(image: &str) -> Vec<String> {
        vec![
            "image".to_string(),
            "inspect".to_string(),
            "--format".to_string(),
            "{{json .Config}}".to_string(),
            image.to_string(),
        ]
    }

    fn squid_process_tree_args(container_name: &str) -> Vec<String> {
        vec![
            "exec".to_string(),
            container_name.to_string(),
            "sh".to_string(),
            "-ceu".to_string(),
            "for proc in /proc/[0-9]*; do pid=${proc##*/}; read -r comm < \"$proc/comm\"; ppid=; while read -r key value rest; do case \"$key\" in PPid:) ppid=$value;; esac; done < \"$proc/status\"; exe=$(readlink \"$proc/exe\" 2>/dev/null || :); printf '%s\\t%s\\t%s\\t%s\\t' \"$pid\" \"$ppid\" \"$exe\" \"$comm\"; tr '\\0' ' ' < \"$proc/cmdline\"; printf '\\n'; done".to_string(),
        ]
    }

    fn squid_unexpected_crash_args(container_name: &str, squid_pid: u32) -> Vec<String> {
        vec![
            "exec".to_string(),
            container_name.to_string(),
            "sh".to_string(),
            "-ceu".to_string(),
            // The pinned entrypoint remains PID 1 instead of execing Squid. Docker API stop/kill
            // suppresses unless-stopped, so the verified direct Squid child is signaled in-container.
            "pid=$1; test \"$pid\" -ne 1; test \"$(readlink \"/proc/$pid/exe\")\" = /usr/sbin/squid; ppid=; while read -r key value rest; do case \"$key\" in PPid:) ppid=$value;; esac; done < \"/proc/$pid/status\"; test \"$ppid\" = 1; kill -TERM \"$pid\"".to_string(),
            "sh".to_string(),
            squid_pid.to_string(),
        ]
    }

    fn container_die_events_args(container_name: &str) -> Vec<String> {
        vec![
            "events".to_string(),
            "--filter".to_string(),
            format!("container={container_name}"),
            "--filter".to_string(),
            "event=die".to_string(),
            "--format".to_string(),
            "{{json .}}".to_string(),
        ]
    }

    fn parse_die_event_exit_code(value: &str) -> Option<i32> {
        let event: serde_json::Value = serde_json::from_str(value).ok()?;
        let action = event
            .get("Action")
            .or_else(|| event.get("status"))
            .and_then(serde_json::Value::as_str)?;
        if action != "die" {
            return None;
        }
        event
            .pointer("/Actor/Attributes/exitCode")
            .and_then(serde_json::Value::as_str)
            .and_then(|code| code.parse().ok())
    }

    struct ContainerDieWatcher {
        child: tokio::process::Child,
        stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    }

    impl ContainerDieWatcher {
        async fn start(container_name: &str) -> Result<Self, String> {
            let mut command = tokio::process::Command::new("docker");
            command
                .kill_on_drop(true)
                .args(container_die_events_args(container_name))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());
            let mut child = command
                .spawn()
                .map_err(|error| format!("start Docker die-event watcher: {error}"))?;
            let stdout = tokio::io::BufReader::new(
                child
                    .stdout
                    .take()
                    .ok_or_else(|| "Docker die-event watcher has no stdout".to_string())?,
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
            if child
                .try_wait()
                .map_err(|error| format!("inspect Docker die-event watcher: {error}"))?
                .is_some()
            {
                return Err("Docker die-event watcher exited before crash injection".to_string());
            }
            Ok(Self { child, stdout })
        }

        async fn require_exit_zero(mut self) -> Result<(), String> {
            let mut line = String::new();
            let read = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::io::AsyncBufReadExt::read_line(&mut self.stdout, &mut line),
            )
            .await
            .map_err(|_| "timed out waiting for Docker die event".to_string())?
            .map_err(|error| format!("read Docker die event: {error}"))?;
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
            if read == 0 {
                return Err("Docker die-event stream ended before the container exit".to_string());
            }
            match parse_die_event_exit_code(&line) {
                Some(0) => Ok(()),
                Some(code) => Err(format!("old container exited with nonzero status {code}")),
                None => Err("Docker die event did not contain a reliable exit code".to_string()),
            }
        }
    }

    fn parse_process_tree(value: &str) -> Result<Vec<ProcessTreeEntry>, String> {
        let mut entries = Vec::new();
        let mut pids = std::collections::HashSet::new();
        for line in value.lines().filter(|line| !line.is_empty()) {
            let fields = line.splitn(5, '\t').collect::<Vec<_>>();
            if fields.len() != 5 {
                return Err(format!(
                    "malformed process-tree line: {}",
                    super::bound_text(line, 128)
                ));
            }
            let pid = fields[0]
                .parse::<u32>()
                .map_err(|_| "invalid process PID".to_string())?;
            let ppid = fields[1]
                .parse::<u32>()
                .map_err(|_| "invalid process PPID".to_string())?;
            if pid == 0 || !pids.insert(pid) {
                return Err("process tree contains a zero or duplicate PID".to_string());
            }
            entries.push(ProcessTreeEntry {
                pid,
                ppid,
                exe: fields[2].to_string(),
                comm: fields[3].to_string(),
                args: fields[4].trim_end().to_string(),
            });
        }
        if entries.is_empty() {
            return Err("process tree is empty".to_string());
        }
        Ok(entries)
    }

    fn validate_pinned_squid_process_tree(entries: &[ProcessTreeEntry]) -> Result<u32, String> {
        let pid_one = entries
            .iter()
            .find(|entry| entry.pid == 1)
            .ok_or_else(|| "process tree has no PID 1".to_string())?;
        if pid_one.exe != "/usr/bin/bash"
            || !pid_one
                .args
                .contains("/usr/local/bin/entrypoint.sh -f /etc/squid/squid.conf -NYC")
        {
            return Err("PID 1 does not match the pinned non-exec Bash entrypoint".to_string());
        }

        let squid = entries
            .iter()
            .filter(|entry| {
                entry.exe == "/usr/sbin/squid"
                    || entry.comm.starts_with("squid")
                    || entry.args.split_whitespace().next() == Some("/usr/sbin/squid")
            })
            .collect::<Vec<_>>();
        if squid.len() != 1 || squid[0].exe != "/usr/sbin/squid" || squid[0].ppid != 1 {
            return Err(
                "process tree must contain exactly one direct Squid child of PID 1".to_string(),
            );
        }

        let expected_logs = ["access", "error", "store", "cache"]
            .map(|name| format!("/var/log/squid/{name}.log"))
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let tail_children =
            entries
                .iter()
                .filter(|entry| {
                    entry.ppid == 1
                        && (entry.exe == "/usr/bin/tail"
                            || entry.comm == "tail"
                            || entry.args.split_whitespace().next().is_some_and(|program| {
                                program == "tail" || program == "/usr/bin/tail"
                            }))
                })
                .collect::<Vec<_>>();
        let actual_logs = tail_children
            .iter()
            .filter_map(|entry| {
                let arguments = entry.args.split_whitespace().collect::<Vec<_>>();
                (arguments.len() == 3 && arguments[1] == "-F").then(|| arguments[2].to_string())
            })
            .collect::<std::collections::BTreeSet<_>>();
        if tail_children.len() != 4
            || tail_children
                .iter()
                .any(|entry| entry.exe != "/usr/bin/tail")
            || actual_logs != expected_logs
        {
            return Err(
                "process tree must contain exactly the four pinned Squid log tail children"
                    .to_string(),
            );
        }
        Ok(squid[0].pid)
    }

    fn parse_container_restart_state(value: &str) -> Option<ContainerRestartState> {
        let mut fields = value.trim().split('\t');
        let state = ContainerRestartState {
            running: fields.next()?.parse().ok()?,
            pid: fields.next()?.parse().ok()?,
            restart_count: fields.next()?.parse().ok()?,
        };
        fields.next().is_none().then_some(state)
    }

    fn container_restarted_after_crash(
        initial: ContainerRestartState,
        current: ContainerRestartState,
    ) -> bool {
        current.running
            && current.restart_count > initial.restart_count
            && current.pid != 0
            && current.pid != initial.pid
    }

    async fn docker_container_restart_state(container_name: &str) -> Option<ContainerRestartState> {
        let mut command = tokio::process::Command::new("docker");
        command
            .kill_on_drop(true)
            .args(container_restart_state_args(container_name));
        let output = tokio::time::timeout(Duration::from_secs(2), command.output())
            .await
            .ok()?
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout))
            .and_then(|value| parse_container_restart_state(&value))
    }

    fn complete_proxy_inspect(manager: &EgressManager) -> serde_json::Value {
        let tmpfs = proxy_tmpfs(&manager.config().egress.proxy)
            .into_iter()
            .map(|(path, options)| (path, serde_json::Value::String(options)))
            .collect::<serde_json::Map<_, _>>();
        serde_json::json!({
            "Config": {
                "Image": manager.config().egress.proxy.image,
                "User": "13:13",
                "Volumes": {"/var/log/squid": {}, "/var/spool/squid": {}},
                "Labels": {
                    (EGRESS_MANAGED_LABEL): "true",
                    (EGRESS_WORKER_LABEL): "worker-1",
                    (EGRESS_KIND_LABEL): "proxy",
                    (EGRESS_TIER_LABEL): "llm",
                    (EGRESS_FINGERPRINT_LABEL): "fingerprint"
                }
            },
            "HostConfig": {
                "ReadonlyRootfs": true,
                "RestartPolicy": {"Name": "unless-stopped"},
                "NetworkMode": "internal",
                "Tmpfs": tmpfs,
                "CapDrop": ["ALL"],
                "CapAdd": null,
                "Binds": ["/private/squid.conf:/etc/squid/squid.conf:ro"],
                "Mounts": []
            },
            "Mounts": [
                {
                    "Type": "volume",
                    "Name": "anonymous-volume",
                    "Source": "/var/lib/docker/volumes/anonymous-volume/_data",
                    "Destination": "/var/log/squid",
                    "RW": true
                },
                {
                    "Type": "bind",
                    "Source": "/private/squid.conf",
                    "Destination": "/etc/squid/squid.conf",
                    "RW": false
                }
            ],
            "NetworkSettings": {"Networks": {"internal": {}, "external": {}}}
        })
    }

    fn proxy_report(
        manager: &EgressManager,
        inspect: &serde_json::Value,
    ) -> super::ProxyCompatibilityReport {
        manager.proxy_compatibility_report(
            inspect,
            LocalAgentEgressTier::Llm,
            std::path::Path::new("/private/squid.conf"),
            "fingerprint",
            "internal",
            "external",
        )
    }

    fn assert_mismatch_field(report: &super::ProxyCompatibilityReport, field: &str) {
        assert!(
            report
                .mismatches
                .iter()
                .any(|mismatch| mismatch.field == field),
            "missing {field} in {}",
            report.diagnostic()
        );
    }

    fn integration_cleanup_commands(
        container: &str,
        internal_network: &str,
        external_network: &str,
    ) -> Vec<Vec<String>> {
        vec![
            proxy_remove_args(container),
            vec![
                "network".to_string(),
                "rm".to_string(),
                internal_network.to_string(),
            ],
            vec![
                "network".to_string(),
                "rm".to_string(),
                external_network.to_string(),
            ],
        ]
    }

    struct IntegrationCleanup {
        worker_id: String,
        container: String,
        internal_network: String,
        external_network: String,
        data_root: std::path::PathBuf,
        log_volume: Option<String>,
        active: bool,
    }

    impl IntegrationCleanup {
        fn cleanup(&mut self) -> anyhow::Result<()> {
            if !self.active {
                return Ok(());
            }
            let mut errors = Vec::new();
            let label_format = format!(
                "{{{{index .Config.Labels \"{EGRESS_MANAGED_LABEL}\"}}}} {{{{index .Config.Labels \"{EGRESS_WORKER_LABEL}\"}}}}"
            );
            match std::process::Command::new("docker")
                .args(["inspect", "--format", &label_format, &self.container])
                .output()
            {
                Ok(output) if output.status.success() => {
                    if String::from_utf8_lossy(&output.stdout).trim()
                        == format!("true {}", self.worker_id)
                    {
                        match std::process::Command::new("docker")
                            .args([
                                "inspect",
                                "--format",
                                "{{range .Mounts}}{{if eq .Destination \"/var/log/squid\"}}{{.Name}}{{end}}{{end}}",
                                &self.container,
                            ])
                            .output()
                        {
                            Ok(volume) if volume.status.success() => {
                                let name = String::from_utf8_lossy(&volume.stdout).trim().to_string();
                                if name.is_empty() {
                                    errors.push(
                                        "integration proxy has no anonymous log volume identity"
                                            .to_string(),
                                    );
                                } else {
                                    self.log_volume = Some(name);
                                }
                            }
                            Ok(volume) => errors.push(format!(
                                "inspect integration log volume failed: {}",
                                volume.status
                            )),
                            Err(error) => errors.push(format!(
                                "launch integration log volume inspection failed: {error}"
                            )),
                        }
                        match std::process::Command::new("docker")
                            .args(proxy_remove_args(&self.container))
                            .status()
                        {
                            Ok(status) if status.success() => {}
                            Ok(status) => errors.push(format!(
                                "remove integration proxy with volumes failed: {status}"
                            )),
                            Err(error) => errors
                                .push(format!("launch integration proxy cleanup failed: {error}")),
                        }
                    } else {
                        errors.push(
                            "refusing to clean up proxy outside integration ownership scope"
                                .to_string(),
                        );
                    }
                }
                Ok(_) => {}
                Err(error) => errors.push(format!("inspect integration proxy failed: {error}")),
            }

            let network_label_format = format!(
                "{{{{index .Labels \"{EGRESS_MANAGED_LABEL}\"}}}} {{{{index .Labels \"{EGRESS_WORKER_LABEL}\"}}}}"
            );
            for network in [&self.internal_network, &self.external_network] {
                match std::process::Command::new("docker")
                    .args([
                        "network",
                        "inspect",
                        "--format",
                        &network_label_format,
                        network,
                    ])
                    .output()
                {
                    Ok(output) if output.status.success() => {
                        if String::from_utf8_lossy(&output.stdout).trim()
                            == format!("true {}", self.worker_id)
                        {
                            match std::process::Command::new("docker")
                                .args(["network", "rm", network])
                                .status()
                            {
                                Ok(status) if status.success() => {}
                                Ok(status) => errors.push(format!(
                                    "remove integration network {network} failed: {status}"
                                )),
                                Err(error) => errors.push(format!(
                                    "launch integration network cleanup for {network} failed: {error}"
                                )),
                            }
                        } else {
                            errors.push(format!(
                                "refusing to clean up network {network} outside integration ownership scope"
                            ));
                        }
                    }
                    Ok(_) => {}
                    Err(error) => errors.push(format!(
                        "inspect integration network {network} failed: {error}"
                    )),
                }
            }
            if self.data_root.exists()
                && let Err(error) = std::fs::remove_dir_all(&self.data_root)
            {
                errors.push(format!(
                    "remove integration data root {} failed: {error}",
                    self.data_root.display()
                ));
            }
            if let Err(error) = self.assert_absent() {
                errors.push(error.to_string());
            }
            if errors.is_empty() {
                self.active = false;
                Ok(())
            } else {
                anyhow::bail!(errors.join("; "))
            }
        }

        fn assert_absent(&self) -> anyhow::Result<()> {
            let container = std::process::Command::new("docker")
                .args(["inspect", &self.container])
                .status()?;
            anyhow::ensure!(!container.success(), "integration proxy still exists");
            for network in [&self.internal_network, &self.external_network] {
                let status = std::process::Command::new("docker")
                    .args(["network", "inspect", network])
                    .status()?;
                anyhow::ensure!(
                    !status.success(),
                    "integration network {network} still exists"
                );
            }
            if let Some(volume) = &self.log_volume {
                let status = std::process::Command::new("docker")
                    .args(["volume", "inspect", volume])
                    .status()?;
                anyhow::ensure!(
                    !status.success(),
                    "anonymous log volume {volume} still exists"
                );
            }
            anyhow::ensure!(
                !self.data_root.exists(),
                "integration data root {} still exists",
                self.data_root.display()
            );
            Ok(())
        }
    }

    impl Drop for IntegrationCleanup {
        fn drop(&mut self) {
            if let Err(error) = self.cleanup() {
                eprintln!("integration cleanup failed: {error:#}");
            }
        }
    }

    #[test]
    fn generated_config_is_atomic_and_mode_0644_with_restrictive_umask_and_existing_file() {
        const CHILD_MARKER: &str = "AGENT_HUB_CONFIG_UMASK_TEST_CHILD";
        if std::env::var_os(CHILD_MARKER).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "egress::tests::generated_config_is_atomic_and_mode_0644_with_restrictive_umask_and_existing_file",
                    "--nocapture",
                ])
                .env(CHILD_MARKER, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let root = test_dir("atomic-config");
        let path = root.join("squid.conf");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let _umask = UmaskGuard::restrictive();
        write_generated_config(&path, b"deterministic\n").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"deterministic\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_config_rejects_symlink_destination() {
        let root = test_dir("symlink-config");
        let target = root.join("target");
        let path = root.join("squid.conf");
        std::fs::write(&target, b"untouched").unwrap();
        symlink(&target, &path).unwrap();

        assert!(write_generated_config(&path, b"replacement").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_config_rejects_non_file_destination() {
        let root = test_dir("directory-config");
        let path = root.join("squid.conf");
        std::fs::create_dir(&path).unwrap();

        assert!(write_generated_config(&path, b"replacement").is_err());
        assert!(path.is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_config_rejects_symlink_parent_replacement() {
        let root = test_dir("symlink-parent");
        let real_parent = root.join("real");
        let linked_parent = root.join("linked");
        std::fs::create_dir(&real_parent).unwrap();
        symlink(&real_parent, &linked_parent).unwrap();

        assert!(write_generated_config(&linked_parent.join("squid.conf"), b"config").is_err());
        assert!(!real_parent.join("squid.conf").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_config_rejects_parent_replacement_at_every_transaction_step() {
        for replacement_step in [
            AtomicWriteStep::ParentOpened,
            AtomicWriteStep::TemporaryCreated,
            AtomicWriteStep::DestinationValidated,
            AtomicWriteStep::ModeSet,
            AtomicWriteStep::ContentsWritten,
            AtomicWriteStep::FileSynced,
            AtomicWriteStep::Renamed,
            AtomicWriteStep::DirectorySynced,
        ] {
            let root = test_dir("parent-replacement");
            let parent = root.join("parent");
            let displaced = root.join("displaced");
            let attacker = root.join("attacker");
            std::fs::create_dir(&parent).unwrap();
            std::fs::create_dir(&attacker).unwrap();
            let path = parent.join("squid.conf");
            let mut replaced = false;

            let result = write_generated_config_with_hook(&path, b"config", |step| {
                if step == replacement_step && !replaced {
                    std::fs::rename(&parent, &displaced).unwrap();
                    symlink(&attacker, &parent).unwrap();
                    replaced = true;
                }
            });

            assert!(result.is_err(), "{replacement_step:?}");
            assert!(
                !attacker.join("squid.conf").exists(),
                "{replacement_step:?}"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn generated_config_rejects_directory_parent_replacement_at_every_transaction_step() {
        for replacement_step in [
            AtomicWriteStep::ParentOpened,
            AtomicWriteStep::TemporaryCreated,
            AtomicWriteStep::DestinationValidated,
            AtomicWriteStep::ModeSet,
            AtomicWriteStep::ContentsWritten,
            AtomicWriteStep::FileSynced,
            AtomicWriteStep::Renamed,
            AtomicWriteStep::DirectorySynced,
        ] {
            let root = test_dir("directory-parent-replacement");
            let parent = root.join("parent");
            let displaced = root.join("displaced");
            std::fs::create_dir(&parent).unwrap();
            let path = parent.join("squid.conf");
            let mut replaced = false;

            let result = write_generated_config_with_hook(&path, b"config", |step| {
                if step == replacement_step && !replaced {
                    std::fs::rename(&parent, &displaced).unwrap();
                    std::fs::create_dir(&parent).unwrap();
                    replaced = true;
                }
            });

            assert!(result.is_err(), "{replacement_step:?}");
            assert!(!parent.join("squid.conf").exists(), "{replacement_step:?}");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn proxy_run_args_bind_runtime_contract_and_writable_tmpfs_ownership() {
        let config = LocalAgentConfig::default();
        let args = proxy_container_run_args(
            &config.egress.proxy,
            "proxy",
            "internal",
            std::path::Path::new("/private/squid.conf"),
            "fingerprint",
            LocalAgentEgressTier::Llm,
            "worker-1",
        );
        let joined = args.join(" ");
        assert!(joined.contains("--user 13:13"));
        assert!(joined.contains("/var/spool/squid:rw,size=1g,uid=13,gid=13,mode=0755"));
        assert!(joined.contains("/var/cache/squid:rw,size=1g,uid=13,gid=13,mode=0755"));
        assert!(joined.contains("--restart unless-stopped"));
    }

    #[test]
    fn proxy_cleanup_removes_anonymous_volumes_with_exact_container() {
        assert_eq!(
            proxy_remove_args("exact-proxy"),
            ["rm", "-f", "-v", "exact-proxy"]
        );
    }

    #[test]
    fn integration_cleanup_commands_remove_exact_resources_in_dependency_order() {
        assert_eq!(
            integration_cleanup_commands("proxy", "internal", "external"),
            [
                vec!["rm", "-f", "-v", "proxy"],
                vec!["network", "rm", "internal"],
                vec!["network", "rm", "external"],
            ]
        );
    }

    #[test]
    fn real_docker_restart_commands_inspect_contract_and_crash_verified_squid_child() {
        assert_eq!(
            squid_image_contract_args("pinned-image"),
            [
                "image",
                "inspect",
                "--format",
                "{{json .Config}}",
                "pinned-image",
            ]
        );
        assert_eq!(
            container_restart_state_args("proxy"),
            [
                "inspect",
                "--format",
                "{{.State.Running}}\t{{.State.Pid}}\t{{.RestartCount}}",
                "proxy",
            ]
        );
        let process_tree = squid_process_tree_args("proxy").join(" ");
        assert!(process_tree.contains("for proc in /proc/[0-9]*"));
        assert!(process_tree.contains("PPid:"));
        assert!(process_tree.contains("cmdline"));
        let crash = squid_unexpected_crash_args("proxy", 42);
        assert_eq!(&crash[..4], ["exec", "proxy", "sh", "-ceu"]);
        assert!(crash[4].contains("test \"$pid\" -ne 1"));
        assert!(crash[4].contains("/usr/sbin/squid"));
        assert!(crash[4].contains("test \"$ppid\" = 1"));
        assert!(crash[4].contains("kill -TERM \"$pid\""));
        assert_eq!(&crash[5..], ["sh", "42"]);
        assert!(
            !crash
                .iter()
                .any(|argument| argument == "stop" || argument == "kill")
        );
        assert_eq!(
            container_die_events_args("proxy"),
            [
                "events",
                "--filter",
                "container=proxy",
                "--filter",
                "event=die",
                "--format",
                "{{json .}}",
            ]
        );
        assert_eq!(
            parse_die_event_exit_code(
                r#"{"Action":"die","Actor":{"Attributes":{"exitCode":"0"}}}"#
            ),
            Some(0)
        );
    }

    fn valid_pinned_process_tree() -> String {
        [
            "1\t0\t/usr/bin/bash\tbash\t/usr/bin/bash /usr/local/bin/entrypoint.sh -f /etc/squid/squid.conf -NYC",
            "42\t1\t/usr/sbin/squid\tsquid\t/usr/sbin/squid -f /etc/squid/squid.conf -NYC",
            "50\t1\t/usr/bin/tail\ttail\ttail -F /var/log/squid/access.log",
            "51\t1\t/usr/bin/tail\ttail\ttail -F /var/log/squid/error.log",
            "52\t1\t/usr/bin/tail\ttail\ttail -F /var/log/squid/store.log",
            "53\t1\t/usr/bin/tail\ttail\ttail -F /var/log/squid/cache.log",
        ]
        .join("\n")
    }

    #[test]
    fn pinned_squid_process_tree_parser_accepts_observed_relationships() {
        let entries = parse_process_tree(&valid_pinned_process_tree()).unwrap();
        assert_eq!(validate_pinned_squid_process_tree(&entries), Ok(42));
    }

    #[test]
    fn pinned_squid_process_tree_parser_rejects_malformed_and_ambiguous_trees() {
        assert!(parse_process_tree("malformed").is_err());
        assert!(parse_process_tree("1\t0\t/a\ta\ta\n1\t0\t/b\tb\tb").is_err());

        let valid = parse_process_tree(&valid_pinned_process_tree()).unwrap();
        let mut wrong_pid_one = valid.clone();
        wrong_pid_one[0].exe = "/usr/sbin/squid".to_string();
        assert!(validate_pinned_squid_process_tree(&wrong_pid_one).is_err());

        let mut missing_squid = valid.clone();
        missing_squid.retain(|entry| entry.exe != "/usr/sbin/squid");
        assert!(validate_pinned_squid_process_tree(&missing_squid).is_err());

        let mut extra_squid = valid.clone();
        let mut duplicate = extra_squid[1].clone();
        duplicate.pid = 43;
        extra_squid.push(duplicate);
        assert!(validate_pinned_squid_process_tree(&extra_squid).is_err());

        let mut indirect_squid = valid.clone();
        indirect_squid[1].ppid = 99;
        assert!(validate_pinned_squid_process_tree(&indirect_squid).is_err());

        let mut missing_tail = valid.clone();
        missing_tail.pop();
        assert!(validate_pinned_squid_process_tree(&missing_tail).is_err());

        let mut wrong_tail = valid;
        wrong_tail[2].args = "tail -F /var/log/squid/unexpected.log".to_string();
        assert!(validate_pinned_squid_process_tree(&wrong_tail).is_err());
    }

    #[test]
    fn restart_poll_requires_crash_restart_count_and_new_running_pid() {
        let initial = ContainerRestartState {
            running: true,
            pid: 100,
            restart_count: 2,
        };
        assert_eq!(
            parse_container_restart_state("true\t100\t2\n"),
            Some(initial)
        );
        for manual_stop_or_incomplete in [
            ContainerRestartState {
                running: false,
                pid: 0,
                restart_count: 2,
            },
            ContainerRestartState {
                running: true,
                pid: 100,
                restart_count: 3,
            },
            ContainerRestartState {
                running: true,
                pid: 101,
                restart_count: 2,
            },
        ] {
            assert!(!container_restarted_after_crash(
                initial,
                manual_stop_or_incomplete
            ));
        }
        assert!(container_restarted_after_crash(
            initial,
            ContainerRestartState {
                running: true,
                pid: 101,
                restart_count: 3,
            }
        ));
    }

    #[test]
    fn proxy_fingerprint_changes_with_every_runtime_contract_field() {
        let config = LocalAgentConfig::default();
        let baseline = proxy_fingerprint(b"config", &config.egress.proxy);
        for mutation in ["image", "port", "uid", "gid", "mode", "declared_volumes"] {
            let mut proxy = config.egress.proxy.clone();
            match mutation {
                "image" => proxy.image.push_str("-different"),
                "port" => proxy.listen_port += 1,
                "uid" => proxy.runtime_uid += 1,
                "gid" => proxy.runtime_gid += 1,
                "mode" => proxy.writable_tmpfs_mode = "0700".to_string(),
                "declared_volumes" => proxy.declared_volumes.push("/extra".to_string()),
                _ => unreachable!(),
            }
            assert_ne!(proxy_fingerprint(b"config", &proxy), baseline, "{mutation}");
        }
    }

    #[test]
    fn compatible_proxy_rejection_covers_runtime_and_anonymous_log_volume_contract() {
        let root = test_dir("proxy-compatibility");
        let manager = EgressManager::new("worker-1", &root, LocalAgentConfig::default()).unwrap();
        let mut inspect = complete_proxy_inspect(&manager);
        let report = proxy_report(&manager, &inspect);
        assert!(report.owned);
        assert!(report.compatible(), "{}", report.diagnostic());

        for (pointer, value, field) in [
            ("/Config/Image", serde_json::json!("wrong-image"), "image"),
            ("/Config/User", serde_json::json!("14:13"), "runtime.uid"),
            ("/Config/User", serde_json::json!("13:14"), "runtime.gid"),
            (
                "/Config/Labels/agent-hub.local-agent.egress.managed",
                serde_json::json!("false"),
                "label.managed",
            ),
            (
                "/Config/Labels/agent-hub.local-agent.worker_id",
                serde_json::json!("other"),
                "label.worker_id",
            ),
            (
                "/Config/Labels/agent-hub.local-agent.egress.kind",
                serde_json::json!("other"),
                "label.kind",
            ),
            (
                "/Config/Labels/agent-hub.local-agent.egress.tier",
                serde_json::json!("build"),
                "label.tier",
            ),
            (
                "/Config/Labels/agent-hub.local-agent.egress.fingerprint",
                serde_json::json!("wrong"),
                "label.fingerprint",
            ),
            (
                "/HostConfig/ReadonlyRootfs",
                serde_json::json!(false),
                "read_only_root",
            ),
            (
                "/HostConfig/RestartPolicy/Name",
                serde_json::json!("no"),
                "restart_policy",
            ),
            (
                "/HostConfig/NetworkMode",
                serde_json::json!("wrong"),
                "network.mode",
            ),
            (
                "/HostConfig/CapDrop",
                serde_json::json!([]),
                "capabilities.drop",
            ),
            (
                "/HostConfig/CapAdd",
                serde_json::json!(["NET_ADMIN"]),
                "capabilities.add",
            ),
            (
                "/HostConfig/Binds",
                serde_json::json!([]),
                "mount.config_bind.declaration",
            ),
            (
                "/HostConfig/Tmpfs/~1tmp",
                serde_json::json!("wrong"),
                "tmpfs./tmp",
            ),
            (
                "/HostConfig/Tmpfs/~1run",
                serde_json::json!("wrong"),
                "tmpfs./run",
            ),
            (
                "/HostConfig/Tmpfs/~1var~1spool~1squid",
                serde_json::json!("wrong"),
                "tmpfs./var/spool/squid",
            ),
            (
                "/HostConfig/Tmpfs/~1var~1cache~1squid",
                serde_json::json!("rw,size=1g,uid=13,gid=13,mode=0700"),
                "tmpfs./var/cache/squid",
            ),
            ("/Mounts/0/RW", serde_json::json!(false), "mount.log.rw"),
            (
                "/Mounts/0/Type",
                serde_json::json!("bind"),
                "mount.log.type",
            ),
            (
                "/Mounts/0/Destination",
                serde_json::json!("/wrong"),
                "mount.log.destination",
            ),
            (
                "/Mounts/0/Source",
                serde_json::json!("/wrong/source"),
                "mount.log.source",
            ),
            ("/Mounts/1/RW", serde_json::json!(true), "mount.config.rw"),
            (
                "/Mounts/1/Type",
                serde_json::json!("volume"),
                "mount.config.type",
            ),
            (
                "/Mounts/1/Source",
                serde_json::json!("/wrong/config"),
                "mount.config.source",
            ),
            (
                "/Mounts/1/Destination",
                serde_json::json!("/wrong/config"),
                "mount.config.destination",
            ),
            (
                "/HostConfig/Mounts",
                serde_json::json!([{"Type": "volume", "Source": "named-volume", "Target": "/var/log/squid"}]),
                "mount.unexpected_explicit",
            ),
            (
                "/Config/Volumes",
                serde_json::json!({"/var/log/squid": {}}),
                "image.declared_volumes",
            ),
            (
                "/Config/Volumes",
                serde_json::json!({"/var/log/squid": {}, "/var/spool/squid": {}, "/extra": {}}),
                "image.declared_volumes",
            ),
        ] {
            let saved = inspect.pointer(pointer).unwrap().clone();
            *inspect.pointer_mut(pointer).unwrap() = value;
            let report = proxy_report(&manager, &inspect);
            assert!(!report.compatible(), "{pointer}");
            assert_mismatch_field(&report, field);
            *inspect.pointer_mut(pointer).unwrap() = saved;
        }

        inspect["HostConfig"]["Tmpfs"]["/var/cache/squid"] =
            serde_json::json!("rw,size=1g,uid=13,gid=13,mode=0700");
        assert_mismatch_field(
            &proxy_report(&manager, &inspect),
            "runtime.writable_tmpfs_mode",
        );
        inspect["HostConfig"]["Tmpfs"]["/var/cache/squid"] =
            serde_json::json!("rw,size=1g,uid=13,gid=13,mode=0755");

        for (extra, field) in [
            (
                serde_json::json!({
                    "Type": "volume", "Name": "extra", "Source": "/var/lib/docker/volumes/extra/_data",
                    "Destination": "/extra", "RW": true
                }),
                "mount.volume.count",
            ),
            (
                serde_json::json!({
                    "Type": "bind", "Source": "/extra", "Destination": "/extra", "RW": false
                }),
                "mount.bind.count",
            ),
        ] {
            inspect["Mounts"].as_array_mut().unwrap().push(extra);
            let report = proxy_report(&manager, &inspect);
            assert_mismatch_field(&report, field);
            assert_mismatch_field(
                &report,
                if field == "mount.volume.count" {
                    "mount.volume.unexpected"
                } else {
                    "mount.bind.unexpected"
                },
            );
            inspect["Mounts"].as_array_mut().unwrap().pop();
        }
        inspect["Mounts"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "Type": "volume",
                "Name": "unexpected-spool",
                "Source": "/var/lib/docker/volumes/unexpected-spool/_data",
                "Destination": "/var/spool/squid",
                "RW": true
            }));
        let spool_report = proxy_report(&manager, &inspect);
        assert_mismatch_field(&spool_report, "mount.spool.runtime");
        assert_mismatch_field(&spool_report, "mount.volume.unexpected");
        inspect["Mounts"].as_array_mut().unwrap().pop();

        inspect["HostConfig"]["Tmpfs"]["/unexpected"] = serde_json::json!("rw");
        assert_mismatch_field(&proxy_report(&manager, &inspect), "tmpfs.unexpected");
        inspect["HostConfig"]["Tmpfs"]
            .as_object_mut()
            .unwrap()
            .remove("/unexpected");
        let tmpfs = inspect["HostConfig"]["Tmpfs"].take();
        assert_mismatch_field(&proxy_report(&manager, &inspect), "tmpfs.map");
        inspect["HostConfig"]["Tmpfs"] = tmpfs;

        let name = inspect["Mounts"][0]["Name"].take();
        assert_mismatch_field(&proxy_report(&manager, &inspect), "mount.log.name");
        inspect["Mounts"][0]["Name"] = name;
        let mounts = inspect["Mounts"].take();
        let missing_mounts = proxy_report(&manager, &inspect);
        assert_mismatch_field(&missing_mounts, "mounts");
        inspect["Mounts"] = serde_json::json!([]);
        let empty_mounts = proxy_report(&manager, &inspect);
        for field in [
            "mount.volume.count",
            "mount.bind.count",
            "mount.volume_or_bind.count",
            "mount.log",
            "mount.config",
        ] {
            assert_mismatch_field(&empty_mounts, field);
        }
        inspect["Mounts"] = mounts;

        inspect["NetworkSettings"]["Networks"]
            .as_object_mut()
            .unwrap()
            .remove("internal");
        assert_mismatch_field(
            &proxy_report(&manager, &inspect),
            "network.attachment.internal",
        );
        inspect["NetworkSettings"]["Networks"]["internal"] = serde_json::json!({});
        inspect["NetworkSettings"]["Networks"]
            .as_object_mut()
            .unwrap()
            .remove("external");
        assert_mismatch_field(
            &proxy_report(&manager, &inspect),
            "network.attachment.external",
        );
        inspect["NetworkSettings"]["Networks"]["external"] = serde_json::json!({});
        inspect["NetworkSettings"]["Networks"]["unexpected"] = serde_json::json!({});
        assert_mismatch_field(
            &proxy_report(&manager, &inspect),
            "network.attachments.unexpected",
        );
        inspect["NetworkSettings"]["Networks"] = serde_json::Value::Null;
        assert_mismatch_field(&proxy_report(&manager, &inspect), "network.attachments");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn proxy_compatibility_diagnostic_is_structured_and_bounded() {
        let root = test_dir("proxy-compatibility-bounds");
        let manager = EgressManager::new("worker-1", &root, LocalAgentConfig::default()).unwrap();
        let mut inspect = complete_proxy_inspect(&manager);
        let long = "x".repeat(100_000);
        inspect["Config"]["Image"] = serde_json::json!(long);
        inspect["HostConfig"]["Tmpfs"]["/unexpected"] = serde_json::json!("y".repeat(100_000));
        let report = proxy_report(&manager, &inspect);
        let diagnostic = report.diagnostic();
        assert!(diagnostic.len() <= super::PROXY_COMPATIBILITY_DIAGNOSTIC_LIMIT);
        let parsed: serde_json::Value = serde_json::from_str(&diagnostic).unwrap();
        assert!(parsed.get("mismatches").is_some() || parsed.get("mismatch_fields").is_some());
        assert_mismatch_field(&report, "image");
        assert_mismatch_field(&report, "tmpfs.unexpected");
        std::fs::remove_dir_all(root).unwrap();
    }

    async fn accepts_create_race_sequence(
        manager: &EgressManager,
        sequence: Vec<Option<serde_json::Value>>,
    ) -> bool {
        let mut sequence = std::collections::VecDeque::from(sequence);
        wait_for_compatible_proxy_winner(
            || std::future::ready(Ok(sequence.pop_front().unwrap_or(None))),
            |json| {
                let report = proxy_report(manager, json);
                report.owned && report.compatible()
            },
            4,
            Duration::ZERO,
        )
        .await
    }

    #[tokio::test]
    async fn failed_create_accepts_concurrent_complete_owned_compatible_winner() {
        let root = test_dir("create-race-compatible");
        let manager = EgressManager::new("worker-1", &root, LocalAgentConfig::default()).unwrap();
        assert!(
            accepts_create_race_sequence(&manager, vec![Some(complete_proxy_inspect(&manager))])
                .await
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failed_create_rejects_unowned_winner() {
        let root = test_dir("create-race-unowned");
        let manager = EgressManager::new("worker-1", &root, LocalAgentConfig::default()).unwrap();
        let mut inspect = complete_proxy_inspect(&manager);
        inspect["Config"]["Labels"][EGRESS_WORKER_LABEL] = serde_json::json!("other-worker");
        assert!(!accepts_create_race_sequence(&manager, vec![Some(inspect)]).await);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failed_create_rejects_incompatible_owned_winner() {
        let root = test_dir("create-race-incompatible");
        let manager = EgressManager::new("worker-1", &root, LocalAgentConfig::default()).unwrap();
        let mut inspect = complete_proxy_inspect(&manager);
        inspect["HostConfig"]["ReadonlyRootfs"] = serde_json::json!(false);
        assert!(!accepts_create_race_sequence(&manager, vec![Some(inspect)]).await);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failed_create_waits_for_delayed_complete_winner() {
        let root = test_dir("create-race-delayed");
        let manager = EgressManager::new("worker-1", &root, LocalAgentConfig::default()).unwrap();
        assert!(
            accepts_create_race_sequence(
                &manager,
                vec![None, None, Some(complete_proxy_inspect(&manager))]
            )
            .await
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failed_create_rejects_when_no_winner_appears() {
        let root = test_dir("create-race-none");
        let manager = EgressManager::new("worker-1", &root, LocalAgentConfig::default()).unwrap();
        assert!(!accepts_create_race_sequence(&manager, vec![None, None, None, None]).await);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_create_without_compatible_winner_preserves_original_error() {
        let error = require_compatible_create_winner(
            false,
            anyhow::anyhow!("original docker create failure"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "original docker create failure");
    }

    #[test]
    fn compatible_existing_proxy_is_readiness_checked_after_both_attachments() {
        assert_eq!(
            compatible_proxy_actions("internal", "external"),
            [
                CompatibleProxyAction::EnsureRunning,
                CompatibleProxyAction::EnsureConnected("internal".to_string()),
                CompatibleProxyAction::EnsureConnected("external".to_string()),
                CompatibleProxyAction::WaitReady("internal".to_string()),
            ]
        );
    }

    #[test]
    fn proxy_diagnostics_are_bounded_and_redact_credentials() {
        let diagnostics = bounded_proxy_diagnostics(
            r#"{"Status":"exited","Error":"token=secret"}"#,
            &format!(
                "https://user:password@example.com/path?api_key=secret\nAuthorization: Bearer secret\n{}",
                "x".repeat(20_000)
            ),
        );
        assert!(diagnostics.len() <= 8192);
        assert!(!diagnostics.contains("password"));
        assert!(!diagnostics.contains("secret"));
        assert!(diagnostics.contains("[REDACTED]"));
    }

    #[test]
    fn squid_acl_host_groups_include_apex_and_prune_redundant_subdomains() {
        let (apex, subdomains) = squid_acl_host_groups(&[
            "github.com".to_string(),
            "crates.io".to_string(),
            "index.crates.io".to_string(),
            "*.github.com".to_string(),
            "*.crates.io".to_string(),
        ]);

        assert_eq!(apex, vec!["crates.io", "github.com", "index.crates.io"]);
        assert_eq!(subdomains, vec![".crates.io", ".github.com"]);
    }

    #[test]
    fn exact_hosts_do_not_implicitly_allow_subdomains() {
        let (apex, subdomains) = squid_acl_host_groups(&["api.openai.com".to_string()]);
        assert_eq!(apex, vec!["api.openai.com"]);
        assert!(subdomains.is_empty());
    }

    #[test]
    fn llm_only_rejects_wildcard_model_hosts() {
        assert!(validate_literal_model_hosts(&["*.openai.com".to_string()]).is_err());
        assert!(validate_literal_model_hosts(&["api.openai.com".to_string()]).is_ok());
    }

    #[test]
    fn provider_specific_policies_cover_default_explicit_unknown_and_build_tiers() {
        let mut config = LocalAgentConfig::default();
        config.egress.llm.default_provider = Some("first".to_string());
        config.egress.llm.providers = HashMap::from([
            (
                "first".to_string(),
                vec!["z.example.com".to_string(), "a.example.com".to_string()],
            ),
            ("second".to_string(), vec!["second.example.com".to_string()]),
        ]);
        config.egress.build.github_hosts = vec!["build.example.com".to_string()];
        config.egress.build.go_registry_hosts.clear();
        config.egress.build.crates_registry_hosts.clear();
        config.set_config_tree_for_tests(ConfigTreeSnapshot {
            network_policies: HashMap::from([
                (
                    "llm-only".to_string(),
                    ConfiguredNetworkPolicy {
                        uses_llm_provider_hosts: true,
                        ..ConfiguredNetworkPolicy::default()
                    },
                ),
                (
                    "build".to_string(),
                    ConfiguredNetworkPolicy {
                        hosts: vec!["build.example.com".to_string()],
                        uses_llm_provider_hosts: true,
                        ..ConfiguredNetworkPolicy::default()
                    },
                ),
            ]),
            llm_provider_hosts: HashMap::from([
                (
                    "first".to_string(),
                    vec!["a.example.com".to_string(), "z.example.com".to_string()],
                ),
                ("second".to_string(), vec!["second.example.com".to_string()]),
            ]),
            ..ConfigTreeSnapshot::default()
        });
        let manager = EgressManager::new("worker-1", std::path::Path::new("/tmp"), config)
            .expect("manager should build");

        assert_eq!(
            manager
                .resolve_tier_hosts(LocalAgentEgressTier::Llm, None)
                .unwrap(),
            vec!["a.example.com", "z.example.com"]
        );
        assert_eq!(
            manager
                .resolve_tier_hosts(LocalAgentEgressTier::Llm, Some("second"))
                .unwrap(),
            vec!["second.example.com"]
        );
        assert!(
            manager
                .resolve_tier_hosts(LocalAgentEgressTier::Llm, Some("missing"))
                .is_err()
        );
        assert_eq!(
            manager
                .resolve_tier_hosts(LocalAgentEgressTier::Build, Some("second"))
                .unwrap(),
            vec!["build.example.com", "second.example.com"]
        );

        let mut mismatched = manager.config().clone();
        mismatched.set_config_tree_for_tests(ConfigTreeSnapshot {
            network_policies: HashMap::from([(
                "llm-only".to_string(),
                ConfiguredNetworkPolicy {
                    hosts: vec!["extra.example.com".to_string()],
                    uses_llm_provider_hosts: true,
                    ..ConfiguredNetworkPolicy::default()
                },
            )]),
            llm_provider_hosts: HashMap::from([
                (
                    "first".to_string(),
                    vec!["a.example.com".to_string(), "z.example.com".to_string()],
                ),
                ("second".to_string(), vec!["second.example.com".to_string()]),
            ]),
            ..ConfigTreeSnapshot::default()
        });
        let mismatched = EgressManager::new("worker-1", std::path::Path::new("/tmp"), mismatched)
            .expect("manager should build");
        assert!(
            mismatched
                .resolve_tier_hosts(LocalAgentEgressTier::Llm, None)
                .is_err()
        );
    }

    #[test]
    fn network_compatibility_requires_full_restricted_shape() {
        let manager = EgressManager::new(
            "worker-1",
            std::path::Path::new("/tmp"),
            LocalAgentConfig::default(),
        )
        .expect("manager should build");
        let compatible = serde_json::json!({
            "Internal": true,
            "Driver": "bridge",
            "Scope": "local",
            "Labels": {
                (EGRESS_MANAGED_LABEL): "true",
                (EGRESS_WORKER_LABEL): "worker-1",
                (EGRESS_KIND_LABEL): "network"
            }
        });
        assert_eq!(
            manager.network_ownership_and_compatibility(&compatible, true),
            (true, true)
        );
        for field in ["Internal", "Driver", "Scope", "Labels"] {
            let mut incompatible = compatible.clone();
            incompatible.as_object_mut().expect("object").remove(field);
            assert!(
                !manager
                    .network_ownership_and_compatibility(&incompatible, true)
                    .1
            );
        }
    }

    #[test]
    fn preflight_uses_job_network_controls_without_credentials_or_mounts() {
        let manager = EgressManager::new(
            "worker-1",
            std::path::Path::new("/tmp"),
            LocalAgentConfig::default(),
        )
        .expect("manager should build");
        let runtime = RestrictedEgressRuntime {
            network_name: "triage-internal".to_string(),
            proxy_url: "http://triage-proxy:3128".to_string(),
            no_proxy: "localhost,127.0.0.1,::1,.local".to_string(),
            tier_hosts: vec!["api.openai.com".to_string()],
            proxy_host: "triage-proxy".to_string(),
            proxy_ip: "172.30.0.2".to_string(),
        };
        let args = manager.preflight_container_invocation(
            &runtime,
            "api.openai.com",
            "preflight@sha256:abc",
        );
        assert!(args.contains(&"--network=triage-internal".to_string()));
        assert!(args.contains(&"--dns=127.0.0.1".to_string()));
        assert!(args.contains(&"--add-host=triage-proxy:172.30.0.2".to_string()));
        assert!(args.contains(&"HTTPS_PROXY=http://triage-proxy:3128".to_string()));
        assert!(
            args.iter()
                .all(|arg| arg != "-v" && !arg.starts_with("--volume"))
        );
        let joined = args.join(" ");
        for forbidden in [
            "MCP_",
            "GH_CONFIG_DIR",
            "GH_TOKEN",
            "CREDENTIAL",
            "/var/run/docker.sock",
            "/run/",
        ] {
            assert!(!joined.contains(forbidden), "found forbidden {forbidden}");
        }
        for proof in [
            "getent hosts preflight-arbitrary.invalid",
            "https://1.1.1.1/",
            "NO_PROXY=\"$model_host\"",
            "https://example.com/",
        ] {
            assert!(joined.contains(proof), "missing proof {proof}");
        }
    }

    #[tokio::test]
    #[ignore = "requires a real Docker daemon and explicitly configured preflight image/model host"]
    async fn real_docker_restricted_egress_preflight() {
        let model_host = std::env::var("AGENT_HUB_EGRESS_INTEGRATION_MODEL_HOST")
            .expect("set AGENT_HUB_EGRESS_INTEGRATION_MODEL_HOST");
        let preflight_image = std::env::var("AGENT_HUB_EGRESS_INTEGRATION_PREFLIGHT_IMAGE")
            .expect("set AGENT_HUB_EGRESS_INTEGRATION_PREFLIGHT_IMAGE");
        let mut config = LocalAgentConfig::default();
        config.egress.llm.default_provider = Some("integration".to_string());
        config
            .egress
            .llm
            .providers
            .insert("integration".to_string(), vec![model_host]);
        config.egress.preflight_image = Some(preflight_image);
        assert_eq!(config.egress.proxy.runtime_uid, 13);
        assert_eq!(config.egress.proxy.runtime_gid, 13);
        assert_eq!(config.egress.proxy.writable_tmpfs_mode, "0755");
        let data_dir = std::env::temp_dir().join(format!(
            "agent-hub-egress-integration-{}",
            uuid::Uuid::new_v4()
        ));
        let worker_id = format!("integration-{}", uuid::Uuid::new_v4());
        let sanitized_worker_id = super::sanitize_name_component(&worker_id);
        let container_name = format!("agent-hub-{sanitized_worker_id}-egress-proxy-llm");
        let internal_network = format!("agent-hub-{sanitized_worker_id}-egress-llm");
        let external_network = format!("agent-hub-{sanitized_worker_id}-egress-external");
        let mut cleanup = IntegrationCleanup {
            worker_id: sanitized_worker_id,
            container: container_name.clone(),
            internal_network: internal_network.clone(),
            external_network: external_network.clone(),
            data_root: data_dir.clone(),
            log_volume: None,
            active: true,
        };
        std::fs::create_dir_all(data_dir.join("egress")).unwrap();
        let config_path = data_dir.join("egress/squid-llm.conf");
        std::fs::write(&config_path, b"pre-existing").unwrap();
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let manager = EgressManager::new(&worker_id, &data_dir, config)
            .expect("build integration egress manager");

        let umask = UmaskGuard::restrictive();
        manager
            .preflight_llm_only()
            .await
            .expect("real Docker preflight must enforce all egress assertions");
        drop(umask);
        assert_eq!(
            std::fs::metadata(&config_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        let log_volume = assert_real_docker_proxy_contract(
            &manager,
            &container_name,
            &config_path,
            &internal_network,
            &external_network,
        )
        .await;
        cleanup.log_volume = Some(log_volume);
        let _initial_squid_pid =
            assert_pinned_squid_process_contract(&manager, &container_name).await;

        let writable = std::process::Command::new("docker")
            .args([
                "exec",
                &container_name,
                "sh",
                "-ceu",
                "test \"$(id -u)\" = 13; test \"$(id -g)\" = 13; touch /var/spool/squid/agent-hub-write-test /var/cache/squid/agent-hub-write-test",
            ])
            .status()
            .expect("verify proxy writable tmpfs as configured UID/GID");
        assert!(writable.success());

        let Some(initial) = docker_container_restart_state(&container_name).await else {
            fail_real_docker_restart(
                &container_name,
                "integration proxy has no readable initial restart state",
            )
            .await;
        };
        if !initial.running || initial.pid == 0 {
            fail_real_docker_restart(&container_name, "initial proxy state is not running").await;
        }
        manager
            .wait_for_proxy_ready(&container_name, &internal_network)
            .await
            .unwrap_or_else(|error| panic!("initial proxy readiness failed: {error:#}"));

        let activation_deadline = tokio::time::Instant::now() + Duration::from_secs(11);
        while tokio::time::Instant::now() < activation_deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Some(current) = docker_container_restart_state(&container_name).await else {
                fail_real_docker_restart(
                    &container_name,
                    "proxy state became unreadable before restart policy activation",
                )
                .await;
            };
            if !current.running
                || current.pid != initial.pid
                || current.restart_count != initial.restart_count
            {
                fail_real_docker_restart(
                    &container_name,
                    "proxy did not remain continuously running before crash injection",
                )
                .await;
            }
        }
        manager
            .wait_for_proxy_ready(&container_name, &internal_network)
            .await
            .unwrap_or_else(|error| panic!("proxy readiness before crash failed: {error:#}"));
        let squid_pid = assert_pinned_squid_process_contract(&manager, &container_name).await;
        let die_watcher = match ContainerDieWatcher::start(&container_name).await {
            Ok(watcher) => watcher,
            Err(error) => {
                fail_real_docker_restart(
                    &container_name,
                    &format!("old container exit is not reliably observable: {error}"),
                )
                .await
            }
        };

        let mut crash = tokio::process::Command::new("docker");
        crash
            .kill_on_drop(true)
            .args(squid_unexpected_crash_args(&container_name, squid_pid));
        if matches!(
            tokio::time::timeout(Duration::from_secs(5), crash.status()).await,
            Ok(Err(_))
        ) {
            fail_real_docker_restart(
                &container_name,
                "failed to launch in-container Squid-child crash",
            )
            .await;
        }
        let mut restarted = None;
        let restart_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while tokio::time::Instant::now() < restart_deadline {
            if let Some(current) = docker_container_restart_state(&container_name).await
                && container_restarted_after_crash(initial, current)
            {
                restarted = Some(current);
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if restarted.is_none() {
            fail_real_docker_restart(
                &container_name,
                "unless-stopped did not produce a higher RestartCount and new running PID",
            )
            .await;
        }
        if let Err(error) = die_watcher.require_exit_zero().await {
            fail_real_docker_restart(
                &container_name,
                &format!("old container die event was invalid: {error}"),
            )
            .await;
        }
        manager
            .wait_for_proxy_ready(&container_name, &internal_network)
            .await
            .unwrap_or_else(|error| panic!("restarted proxy readiness failed: {error:#}"));
        let _restarted_squid_pid =
            assert_pinned_squid_process_contract(&manager, &container_name).await;
        if let Err(error) = manager.preflight_llm_only().await {
            fail_real_docker_restart(
                &container_name,
                &format!("restricted egress probes failed after restart: {error:#}"),
            )
            .await;
        }
        assert_real_docker_proxy_contract(
            &manager,
            &container_name,
            &config_path,
            &internal_network,
            &external_network,
        )
        .await;
        cleanup
            .cleanup()
            .expect("integration cleanup must remove every owned resource");
        cleanup
            .assert_absent()
            .expect("integration resources must be absent after cleanup");
    }

    async fn assert_pinned_squid_process_contract(
        manager: &EgressManager,
        container_name: &str,
    ) -> u32 {
        let mut image_inspect = tokio::process::Command::new("docker");
        image_inspect
            .kill_on_drop(true)
            .args(squid_image_contract_args(
                &manager.config().egress.proxy.image,
            ));
        let image_output =
            tokio::time::timeout(Duration::from_secs(5), image_inspect.output()).await;
        let image_config = match image_output {
            Ok(Ok(output)) if output.status.success() => {
                serde_json::from_slice::<serde_json::Value>(&output.stdout).ok()
            }
            _ => None,
        };
        let image_contract_matches = image_config.as_ref().is_some_and(|config| {
            config["Entrypoint"] == serde_json::json!(["entrypoint.sh"])
                && config["Cmd"] == serde_json::json!(["-f", "/etc/squid/squid.conf", "-NYC"])
                && config["Volumes"]
                    == serde_json::json!({
                        "/var/log/squid": {},
                        "/var/spool/squid": {}
                    })
        });
        if !image_contract_matches {
            fail_real_docker_restart(
                container_name,
                "pinned image entrypoint, command, or declared-volume contract mismatched",
            )
            .await;
        }

        let mut process_inspect = tokio::process::Command::new("docker");
        process_inspect
            .kill_on_drop(true)
            .args(squid_process_tree_args(container_name));
        let process_tree =
            tokio::time::timeout(Duration::from_secs(5), process_inspect.output()).await;
        let output = match process_tree {
            Ok(Ok(output)) if output.status.success() && output.stdout.len() <= 8192 => output,
            _ => {
                fail_real_docker_restart(
                    container_name,
                    "bounded runtime process-tree inspection failed",
                )
                .await
            }
        };
        let entries = match parse_process_tree(&String::from_utf8_lossy(&output.stdout)) {
            Ok(entries) => entries,
            Err(error) => {
                fail_real_docker_restart(
                    container_name,
                    &format!("runtime process-tree parse failed: {error}"),
                )
                .await
            }
        };
        match validate_pinned_squid_process_tree(&entries) {
            Ok(squid_pid) => squid_pid,
            Err(error) => {
                fail_real_docker_restart(
                    container_name,
                    &format!("runtime process-tree contract mismatched: {error}"),
                )
                .await
            }
        }
    }

    async fn fail_real_docker_restart(container_name: &str, reason: &str) -> ! {
        let state = super::docker_bounded_output(
            [
                "inspect",
                "--format",
                "state={{json .State}} restart_count={{.RestartCount}}",
                container_name,
            ],
            super::PROXY_DIAGNOSTIC_LIMIT / 2,
        )
        .await;
        let tail_lines = super::PROXY_LOG_TAIL_LINES.to_string();
        let logs = super::docker_bounded_output(
            ["logs", "--tail", tail_lines.as_str(), container_name],
            super::PROXY_DIAGNOSTIC_LIMIT / 2,
        )
        .await;
        panic!(
            "{}; {}",
            super::bound_text(reason, 512),
            super::bounded_proxy_diagnostics(&state, &logs)
        )
    }

    async fn assert_real_docker_proxy_contract(
        manager: &EgressManager,
        container_name: &str,
        config_path: &Path,
        internal_network: &str,
        external_network: &str,
    ) -> String {
        let inspect = super::docker_container_inspect(container_name)
            .await
            .expect("inspect integration proxy")
            .expect("integration proxy exists");
        let fingerprint = proxy_fingerprint(
            &std::fs::read(config_path).expect("read integration Squid config"),
            &manager.config().egress.proxy,
        );
        assert_eq!(
            inspect
                .pointer("/Config/Labels")
                .and_then(serde_json::Value::as_object)
                .and_then(|labels| labels.get(EGRESS_FINGERPRINT_LABEL))
                .and_then(serde_json::Value::as_str)
                .expect("integration proxy fingerprint label"),
            fingerprint
        );
        let report = manager.proxy_compatibility_report(
            &inspect,
            LocalAgentEgressTier::Llm,
            config_path,
            &fingerprint,
            internal_network,
            external_network,
        );
        assert!(
            report.owned,
            "actual proxy is not owned: {}",
            report.diagnostic()
        );
        assert!(
            report.compatible(),
            "actual proxy runtime contract mismatches: {}",
            report.diagnostic()
        );
        for (network_name, internal) in [(internal_network, true), (external_network, false)] {
            let network = super::docker_network_inspect(network_name)
                .await
                .expect("inspect integration network")
                .expect("integration network exists");
            assert_eq!(
                manager.network_ownership_and_compatibility(&network, internal),
                (true, true),
                "actual network must match exact labels and topology"
            );
        }
        inspect["Mounts"]
            .as_array()
            .expect("integration mounts")
            .iter()
            .find(|mount| mount["Type"] == "volume" && mount["Destination"] == "/var/log/squid")
            .and_then(|mount| mount["Name"].as_str())
            .filter(|name| !name.is_empty())
            .expect("anonymous log volume identity")
            .to_string()
    }

    #[test]
    fn rendered_squid_config_omits_obsolete_dns_v4_first() {
        let manager = EgressManager::new(
            "worker-1",
            std::path::Path::new("/tmp"),
            LocalAgentConfig::default(),
        )
        .expect("manager should build");

        let config = manager.render_squid_config(&["api.openai.com".to_string()]);

        assert!(!config.contains("dns_v4_first"));
    }

    #[test]
    fn rendered_squid_config_uses_stdio_cache_log_and_file_access_log() {
        let manager = EgressManager::new(
            "worker-1",
            std::path::Path::new("/tmp"),
            LocalAgentConfig::default(),
        )
        .expect("manager should build");

        let config = manager.render_squid_config(&["api.openai.com".to_string()]);

        assert!(config.contains("cache_log stdio:/dev/stderr"));
        assert!(config.contains("access_log daemon:/var/log/squid/access.log"));
    }

    #[test]
    fn triage_proxy_allows_agent_api_and_denies_other_endpoints() {
        let manager = EgressManager::new(
            "worker-1",
            std::path::Path::new("/tmp"),
            LocalAgentConfig::default(),
        )
        .expect("manager should build");
        let config = manager.render_squid_config(&["api.openai.com".to_string()]);
        assert!(config.contains("acl allowed_exact dstdomain api.openai.com"));
        assert!(!config.contains("github.com"));
        assert!(config.contains("http_access deny all"));
    }
}
