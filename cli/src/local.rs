use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, ensure};
pub use protocol::WorkspaceAccess;
use protocol::{ContextRootSpec, RepositoryMountSpec, WorkspaceSpec};

const RESERVED_ALIASES: &[&str] = &["artifacts", "config", "context", "opencode", "output"];

#[derive(Debug, Clone)]
pub struct ApprovedRoots(Vec<PathBuf>);

#[derive(Debug, Clone)]
pub struct RequestedWorkspace {
    pub alias: String,
    pub path: PathBuf,
    pub access: WorkspaceAccess,
}

#[derive(Debug, Clone)]
pub struct RequestedContextRoot {
    pub alias: String,
    pub path: PathBuf,
    pub access: WorkspaceAccess,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMounts {
    pub workspaces: Vec<WorkspaceSpec>,
    pub context_roots: Vec<ContextRootSpec>,
    pub repositories: Vec<RepositoryMountSpec>,
    pub primary_workspace: String,
}

impl ApprovedRoots {
    pub fn new(roots: Vec<PathBuf>) -> anyhow::Result<Self> {
        ensure!(!roots.is_empty(), "approved roots must not be empty");
        let mut canonical = roots
            .into_iter()
            .map(|root| {
                let root = std::fs::canonicalize(&root)
                    .with_context(|| format!("canonicalize approved root {}", root.display()))?;
                ensure!(
                    root.is_dir(),
                    "approved root '{}' is not a directory",
                    root.display()
                );
                ensure!(root != Path::new("/"), "filesystem root cannot be approved");
                Ok(root)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        canonical.sort();
        canonical.dedup();
        for pair in canonical.windows(2) {
            ensure!(
                !pair[1].starts_with(&pair[0]),
                "approved roots overlap: '{}' and '{}'",
                pair[0].display(),
                pair[1].display()
            );
        }
        Ok(Self(canonical))
    }

    fn resolve(&self, requested: &Path) -> anyhow::Result<PathBuf> {
        ensure_canonical_input_path(requested)?;
        let path = std::fs::canonicalize(requested)
            .with_context(|| format!("canonicalize requested path {}", requested.display()))?;
        ensure!(
            path.is_dir(),
            "requested path '{}' is not a directory",
            path.display()
        );
        ensure!(
            self.0.iter().any(|root| path.starts_with(root)),
            "requested path '{}' is outside approved roots",
            path.display()
        );
        Ok(path)
    }
}

pub fn resolve_mounts(
    roots: &ApprovedRoots,
    requested_workspaces: &[RequestedWorkspace],
    requested_contexts: &[RequestedContextRoot],
    primary_workspace: &str,
) -> anyhow::Result<ResolvedMounts> {
    ensure!(
        !requested_workspaces.is_empty(),
        "attended execution requires a workspace"
    );
    validate_alias(primary_workspace)?;
    let mut aliases = BTreeSet::new();
    let mut hosts = Vec::new();
    let mut workspaces = Vec::new();
    let mut repositories = Vec::new();
    for requested in requested_workspaces {
        validate_alias(&requested.alias)?;
        ensure!(
            aliases.insert(requested.alias.clone()),
            "duplicate mount alias '{}'",
            requested.alias
        );
        let host = roots.resolve(&requested.path)?;
        ensure!(
            !is_read_only_context_namespace(&host),
            "{} is restricted to read-only context mounts",
            host.display()
        );
        reject_host_overlap(&hosts, &host)?;
        hosts.push(host.clone());
        let container_path = format!("/workspace/{}", requested.alias);
        workspaces.push(WorkspaceSpec {
            alias: requested.alias.clone(),
            host_path: path_string(&host)?,
            container_path: container_path.clone(),
            access: requested.access,
        });
        repositories.push(repository_spec(&requested.alias, &host, &container_path)?);
    }
    ensure!(
        workspaces
            .iter()
            .any(|workspace| workspace.alias == primary_workspace),
        "primary workspace '{}' was not requested",
        primary_workspace
    );
    let mut context_roots = Vec::new();
    for requested in requested_contexts {
        ensure!(
            requested.access == WorkspaceAccess::ReadOnly,
            "context roots must use explicit read-only access"
        );
        validate_alias(&requested.alias)?;
        ensure!(
            aliases.insert(requested.alias.clone()),
            "duplicate mount alias '{}'",
            requested.alias
        );
        let host = roots.resolve(&requested.path)?;
        reject_host_overlap(&hosts, &host)?;
        hosts.push(host.clone());
        context_roots.push(ContextRootSpec {
            alias: requested.alias.clone(),
            host_path: path_string(&host)?,
            container_path: format!("/context/{}", requested.alias),
        });
    }
    workspaces.sort();
    context_roots.sort();
    repositories.sort();
    Ok(ResolvedMounts {
        workspaces,
        context_roots,
        repositories,
        primary_workspace: primary_workspace.to_string(),
    })
}

fn validate_alias(alias: &str) -> anyhow::Result<()> {
    ensure!(
        !alias.is_empty()
            && alias.len() <= 63
            && alias
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && alias.as_bytes()[0].is_ascii_alphanumeric()
            && alias.as_bytes()[alias.len() - 1].is_ascii_alphanumeric(),
        "mount alias '{}' is not canonical",
        alias
    );
    ensure!(
        !RESERVED_ALIASES.contains(&alias),
        "mount alias '{}' is reserved",
        alias
    );
    Ok(())
}

fn ensure_canonical_input_path(path: &Path) -> anyhow::Result<()> {
    let value = path
        .to_str()
        .context("attended paths must be valid UTF-8")?;
    ensure!(!value.is_empty(), "attended path must not be empty");
    ensure!(path.is_absolute(), "attended path must be absolute");
    ensure!(
        !value.contains(':') && !value.contains(',') && !value.chars().any(char::is_control),
        "attended path contains an unsupported delimiter"
    );
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("canonicalize attended path {}", path.display()))?;
    ensure!(
        canonical == path,
        "attended path '{}' is not canonical",
        path.display()
    );
    Ok(())
}

fn is_read_only_context_namespace(path: &Path) -> bool {
    ["/home/nick.davies/code", "/home/nick.davies/oss"]
        .iter()
        .any(|root| path.starts_with(root))
}

fn reject_host_overlap(existing: &[PathBuf], candidate: &Path) -> anyhow::Result<()> {
    for path in existing {
        ensure!(
            !candidate.starts_with(path) && !path.starts_with(candidate),
            "requested mount paths overlap: '{}' and '{}'",
            path.display(),
            candidate.display()
        );
    }
    Ok(())
}

fn repository_spec(
    alias: &str,
    root: &Path,
    mount_path: &str,
) -> anyhow::Result<RepositoryMountSpec> {
    let git = |args: &[&str]| -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()?;
        if output.status.success() {
            Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
        } else {
            None
        }
    };
    Ok(RepositoryMountSpec {
        workspace_alias: alias.to_string(),
        repository_root: path_string(root)?,
        mount_path: mount_path.to_string(),
        remote: git(&["remote", "get-url", "origin"]),
        head: git(&["rev-parse", "HEAD"]).unwrap_or_default(),
    })
}

fn path_string(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_string)
        .context("attended paths must be valid UTF-8")
}
