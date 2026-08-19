use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::Utc;
use serde_json::json;

use crate::BuildTarget;

pub fn run(config_root: String, output_dir: Option<String>, target: Option<BuildTarget>) -> i32 {
    let config_root = PathBuf::from(config::expand_tilde(&config_root));
    let output_dir = match output_dir {
        Some(path) => PathBuf::from(config::expand_tilde(&path)),
        None => config_root.join("artifacts"),
    };

    let system_path = config_root.join("system.toml");
    let system = match config::SystemConfig::load(&system_path) {
        Ok(system) => system,
        Err(err) => {
            eprintln!("[FAIL] failed to load {}: {}", system_path.display(), err);
            return 1;
        }
    };

    match target {
        Some(BuildTarget::Images) => {
            let (ok, _) = build_images(&config_root, &system);
            if ok { 0 } else { 1 }
        }
        Some(BuildTarget::Environments) => {
            let (ok, _) = build_environments(&config_root, &output_dir, &system);
            if ok { 0 } else { 1 }
        }
        None => {
            let (images_ok, built_images) = build_images(&config_root, &system);
            let (envs_ok, materialized_environments) =
                build_environments(&config_root, &output_dir, &system);

            let manifest_path = output_dir.join("manifest.json");
            let manifest_ok = write_manifest(
                &manifest_path,
                &config_root,
                &built_images,
                &materialized_environments,
            );

            if images_ok && envs_ok && manifest_ok {
                0
            } else {
                1
            }
        }
    }
}

fn build_images(config_root: &Path, system: &config::SystemConfig) -> (bool, Vec<String>) {
    let Some(images_cfg) = &system.images else {
        println!("[SKIP] no images configured in system.toml");
        return (true, Vec::new());
    };

    let mut image_configs = Vec::with_capacity(images_cfg.installed.len());
    for image_name in &images_cfg.installed {
        let image_toml = config_root
            .join("images")
            .join(image_name)
            .join("image.toml");
        match config::ImageConfig::load(&image_toml) {
            Ok(cfg) => image_configs.push(cfg),
            Err(err) => {
                eprintln!("[FAIL] failed to load {}: {}", image_toml.display(), err);
                return (false, Vec::new());
            }
        }
    }

    let build_order = match config::resolve_build_order(&image_configs) {
        Ok(order) => order,
        Err(err) => {
            eprintln!("[FAIL] failed to resolve image build order: {}", err);
            return (false, Vec::new());
        }
    };
    let by_name: HashMap<String, &config::ImageConfig> = image_configs
        .iter()
        .map(|cfg| (cfg.name.clone(), cfg))
        .collect();

    for cfg in &image_configs {
        let dockerfile = config_root
            .join("images")
            .join(&cfg.name)
            .join("Dockerfile");
        if cfg.name == "job-code"
            && let Err(error) = validate_reproducible_job_code_context(&dockerfile)
        {
            eprintln!("[FAIL] {error}");
            return (false, Vec::new());
        }
    }

    if build_order.is_empty() {
        println!("[SKIP] no installed images to build");
        return (true, Vec::new());
    }

    let mut ok = true;
    let mut built = Vec::new();
    let total = build_order.len();

    for (index, image_name) in build_order.iter().enumerate() {
        let image_dir = config_root.join("images").join(image_name);
        let dockerfile = image_dir.join("Dockerfile");
        let cfg = by_name[image_name];
        let tag = &cfg.tag;
        println!("[{}/{}] Building image {}...", index + 1, total, tag);

        let mut command = Command::new("docker");
        command
            .arg("build")
            .arg("-f")
            .arg(&dockerfile)
            .arg("-t")
            .arg(tag);
        let status = command.arg(&image_dir).status();

        match status {
            Ok(status) if status.success() => {
                println!("[OK]   built {}", tag);
                built.push(tag.clone());
            }
            Ok(status) => {
                eprintln!("[FAIL] docker build failed for {} (exit: {})", tag, status);
                ok = false;
            }
            Err(err) => {
                eprintln!("[FAIL] failed to run docker build for {}: {}", tag, err);
                ok = false;
            }
        }
    }

    (ok, built)
}

fn validate_reproducible_job_code_context(dockerfile: &Path) -> Result<(), String> {
    let source = std::fs::read_to_string(dockerfile)
        .map_err(|error| format!("failed to read {}: {error}", dockerfile.display()))?;
    let from = source
        .lines()
        .find(|line| line.trim_start().starts_with("FROM "))
        .ok_or_else(|| "job-code Dockerfile has no base image".to_string())?;
    if !from.contains("@sha256:") {
        return Err("job-code base image must be pinned by digest".to_string());
    }
    for forbidden in ["COPY bin/", "curl |", "curl|", ":latest", "ARG "] {
        if source.contains(forbidden) {
            return Err(format!(
                "job-code Dockerfile contains non-reproducible input '{forbidden}'"
            ));
        }
    }
    Ok(())
}

fn build_environments(
    config_root: &Path,
    output_dir: &Path,
    system: &config::SystemConfig,
) -> (bool, Vec<String>) {
    let Some(env_cfg) = &system.environments else {
        println!("[SKIP] no environments configured in system.toml");
        return (true, Vec::new());
    };

    if env_cfg.installed.is_empty() {
        println!("[SKIP] no installed environments to materialize");
        return (true, Vec::new());
    }

    let policies = match load_network_policies(config_root, system) {
        Ok(policies) => policies,
        Err(err) => {
            eprintln!("[FAIL] {}", err);
            return (false, Vec::new());
        }
    };

    let mut ok = true;
    let mut materialized = Vec::new();
    let total = env_cfg.installed.len();

    for (index, env_name) in env_cfg.installed.iter().enumerate() {
        println!(
            "[{}/{}] Materializing environment {}...",
            index + 1,
            total,
            env_name
        );

        let env_dir = config_root.join("environments").join(env_name);
        let environment_toml = env_dir.join("environment.toml");
        let environment = match config::EnvironmentConfig::load(&environment_toml) {
            Ok(env) => env,
            Err(err) => {
                eprintln!(
                    "[FAIL] failed to load {}: {}",
                    environment_toml.display(),
                    err
                );
                ok = false;
                continue;
            }
        };

        let output_env_dir = output_dir.join("environments").join(env_name);
        if let Err(err) = materialize_environment(
            config_root,
            &env_dir,
            &output_env_dir,
            &environment,
            &policies,
            system,
        ) {
            eprintln!("[FAIL] {}", err);
            ok = false;
            continue;
        }

        println!("[OK]   materialized {}", output_env_dir.display());
        materialized.push(env_name.clone());
    }

    (ok, materialized)
}

fn load_network_policies(
    config_root: &Path,
    system: &config::SystemConfig,
) -> Result<HashMap<String, config::NetworkPolicyConfig>, String> {
    let mut policies = HashMap::new();
    let Some(policy_cfg) = &system.network_policies else {
        return Ok(policies);
    };

    for policy_name in &policy_cfg.installed {
        let path = config_root
            .join("network_policies")
            .join(policy_name)
            .join("policy.toml");
        let policy = config::NetworkPolicyConfig::load(&path)?;
        policies.insert(policy.name.clone(), policy);
    }

    Ok(policies)
}

fn materialize_environment(
    config_root: &Path,
    source_env_dir: &Path,
    output_env_dir: &Path,
    environment: &config::EnvironmentConfig,
    policies: &HashMap<String, config::NetworkPolicyConfig>,
    system: &config::SystemConfig,
) -> Result<(), String> {
    if output_env_dir.exists() {
        std::fs::remove_dir_all(output_env_dir)
            .map_err(|e| format!("failed to clear {}: {e}", output_env_dir.display()))?;
    }
    std::fs::create_dir_all(output_env_dir)
        .map_err(|e| format!("failed to create {}: {e}", output_env_dir.display()))?;

    copy_required_file(source_env_dir, output_env_dir, "agent-hub-permissions.json")?;
    copy_required_file(source_env_dir, output_env_dir, "dippy.conf")?;
    copy_required_file(source_env_dir, output_env_dir, "opencode.json")?;

    {
        let dir_name = "prompts";
        let source = source_env_dir.join(dir_name);
        let destination = output_env_dir.join(dir_name);
        if source.exists() {
            copy_dir_recursive(&source, &destination)?;
        }
    }

    let output_skills_dir = output_env_dir.join("skills");
    for skill_name in &environment.include_skills {
        let shared_skill_dir = config_root.join("shared").join("skills").join(skill_name);
        if !shared_skill_dir.exists() {
            return Err(format!(
                "shared skill '{}' not found at {}",
                skill_name,
                shared_skill_dir.display()
            ));
        }
        copy_dir_recursive(&shared_skill_dir, &output_skills_dir.join(skill_name))?;
    }
    let local_skills = source_env_dir.join("skills");
    if local_skills.exists() {
        let entries = std::fs::read_dir(&local_skills)
            .map_err(|e| format!("failed to read directory {}: {e}", local_skills.display()))?;

        for entry in entries {
            let entry = entry
                .map_err(|e| format!("failed to read entry in {}: {e}", local_skills.display()))?;
            let source_path = entry.path();
            let destination_path = output_skills_dir.join(entry.file_name());
            let metadata = entry.metadata().map_err(|e| {
                format!("failed to read metadata for {}: {e}", source_path.display())
            })?;

            if metadata.is_dir() {
                if destination_path.exists() {
                    std::fs::remove_dir_all(&destination_path).map_err(|e| {
                        format!("failed to clear {}: {e}", destination_path.display())
                    })?;
                }
                copy_dir_recursive(&source_path, &destination_path)?;
            } else if metadata.is_file() {
                std::fs::copy(&source_path, &destination_path).map_err(|e| {
                    format!(
                        "failed to copy {} to {}: {e}",
                        source_path.display(),
                        destination_path.display()
                    )
                })?;
            }
        }
    }

    let output_agents_dir = output_env_dir.join("agents");
    std::fs::create_dir_all(&output_agents_dir)
        .map_err(|e| format!("failed to create {}: {e}", output_agents_dir.display()))?;
    for agent_name in &environment.include_agents {
        let shared_agent = config_root
            .join("shared")
            .join("agents")
            .join(format!("{agent_name}.md"));
        if !shared_agent.exists() {
            return Err(format!(
                "shared agent '{}' not found at {}",
                agent_name,
                shared_agent.display()
            ));
        }
        let destination = output_agents_dir.join(format!("{agent_name}.md"));
        std::fs::copy(&shared_agent, &destination).map_err(|e| {
            format!(
                "failed to copy shared agent {} to {}: {e}",
                shared_agent.display(),
                destination.display()
            )
        })?;
    }
    let local_agents = source_env_dir.join("agents");
    if local_agents.exists() {
        // Intentionally replace shared agents with local files of the same name.
        copy_dir_recursive(&local_agents, &output_agents_dir)?;
    }

    let flattened = config::flatten_policy(&environment.network_policy, policies, system)?;
    let network_policy_path = output_env_dir.join("network_policy.json");
    let network_policy_json = json!({
        "name": flattened.name,
        "description": flattened.description,
        "unrestricted": flattened.unrestricted,
        "docker_network": flattened.docker_network,
        "hosts": flattened.hosts,
        "env": flattened.env,
    });
    let serialized = serde_json::to_string_pretty(&network_policy_json)
        .map_err(|e| format!("failed to serialize network policy json: {e}"))?;
    std::fs::write(&network_policy_path, serialized).map_err(|e| {
        format!(
            "failed to write network policy at {}: {e}",
            network_policy_path.display()
        )
    })?;

    Ok(())
}

fn copy_required_file(source_dir: &Path, dest_dir: &Path, filename: &str) -> Result<(), String> {
    let source = source_dir.join(filename);
    if !source.exists() {
        return Err(format!(
            "required environment file missing: {}",
            source.display()
        ));
    }
    let destination = dest_dir.join(filename);
    std::fs::copy(&source, &destination).map_err(|e| {
        format!(
            "failed to copy {} to {}: {e}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<(), String> {
    std::fs::create_dir_all(destination)
        .map_err(|e| format!("failed to create {}: {e}", destination.display()))?;

    let entries = std::fs::read_dir(source)
        .map_err(|e| format!("failed to read directory {}: {e}", source.display()))?;

    for entry in entries {
        let entry =
            entry.map_err(|e| format!("failed to read entry in {}: {e}", source.display()))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = entry
            .metadata()
            .map_err(|e| format!("failed to read metadata for {}: {e}", source_path.display()))?;

        if metadata.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            std::fs::copy(&source_path, &destination_path).map_err(|e| {
                format!(
                    "failed to copy {} to {}: {e}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
        }
    }

    Ok(())
}

fn write_manifest(
    manifest_path: &Path,
    config_root: &Path,
    images: &[String],
    environments: &[String],
) -> bool {
    if let Some(parent) = manifest_path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!("[FAIL] failed to create {}: {}", parent.display(), err);
        return false;
    }

    let manifest = json!({
        "build_timestamp": Utc::now().to_rfc3339(),
        "images": images,
        "environments": environments,
        "config_root": config_root.display().to_string(),
    });

    match serde_json::to_string_pretty(&manifest) {
        Ok(content) => {
            if let Err(err) = std::fs::write(manifest_path, content) {
                eprintln!(
                    "[FAIL] failed to write manifest {}: {}",
                    manifest_path.display(),
                    err
                );
                false
            } else {
                println!("[OK]   wrote manifest {}", manifest_path.display());
                true
            }
        }
        Err(err) => {
            eprintln!("[FAIL] failed to serialize manifest: {}", err);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_code_context_requires_fully_locked_inputs() {
        let root = std::env::temp_dir().join(format!("job-code-context-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let dockerfile = root.join("Dockerfile");
        std::fs::write(
            &dockerfile,
            "FROM debian@sha256:abc\nENV TOOL_VERSION=1.0\nRUN true\n",
        )
        .unwrap();
        validate_reproducible_job_code_context(&dockerfile).unwrap();

        for invalid in [
            "FROM debian:latest\n",
            "FROM debian@sha256:abc\nCOPY bin/opencode /usr/bin/opencode\n",
            "FROM debian@sha256:abc\nRUN curl|bash\n",
            "FROM debian@sha256:abc\nARG TOOL_URL\n",
            "FROM debian@sha256:abc\n",
        ] {
            std::fs::write(&dockerfile, invalid).unwrap();
            let result = validate_reproducible_job_code_context(&dockerfile);
            if invalid == "FROM debian@sha256:abc\n" {
                assert!(result.is_ok());
            } else {
                assert!(result.is_err());
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
