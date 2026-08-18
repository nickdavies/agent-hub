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
        let network_policy_name = match tier {
            LocalAgentEgressTier::Llm => Some("llm-only"),
            LocalAgentEgressTier::Build => Some("build"),
            LocalAgentEgressTier::Full => None,
        };

        if let Some(policy_name) = network_policy_name
            && let Some(policy) = self.config.network_policy(policy_name)
        {
            let hosts = normalize_hosts(&policy.hosts);
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
            if tier == LocalAgentEgressTier::Llm {
                let expected =
                    normalize_hosts(&self.config.egress.llm.hosts_for_provider(llm_provider));
                validate_literal_model_hosts(&expected)?;
                if hosts != expected {
                    anyhow::bail!(
                        "llm-only hosts must exactly match normalized configured model hosts: policy={hosts:?} configured={expected:?}"
                    );
                }
            }
            return Ok(hosts);
        }

        let llm_hosts = self.config.egress.llm.hosts_for_provider(llm_provider);
        match tier {
            LocalAgentEgressTier::Llm => {
                if llm_hosts.is_empty() {
                    anyhow::bail!(
                        "restricted llm tier requested but no LLM hosts are configured; configure egress.llm.providers in local-agent config"
                    );
                }
                validate_literal_model_hosts(&llm_hosts)?;
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
            rm.arg("network").arg("rm").arg(name);
            run_status(
                &mut rm,
                "docker network rm incompatible owned network failed",
            )
            .await?;
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
        let (owned, compatible) = inspect.as_ref().map_or((false, false), |json| {
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
            let networks = json
                .pointer("/NetworkSettings/Networks")
                .and_then(serde_json::Value::as_object);
            let compatible = owned
                && labels
                    .and_then(|labels| labels.get(EGRESS_KIND_LABEL))
                    .and_then(serde_json::Value::as_str)
                    == Some("proxy")
                && labels
                    .and_then(|labels| labels.get(EGRESS_TIER_LABEL))
                    .and_then(serde_json::Value::as_str)
                    == Some(tier.as_str())
                && labels
                    .and_then(|labels| labels.get(EGRESS_FINGERPRINT_LABEL))
                    .and_then(serde_json::Value::as_str)
                    == Some(fingerprint)
                && json
                    .pointer("/Config/Image")
                    .and_then(serde_json::Value::as_str)
                    == Some(self.config.egress.proxy.image.as_str())
                && networks.is_some_and(|networks| {
                    networks.len() == 2
                        && networks.contains_key(internal_network)
                        && networks.contains_key(external_network)
                });
            (owned, compatible)
        });
        let should_recreate = !compatible;

        if should_recreate && inspect.is_some() {
            if !owned {
                anyhow::bail!(
                    "Docker container '{container_name}' conflicts with restricted egress and is not owned by this worker"
                );
            }
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

async fn docker_network_inspect(name: &str) -> anyhow::Result<Option<serde_json::Value>> {
    let mut cmd = tokio::process::Command::new("docker");
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
        EGRESS_KIND_LABEL, EGRESS_MANAGED_LABEL, EGRESS_WORKER_LABEL, EgressManager,
        RestrictedEgressRuntime, squid_acl_host_groups, validate_literal_model_hosts,
    };
    use crate::egress_config::LocalAgentConfig;

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
    #[ignore = "requires a real Docker daemon and explicitly configured integration images/host"]
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
        if let Ok(proxy_image) = std::env::var("AGENT_HUB_EGRESS_INTEGRATION_PROXY_IMAGE") {
            config.egress.proxy.image = proxy_image;
        }
        let data_dir = std::env::temp_dir().join(format!(
            "agent-hub-egress-integration-{}",
            uuid::Uuid::new_v4()
        ));
        let manager = EgressManager::new(
            &format!("integration-{}", uuid::Uuid::new_v4()),
            &data_dir,
            config,
        )
        .expect("build integration egress manager");
        manager
            .preflight_llm_only()
            .await
            .expect("real Docker preflight must enforce all egress assertions");
        let _ = std::fs::remove_dir_all(data_dir);
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
