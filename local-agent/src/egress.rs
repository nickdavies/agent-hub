use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use protocol::orchestration::NetworkResourceKind;

use crate::LocalAgentEgressTier;
use crate::egress_config::{LocalAgentConfig, canonical_hosts};
use agent_hub_executor::egress::{
    PreparedRestrictedEgress, ProxyRuntimeSpec, ResolverRuntimeSpec, RestrictedEgressRequest,
    RestrictedPolicy,
};
use agent_hub_executor::{Cancellation, Deadline};

#[derive(Debug, Clone)]
pub struct EgressManager {
    namespace: String,
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
        let namespace = sanitize_name_component(worker_id);
        anyhow::ensure!(!namespace.is_empty(), "worker egress namespace is empty");
        Ok(Self {
            namespace,
            egress_dir: data_dir.join("egress"),
            config,
        })
    }

    pub fn config(&self) -> &LocalAgentConfig {
        &self.config
    }

    pub async fn prepare_restricted_tier(
        &self,
        tier: LocalAgentEgressTier,
        resources: &[NetworkResourceKind],
        llm_provider: Option<&str>,
        execution_namespace: &str,
        cancellation: Cancellation,
        deadline: &Deadline,
    ) -> anyhow::Result<PreparedRestrictedEgress> {
        anyhow::ensure!(
            matches!(
                tier,
                LocalAgentEgressTier::Llm
                    | LocalAgentEgressTier::Github
                    | LocalAgentEgressTier::Build
                    | LocalAgentEgressTier::GuardedWeb
            ),
            "egress manager can only prepare restricted tiers"
        );
        let policies = self.resolve_resource_policies(resources, llm_provider)?;
        let proxy = &self.config.egress.proxy;
        let resolver_image = self
            .config
            .egress
            .preflight_image
            .as_deref()
            .context("egress.preflight_image is required for Unbound")?;
        let namespace = format!(
            "{}-{}",
            truncate_label(&self.namespace, 12),
            truncate_label(&sanitize_name_component(execution_namespace), 24)
        );
        RestrictedEgressRequest {
            namespace,
            state_dir: self.egress_dir.clone(),
            proxy: ProxyRuntimeSpec {
                image: proxy.image.clone(),
                listen_port: proxy.listen_port,
                uid: proxy.runtime_uid,
                gid: proxy.runtime_gid,
            },
            resolver: ResolverRuntimeSpec {
                image: resolver_image.to_string(),
                uid: 100,
                gid: 101,
                upstream: self.config.egress.resolver.upstream.clone(),
                readiness_host: self.config.egress.resolver.readiness_host.clone(),
            },
            policies,
            cancellation,
            external_subnets: Vec::new(),
            failure_after: None,
        }
        .prepare(Path::new("docker"), deadline)
        .await
    }

    pub async fn preflight_llm_only(&self) -> anyhow::Result<()> {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let deadline = Deadline::new(Some(60))?;
        let mut prepared = self
            .prepare_restricted_tier(
                LocalAgentEgressTier::Llm,
                &[NetworkResourceKind::Llm],
                None,
                "startup-preflight",
                Cancellation::new(rx),
                &deadline,
            )
            .await?;
        prepared.cleanup(&Deadline::new(Some(30))?).await
    }

    fn resolve_resource_policies(
        &self,
        resources: &[NetworkResourceKind],
        llm_provider: Option<&str>,
    ) -> anyhow::Result<Vec<RestrictedPolicy>> {
        let resources = canonical_resources(resources);
        anyhow::ensure!(
            !resources.is_empty(),
            "restricted egress requires a resource"
        );
        if resources.contains(&NetworkResourceKind::GuardedWeb) {
            anyhow::ensure!(
                resources.len() == 1,
                "guarded_web cannot be combined with other resources"
            );
            let policy = self.required_policy("guarded-web")?;
            anyhow::ensure!(
                policy.hosts.is_empty() && !policy.uses_llm_provider_hosts,
                "guarded-web policy must not declare hosts"
            );
            return Ok(vec![RestrictedPolicy::GuardedWeb]);
        }

        resources
            .into_iter()
            .map(|resource| {
                let (name, expected) = match resource {
                    NetworkResourceKind::Llm => {
                        let (provider, hosts) = self
                            .config
                            .egress
                            .llm
                            .resolve_provider_hosts(llm_provider)?;
                        let system_hosts = self
                            .config
                            .config_tree()
                            .and_then(|tree| tree.llm_provider_hosts.get(&provider))
                            .with_context(|| {
                                format!("system LLM provider '{provider}' is unavailable")
                            })?;
                        anyhow::ensure!(
                            system_hosts == &hosts,
                            "system and runner LLM provider hosts differ"
                        );
                        ("llm-only", hosts)
                    }
                    NetworkResourceKind::Github => (
                        "github-only",
                        canonical_hosts(&self.config.egress.build.github_hosts)?,
                    ),
                    NetworkResourceKind::GoRegistry => (
                        "go-registry",
                        canonical_hosts(&self.config.egress.build.go_registry_hosts)?,
                    ),
                    NetworkResourceKind::CratesRegistry => (
                        "crates-registry",
                        canonical_hosts(&self.config.egress.build.crates_registry_hosts)?,
                    ),
                    NetworkResourceKind::PypiRegistry => (
                        "pypi-registry",
                        canonical_hosts(&self.config.egress.build.pypi_registry_hosts)?,
                    ),
                    NetworkResourceKind::NpmRegistry => (
                        "npm-registry",
                        canonical_hosts(&self.config.egress.build.npm_registry_hosts)?,
                    ),
                    NetworkResourceKind::GuardedWeb => unreachable!(),
                };
                let policy = self.required_policy(name)?;
                let mut actual = policy.hosts.clone();
                if policy.uses_llm_provider_hosts {
                    actual.extend(expected.iter().cloned());
                }
                let actual = canonical_hosts(&actual)?;
                anyhow::ensure!(
                    actual == expected,
                    "restricted {name} hosts differ from the exact configured resource hosts"
                );
                Ok(RestrictedPolicy::Hosts(actual))
            })
            .collect()
    }

    fn required_policy(
        &self,
        name: &str,
    ) -> anyhow::Result<&crate::egress_config::ConfiguredNetworkPolicy> {
        let policy = self
            .config
            .network_policy(name)
            .with_context(|| format!("{name} policy must be loaded from CONFIG_ROOT"))?;
        anyhow::ensure!(
            !policy.unrestricted && policy.docker_network.is_none(),
            "restricted '{name}' policy selected unrestricted or operator Docker networking"
        );
        Ok(policy)
    }
}

fn canonical_resources(resources: &[NetworkResourceKind]) -> Vec<NetworkResourceKind> {
    let mut resources = resources.to_vec();
    resources.sort_by_key(ToString::to_string);
    resources.dedup();
    resources
}

fn sanitize_name_component(input: &str) -> String {
    input
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn truncate_label(value: &str, length: usize) -> &str {
    &value[..value.len().min(length)]
}

pub fn llm_provider_from_env(env: &HashMap<String, String>) -> Option<String> {
    env.get("AGENT_LLM_PROVIDER")
        .or_else(|| env.get("LLM_PROVIDER"))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_namespaces_are_label_safe_and_bounded() {
        let worker = sanitize_name_component("Worker_ID");
        let execution = sanitize_name_component("12345678-1234-1234-1234-123456789abc");
        let namespace = format!(
            "{}-{}",
            truncate_label(&worker, 12),
            truncate_label(&execution, 24)
        );
        assert!(namespace.len() <= 37);
        assert!(
            namespace
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        );
    }
}
