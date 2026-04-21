use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::LocalAgentEgressTier;
use crate::egress_config::{LocalAgentConfig, normalize_hosts};

const EGRESS_WORKER_LABEL: &str = "agent-hub.local-agent.worker_id";
const EGRESS_MANAGED_LABEL: &str = "agent-hub.local-agent.egress.managed";
const EGRESS_KIND_LABEL: &str = "agent-hub.local-agent.egress.kind";
const EGRESS_TIER_LABEL: &str = "agent-hub.local-agent.egress.tier";
const EGRESS_FINGERPRINT_LABEL: &str = "agent-hub.local-agent.egress.fingerprint";
const PROXY_READINESS_RETRIES: usize = 10;
const PROXY_READINESS_DELAY_MS: u64 = 500;
const PROXY_READINESS_CONNECT_TIMEOUT_SECS: u64 = 1;

#[derive(Debug, Clone)]
pub struct RestrictedEgressRuntime {
    pub network_name: String,
    pub proxy_url: String,
    pub no_proxy: String,
    pub tier_hosts: Vec<String>,
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
        std::fs::create_dir_all(&egress_dir)
            .with_context(|| format!("create {}", egress_dir.display()))?;
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
        std::fs::write(&config_path, squid_config.as_bytes())
            .with_context(|| format!("write {}", config_path.display()))?;

        let fingerprint = format!("{:016x}", fnv1a64(squid_config.as_bytes()));
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

        self.write_state_file(tier, &fingerprint, &tier_hosts)?;

        Ok(RestrictedEgressRuntime {
            network_name: internal_network,
            proxy_url: format!(
                "http://{}:{}",
                container_name, self.config.egress.proxy.listen_port
            ),
            no_proxy: "localhost,127.0.0.1,::1,.local".to_string(),
            tier_hosts,
        })
    }

    pub fn config(&self) -> &LocalAgentConfig {
        &self.config
    }

    fn resolve_tier_hosts(
        &self,
        tier: LocalAgentEgressTier,
        llm_provider: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let network_policy_name = match tier {
            LocalAgentEgressTier::Llm => Some("llm-only"),
            LocalAgentEgressTier::Build => Some("build"),
            LocalAgentEgressTier::Full => None,
        };

        if let Some(policy_name) = network_policy_name
            && let Some(hosts) = self.config.network_policy_hosts(policy_name)
        {
            if hosts.is_empty() {
                anyhow::bail!(
                    "restricted {} tier requested but '{}' policy has no hosts",
                    tier.as_str(),
                    policy_name
                );
            }
            return Ok(hosts.to_vec());
        }

        let llm_hosts = self.config.egress.llm.hosts_for_provider(llm_provider);
        match tier {
            LocalAgentEgressTier::Llm => {
                if llm_hosts.is_empty() {
                    anyhow::bail!(
                        "restricted llm tier requested but no LLM hosts are configured; configure egress.llm.providers in local-agent config"
                    );
                }
                Ok(llm_hosts)
            }
            LocalAgentEgressTier::Build => {
                if llm_hosts.is_empty() {
                    anyhow::bail!(
                        "restricted build tier requested but LLM hosts are missing; configure egress.llm.providers in local-agent config"
                    );
                }
                let mut hosts = Vec::new();
                hosts.extend(llm_hosts);
                hosts.extend(self.config.egress.build.github_hosts.clone());
                hosts.extend(self.config.egress.build.go_registry_hosts.clone());
                hosts.extend(self.config.egress.build.crates_registry_hosts.clone());
                Ok(normalize_hosts(&hosts))
            }
            LocalAgentEgressTier::Full => {
                anyhow::bail!("full tier does not use restricted egress proxy")
            }
        }
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
        if docker_network_exists(name).await? {
            return Ok(());
        }

        let mut cmd = tokio::process::Command::new("docker");
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
                if docker_network_exists(name).await? {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
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
        let should_recreate = inspect
            .as_ref()
            .map(|json| {
                json.pointer("/Config/Labels")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|labels| labels.get(EGRESS_FINGERPRINT_LABEL))
                    .and_then(serde_json::Value::as_str)
                    != Some(fingerprint)
            })
            .unwrap_or(true);

        if should_recreate && inspect.is_some() {
            let mut rm = tokio::process::Command::new("docker");
            rm.arg("rm").arg("-f").arg(container_name);
            run_status(&mut rm, "docker rm proxy container failed").await?;
        }

        if should_recreate {
            let mut run = tokio::process::Command::new("docker");
            run.arg("run")
                .arg("-d")
                .arg("--restart")
                .arg("unless-stopped")
                .arg("--cap-drop=ALL")
                .arg("--cap-add=SETUID")
                .arg("--cap-add=SETGID")
                .arg("--read-only")
                .arg("--tmpfs")
                .arg("/tmp:rw,size=256m")
                .arg("--tmpfs")
                .arg("/run:rw,size=64m")
                .arg("--tmpfs")
                .arg("/var/spool/squid:rw,size=1g")
                .arg("--tmpfs")
                .arg("/var/cache/squid:rw,size=1g")
                .arg("--name")
                .arg(container_name)
                .arg("--network")
                .arg(internal_network)
                .arg("--label")
                .arg(format!("{EGRESS_MANAGED_LABEL}=true"))
                .arg("--label")
                .arg(format!("{EGRESS_WORKER_LABEL}={}", self.worker_id))
                .arg("--label")
                .arg(format!("{EGRESS_KIND_LABEL}=proxy"))
                .arg("--label")
                .arg(format!("{EGRESS_TIER_LABEL}={}", tier.as_str()))
                .arg("--label")
                .arg(format!("{EGRESS_FINGERPRINT_LABEL}={fingerprint}"))
                .arg("-v")
                .arg(format!(
                    "{}:/etc/squid/squid.conf:ro",
                    config_path.to_string_lossy()
                ))
                .arg(self.config.egress.proxy.image.as_str());
            if let Err(error) = run_status(&mut run, "docker run squid proxy failed").await
                && docker_container_inspect(container_name).await?.is_none()
            {
                return Err(error);
            }

            let mut connect = tokio::process::Command::new("docker");
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

        let started_now = self.ensure_container_running(container_name).await?;
        self.ensure_container_connected(container_name, internal_network)
            .await?;
        self.ensure_container_connected(container_name, external_network)
            .await?;

        if started_now {
            self.wait_for_proxy_ready(container_name, internal_network)
                .await?;
        }
        Ok(())
    }

    async fn ensure_container_running(&self, container_name: &str) -> anyhow::Result<bool> {
        let mut inspect = tokio::process::Command::new("docker");
        inspect
            .arg("inspect")
            .arg("--format")
            .arg("{{.State.Running}}")
            .arg(container_name);
        let output = inspect.output().await?;
        let running =
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true";
        if running {
            return Ok(false);
        }

        let mut start = tokio::process::Command::new("docker");
        start.arg("start").arg(container_name);
        run_status(&mut start, "docker start proxy container failed").await?;
        Ok(true)
    }

    async fn ensure_container_connected(
        &self,
        container_name: &str,
        network_name: &str,
    ) -> anyhow::Result<()> {
        let mut inspect = tokio::process::Command::new("docker");
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
                anyhow::bail!("proxy container '{}' is not running", container_name);
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

        anyhow::bail!(
            "proxy container '{}' did not accept TCP connections on port {} after {} retries",
            container_name,
            self.config.egress.proxy.listen_port,
            PROXY_READINESS_RETRIES
        );
    }

    async fn container_connected_to_network(
        &self,
        container_name: &str,
        network_name: &str,
    ) -> anyhow::Result<bool> {
        let mut inspect = tokio::process::Command::new("docker");
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
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&payload)?)
            .with_context(|| format!("write {}", path.display()))
    }
}

async fn docker_network_exists(name: &str) -> anyhow::Result<bool> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("network").arg("inspect").arg(name);
    let status = cmd.status().await?;
    Ok(status.success())
}

async fn docker_container_inspect(name: &str) -> anyhow::Result<Option<serde_json::Value>> {
    let mut cmd = tokio::process::Command::new("docker");
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
    let mut apex_hosts: Vec<String> = hosts.to_vec();
    apex_hosts.sort();
    apex_hosts.dedup();

    let mut subdomain_hosts = Vec::new();
    for host in &apex_hosts {
        let covered_by_parent = apex_hosts
            .iter()
            .any(|candidate| candidate != host && host.ends_with(&format!(".{candidate}")));
        if !covered_by_parent {
            subdomain_hosts.push(format!(".{host}"));
        }
    }

    (apex_hosts, subdomain_hosts)
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
    use super::{EgressManager, squid_acl_host_groups};
    use crate::egress_config::LocalAgentConfig;

    #[test]
    fn squid_acl_host_groups_include_apex_and_prune_redundant_subdomains() {
        let (apex, subdomains) = squid_acl_host_groups(&[
            "github.com".to_string(),
            "crates.io".to_string(),
            "index.crates.io".to_string(),
        ]);

        assert_eq!(apex, vec!["crates.io", "github.com", "index.crates.io"]);
        assert_eq!(subdomains, vec![".crates.io", ".github.com"]);
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
}
