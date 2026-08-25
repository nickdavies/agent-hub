use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use sha2::{Digest, Sha256};
use tokio::net::UdpSocket;

use crate::{
    Cancellation, Deadline, ExecutionCancellationContext, RestrictedEgress, execute_output,
    inspect_image_identity,
};

const MANAGED_LABEL: &str = "agent-hub.reddit.com/executor-egress";
const NAMESPACE_LABEL: &str = "agent-hub.reddit.com/executor-namespace";
const KIND_LABEL: &str = "agent-hub.reddit.com/executor-kind";
const FINGERPRINT_LABEL: &str = "agent-hub.reddit.com/executor-fingerprint";
const PROXY_ALIAS: &str = "agent-hub-proxy";
const DOCKER_RESOLVER: &str = "127.0.0.11@53";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyRuntimeSpec {
    pub image: String,
    pub listen_port: u16,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverRuntimeSpec {
    pub image: String,
    pub uid: u32,
    pub gid: u32,
    pub upstream: String,
    pub readiness_host: String,
}

impl ResolverRuntimeSpec {
    pub fn docker_embedded(image: impl Into<String>, uid: u32, gid: u32) -> Self {
        Self {
            image: image.into(),
            uid,
            gid,
            upstream: DOCKER_RESOLVER.to_string(),
            readiness_host: "example.com".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestrictedPolicy {
    Hosts(Vec<String>),
    GuardedWeb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparationBoundary {
    ExternalNetworkCreated,
    ExternalNetworkInspected,
    InternalNetworkCreated,
    InternalNetworkInspected,
    ResolverCreated,
    ResolverAddressInspected,
    ResolverReady,
    ProxyCreated,
    ProxyAttached,
    ProxyAddressInspected,
    ContractsInspected,
    ProxyReady,
}

impl PreparationBoundary {
    pub const ALL: [Self; 12] = [
        Self::ExternalNetworkCreated,
        Self::ExternalNetworkInspected,
        Self::InternalNetworkCreated,
        Self::InternalNetworkInspected,
        Self::ResolverCreated,
        Self::ResolverAddressInspected,
        Self::ResolverReady,
        Self::ProxyCreated,
        Self::ProxyAttached,
        Self::ProxyAddressInspected,
        Self::ContractsInspected,
        Self::ProxyReady,
    ];
}

#[derive(Debug, Clone)]
pub struct RestrictedEgressRequest {
    pub namespace: String,
    pub state_dir: PathBuf,
    pub proxy: ProxyRuntimeSpec,
    pub resolver: ResolverRuntimeSpec,
    pub policies: Vec<RestrictedPolicy>,
    pub cancellation: Cancellation,
    pub external_subnets: Vec<String>,
    pub failure_after: Option<PreparationBoundary>,
}

#[derive(Debug)]
pub struct PreparedRestrictedEgress {
    pub runtime: RestrictedEgress,
    docker: PathBuf,
    namespace: String,
    proxy_name: String,
    resolver_name: String,
    internal_network: String,
    external_network: String,
    config_paths: Vec<PathBuf>,
    effects: ExecutionCancellationContext,
    cleaned: bool,
}

#[derive(Debug)]
struct PartialResources {
    docker: PathBuf,
    namespace: String,
    proxy_name: String,
    resolver_name: String,
    internal_network: String,
    external_network: String,
    config_paths: Vec<PathBuf>,
}

impl RestrictedEgressRequest {
    pub async fn prepare(
        self,
        docker: &Path,
        deadline: &Deadline,
    ) -> anyhow::Result<PreparedRestrictedEgress> {
        self.validate()?;
        check_cancelled(&self.cancellation, "restricted egress preparation")?;
        ensure_private_directory(&self.state_dir)?;
        let effects = ExecutionCancellationContext::new(
            &self.state_dir.join("effects").join(&self.namespace),
            self.cancellation.clone(),
        )?;
        let preparation = effects.begin_external_operation("egress_prepare", &self.namespace)?;

        let proxy_image = inspect_image_identity(docker, &self.proxy.image, deadline)
            .await
            .context("resolve immutable Squid image")?;
        let resolver_image = inspect_image_identity(docker, &self.resolver.image, deadline)
            .await
            .context("resolve immutable Unbound image")?;
        let hosts = exact_host_union(&self.policies)?;
        let guarded_web = matches!(self.policies.as_slice(), [RestrictedPolicy::GuardedWeb]);

        let internal_network = format!("agent-hub-executor-{}-internal", self.namespace);
        let external_network = format!("agent-hub-executor-{}-external", self.namespace);
        let proxy_name = format!("agent-hub-executor-{}-proxy", self.namespace);
        let resolver_name = format!("agent-hub-executor-{}-resolver", self.namespace);
        let unbound_path = self
            .state_dir
            .join(format!("unbound-{}.conf", self.namespace));
        let squid_path = self
            .state_dir
            .join(format!("squid-{}.conf", self.namespace));
        let unbound_config = render_unbound_config(&self.resolver.upstream)?;

        let mut partial = PartialResources {
            docker: docker.to_path_buf(),
            namespace: self.namespace.clone(),
            proxy_name: proxy_name.clone(),
            resolver_name: resolver_name.clone(),
            internal_network: internal_network.clone(),
            external_network: external_network.clone(),
            config_paths: vec![unbound_path.clone(), squid_path.clone()],
        };
        let cleanup_deadline = Deadline::new(Some(30))?;

        let operation = async {
            write_private_file(&unbound_path, unbound_config.as_bytes())?;
            create_network(
                docker,
                &external_network,
                false,
                &self.external_subnets,
                &self.namespace,
                &self.cancellation,
                self.failure_after,
                PreparationBoundary::ExternalNetworkCreated,
                PreparationBoundary::ExternalNetworkInspected,
                deadline,
            )
            .await?;
            create_network(
                docker,
                &internal_network,
                true,
                &[],
                &self.namespace,
                &self.cancellation,
                self.failure_after,
                PreparationBoundary::InternalNetworkCreated,
                PreparationBoundary::InternalNetworkInspected,
                deadline,
            )
            .await?;

            let resolver_fingerprint = fingerprint(&[
                unbound_config.as_bytes(),
                resolver_image.as_bytes(),
                self.resolver.upstream.as_bytes(),
                self.resolver.readiness_host.as_bytes(),
            ]);
            run_resolver(
                docker,
                &resolver_name,
                &external_network,
                &unbound_path,
                &resolver_image,
                &self.resolver,
                &resolver_fingerprint,
                &self.namespace,
                &self.cancellation,
                deadline,
            )
            .await?;
            inject_failure(self.failure_after, PreparationBoundary::ResolverCreated)?;
            let resolver_ip = inspect_network_ip(
                docker,
                &resolver_name,
                &external_network,
                &self.cancellation,
                deadline,
            )
            .await?;
            inject_failure(
                self.failure_after,
                PreparationBoundary::ResolverAddressInspected,
            )?;
            wait_for_dns(
                &resolver_ip,
                &self.resolver.readiness_host,
                &self.cancellation,
                deadline,
            )
            .await?;
            inject_failure(self.failure_after, PreparationBoundary::ResolverReady)?;

            let squid_config =
                render_squid_config(self.proxy.listen_port, &resolver_ip, &hosts, guarded_web);
            write_private_file(&squid_path, squid_config.as_bytes())?;
            let proxy_fingerprint = fingerprint(&[
                squid_config.as_bytes(),
                proxy_image.as_bytes(),
                resolver_ip.as_bytes(),
                b"net.ipv6.conf.all.disable_ipv6=1",
            ]);
            run_proxy(
                docker,
                &proxy_name,
                &internal_network,
                &squid_path,
                &proxy_image,
                &self.proxy,
                &proxy_fingerprint,
                &self.namespace,
                &self.cancellation,
                deadline,
            )
            .await?;
            inject_failure(self.failure_after, PreparationBoundary::ProxyCreated)?;
            connect_network(
                docker,
                &external_network,
                &proxy_name,
                &self.cancellation,
                deadline,
            )
            .await?;
            inject_failure(self.failure_after, PreparationBoundary::ProxyAttached)?;
            let proxy_ip = inspect_network_ip(
                docker,
                &proxy_name,
                &internal_network,
                &self.cancellation,
                deadline,
            )
            .await?;
            inject_failure(
                self.failure_after,
                PreparationBoundary::ProxyAddressInspected,
            )?;
            verify_container_contract(
                docker,
                &resolver_name,
                ContainerContract {
                    kind: "resolver",
                    namespace: &self.namespace,
                    fingerprint: &resolver_fingerprint,
                    image: &resolver_image,
                    user: &format!("{}:{}", self.resolver.uid, self.resolver.gid),
                    network_mode: &external_network,
                    networks: &[external_network.as_str()],
                    config_path: &unbound_path,
                    config_destination: "/etc/unbound/unbound.conf",
                    cap_add: Some("NET_BIND_SERVICE"),
                    sysctl: None,
                },
                deadline,
            )
            .await?;
            inject_failure(self.failure_after, PreparationBoundary::ContractsInspected)?;
            verify_container_contract(
                docker,
                &proxy_name,
                ContainerContract {
                    kind: "proxy",
                    namespace: &self.namespace,
                    fingerprint: &proxy_fingerprint,
                    image: &proxy_image,
                    user: &format!("{}:{}", self.proxy.uid, self.proxy.gid),
                    network_mode: &internal_network,
                    networks: &[internal_network.as_str(), external_network.as_str()],
                    config_path: &squid_path,
                    config_destination: "/etc/squid/squid.conf",
                    cap_add: None,
                    sysctl: Some(("net.ipv6.conf.all.disable_ipv6", "1")),
                },
                deadline,
            )
            .await?;
            inject_failure(self.failure_after, PreparationBoundary::ProxyReady)?;
            check_cancelled(&self.cancellation, "restricted egress finalization")?;
            wait_for_proxy(
                docker,
                &internal_network,
                &proxy_ip,
                self.proxy.listen_port,
                &resolver_image,
                &self.cancellation,
                deadline,
            )
            .await?;
            Ok::<_, anyhow::Error>((proxy_ip, resolver_ip))
        }
        .await;

        match operation {
            Ok((proxy_ip, _resolver_ip)) => {
                preparation.completed()?;
                Ok(PreparedRestrictedEgress {
                    runtime: RestrictedEgress {
                        network_name: internal_network.clone(),
                        proxy_url: format!("http://{PROXY_ALIAS}:{}", self.proxy.listen_port),
                        no_proxy: "localhost,127.0.0.1,::1,.local".to_string(),
                        proxy_host: PROXY_ALIAS.to_string(),
                        proxy_ip,
                        allowed_hosts: hosts,
                    },
                    docker: docker.to_path_buf(),
                    namespace: self.namespace,
                    proxy_name,
                    resolver_name,
                    internal_network,
                    external_network,
                    config_paths: vec![unbound_path, squid_path],
                    effects,
                    cleaned: false,
                })
            }
            Err(error) => match partial.cleanup(&cleanup_deadline).await {
                Ok(()) => {
                    preparation.reconciled()?;
                    effects.finalize()?;
                    Err(error)
                }
                Err(cleanup) => {
                    preparation.cleanup_failed("egress_prepare_cleanup_failed")?;
                    Err(anyhow::anyhow!(
                        "restricted egress preparation failed: {error:#}; cleanup also failed: {cleanup:#}"
                    ))
                }
            },
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        validate_namespace(&self.namespace)?;
        validate_image_reference(&self.proxy.image, "Squid")?;
        validate_image_reference(&self.resolver.image, "Unbound")?;
        anyhow::ensure!(self.proxy.listen_port != 0, "restricted proxy port is zero");
        anyhow::ensure!(
            !self.policies.is_empty(),
            "restricted egress policy is empty"
        );
        let guarded = self
            .policies
            .iter()
            .filter(|policy| matches!(policy, RestrictedPolicy::GuardedWeb))
            .count();
        anyhow::ensure!(
            guarded == 0 || (guarded == 1 && self.policies.len() == 1),
            "guarded_web cannot be combined with other network resources"
        );
        validate_upstream(&self.resolver.upstream)
            .and_then(|()| validate_dns_name(&self.resolver.readiness_host))
            .and_then(|()| {
                self.external_subnets
                    .iter()
                    .try_for_each(|subnet| validate_subnet(subnet))
            })
    }
}

impl PreparedRestrictedEgress {
    pub async fn cleanup(&mut self, deadline: &Deadline) -> anyhow::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        let cleanup = self
            .effects
            .begin_external_operation("egress_cleanup", &self.namespace)?;
        let mut partial = PartialResources {
            docker: self.docker.clone(),
            namespace: self.namespace.clone(),
            proxy_name: self.proxy_name.clone(),
            resolver_name: self.resolver_name.clone(),
            internal_network: self.internal_network.clone(),
            external_network: self.external_network.clone(),
            config_paths: self.config_paths.clone(),
        };
        match partial.cleanup(deadline).await {
            Ok(()) => {
                self.cleaned = true;
                cleanup.reconciled()?;
                self.effects.finalize()
            }
            Err(error) => {
                cleanup.cleanup_failed("egress_cleanup_failed")?;
                Err(error)
            }
        }
    }
}

impl PartialResources {
    async fn cleanup(&mut self, deadline: &Deadline) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        if let Err(error) =
            remove_owned_container(&self.docker, &self.proxy_name, &self.namespace, deadline).await
        {
            failures.push(format!("proxy: {error:#}"));
        }
        if let Err(error) =
            remove_owned_container(&self.docker, &self.resolver_name, &self.namespace, deadline)
                .await
        {
            failures.push(format!("resolver: {error:#}"));
        }
        for network in [&self.internal_network, &self.external_network] {
            if let Err(error) =
                remove_owned_network(&self.docker, network, &self.namespace, deadline).await
            {
                failures.push(format!("network {network}: {error:#}"));
            }
        }
        for path in &self.config_paths {
            if let Err(error) = std::fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                failures.push(format!("config {}: {error}", path.display()));
            }
        }
        anyhow::ensure!(
            failures.is_empty(),
            "restricted egress cleanup failed: {}",
            failures.join("; ")
        );
        Ok(())
    }
}

fn exact_host_union(policies: &[RestrictedPolicy]) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        !policies
            .iter()
            .any(|policy| matches!(policy, RestrictedPolicy::GuardedWeb))
            || matches!(policies, [RestrictedPolicy::GuardedWeb]),
        "guarded_web cannot be combined with other network resources"
    );
    if matches!(policies, [RestrictedPolicy::GuardedWeb]) {
        return Ok(Vec::new());
    }
    let hosts = policies
        .iter()
        .filter_map(|policy| match policy {
            RestrictedPolicy::Hosts(hosts) => Some(hosts.as_slice()),
            RestrictedPolicy::GuardedWeb => None,
        })
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    anyhow::ensure!(!hosts.is_empty(), "restricted egress host union is empty");
    Ok(hosts)
}

#[allow(clippy::too_many_arguments)]
async fn create_network(
    docker: &Path,
    name: &str,
    internal: bool,
    subnets: &[String],
    namespace: &str,
    cancellation: &Cancellation,
    failure_after: Option<PreparationBoundary>,
    created_boundary: PreparationBoundary,
    inspected_boundary: PreparationBoundary,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    check_cancelled(cancellation, "network creation")?;
    let mut args = vec![
        "network".to_string(),
        "create".to_string(),
        "--driver=bridge".to_string(),
    ];
    if internal {
        args.push("--internal".to_string());
    }
    for subnet in subnets {
        args.extend(["--subnet".to_string(), subnet.clone()]);
    }
    let subnet_fingerprint = fingerprint(&[subnets.join(",").as_bytes()]);
    append_labels(&mut args, namespace, "network", &subnet_fingerprint);
    args.push(name.to_string());
    require_success(
        execute_output(docker, &args, deadline, "create restricted network").await?,
        &format!("create restricted network '{name}'"),
    )?;
    inject_failure(failure_after, created_boundary)?;
    verify_network_contract(
        docker,
        name,
        namespace,
        &subnet_fingerprint,
        internal,
        subnets,
        deadline,
    )
    .await?;
    inject_failure(failure_after, inspected_boundary)
}

fn inject_failure(
    failure_after: Option<PreparationBoundary>,
    boundary: PreparationBoundary,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        failure_after != Some(boundary),
        "injected restricted egress failure after {boundary:?}"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_resolver(
    docker: &Path,
    name: &str,
    network: &str,
    config: &Path,
    image: &str,
    resolver: &ResolverRuntimeSpec,
    fingerprint: &str,
    namespace: &str,
    cancellation: &Cancellation,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    check_cancelled(cancellation, "Unbound start")?;
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--network".to_string(),
        network.to_string(),
        "--security-opt=no-new-privileges".to_string(),
        "--cap-drop=ALL".to_string(),
        "--cap-add=NET_BIND_SERVICE".to_string(),
        "--read-only".to_string(),
        "--user".to_string(),
        format!("{}:{}", resolver.uid, resolver.gid),
        "--tmpfs".to_string(),
        format!(
            "/run:rw,nosuid,nodev,size=16m,uid={},gid={}",
            resolver.uid, resolver.gid
        ),
        "--tmpfs".to_string(),
        format!(
            "/var/lib/unbound:rw,nosuid,nodev,size=16m,uid={},gid={}",
            resolver.uid, resolver.gid
        ),
    ];
    append_labels(&mut args, namespace, "resolver", fingerprint);
    args.extend([
        "-v".to_string(),
        format!("{}:/etc/unbound/unbound.conf:ro", config.display()),
        "--entrypoint=/usr/sbin/unbound".to_string(),
        image.to_string(),
        "-d".to_string(),
        "-c".to_string(),
        "/etc/unbound/unbound.conf".to_string(),
    ]);
    require_success(
        execute_output(docker, &args, deadline, "start Unbound resolver").await?,
        "start Unbound resolver",
    )
}

#[allow(clippy::too_many_arguments)]
async fn run_proxy(
    docker: &Path,
    name: &str,
    network: &str,
    config: &Path,
    image: &str,
    proxy: &ProxyRuntimeSpec,
    fingerprint: &str,
    namespace: &str,
    cancellation: &Cancellation,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    check_cancelled(cancellation, "Squid start")?;
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--network".to_string(),
        network.to_string(),
        "--sysctl".to_string(),
        "net.ipv6.conf.all.disable_ipv6=1".to_string(),
        "--security-opt=no-new-privileges".to_string(),
        "--cap-drop=ALL".to_string(),
        "--read-only".to_string(),
        "--user".to_string(),
        format!("{}:{}", proxy.uid, proxy.gid),
        "--tmpfs".to_string(),
        format!(
            "/tmp:rw,nosuid,nodev,size=256m,uid={},gid={}",
            proxy.uid, proxy.gid
        ),
        "--tmpfs".to_string(),
        format!(
            "/run:rw,nosuid,nodev,size=64m,uid={},gid={}",
            proxy.uid, proxy.gid
        ),
        "--tmpfs".to_string(),
        format!(
            "/var/spool/squid:rw,nosuid,nodev,size=1g,uid={},gid={}",
            proxy.uid, proxy.gid
        ),
        "--tmpfs".to_string(),
        format!(
            "/var/cache/squid:rw,nosuid,nodev,size=1g,uid={},gid={}",
            proxy.uid, proxy.gid
        ),
        "--tmpfs".to_string(),
        format!(
            "/var/log/squid:rw,nosuid,nodev,size=256m,uid={},gid={}",
            proxy.uid, proxy.gid
        ),
    ];
    append_labels(&mut args, namespace, "proxy", fingerprint);
    args.extend([
        "-v".to_string(),
        format!("{}:/etc/squid/squid.conf:ro", config.display()),
        image.to_string(),
    ]);
    require_success(
        execute_output(docker, &args, deadline, "start Squid proxy").await?,
        "start Squid proxy",
    )
}

async fn connect_network(
    docker: &Path,
    network: &str,
    container: &str,
    cancellation: &Cancellation,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    check_cancelled(cancellation, "proxy network attachment")?;
    let args = vec![
        "network".to_string(),
        "connect".to_string(),
        network.to_string(),
        container.to_string(),
    ];
    require_success(
        execute_output(docker, &args, deadline, "connect Squid external network").await?,
        "connect Squid external network",
    )
}

async fn inspect_network_ip(
    docker: &Path,
    name: &str,
    network: &str,
    cancellation: &Cancellation,
    deadline: &Deadline,
) -> anyhow::Result<String> {
    let args = vec![
        "inspect".to_string(),
        "--format".to_string(),
        format!("{{{{(index .NetworkSettings.Networks \"{network}\").IPAddress}}}}"),
        name.to_string(),
    ];
    for _ in 0..50 {
        check_cancelled(cancellation, "container network inspection")?;
        let output = execute_output(docker, &args, deadline, "inspect enforcement address").await?;
        if output.status.success() {
            let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if ip.parse::<IpAddr>().is_ok() {
                return Ok(ip);
            }
        }
        sleep_bounded(deadline, Duration::from_millis(100)).await?;
    }
    let logs = execute_output(
        docker,
        &[
            "logs".to_string(),
            "--tail=80".to_string(),
            name.to_string(),
        ],
        deadline,
        "read enforcement logs",
    )
    .await?;
    anyhow::bail!(
        "enforcement container has no valid network address: stdout={} stderr={}",
        String::from_utf8_lossy(&logs.stdout).trim(),
        String::from_utf8_lossy(&logs.stderr).trim()
    )
}

struct ContainerContract<'a> {
    kind: &'a str,
    namespace: &'a str,
    fingerprint: &'a str,
    image: &'a str,
    user: &'a str,
    network_mode: &'a str,
    networks: &'a [&'a str],
    config_path: &'a Path,
    config_destination: &'a str,
    cap_add: Option<&'a str>,
    sysctl: Option<(&'a str, &'a str)>,
}

async fn verify_container_contract(
    docker: &Path,
    name: &str,
    contract: ContainerContract<'_>,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    let args = vec!["inspect".to_string(), name.to_string()];
    let output = execute_output(docker, &args, deadline, "inspect enforcement container").await?;
    anyhow::ensure!(
        output.status.success(),
        "inspect enforcement container failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let values: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    let value = values
        .first()
        .context("Docker inspect returned no container")?;
    let labels = value
        .pointer("/Config/Labels")
        .and_then(serde_json::Value::as_object);
    anyhow::ensure!(
        labels
            .and_then(|labels| labels.get(MANAGED_LABEL))
            .and_then(serde_json::Value::as_str)
            == Some("true")
            && labels
                .and_then(|labels| labels.get(NAMESPACE_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some(contract.namespace)
            && labels
                .and_then(|labels| labels.get(KIND_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some(contract.kind)
            && labels
                .and_then(|labels| labels.get(FINGERPRINT_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some(contract.fingerprint),
        "enforcement container ownership/fingerprint mismatch"
    );
    anyhow::ensure!(
        value
            .pointer("/Config/Image")
            .and_then(serde_json::Value::as_str)
            == Some(contract.image),
        "enforcement image identity mismatch"
    );
    anyhow::ensure!(
        value
            .pointer("/Config/User")
            .and_then(serde_json::Value::as_str)
            == Some(contract.user),
        "enforcement user mismatch"
    );
    anyhow::ensure!(
        value
            .pointer("/HostConfig/ReadonlyRootfs")
            .and_then(serde_json::Value::as_bool)
            == Some(true),
        "enforcement root is writable"
    );
    anyhow::ensure!(
        value
            .pointer("/HostConfig/NetworkMode")
            .and_then(serde_json::Value::as_str)
            == Some(contract.network_mode),
        "enforcement initial network mismatch"
    );
    let security = value
        .pointer("/HostConfig/SecurityOpt")
        .and_then(serde_json::Value::as_array)
        .context("enforcement security options missing")?;
    anyhow::ensure!(
        security.iter().any(|item| matches!(
            item.as_str(),
            Some("no-new-privileges" | "no-new-privileges:true")
        )),
        "no-new-privileges is absent"
    );
    let cap_drop = value
        .pointer("/HostConfig/CapDrop")
        .and_then(serde_json::Value::as_array)
        .context("cap-drop is absent")?;
    anyhow::ensure!(
        cap_drop.len() == 1
            && cap_drop[0]
                .as_str()
                .is_some_and(|cap| cap.eq_ignore_ascii_case("ALL")),
        "cap-drop is not exact"
    );
    let cap_add = value
        .pointer("/HostConfig/CapAdd")
        .and_then(serde_json::Value::as_array);
    match contract.cap_add {
        Some(expected) => anyhow::ensure!(
            cap_add.is_some_and(|caps| caps.len() == 1
                && caps[0].as_str().is_some_and(|cap| {
                    cap.eq_ignore_ascii_case(expected)
                        || cap
                            .strip_prefix("CAP_")
                            .is_some_and(|cap| cap.eq_ignore_ascii_case(expected))
                })),
            "cap-add is not exact"
        ),
        None => anyhow::ensure!(
            cap_add.is_none() || cap_add.is_some_and(Vec::is_empty),
            "unexpected capability added"
        ),
    }
    let networks = value
        .pointer("/NetworkSettings/Networks")
        .and_then(serde_json::Value::as_object)
        .context("network attachments missing")?;
    let actual = networks.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = contract.networks.iter().copied().collect::<BTreeSet<_>>();
    anyhow::ensure!(
        actual == expected,
        "enforcement network topology mismatch: actual={actual:?} expected={expected:?}"
    );
    let (uid, gid) = contract
        .user
        .split_once(':')
        .context("enforcement user is not UID:GID")?;
    let expected_tmpfs = expected_tmpfs(contract.kind, uid, gid);
    let actual_tmpfs = value
        .pointer("/HostConfig/Tmpfs")
        .and_then(serde_json::Value::as_object)
        .context("enforcement tmpfs contract is missing")?;
    anyhow::ensure!(
        actual_tmpfs.len() == expected_tmpfs.len()
            && expected_tmpfs.iter().all(|(path, expected)| {
                actual_tmpfs
                    .get(path)
                    .and_then(serde_json::Value::as_str)
                    .map(option_set)
                    .as_ref()
                    == Some(expected)
            }),
        "enforcement tmpfs contract mismatch"
    );
    let mounts = value
        .get("Mounts")
        .and_then(serde_json::Value::as_array)
        .context("enforcement mounts missing")?;
    anyhow::ensure!(
        mounts.len() == 1
            && mounts[0].get("Type").and_then(serde_json::Value::as_str) == Some("bind")
            && mounts[0].get("Source").and_then(serde_json::Value::as_str)
                == contract.config_path.to_str()
            && mounts[0]
                .get("Destination")
                .and_then(serde_json::Value::as_str)
                == Some(contract.config_destination)
            && mounts[0].get("RW").and_then(serde_json::Value::as_bool) == Some(false),
        "enforcement config mount mismatch"
    );
    let sysctls = value
        .pointer("/HostConfig/Sysctls")
        .and_then(serde_json::Value::as_object);
    match contract.sysctl {
        Some((key, expected)) => anyhow::ensure!(
            sysctls
                .and_then(|values| values.get(key))
                .and_then(serde_json::Value::as_str)
                == Some(expected),
            "enforcement sysctl mismatch"
        ),
        None => anyhow::ensure!(
            sysctls.is_none_or(serde_json::Map::is_empty),
            "unexpected enforcement sysctl"
        ),
    }
    Ok(())
}

fn expected_tmpfs(kind: &str, uid: &str, gid: &str) -> BTreeMap<String, BTreeSet<String>> {
    let entries = match kind {
        "resolver" => vec![("/run", "size=16m"), ("/var/lib/unbound", "size=16m")],
        "proxy" => vec![
            ("/tmp", "size=256m"),
            ("/run", "size=64m"),
            ("/var/spool/squid", "size=1g"),
            ("/var/cache/squid", "size=1g"),
            ("/var/log/squid", "size=256m"),
        ],
        _ => Vec::new(),
    };
    entries
        .into_iter()
        .map(|(path, size)| {
            let options = vec![
                "rw".to_string(),
                "nosuid".to_string(),
                "nodev".to_string(),
                size.to_string(),
                format!("uid={uid}"),
                format!("gid={gid}"),
            ];
            (path.to_string(), options.into_iter().collect())
        })
        .collect()
}

fn option_set(options: &str) -> BTreeSet<String> {
    options.split(',').map(str::to_string).collect()
}

async fn verify_network_contract(
    docker: &Path,
    name: &str,
    namespace: &str,
    fingerprint: &str,
    internal: bool,
    expected_subnets: &[String],
    deadline: &Deadline,
) -> anyhow::Result<()> {
    let args = vec![
        "network".to_string(),
        "inspect".to_string(),
        name.to_string(),
    ];
    let output = execute_output(docker, &args, deadline, "inspect enforcement network").await?;
    anyhow::ensure!(
        output.status.success(),
        "inspect enforcement network failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let values: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    let value = values
        .first()
        .context("Docker inspect returned no network")?;
    if !expected_subnets.is_empty() {
        let actual_subnets = value
            .pointer("/IPAM/Config")
            .and_then(serde_json::Value::as_array)
            .context("enforcement network IPAM config is missing")?
            .iter()
            .filter_map(|entry| entry.get("Subnet").and_then(serde_json::Value::as_str))
            .collect::<BTreeSet<_>>();
        let expected_subnets = expected_subnets
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        anyhow::ensure!(
            actual_subnets == expected_subnets,
            "enforcement network subnet mismatch"
        );
    }
    let labels = value.get("Labels").and_then(serde_json::Value::as_object);
    anyhow::ensure!(
        value.get("Internal").and_then(serde_json::Value::as_bool) == Some(internal)
            && value.get("Driver").and_then(serde_json::Value::as_str) == Some("bridge")
            && value.get("Scope").and_then(serde_json::Value::as_str) == Some("local")
            && labels
                .and_then(|labels| labels.get(MANAGED_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some("true")
            && labels
                .and_then(|labels| labels.get(NAMESPACE_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some(namespace)
            && labels
                .and_then(|labels| labels.get(KIND_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some("network")
            && labels
                .and_then(|labels| labels.get(FINGERPRINT_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some(fingerprint),
        "enforcement network contract mismatch"
    );
    Ok(())
}

async fn wait_for_proxy(
    docker: &Path,
    network: &str,
    proxy_ip: &str,
    port: u16,
    image: &str,
    cancellation: &Cancellation,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    let mut diagnostic = String::new();
    for _ in 0..50 {
        check_cancelled(cancellation, "Squid readiness")?;
        let args = vec![
            "run".to_string(),
            "--rm".to_string(),
            format!("--network={network}"),
            "--entrypoint=curl".to_string(),
            image.to_string(),
            "--silent".to_string(),
            "--show-error".to_string(),
            "--noproxy".to_string(),
            "*".to_string(),
            "--connect-timeout".to_string(),
            "1".to_string(),
            "--max-time".to_string(),
            "2".to_string(),
            "-o".to_string(),
            "/dev/null".to_string(),
            format!("http://{proxy_ip}:{port}/"),
        ];
        let output = execute_output(docker, &args, deadline, "probe Squid readiness").await?;
        if output.status.success() {
            return Ok(());
        }
        diagnostic = format!(
            "status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        sleep_bounded(deadline, Duration::from_millis(100)).await?;
    }
    anyhow::bail!("Squid readiness failed: {diagnostic}")
}

async fn wait_for_dns(
    resolver_ip: &str,
    readiness_host: &str,
    cancellation: &Cancellation,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    let address = SocketAddr::new(resolver_ip.parse()?, 53);
    let query = dns_query(0x4a48, readiness_host, 1)?;
    for _ in 0..50 {
        check_cancelled(cancellation, "Unbound readiness")?;
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.send_to(&query, address).await?;
        let mut response = [0u8; 2048];
        let attempt = deadline
            .remaining()
            .context("DNS readiness requires deadline")?
            .min(Duration::from_secs(1));
        if let Ok(Ok((length, _))) =
            tokio::time::timeout(attempt, socket.recv_from(&mut response)).await
            && successful_dns_response(&response[..length], 0x4a48)
        {
            return Ok(());
        }
        sleep_bounded(deadline, Duration::from_millis(100)).await?;
    }
    anyhow::bail!("Unbound readiness failed")
}

fn dns_query(id: u16, name: &str, record_type: u16) -> anyhow::Result<Vec<u8>> {
    let mut query = Vec::from(id.to_be_bytes());
    query.extend_from_slice(&[0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    for label in name.split('.') {
        anyhow::ensure!(
            !label.is_empty() && label.len() <= 63,
            "invalid DNS query name"
        );
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
    }
    query.push(0);
    query.extend_from_slice(&record_type.to_be_bytes());
    query.extend_from_slice(&1u16.to_be_bytes());
    Ok(query)
}

fn valid_dns_response(response: &[u8], id: u16) -> bool {
    response.len() >= 12
        && response[0..2] == id.to_be_bytes()
        && response[2] & 0x80 != 0
        && response[2] & 0x02 == 0
        && response[3] & 0x0f <= 3
}

fn successful_dns_response(response: &[u8], id: u16) -> bool {
    valid_dns_response(response, id)
        && response[3] & 0x0f == 0
        && u16::from_be_bytes([response[6], response[7]]) > 0
}

async fn remove_owned_container(
    docker: &Path,
    name: &str,
    namespace: &str,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    reconcile_owned_absent(docker, "container", name, namespace, deadline).await
}

async fn remove_owned_network(
    docker: &Path,
    name: &str,
    namespace: &str,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    reconcile_owned_absent(docker, "network", name, namespace, deadline).await
}

async fn reconcile_owned_absent(
    docker: &Path,
    kind: &str,
    name: &str,
    namespace: &str,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    let mut failures = Vec::new();
    loop {
        let owned = ensure_owned(docker, kind, name, namespace, deadline).await?;
        if !owned {
            sleep_bounded(deadline, Duration::from_millis(100)).await?;
            if !ensure_owned(docker, kind, name, namespace, deadline).await? {
                return Ok(());
            }
            failures.push("resource appeared between absence proofs".to_string());
            continue;
        }
        let args = if kind == "container" {
            vec![
                "rm".to_string(),
                "-f".to_string(),
                "-v".to_string(),
                name.to_string(),
            ]
        } else {
            vec!["network".to_string(), "rm".to_string(), name.to_string()]
        };
        let output = execute_output(docker, &args, deadline, "remove enforcement resource").await?;
        if !output.status.success() {
            failures.push(format!(
                "remove: {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        anyhow::ensure!(
            !deadline.is_expired(),
            "could not prove enforcement resource '{name}' absent: {}",
            failures.join("; ")
        );
        sleep_bounded(deadline, Duration::from_millis(50)).await?;
    }
}

async fn ensure_owned(
    docker: &Path,
    kind: &str,
    name: &str,
    namespace: &str,
    deadline: &Deadline,
) -> anyhow::Result<bool> {
    let args = vec![kind.to_string(), "inspect".to_string(), name.to_string()];
    let output = execute_output(docker, &args, deadline, "inspect enforcement ownership").await?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        if error.contains("no such") || error.contains("not found") {
            return Ok(false);
        }
        anyhow::bail!(
            "inspect enforcement ownership failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let values: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    let labels = values
        .first()
        .and_then(|value| {
            if kind == "container" {
                value.pointer("/Config/Labels")
            } else {
                value.get("Labels")
            }
        })
        .and_then(serde_json::Value::as_object);
    let expected_kind = if kind == "network" {
        "network"
    } else if name.ends_with("-proxy") {
        "proxy"
    } else if name.ends_with("-resolver") {
        "resolver"
    } else {
        anyhow::bail!("unknown deterministic enforcement container name '{name}'");
    };
    anyhow::ensure!(
        labels
            .and_then(|labels| labels.get(MANAGED_LABEL))
            .and_then(serde_json::Value::as_str)
            == Some("true")
            && labels
                .and_then(|labels| labels.get(NAMESPACE_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some(namespace)
            && labels
                .and_then(|labels| labels.get(KIND_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some(expected_kind),
        "refusing to remove enforcement resource '{name}' with mismatched ownership"
    );
    Ok(true)
}

fn render_unbound_config(upstream: &str) -> anyhow::Result<String> {
    validate_upstream(upstream)?;
    let blocked = blocked_ranges();
    let mut config = String::from(
        "server:\n  verbosity: 1\n  interface: 0.0.0.0\n  port: 53\n  username: \"\"\n  chroot: \"\"\n  directory: \"/\"\n  pidfile: \"/run/unbound.pid\"\n  module-config: \"iterator\"\n  do-ip4: yes\n  do-ip6: no\n  do-udp: yes\n  do-tcp: yes\n  tcp-upstream: no\n  access-control: 0.0.0.0/0 allow\n  hide-identity: yes\n  hide-version: yes\n  harden-short-bufsize: yes\n  harden-large-queries: yes\n  harden-glue: yes\n  harden-dnssec-stripped: no\n  harden-below-nxdomain: yes\n  harden-referral-path: yes\n  qname-minimisation: yes\n  use-caps-for-id: yes\n  serve-expired: no\n  cache-min-ttl: 0\n  cache-max-ttl: 1\n  infra-cache-min-rtt: 50\n  unwanted-reply-threshold: 1\n",
    );
    config.push_str("  use-syslog: no\n  logfile: \"\"\n");
    if upstream.starts_with("127.") {
        config.push_str("  do-not-query-localhost: no\n");
    }
    for range in blocked {
        config.push_str(&format!("  private-address: {range}\n"));
    }
    config.push_str(
        "forward-zone:\n  name: \".\"\n  forward-first: no\n  forward-tls-upstream: no\n",
    );
    config.push_str(&format!("  forward-addr: {upstream}\n"));
    Ok(config)
}

fn render_squid_config(
    port: u16,
    resolver_ip: &str,
    hosts: &[String],
    guarded_web: bool,
) -> String {
    let mut config = format!(
        "http_port 0.0.0.0:{port}\nvisible_hostname agent-hub-egress\nhosts_file none\ndns_nameservers {resolver_ip}\npositive_dns_ttl 1 seconds\nnegative_dns_ttl 1 seconds\nconnect_timeout 10 seconds\nforward_timeout 15 seconds\n"
    );
    config.push_str("acl SSL_ports port 443\nacl Safe_ports port 80 443\nacl CONNECT method CONNECT\nacl HTTP_protocol proto HTTP HTTPS\n");
    config.push_str("acl valid_public_hostname dstdom_regex -n -i ^([a-z0-9]([a-z0-9-]*[a-z0-9])?\\.)+[a-z]([a-z0-9-]*[a-z0-9])?$\n");
    config.push_str("acl blocked_destination dst");
    for range in blocked_ranges() {
        config.push(' ');
        config.push_str(range);
    }
    config.push('\n');
    config.push_str("http_access deny !Safe_ports\nhttp_access deny CONNECT !SSL_ports\nhttp_access deny !HTTP_protocol !CONNECT\nhttp_access deny !valid_public_hostname\nhttp_access deny blocked_destination\n");
    if guarded_web {
        config.push_str("http_access allow valid_public_hostname\n");
    } else {
        config.push_str("acl allowed_exact dstdomain -n");
        for host in hosts {
            config.push(' ');
            config.push_str(host);
        }
        config.push_str("\nhttp_access allow allowed_exact\n");
    }
    config.push_str("http_access deny all\ncache deny all\nforwarded_for delete\nrequest_header_access X-Forwarded-For deny all\naccess_log stdio:/dev/stderr\ncache_log stdio:/dev/stderr\n");
    config
}

fn blocked_ranges() -> &'static [&'static str] {
    &[
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.0.0.0/24",
        "192.0.2.0/24",
        "192.31.196.0/24",
        "192.52.193.0/24",
        "192.88.99.0/24",
        "192.168.0.0/16",
        "192.175.48.0/24",
        "198.18.0.0/15",
        "198.51.100.0/24",
        "203.0.113.0/24",
        "224.0.0.0/4",
        "240.0.0.0/4",
        "::/128",
        "::1/128",
        "::ffff:0:0/96",
        "64:ff9b::/96",
        "64:ff9b:1::/48",
        "100::/64",
        "2001::/23",
        "2001:db8::/32",
        "2002::/16",
        "3fff::/20",
        "5f00::/16",
        "fc00::/7",
        "fe80::/10",
        "ff00::/8",
    ]
}

fn append_labels(args: &mut Vec<String>, namespace: &str, kind: &str, fingerprint: &str) {
    for (key, value) in [
        (MANAGED_LABEL, "true"),
        (NAMESPACE_LABEL, namespace),
        (KIND_LABEL, kind),
        (FINGERPRINT_LABEL, fingerprint),
    ] {
        args.extend(["--label".to_string(), format!("{key}={value}")]);
    }
}

fn fingerprint(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_namespace(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 40
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "executor egress namespace must be a lowercase label-safe identifier"
    );
    Ok(())
}

fn validate_image_reference(value: &str, kind: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.trim() == value,
        "{kind} image is invalid"
    );
    Ok(())
}

fn validate_upstream(value: &str) -> anyhow::Result<()> {
    let (address, port) = value
        .rsplit_once('@')
        .context("Unbound upstream must include @port")?;
    anyhow::ensure!(
        address.parse::<IpAddr>().is_ok(),
        "Unbound upstream address is not an IP literal"
    );
    anyhow::ensure!(
        port.parse::<u16>().is_ok_and(|port| port != 0),
        "Unbound upstream port is invalid"
    );
    Ok(())
}

fn validate_dns_name(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() <= 253
            && value.contains('.')
            && value.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            }),
        "resolver readiness host is invalid"
    );
    Ok(())
}

fn validate_subnet(value: &str) -> anyhow::Result<()> {
    let (address, prefix) = value
        .split_once('/')
        .context("external subnet must use CIDR notation")?;
    let address: IpAddr = address
        .parse()
        .context("external subnet address is invalid")?;
    let prefix: u8 = prefix
        .parse()
        .context("external subnet prefix is invalid")?;
    anyhow::ensure!(
        match address {
            IpAddr::V4(_) => prefix <= 32,
            IpAddr::V6(_) => prefix <= 128,
        },
        "external subnet prefix is invalid"
    );
    Ok(())
}

fn ensure_private_directory(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "egress state directory is unsafe"
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "egress config destination is unsafe"
        );
    }
    std::fs::write(path, bytes)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    Ok(())
}

fn check_cancelled(cancellation: &Cancellation, operation: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!cancellation.requested(), "{operation} cancelled");
    Ok(())
}

async fn sleep_bounded(deadline: &Deadline, duration: Duration) -> anyhow::Result<()> {
    let remaining = deadline
        .remaining()
        .context("operation requires deadline")?;
    anyhow::ensure!(!remaining.is_zero(), "operation deadline exceeded");
    tokio::time::sleep(duration.min(remaining)).await;
    Ok(())
}

fn require_success(output: std::process::Output, operation: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        output.status.success(),
        "{operation} failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbound_filters_every_private_special_and_mapped_range() {
        let config = render_unbound_config(DOCKER_RESOLVER).unwrap();
        for range in blocked_ranges() {
            assert!(config.contains(&format!("private-address: {range}")));
        }
        assert!(config.contains("do-ip6: no"));
        assert!(config.contains("forward-first: no"));
        assert!(config.contains("serve-expired: no"));
    }

    #[test]
    fn squid_uses_only_unbound_and_retains_destination_defense() {
        let config = render_squid_config(3128, "172.20.0.2", &[], true);
        assert!(config.contains("hosts_file none"));
        assert!(config.contains("dns_nameservers 172.20.0.2"));
        assert!(config.contains("!valid_public_hostname"));
        assert!(config.contains("blocked_destination dst"));
    }

    #[test]
    fn guarded_web_cannot_be_combined_with_host_policy() {
        assert!(
            exact_host_union(&[
                RestrictedPolicy::GuardedWeb,
                RestrictedPolicy::Hosts(vec!["example.com".to_string()]),
            ])
            .is_err()
        );
    }

    #[test]
    fn fingerprints_bind_order_boundaries_and_sysctl() {
        assert_ne!(fingerprint(&[b"ab", b"c"]), fingerprint(&[b"a", b"bc"]));
        assert_ne!(
            fingerprint(&[b"config"]),
            fingerprint(&[b"config", b"net.ipv6.conf.all.disable_ipv6=1"])
        );
    }

    #[test]
    fn dns_response_rejects_truncation_and_malformed_packets() {
        let mut response = vec![0x4a, 0x48, 0x81, 0x83];
        response.resize(12, 0);
        assert!(valid_dns_response(&response, 0x4a48));
        response[2] |= 0x02;
        assert!(!valid_dns_response(&response, 0x4a48));
        assert!(!valid_dns_response(&response[..8], 0x4a48));
    }
}
