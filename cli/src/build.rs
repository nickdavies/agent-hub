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
        if cfg.name == "job-code" {
            if let Err(error) = validate_reproducible_job_code_context(&dockerfile) {
                eprintln!("[FAIL] {error}");
                return (false, Vec::new());
            }
            if cfg.build_contexts.len() != 1 || !cfg.build_contexts.contains_key("agent-hub-source")
            {
                eprintln!("[FAIL] job-code requires only the agent-hub-source build context");
                return (false, Vec::new());
            }
            if let Err(error) = cfg.build_contexts["agent-hub-source"].validate() {
                eprintln!("[FAIL] {error}");
                return (false, Vec::new());
            }
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
        let candidate_identity_path = (image_name == "job-code").then(|| {
            config_root.join("artifacts/build-contexts").join(format!(
                "job-code-candidate-{}-{}.iid",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ))
        });
        let candidate_reference = (image_name == "job-code").then(|| {
            format!(
                "agent-hub/job-code-build-candidate:{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            )
        });

        let mut command = Command::new("docker");
        command.arg("build").arg("-f").arg(&dockerfile);
        if let Some(identity_path) = &candidate_identity_path {
            command
                .arg("--iidfile")
                .arg(identity_path)
                .arg("-t")
                .arg(candidate_reference.as_ref().expect("candidate reference"));
        } else {
            command.arg("-t").arg(tag);
        }
        if image_name == "job-code" {
            let context = &cfg.build_contexts["agent-hub-source"];
            let materialized = match materialize_git_build_context(config_root, image_name, context)
            {
                Ok(path) => path,
                Err(error) => {
                    eprintln!("[FAIL] {error}");
                    return (false, built);
                }
            };
            command
                .arg("--build-context")
                .arg(format!("agent-hub-source={}", materialized.display()));
        }
        if let Some(reference) = &candidate_reference
            && let Err(error) = claim_candidate_reference(reference)
        {
            eprintln!("[FAIL] {error}");
            return (false, built);
        }
        let status = command.arg(&image_dir).status();

        let build_result = match status {
            Ok(status) if status.success() => {
                if let Some(identity_path) = &candidate_identity_path {
                    let qualification =
                        qualify_and_publish_job_code_candidate(identity_path, tag, config_root);
                    let _ = std::fs::remove_file(identity_path);
                    if let Err(error) = qualification {
                        Err(format!("job-code qualification failed for {tag}: {error}"))
                    } else {
                        Ok(())
                    }
                } else {
                    Ok(())
                }
            }
            Ok(status) => {
                if let Some(identity_path) = &candidate_identity_path {
                    let _ = std::fs::remove_file(identity_path);
                }
                Err(format!("docker build failed for {tag} (exit: {status})"))
            }
            Err(err) => {
                if let Some(identity_path) = &candidate_identity_path {
                    let _ = std::fs::remove_file(identity_path);
                }
                Err(format!("failed to run docker build for {tag}: {err}"))
            }
        };
        let candidate_cleanup = candidate_reference
            .as_deref()
            .map_or(Ok(()), remove_candidate_reference);
        match (build_result, candidate_cleanup) {
            (Ok(()), Ok(())) => {
                println!("[OK]   built {}", tag);
                built.push(tag.clone());
            }
            (Err(error), Ok(())) => {
                eprintln!("[FAIL] {error}");
                ok = false;
            }
            (Ok(()), Err(cleanup)) => {
                eprintln!("[FAIL] {cleanup}");
                ok = false;
            }
            (Err(error), Err(cleanup)) => {
                eprintln!("[FAIL] {error}; {cleanup}");
                ok = false;
            }
        }
    }

    (ok, built)
}

fn claim_candidate_reference(reference: &str) -> Result<(), String> {
    let output = Command::new("docker")
        .args(["image", "inspect", reference])
        .output()
        .map_err(|error| format!("failed to establish candidate reference ownership: {error}"))?;
    if output.status.success() {
        return Err(format!(
            "generated candidate image reference already exists: {reference}"
        ));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !docker_reports_no_such_image(&stderr, reference) {
        return Err(format!(
            "failed to establish candidate reference ownership (exit: {}): {}",
            output.status,
            stderr.trim()
        ));
    }
    Ok(())
}

fn remove_candidate_reference(reference: &str) -> Result<(), String> {
    let output = Command::new("docker")
        .args(["image", "rm", reference])
        .output()
        .map_err(|error| {
            format!("failed to remove owned candidate reference {reference}: {error}")
        })?;
    if output.status.success()
        || docker_reports_no_such_image(&String::from_utf8_lossy(&output.stderr), reference)
    {
        return Ok(());
    }
    Err(format!(
        "failed to remove owned candidate reference {reference} (exit: {}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn docker_reports_no_such_image(stderr: &str, reference: &str) -> bool {
    stderr.lines().any(|line| {
        let line = line.trim();
        line.strip_prefix("Error response from daemon: No such image: ") == Some(reference)
            || line.strip_prefix("Error: No such image: ") == Some(reference)
    })
}

fn materialize_git_build_context(
    config_root: &Path,
    image_name: &str,
    context: &config::ImageBuildContext,
) -> Result<PathBuf, String> {
    context.validate()?;
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "Agent Hub source repository path is unavailable".to_string())?;
    let remote = git_output(repository, &["remote", "get-url", "origin"])?;
    if canonical_git_url(remote.trim()) != canonical_git_url(&context.url) {
        return Err("local Agent Hub object database does not match configured repository".into());
    }
    let expression = format!("{}^{{commit}}", context.commit);
    let resolved = git_output(repository, &["rev-parse", &expression])?;
    if resolved.trim() != context.commit {
        return Err("configured Agent Hub source commit is unavailable locally".into());
    }
    let archive = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["archive", "--format=tar", &context.commit])
        .output()
        .map_err(|error| format!("failed to materialize Agent Hub git archive: {error}"))?;
    if !archive.status.success() {
        return Err(format!(
            "failed to materialize Agent Hub git archive: {}",
            String::from_utf8_lossy(&archive.stderr).trim()
        ));
    }
    verify_source_archive(&archive.stdout, context)?;
    let destination = config_root
        .join("artifacts")
        .join("build-contexts")
        .join(format!("{image_name}-{}", context.commit));
    if destination.exists() {
        std::fs::remove_dir_all(&destination)
            .map_err(|error| format!("failed to clear {}: {error}", destination.display()))?;
    }
    std::fs::create_dir_all(&destination)
        .map_err(|error| format!("failed to create {}: {error}", destination.display()))?;
    let mut child = Command::new("tar")
        .args(["-x", "-C"])
        .arg(&destination)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to launch tar for source archive: {error}"))?;
    std::io::Write::write_all(
        child.stdin.as_mut().expect("piped tar stdin"),
        &archive.stdout,
    )
    .map_err(|error| format!("failed to write source archive to tar: {error}"))?;
    drop(child.stdin.take());
    let status = child
        .wait()
        .map_err(|error| format!("failed to wait for source archive extraction: {error}"))?;
    if !status.success() {
        return Err(format!("source archive extraction failed with {status}"));
    }
    Ok(destination)
}

fn verify_source_archive(bytes: &[u8], context: &config::ImageBuildContext) -> Result<(), String> {
    let actual = config::sha256_hex(bytes);
    if actual != context.archive_sha256 {
        return Err(format!(
            "Agent Hub source archive digest mismatch: expected {}, got {actual}",
            context.archive_sha256
        ));
    }
    Ok(())
}

fn git_output(repository: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .map_err(|error| format!("failed to inspect local Agent Hub repository: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to inspect local Agent Hub repository: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("local Agent Hub repository output is not UTF-8: {error}"))
}

fn canonical_git_url(value: &str) -> String {
    let normalized = value
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_ascii_lowercase();
    if let Some((_, repository)) = normalized.split_once("://") {
        return repository
            .rsplit_once('@')
            .map_or(repository, |(_, repository)| repository)
            .to_string();
    }
    if let Some((authority, path)) = normalized.split_once(':') {
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        return format!("{host}/{path}");
    }
    normalized
}

fn qualify_and_publish_job_code_candidate(
    identity_path: &Path,
    release_tag: &str,
    representative_path: &Path,
) -> Result<(), String> {
    let candidate_identity = std::fs::read_to_string(identity_path).map_err(|error| {
        format!(
            "failed to read candidate image identity from {}: {error}",
            identity_path.display()
        )
    })?;
    let candidate_identity = candidate_identity.trim();
    validate_image_identity(candidate_identity)?;
    let inspected_identity = inspect_image_identity(candidate_identity)?;
    if inspected_identity != candidate_identity {
        return Err(format!(
            "candidate image identity changed: expected {candidate_identity}, got {inspected_identity}"
        ));
    }
    run_job_code_preflight(candidate_identity, representative_path)?;
    let status = Command::new("docker")
        .args(["image", "tag", candidate_identity, release_tag])
        .status()
        .map_err(|error| format!("failed to publish qualified image tag: {error}"))?;
    if !status.success() {
        return Err(format!(
            "failed to publish qualified image tag {release_tag} (exit: {status})"
        ));
    }
    Ok(())
}

fn validate_image_identity(identity: &str) -> Result<(), String> {
    let digest = identity
        .strip_prefix("sha256:")
        .ok_or_else(|| "candidate image identity is not a canonical sha256 digest".to_string())?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("candidate image identity is not a canonical sha256 digest".to_string());
    }
    Ok(())
}

fn inspect_image_identity(image: &str) -> Result<String, String> {
    let inspect = Command::new("docker")
        .args(["image", "inspect", "--format={{.Id}}", image])
        .output()
        .map_err(|error| format!("failed to inspect built image identity: {error}"))?;
    if !inspect.status.success() {
        return Err(format!(
            "failed to inspect built image identity: {}",
            String::from_utf8_lossy(&inspect.stderr).trim()
        ));
    }
    let identity = String::from_utf8(inspect.stdout)
        .map_err(|error| format!("built image identity is not UTF-8: {error}"))?;
    let identity = identity.trim();
    validate_image_identity(identity)?;
    Ok(identity.to_string())
}

fn run_job_code_preflight(image_identity: &str, representative_path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    let (uid, gid) = {
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::metadata(representative_path).map_err(|error| {
            format!(
                "failed to inspect representative runtime identity from {}: {error}",
                representative_path.display()
            )
        })?;
        (metadata.uid(), metadata.gid())
    };
    #[cfg(not(unix))]
    return Err("job-code preflight requires a Unix runtime UID and GID".to_string());

    let args = config::job_code_preflight_docker_args(
        image_identity,
        uid,
        gid,
        protocol::APPROVED_EXECUTION_PIDS_LIMIT,
    )?;
    let output = Command::new("docker")
        .args(&args)
        .output()
        .map_err(|error| format!("failed to launch Docker: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let report = config::validate_supervised_opencode_preflight_report(
        &output.stdout,
        image_identity,
        uid,
        gid,
    )?;
    println!("[OK]   job-code preflight {}", report);
    Ok(())
}

fn validate_reproducible_job_code_context(dockerfile: &Path) -> Result<(), String> {
    let source = std::fs::read_to_string(dockerfile)
        .map_err(|error| format!("failed to read {}: {error}", dockerfile.display()))?;
    let instructions = dockerfile_instructions(&source)?;
    let from = instructions
        .iter()
        .filter(|instruction| instruction.keyword == "from")
        .collect::<Vec<_>>();
    let [from] = from.as_slice() else {
        return Err("job-code Dockerfile must contain exactly one base image".to_string());
    };
    if !from.value.contains("@sha256:") {
        return Err("job-code base image must be pinned by digest".to_string());
    }
    if instructions
        .iter()
        .any(|instruction| instruction.value.to_ascii_lowercase().contains(":latest"))
        || instructions
            .iter()
            .any(|instruction| instruction.keyword == "arg")
        || instructions.iter().any(|instruction| {
            instruction.keyword == "copy" && instruction.value.to_ascii_lowercase().contains("bin/")
        })
    {
        return Err("job-code Dockerfile contains a non-reproducible input".to_string());
    }
    for instruction in &instructions {
        match instruction.keyword.as_str() {
            "add" => return Err("job-code Dockerfile must not use ADD".to_string()),
            "copy" => validate_copy_instruction(&instruction.value)?,
            _ => {}
        }
    }
    let gateway_builds = instructions
        .iter()
        .filter(|instruction| {
            instruction.keyword == "run"
                && instruction
                    .value
                    .contains("cargo build --locked --release -p agent-hub-gateway")
        })
        .count();
    let cargo_builds = instructions
        .iter()
        .map(|instruction| instruction.value.matches("cargo build").count())
        .sum::<usize>();
    let source_copies = instructions
        .iter()
        .filter(|instruction| {
            instruction.keyword == "copy"
                && instruction.value == "--from=agent-hub-source / /opt/src/agent-hub"
        })
        .count();
    if gateway_builds != 1 || cargo_builds != 1 || source_copies != 1 {
        return Err(
            "job-code Dockerfile must copy only the named Agent Hub source context and build agent-hub-gateway exactly once with Cargo --locked".into(),
        );
    }

    let snapshot = instructions
        .iter()
        .find_map(|instruction| docker_env_value(instruction, "DEBIAN_SNAPSHOT"))
        .ok_or_else(|| "job-code Dockerfile must pin DEBIAN_SNAPSHOT".to_string())?;
    if snapshot.len() != 16
        || !snapshot.ends_with('Z')
        || snapshot.as_bytes()[8] != b'T'
        || !snapshot[..8]
            .chars()
            .all(|character| character.is_ascii_digit())
        || !snapshot[9..15]
            .chars()
            .all(|character| character.is_ascii_digit())
    {
        return Err("job-code DEBIAN_SNAPSHOT must be a timestamp".to_string());
    }

    let apt_runs = instructions
        .iter()
        .enumerate()
        .filter(|(_, instruction)| {
            instruction.keyword == "run"
                && run_command_kinds(&instruction.value).contains(&CommandKind::AptGet)
        })
        .collect::<Vec<_>>();
    let [(bootstrap_index, bootstrap), (packages_index, packages)] = apt_runs.as_slice() else {
        return Err("job-code Dockerfile must contain exactly two APT transactions".to_string());
    };
    if bootstrap_index >= packages_index {
        return Err("job-code APT transactions are out of order".to_string());
    }

    validate_certificate_bootstrap(&bootstrap.value)?;
    validate_https_package_install(&packages.value)?;

    for (index, instruction) in instructions.iter().enumerate() {
        if instruction.keyword == "run" {
            let commands = run_command_kinds(&instruction.value);
            let normalized = instruction.value.to_ascii_lowercase();
            if run_uses_insecure_curl(&instruction.value) {
                return Err("job-code Dockerfile must not disable curl TLS verification".into());
            }
            if normalized.contains("corepack prepare") || normalized.contains("rustup-init") {
                return Err(
                    "job-code Dockerfile must use directly checksum-verified package inputs".into(),
                );
            }
            let download_count = commands
                .iter()
                .filter(|command| **command == CommandKind::Curl)
                .count();
            let checksum_count = normalized.matches("sha256sum -c -").count()
                + normalized.matches("sha512sum -c -").count();
            if download_count != checksum_count || commands.contains(&CommandKind::Wget) {
                return Err(
                    "job-code Dockerfile downloads must have one committed checksum each".into(),
                );
            }
            if commands.iter().any(|command| {
                matches!(
                    command,
                    CommandKind::Apt | CommandKind::Aptitude | CommandKind::Dpkg
                )
            }) {
                return Err(
                    "job-code Dockerfile contains an unsupported package transaction".into(),
                );
            }
            if !commands.contains(&CommandKind::AptGet) {
                validate_run_network_inputs(&instruction.value)?;
            }
        }
        if matches!(instruction.keyword.as_str(), "run" | "add")
            && contains_url_scheme(&instruction.value, "http")
            && index != *bootstrap_index
        {
            return Err(
                "job-code Dockerfile contains an HTTP input outside the CA bootstrap".into(),
            );
        }
        if (index < *bootstrap_index && instruction_has_network_activity(instruction))
            || (*bootstrap_index < index
                && index < *packages_index
                && instruction_has_package_or_network_activity(instruction))
        {
            return Err(
                "job-code Dockerfile contains package or network activity between trusted APT phases"
                    .into(),
            );
        }
    }
    Ok(())
}

fn validate_copy_instruction(value: &str) -> Result<(), String> {
    if !matches!(
        value,
        "preflight.sh /usr/local/libexec/agent-hub-job-code-preflight"
            | "--from=agent-hub-source / /opt/src/agent-hub"
    ) {
        return Err("job-code COPY input is not an approved local context file".to_string());
    }
    Ok(())
}

fn validate_run_network_inputs(run: &str) -> Result<(), String> {
    let fragments = shell_command_fragments(run);
    let mut download_targets = Vec::new();
    let mut checksum_fragments = 0usize;

    for (index, fragment) in fragments.iter().enumerate() {
        let tokens = fragment.split_whitespace().collect::<Vec<_>>();
        let first = tokens.first().copied().unwrap_or_default();
        let command = classify_command_token(first);
        let normalized = fragment.to_ascii_lowercase();

        if command == Some(CommandKind::Wget) {
            return Err("job-code Dockerfile must not use wget".to_string());
        }
        if is_remote_package_manager_fragment(&tokens) {
            return Err(
                "job-code Dockerfile contains a remote package-manager install".to_string(),
            );
        }
        if command == Some(CommandKind::Curl) {
            if fragment.contains("$(") || fragment.contains('`') {
                return Err("job-code curl download uses shell indirection".to_string());
            }
            let urls = tokens
                .iter()
                .filter(|token| {
                    contains_url_scheme(token, "http") || contains_url_scheme(token, "https")
                })
                .count();
            if urls != 1 {
                return Err("job-code curl must contain exactly one direct URL".to_string());
            }
            let output = tokens
                .windows(2)
                .find_map(|pair| (pair[0] == "-o").then_some(pair[1]))
                .ok_or_else(|| "job-code curl must use a literal -o target".to_string())?
                .trim_matches(['\'', '"']);
            if !output.starts_with("/tmp/")
                || output.contains('$')
                || output.contains('*')
                || download_targets.iter().any(|existing| existing == output)
            {
                return Err("job-code curl output target is not unique and literal".to_string());
            }
            download_targets.push(output.to_string());
            continue;
        }

        let checksum = matches!(first.trim_matches(['\'', '"']), "sha256sum" | "sha512sum")
            && tokens.get(1) == Some(&"-c")
            && tokens.get(2) == Some(&"-")
            && tokens.len() == 3;
        if checksum {
            checksum_fragments += 1;
            continue;
        }

        if contains_url_scheme(fragment, "http") || contains_url_scheme(fragment, "https") {
            let approved_provenance = (command == Some(CommandKind::Printf)
                && normalized.contains("snapshot.debian.org"))
                || (first.rsplit('/').next() == Some("grep")
                    && normalized.contains("xz_url =")
                    && normalized.ends_with("/tmp/rust-channel.toml"))
                || (first.rsplit('/').next() == Some("jq")
                    && normalized.starts_with("jq -n")
                    && run.ends_with("> /usr/local/share/agent-hub/job-code-tool-manifest.json"));
            if !approved_provenance {
                return Err("job-code RUN contains an unrecognized network input".to_string());
            }
        }

        let manifest_generation = first.rsplit('/').next() == Some("jq")
            && normalized.starts_with("jq -n")
            && run.ends_with("> /usr/local/share/agent-hub/job-code-tool-manifest.json");
        if (normalized.contains("curl") || normalized.contains("wget")) && !manifest_generation {
            return Err("job-code RUN hides a network fetch behind a wrapper".to_string());
        }

        if checksum && index == 0 {
            return Err("job-code checksum has no download".to_string());
        }
    }

    if download_targets.len() != checksum_fragments {
        return Err("job-code downloads and checksums are not one-to-one".to_string());
    }
    for target in &download_targets {
        let matches = fragments
            .windows(2)
            .filter(|pair| {
                pair[0]
                    .split_whitespace()
                    .next()
                    .and_then(classify_command_token)
                    == Some(CommandKind::Printf)
                    && pair[0]
                        .split_whitespace()
                        .any(|token| token.trim_matches(['\'', '"']) == target)
                    && matches!(
                        pair[1].split_whitespace().next(),
                        Some("sha256sum" | "sha512sum")
                    )
            })
            .count();
        if matches != 1 {
            return Err(format!(
                "job-code download target '{target}' has no exact committed checksum"
            ));
        }
    }
    Ok(())
}

fn is_remote_package_manager_fragment(tokens: &[&str]) -> bool {
    let command = tokens
        .first()
        .and_then(|token| token.rsplit('/').next())
        .unwrap_or_default();
    match command {
        "npm" => matches!(
            tokens.get(1),
            Some(&("install" | "i" | "ci" | "update" | "add"))
        ),
        "yarn" | "pnpm" => matches!(
            tokens.get(1),
            Some(&("add" | "install" | "i" | "upgrade" | "update"))
        ),
        "pip" | "pip3" => matches!(tokens.get(1), Some(&("install" | "download" | "wheel"))),
        "cargo" => matches!(
            tokens.get(1),
            Some(&("install" | "fetch" | "search" | "update"))
        ),
        "go" => matches!(tokens.get(1), Some(&("install" | "get" | "mod"))),
        "uv" => matches!(
            (tokens.get(1), tokens.get(2)),
            (Some(&("pip" | "tool")), Some(&"install"))
        ),
        "corepack" => matches!(tokens.get(1), Some(&("prepare" | "install" | "use"))),
        _ => false,
    }
}

fn instruction_has_network_activity(instruction: &DockerInstruction) -> bool {
    instruction.keyword == "add"
        || (instruction.keyword == "run"
            && (contains_url_scheme(&instruction.value, "http")
                || contains_url_scheme(&instruction.value, "https")
                || run_command_kinds(&instruction.value)
                    .iter()
                    .any(|command| matches!(command, CommandKind::Curl | CommandKind::Wget))))
}

fn instruction_has_package_or_network_activity(instruction: &DockerInstruction) -> bool {
    instruction_has_network_activity(instruction)
        || (instruction.keyword == "run"
            && run_command_kinds(&instruction.value).iter().any(|command| {
                matches!(
                    command,
                    CommandKind::AptGet
                        | CommandKind::Apt
                        | CommandKind::Aptitude
                        | CommandKind::Dpkg
                )
            }))
        || matches!(instruction.keyword.as_str(), "copy" | "add")
}

// Generic Dockerfile parsing stops at instructions and continued lines. Job-code policy
// validation below deliberately supports only the shell shapes it needs to trust.
#[derive(Debug)]
struct DockerInstruction {
    keyword: String,
    value: String,
}

fn dockerfile_instructions(source: &str) -> Result<Vec<DockerInstruction>, String> {
    let mut instructions = Vec::new();
    let mut current = String::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let continued = trimmed.ends_with('\\');
        let part = trimmed.trim_end_matches('\\').trim_end();
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(part);
        if !continued {
            let complete = std::mem::take(&mut current);
            let (keyword, value) = complete
                .split_once(char::is_whitespace)
                .ok_or_else(|| format!("unsupported Dockerfile instruction '{complete}'"))?;
            instructions.push(DockerInstruction {
                keyword: keyword.to_ascii_lowercase(),
                value: normalize_whitespace(value),
            });
        }
    }
    if !current.is_empty() {
        return Err("unterminated Dockerfile line continuation".to_string());
    }
    Ok(instructions)
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn docker_env_value<'a>(instruction: &'a DockerInstruction, key: &str) -> Option<&'a str> {
    if instruction.keyword != "env" {
        return None;
    }
    if let Some((name, value)) = instruction.value.split_once('=') {
        return (name == key).then_some(value.trim());
    }
    let (name, value) = instruction.value.split_once(char::is_whitespace)?;
    (name == key).then_some(value.trim())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandKind {
    AptGet,
    Apt,
    Aptitude,
    Dpkg,
    Curl,
    Wget,
    Printf,
    Rm,
}

fn classify_command_token(token: &str) -> Option<CommandKind> {
    let token = token
        .trim_matches(|character| matches!(character, '&' | '|' | ';' | '(' | ')' | '\'' | '"'));
    match token.rsplit('/').next()?.to_ascii_lowercase().as_str() {
        "apt-get" => Some(CommandKind::AptGet),
        "apt" => Some(CommandKind::Apt),
        "aptitude" => Some(CommandKind::Aptitude),
        "dpkg" => Some(CommandKind::Dpkg),
        "curl" => Some(CommandKind::Curl),
        "wget" => Some(CommandKind::Wget),
        "printf" => Some(CommandKind::Printf),
        "rm" => Some(CommandKind::Rm),
        _ => None,
    }
}

fn shell_command_fragments(run: &str) -> Vec<&str> {
    let mut fragments = Vec::new();
    let mut start = 0;
    let mut quote = None;
    for (index, character) in run.char_indices() {
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
        } else if quote.is_none() && matches!(character, '&' | '|' | ';') {
            if start < index {
                fragments.push(run[start..index].trim());
            }
            start = index + character.len_utf8();
        }
    }
    if start < run.len() {
        fragments.push(run[start..].trim());
    }
    fragments
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect()
}

fn run_command_kinds(run: &str) -> Vec<CommandKind> {
    shell_command_fragments(run)
        .into_iter()
        .filter_map(|command| command.split_whitespace().next())
        .filter_map(classify_command_token)
        .collect()
}

fn run_uses_insecure_curl(run: &str) -> bool {
    shell_command_fragments(run).into_iter().any(|command| {
        let tokens = command.split_whitespace().collect::<Vec<_>>();
        if tokens
            .first()
            .and_then(|token| classify_command_token(token))
            != Some(CommandKind::Curl)
        {
            return false;
        }
        tokens[1..].iter().any(|token| {
            token.eq_ignore_ascii_case("--insecure")
                || (token.starts_with('-') && !token.starts_with("--") && token[1..].contains('k'))
        })
    })
}

fn contains_url_scheme(value: &str, scheme: &str) -> bool {
    value
        .to_ascii_lowercase()
        .contains(&format!("{}://", scheme.to_ascii_lowercase()))
}

fn trusted_run_commands(run: &str) -> Result<Vec<&str>, String> {
    let mut commands = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let bytes = run.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let character = bytes[index] as char;
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            index += 1;
            continue;
        }
        if quote.is_none() {
            if character == '&' && bytes.get(index + 1) == Some(&b'&') {
                commands.push(run[start..index].trim());
                index += 2;
                start = index;
                continue;
            }
            if matches!(character, '&' | '|' | ';' | '`' | '#')
                || (character == '$' && bytes.get(index + 1) == Some(&b'('))
            {
                return Err("trusted APT RUN uses unsupported shell grammar".to_string());
            }
        }
        index += 1;
    }
    commands.push(run[start..].trim());
    if quote.is_some() || commands.iter().any(|command| command.is_empty()) {
        return Err("trusted APT RUN contains an empty command".to_string());
    }
    Ok(commands)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotSource {
    Debian,
    Security,
}

fn parse_snapshot_source_write(command: &str) -> Result<(bool, SnapshotSource, bool), String> {
    let redirect = command
        .rfind(">>")
        .map(|index| (index, true, 2))
        .or_else(|| command.rfind('>').map(|index| (index, false, 1)))
        .ok_or_else(|| "snapshot source write must redirect to sources.list".to_string())?;
    if command[redirect.0 + redirect.2..].trim() != "/etc/apt/sources.list" {
        return Err("snapshot source write targets an unsupported destination".to_string());
    }
    let invocation = command[..redirect.0].trim();
    let first_quote = invocation
        .find('\'')
        .ok_or_else(|| "snapshot source write must use a literal format".to_string())?;
    let last_quote = invocation
        .rfind('\'')
        .filter(|index| *index > first_quote)
        .ok_or_else(|| "snapshot source write has an unterminated format".to_string())?;
    if classify_command_token(invocation[..first_quote].trim()) != Some(CommandKind::Printf)
        || invocation[last_quote + 1..].trim() != "\"$DEBIAN_SNAPSHOT\""
    {
        return Err("snapshot source write has an unsupported printf invocation".to_string());
    }
    let fields = invocation[first_quote + 1..last_quote]
        .split_whitespace()
        .collect::<Vec<_>>();
    if fields.len() != 5
        || fields[0] != "deb"
        || fields[1] != "[check-valid-until=no]"
        || fields[4] != r#"main\n"#
    {
        return Err("snapshot source entry has an unsupported shape".to_string());
    }
    let url = fields[2].to_ascii_lowercase();
    let source = if url == "http://snapshot.debian.org/archive/debian/%s"
        || url == "https://snapshot.debian.org/archive/debian/%s"
    {
        SnapshotSource::Debian
    } else if url == "http://snapshot.debian.org/archive/debian-security/%s"
        || url == "https://snapshot.debian.org/archive/debian-security/%s"
    {
        SnapshotSource::Security
    } else {
        return Err("snapshot source entry uses an unsupported URL".to_string());
    };
    let expected_suite = match source {
        SnapshotSource::Debian => "bookworm",
        SnapshotSource::Security => "bookworm-security",
    };
    if fields[3] != expected_suite {
        return Err("snapshot source entry uses an unsupported suite".to_string());
    }
    Ok((url.starts_with("https://"), source, redirect.1))
}

fn validate_source_pair(commands: &[&str], https: bool) -> Result<(), String> {
    if parse_snapshot_source_write(commands[0])? != (https, SnapshotSource::Debian, false)
        || parse_snapshot_source_write(commands[1])? != (https, SnapshotSource::Security, true)
    {
        let scheme = if https { "HTTPS" } else { "HTTP" };
        return Err(format!(
            "snapshot sources must be the ordered {scheme} Debian pair"
        ));
    }
    Ok(())
}

fn validate_rm(command: &str, flags: &str, path: &str) -> bool {
    let tokens = command.split_whitespace().collect::<Vec<_>>();
    tokens.len() == 3
        && classify_command_token(tokens[0]) == Some(CommandKind::Rm)
        && tokens[1] == flags
        && tokens[2] == path
}

fn validate_certificate_bootstrap(run: &str) -> Result<(), String> {
    let commands = trusted_run_commands(run)?;
    if commands.len() != 8 {
        return Err("job-code certificate bootstrap has an unsupported command shape".to_string());
    }
    validate_source_pair(&commands[0..2], false)?;
    validate_source_pair(&commands[6..8], true)?;
    if !validate_rm(commands[2], "-f", "/etc/apt/sources.list.d/debian.sources")
        || parse_apt_command(commands[3], "update")? != Vec::<String>::new()
        || parse_apt_command(commands[4], "install")? != ["ca-certificates"]
        || !validate_rm(commands[5], "-rf", "/var/lib/apt/lists/*")
    {
        return Err("job-code certificate bootstrap has an unsupported command shape".to_string());
    }
    Ok(())
}

fn validate_https_package_install(run: &str) -> Result<(), String> {
    let commands = trusted_run_commands(run)?;
    if commands.len() != 3
        || parse_apt_command(commands[0], "update")? != Vec::<String>::new()
        || !validate_rm(commands[2], "-rf", "/var/lib/apt/lists/*")
    {
        return Err("job-code HTTPS APT transaction has an unsupported command shape".to_string());
    }
    let packages = parse_apt_command(commands[1], "install")?;
    if packages.is_empty() || packages.iter().any(|package| package == "ca-certificates") {
        return Err(
            "job-code remaining packages must be installed only after HTTPS is active".into(),
        );
    }
    Ok(())
}

fn parse_apt_command(command: &str, expected_action: &str) -> Result<Vec<String>, String> {
    const SECURITY_KEYS: [&str; 3] = [
        "acquire::allowinsecurerepositories",
        "acquire::allowdowngradetoinsecurerepositories",
        "apt::get::allowunauthenticated",
    ];
    const CHECK_VALID_UNTIL: &str = "acquire::check-valid-until";

    let tokens = command.split_whitespace().collect::<Vec<_>>();
    if tokens
        .first()
        .and_then(|token| classify_command_token(token))
        != Some(CommandKind::AptGet)
    {
        return Err("trusted APT command must use apt-get directly".to_string());
    }
    let mut index = 1;
    let mut disabled = std::collections::HashMap::new();
    let mut check_valid_until = None;
    while tokens.get(index) == Some(&"-o") {
        let assignment = tokens
            .get(index + 1)
            .ok_or_else(|| "APT -o requires KEY=VALUE".to_string())?;
        let (key, value) = assignment
            .split_once('=')
            .ok_or_else(|| "APT -o requires KEY=VALUE".to_string())?;
        let key = key.to_ascii_lowercase();
        let value = value.to_ascii_lowercase();
        let is_disabled = match value.as_str() {
            "false" | "no" | "off" | "0" => true,
            "true" | "yes" | "on" | "1" => {
                return Err(format!(
                    "APT verification option '{key}' must remain disabled"
                ));
            }
            _ => {
                return Err(format!(
                    "unsupported boolean value '{value}' for APT option '{key}'"
                ));
            }
        };
        if SECURITY_KEYS.contains(&key.as_str()) {
            disabled.insert(key, is_disabled);
        } else if key == CHECK_VALID_UNTIL {
            check_valid_until = Some(is_disabled);
        } else {
            return Err(format!("unsupported APT option '{key}'"));
        }
        index += 2;
    }
    if SECURITY_KEYS
        .iter()
        .any(|key| disabled.get(*key) != Some(&true))
    {
        return Err(
            "APT command must explicitly disable insecure repositories and packages".into(),
        );
    }
    if expected_action == "update" && check_valid_until != Some(true) {
        return Err("snapshot APT update must disable Check-Valid-Until".to_string());
    }
    if expected_action == "install" && check_valid_until.is_some() {
        return Err("APT install contains an unsupported option".to_string());
    }
    if tokens.get(index) != Some(&expected_action) {
        return Err(format!("expected apt-get {expected_action}"));
    }
    index += 1;
    if expected_action == "update" {
        if index != tokens.len() {
            return Err("apt-get update does not accept additional arguments here".to_string());
        }
        return Ok(Vec::new());
    }

    let mut assume_yes = false;
    let mut no_recommends = false;
    while let Some(flag) = tokens.get(index).filter(|token| token.starts_with('-')) {
        match *flag {
            "-y" => assume_yes = true,
            "--no-install-recommends" => no_recommends = true,
            _ => return Err(format!("unsupported apt-get install flag '{flag}'")),
        }
        index += 1;
    }
    if !assume_yes || !no_recommends {
        return Err("apt-get install must use -y --no-install-recommends".to_string());
    }
    let packages = tokens[index..]
        .iter()
        .map(|package| package.to_string())
        .collect::<Vec<_>>();
    if packages.iter().any(|package| {
        !package
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "+.-=".contains(character))
            || package.split_once('=').is_some_and(|(name, version)| {
                name.is_empty() || version.is_empty() || version.contains('=')
            })
    }) {
        return Err("apt-get install contains an unsupported package expression".to_string());
    }
    Ok(packages)
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
    _source_env_dir: &Path,
    output_env_dir: &Path,
    environment: &config::EnvironmentConfig,
    _policies: &HashMap<String, config::NetworkPolicyConfig>,
    _system: &config::SystemConfig,
) -> Result<(), String> {
    config::materialize_environment_bundle(config_root, &environment.name)?.write_to(output_env_dir)
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
    fn source_archive_digest_mismatch_fails_closed() {
        let context = config::ImageBuildContext {
            context_type: "git".into(),
            url: "https://example.com/agent-hub.git".into(),
            commit: "a".repeat(40),
            archive_sha256: config::sha256_hex(b"expected"),
        };
        assert!(verify_source_archive(b"expected", &context).is_ok());
        assert!(verify_source_archive(b"tampered", &context).is_err());
    }

    #[test]
    fn source_repository_identity_accepts_same_https_and_ssh_repository() {
        assert_eq!(
            canonical_git_url("git@github.com:nickdavies/agent-hub.git"),
            canonical_git_url("https://github.com/nickdavies/agent-hub.git")
        );
    }

    #[test]
    fn candidate_image_identity_must_be_canonical() {
        assert!(validate_image_identity(&format!("sha256:{}", "a".repeat(64))).is_ok());
        for invalid in [
            format!("sha256:{}", "A".repeat(64)),
            format!("sha256:{}", "a".repeat(63)),
            "job-code:latest".to_string(),
        ] {
            assert!(
                validate_image_identity(&invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn only_explicit_matching_no_such_image_is_absence() {
        let reference = "agent-hub/job-code-build-candidate:test";
        for stderr in [
            format!("Error response from daemon: No such image: {reference}\n"),
            format!("Error: No such image: {reference}\n"),
        ] {
            assert!(docker_reports_no_such_image(&stderr, reference));
        }
        for stderr in [
            "permission denied: no such image".to_string(),
            "Cannot connect to the Docker daemon".to_string(),
            "Error response from daemon: No such object: test".to_string(),
            "Error response from daemon: No such image: another:tag".to_string(),
        ] {
            assert!(!docker_reports_no_such_image(&stderr, reference));
        }
    }

    const LOCKED_DOCKERFILE: &str = r#"FROM debian:bookworm-slim@sha256:abc
ENV DEBIAN_FRONTEND=noninteractive
ENV DEBIAN_SNAPSHOT=20260803T000000Z
# The digest-pinned base trusts Debian archive signatures and hashes only long enough to install the CA bundle.
RUN printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/%s bookworm main\n' "$DEBIAN_SNAPSHOT" > /etc/apt/sources.list \
    && printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian-security/%s bookworm-security main\n' "$DEBIAN_SNAPSHOT" >> /etc/apt/sources.list \
    && rm -f /etc/apt/sources.list.d/debian.sources \
    && apt-get -o Acquire::AllowInsecureRepositories=false -o Acquire::AllowDowngradeToInsecureRepositories=false -o APT::Get::AllowUnauthenticated=false -o Acquire::Check-Valid-Until=false update \
    && apt-get -o Acquire::AllowInsecureRepositories=false -o Acquire::AllowDowngradeToInsecureRepositories=false -o APT::Get::AllowUnauthenticated=false install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && printf 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/%s bookworm main\n' "$DEBIAN_SNAPSHOT" > /etc/apt/sources.list \
    && printf 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/%s bookworm-security main\n' "$DEBIAN_SNAPSHOT" >> /etc/apt/sources.list
RUN apt-get -o Acquire::AllowInsecureRepositories=false -o Acquire::AllowDowngradeToInsecureRepositories=false -o APT::Get::AllowUnauthenticated=false -o Acquire::Check-Valid-Until=false update \
    && apt-get -o Acquire::AllowInsecureRepositories=false -o Acquire::AllowDowngradeToInsecureRepositories=false -o APT::Get::AllowUnauthenticated=false install -y --no-install-recommends curl git \
    && rm -rf /var/lib/apt/lists/*
ENV TOOL_VERSION=1.0
ENV TOOL_SHA256=abc
RUN curl -fsSL "https://example.com/tool-${TOOL_VERSION}" -o /tmp/tool \
    && printf '%s  %s\n' "$TOOL_SHA256" /tmp/tool | sha256sum -c -
COPY --from=agent-hub-source / /opt/src/agent-hub
RUN cd /opt/src/agent-hub && cargo build --locked --release -p agent-hub-gateway
"#;

    fn validate(source: &str) -> Result<(), String> {
        let root = std::env::temp_dir().join(format!("job-code-context-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let dockerfile = root.join("Dockerfile");
        std::fs::write(&dockerfile, source).unwrap();
        let result = validate_reproducible_job_code_context(&dockerfile);
        std::fs::remove_dir_all(root).unwrap();
        result
    }

    #[test]
    fn accepts_locked_job_code_shape_and_ignores_comments() {
        validate(LOCKED_DOCKERFILE).unwrap();
        let with_comments = format!(
            "# trusted=yes --allow-unauthenticated curl -k http://example.com\n{LOCKED_DOCKERFILE}"
        );
        validate(&with_comments).unwrap();

        let varied = LOCKED_DOCKERFILE
            .replace("FROM ", "from   ")
            .replace("RUN printf", "run    printf")
            .replace(
                "Acquire::AllowInsecureRepositories=false",
                "ACQUIRE::ALLOWINSECUREREPOSITORIES=OFF",
            )
            .replace(
                "Acquire::AllowDowngradeToInsecureRepositories=false",
                "acquire::allowdowngradetoinsecurerepositories=No",
            )
            .replace(
                "APT::Get::AllowUnauthenticated=false",
                "apt::get::allowunauthenticated=0",
            );
        validate(&varied).unwrap();
    }

    #[test]
    fn command_classification_handles_paths_and_attached_separators() {
        for (token, expected) in [
            ("/usr/bin/apt-get&&", CommandKind::AptGet),
            ("&&/usr/bin/curl;", CommandKind::Curl),
            ("|/usr/bin/dpkg", CommandKind::Dpkg),
            ("/bin/rm", CommandKind::Rm),
        ] {
            assert_eq!(classify_command_token(token), Some(expected), "{token}");
        }
        assert!(run_command_kinds("true&&/usr/bin/apt-get update").contains(&CommandKind::AptGet));
        assert!(
            run_command_kinds("true;/usr/bin/curl -fsSL HtTpS://example.com")
                .contains(&CommandKind::Curl)
        );
        assert!(contains_url_scheme("HtTp://example.com", "http"));
        assert!(contains_url_scheme("hTtPs://example.com", "https"));
    }

    #[test]
    fn accepts_semantic_variations_of_the_committed_shape() {
        let varied = LOCKED_DOCKERFILE
            .replace(
                "ENV DEBIAN_SNAPSHOT=20260803T000000Z",
                "ENV DEBIAN_SNAPSHOT 20260803T000000Z",
            )
            .replace("apt-get", "/usr/bin/apt-get")
            .replace("RUN curl", "RUN /usr/bin/curl")
            .replace("http://snapshot.debian.org", "HtTp://snapshot.debian.org")
            .replace(" >> /etc/apt/sources.list", ">>/etc/apt/sources.list")
            .replace(" > /etc/apt/sources.list", ">/etc/apt/sources.list")
            .replace(
                "bookworm-security main\\n' \"$DEBIAN_SNAPSHOT\">>/etc/apt/sources.list\nRUN /usr/bin/apt-get",
                "bookworm-security main\\n' \"$DEBIAN_SNAPSHOT\">>/etc/apt/sources.list\nLABEL phase=certificates-ready\nRUN true\nRUN /usr/bin/apt-get",
            );
        assert!(varied.contains("LABEL phase=certificates-ready"));
        validate(&varied).unwrap();
    }

    #[test]
    fn rejects_path_and_separator_bypasses_in_complete_fixtures() {
        for invalid in [
            LOCKED_DOCKERFILE.replace(
                "https://example.com/tool-${TOOL_VERSION}",
                "HtTp://example.com/tool-${TOOL_VERSION}",
            ),
            format!("{LOCKED_DOCKERFILE}RUN true&&/usr/bin/curl -k HtTpS://example.com/tool\n"),
            format!("{LOCKED_DOCKERFILE}RUN true&&/usr/bin/apt-get update\n"),
        ] {
            assert!(validate(&invalid).is_err());
        }
    }

    #[test]
    fn rejects_truthy_apt_options_and_later_overrides() {
        for truthy in ["true", "YES", "On", "1"] {
            let invalid = LOCKED_DOCKERFILE.replacen(
                "Acquire::AllowInsecureRepositories=false",
                &format!("Acquire::AllowInsecureRepositories={truthy}"),
                1,
            );
            assert!(
                validate(&invalid).is_err(),
                "accepted truthy value {truthy}"
            );
        }

        let later_override = LOCKED_DOCKERFILE.replacen(
            "Acquire::AllowInsecureRepositories=false -o Acquire::AllowDowngradeToInsecureRepositories=false",
            "Acquire::AllowInsecureRepositories=false -o Acquire::AllowInsecureRepositories=yes -o Acquire::AllowDowngradeToInsecureRepositories=false",
            1,
        );
        assert!(validate(&later_override).is_err());
    }

    #[test]
    fn rejects_extra_http_repositories_downloads_and_packages() {
        let extra_repository = LOCKED_DOCKERFILE.replacen(
            "    && rm -f /etc/apt/sources.list.d/debian.sources \\",
            "    && printf 'deb http://mirror.example/debian bookworm main\\n' >> /etc/apt/sources.list && rm -f /etc/apt/sources.list.d/debian.sources \\",
            1,
        );
        assert!(validate(&extra_repository).is_err());

        let http_download = LOCKED_DOCKERFILE.replacen(
            "    && rm -f /etc/apt/sources.list.d/debian.sources \\",
            "    && curl http://example.com/tool -o /tmp/tool && rm -f /etc/apt/sources.list.d/debian.sources \\",
            1,
        );
        assert!(validate(&http_download).is_err());

        let non_bootstrap_package = LOCKED_DOCKERFILE.replace(
            "--no-install-recommends ca-certificates \\",
            "--no-install-recommends ca-certificates curl \\",
        );
        assert!(validate(&non_bootstrap_package).is_err());
    }

    #[test]
    fn preserves_locked_input_checks() {
        for invalid in [
            LOCKED_DOCKERFILE.replace("@sha256:abc", ":latest"),
            LOCKED_DOCKERFILE.replace(
                "RUN apt-get -o",
                "COPY bin/opencode /usr/bin/opencode\nRUN apt-get -o",
            ),
            LOCKED_DOCKERFILE.replace("RUN apt-get -o", "ARG TOOL_URL\nRUN apt-get -o"),
            LOCKED_DOCKERFILE.replace("curl -fsSL", "curl -kfsSL"),
            LOCKED_DOCKERFILE.replace(
                "ENV DEBIAN_SNAPSHOT=20260803T000000Z",
                "FROM ubuntu:latest\nENV DEBIAN_SNAPSHOT=20260803T000000Z",
            ),
        ] {
            assert!(validate(&invalid).is_err());
        }
    }

    #[test]
    fn rejects_unverified_installers_and_package_manager_resolution() {
        for invalid in [
            format!("{LOCKED_DOCKERFILE}RUN corepack prepare yarn@1.22.22 --activate\n"),
            format!("{LOCKED_DOCKERFILE}RUN /tmp/rustup-init -y --default-toolchain 1.89.0\n"),
            format!(
                "{LOCKED_DOCKERFILE}RUN curl -fsSL https://example.com/unverified-1.0.tgz -o /tmp/unverified.tgz\n"
            ),
        ] {
            assert!(validate(&invalid).is_err());
        }
    }

    #[test]
    fn apt_parser_accepts_only_literal_exact_package_versions() {
        let command = "apt-get -o Acquire::AllowInsecureRepositories=false -o Acquire::AllowDowngradeToInsecureRepositories=false -o APT::Get::AllowUnauthenticated=false install -y --no-install-recommends unbound=1.17.1-2+deb12u4";
        assert_eq!(
            parse_apt_command(command, "install").unwrap(),
            ["unbound=1.17.1-2+deb12u4"]
        );
        for package in ["unbound=$VERSION", "unbound==1.0", "=1.0", "unbound="] {
            assert!(
                parse_apt_command(
                    &command.replace("unbound=1.17.1-2+deb12u4", package),
                    "install"
                )
                .is_err()
            );
        }
    }

    #[test]
    fn rejects_remote_or_unapproved_add_and_copy_inputs() {
        for instruction in [
            "ADD local.tar /tmp/local.tar",
            "ADD https://example.com/archive.tar /tmp/archive.tar",
            "COPY https://example.com/tool /usr/local/bin/tool",
            "COPY other-file /usr/local/share/other-file",
            "COPY --from=evil.example/image@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa /tool /usr/local/bin/tool",
            "COPY --from=unknown-stage /tool /usr/local/bin/tool",
        ] {
            let invalid = format!("{LOCKED_DOCKERFILE}{instruction}\n");
            assert!(validate(&invalid).is_err(), "accepted {instruction}");
        }
    }

    #[test]
    fn rejects_indirect_or_unrecognized_network_run_fragments() {
        for instruction in [
            r#"RUN env MODE=strict curl -fsSL https://example.com/tool -o /tmp/tool && printf '%s  %s\n' "$TOOL_SHA256" /tmp/tool | sha256sum -c -"#,
            r#"RUN sh -c 'curl -fsSL https://example.com/tool -o /tmp/tool' && printf '%s  %s\n' "$TOOL_SHA256" /tmp/tool | sha256sum -c -"#,
            r#"RUN downloader=https://example.com/tool; curl -fsSL "$downloader" -o /tmp/tool && printf '%s  %s\n' "$TOOL_SHA256" /tmp/tool | sha256sum -c -"#,
            r#"RUN python3 -c 'import urllib.request; urllib.request.urlopen("https://example.com/tool")'"#,
            "RUN npm install https://example.com/package.tgz",
            "RUN npm ci",
            "RUN pnpm i",
            "RUN pip install https://example.com/package.whl",
            "RUN pip download package",
            "RUN cargo install --git https://example.com/repository",
            "RUN cargo fetch",
            "RUN go mod download",
            "RUN corepack use pnpm@latest",
            r#"RUN wget https://example.com/tool -O /tmp/tool && printf '%s  %s\n' "$TOOL_SHA256" /tmp/tool | sha256sum -c -"#,
        ] {
            let invalid = format!("{LOCKED_DOCKERFILE}{instruction}\n");
            assert!(validate(&invalid).is_err(), "accepted {instruction}");
        }
    }

    #[test]
    fn rejects_download_checksum_count_or_target_mismatches() {
        for instruction in [
            r#"RUN curl -fsSL https://example.com/one -o /tmp/one && curl -fsSL https://example.com/two -o /tmp/two && printf '%s  %s\n' "$ONE_SHA256" /tmp/one | sha256sum -c -"#,
            r#"RUN curl -fsSL https://example.com/one -o /tmp/one && printf '%s  %s\n' "$ONE_SHA256" /tmp/other | sha256sum -c -"#,
            r#"RUN curl -fsSL https://example.com/one -o /tmp/one && printf '%s  %s\n' "$ONE_SHA256" /tmp/one | sha256sum -c - && printf '%s  %s\n' "$TWO_SHA256" /tmp/two | sha256sum -c -"#,
        ] {
            let invalid = format!("{LOCKED_DOCKERFILE}{instruction}\n");
            assert!(validate(&invalid).is_err(), "accepted {instruction}");
        }
    }
}
