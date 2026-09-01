use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, ensure};
pub use protocol::WorkspaceAccess;
use protocol::{
    ContextRootSpec, RepositoryMountSpec, RepositoryState, ResolvedRoot, RootRole, WorkspaceSpec,
};

const RESERVED_ALIASES: &[&str] = &["artifacts", "config", "context", "opencode", "output"];

#[derive(Debug, Clone)]
pub struct ApprovedRoots(Vec<ApprovedRoot>);

#[derive(Debug, Clone)]
struct ApprovedRoot {
    identity: String,
    path: PathBuf,
    roles: BTreeSet<RootRole>,
}

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
    pub roots: Vec<ResolvedRoot>,
    pub workspaces: Vec<WorkspaceSpec>,
    pub context_roots: Vec<ContextRootSpec>,
    pub repositories: Vec<RepositoryMountSpec>,
    pub git_views: Vec<protocol::GitViewSpec>,
    pub primary_workspace: String,
}

impl ApprovedRoots {
    pub fn new(registry: config::RootRegistryConfig) -> anyhow::Result<Self> {
        let roots = registry.roots;
        ensure!(!roots.is_empty(), "approved roots must not be empty");
        let mut canonical = roots
            .into_iter()
            .map(|root| {
                let configured = PathBuf::from(&root.path);
                let canonical = std::fs::canonicalize(&configured).with_context(|| {
                    format!("canonicalize approved root {}", configured.display())
                })?;
                ensure!(
                    canonical == configured,
                    "approved root '{}' resolves through a symlink or noncanonical component",
                    configured.display()
                );
                ensure!(
                    canonical.is_dir(),
                    "approved root '{}' is not a directory",
                    canonical.display()
                );
                ensure!(
                    canonical != Path::new("/"),
                    "filesystem root cannot be approved"
                );
                Ok(ApprovedRoot {
                    identity: root.identity,
                    path: canonical,
                    roles: root.roles.into_iter().map(protocol_root_role).collect(),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        canonical.sort_by(|left, right| left.path.cmp(&right.path));
        for pair in canonical.windows(2) {
            ensure!(
                !pair[1].path.starts_with(&pair[0].path),
                "approved roots overlap: '{}' and '{}'",
                pair[0].path.display(),
                pair[1].path.display()
            );
        }
        Ok(Self(canonical))
    }

    fn resolve(
        &self,
        requested: &Path,
        role: RootRole,
    ) -> anyhow::Result<(&ApprovedRoot, PathBuf)> {
        ensure_canonical_input_path(requested)?;
        let path = std::fs::canonicalize(requested)
            .with_context(|| format!("canonicalize requested path {}", requested.display()))?;
        ensure!(
            path.is_dir(),
            "requested path '{}' is not a directory",
            path.display()
        );
        let root = self
            .0
            .iter()
            .find(|root| path.starts_with(&root.path))
            .with_context(|| {
                format!(
                    "requested path '{}' is outside approved roots",
                    path.display()
                )
            })?;
        ensure!(
            root.roles.contains(&role),
            "requested path '{}' is not authorized for root role '{}'",
            path.display(),
            root_role_name(role)
        );
        Ok((root, path))
    }
}

fn protocol_root_role(role: config::RootRole) -> RootRole {
    match role {
        config::RootRole::WorkspaceRo => RootRole::WorkspaceRo,
        config::RootRole::WorkspaceRw => RootRole::WorkspaceRw,
        config::RootRole::ContextRo => RootRole::ContextRo,
        config::RootRole::GitMetadata => RootRole::GitMetadata,
    }
}

fn root_role_name(role: RootRole) -> &'static str {
    match role {
        RootRole::WorkspaceRo => "workspace_ro",
        RootRole::WorkspaceRw => "workspace_rw",
        RootRole::ContextRo => "context_ro",
        RootRole::GitMetadata => "git_metadata",
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
    let mut git_views = Vec::new();
    for requested in requested_workspaces {
        validate_alias(&requested.alias)?;
        ensure!(
            aliases.insert(requested.alias.clone()),
            "duplicate mount alias '{}'",
            requested.alias
        );
        let role = match requested.access {
            WorkspaceAccess::ReadOnly => RootRole::WorkspaceRo,
            WorkspaceAccess::ReadWrite => RootRole::WorkspaceRw,
        };
        let (root, host) = roots.resolve(&requested.path, role)?;
        reject_host_overlap(&hosts, &host)?;
        hosts.push(host.clone());
        let container_path = format!("/workspace/{}", requested.alias);
        workspaces.push(WorkspaceSpec {
            root_identity: root.identity.clone(),
            alias: requested.alias.clone(),
            host_path: path_string(&host)?,
            container_path: container_path.clone(),
            access: requested.access,
        });
        let git_view = crate::git_view::inspect_git_view(
            &requested.alias,
            &host,
            &container_path,
            requested.access,
        )?;
        ensure!(
            root.roles.contains(&RootRole::GitMetadata),
            "workspace '{}' is a Git repository but its root lacks git_metadata authority",
            host.display()
        );
        repositories.push(RepositoryMountSpec {
            workspace_alias: requested.alias.clone(),
            repository_root: path_string(&host)?,
            mount_path: container_path,
            remote: git_view.remote.clone(),
            access: requested.access,
            starting: RepositoryState {
                head: Some(git_view.head.clone()),
                reference: git_view.reference.clone(),
                branch: git_view
                    .reference
                    .as_deref()
                    .and_then(|reference| reference.strip_prefix("refs/heads/"))
                    .map(str::to_string),
                detached: git_view.reference.is_none(),
                dirty_paths: repository_state(&host)?.dirty_paths,
            },
        });
        git_views.push(git_view);
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
        let (root, host) = roots.resolve(&requested.path, RootRole::ContextRo)?;
        reject_host_overlap(&hosts, &host)?;
        hosts.push(host.clone());
        context_roots.push(ContextRootSpec {
            root_identity: root.identity.clone(),
            alias: requested.alias.clone(),
            host_path: path_string(&host)?,
            container_path: format!("/context/{}", requested.alias),
        });
    }
    workspaces.sort();
    context_roots.sort();
    repositories.sort();
    git_views.sort();
    let mut resolved_roots = roots
        .0
        .iter()
        .filter(|root| {
            workspaces
                .iter()
                .any(|mount| mount.root_identity == root.identity)
                || context_roots
                    .iter()
                    .any(|mount| mount.root_identity == root.identity)
        })
        .map(|root| ResolvedRoot {
            identity: root.identity.clone(),
            host_path: path_string(&root.path).expect("validated root paths are UTF-8"),
            roles: root.roles.iter().copied().collect(),
        })
        .collect::<Vec<_>>();
    resolved_roots.sort();
    Ok(ResolvedMounts {
        roots: resolved_roots,
        workspaces,
        context_roots,
        repositories,
        git_views,
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

pub(crate) fn canonical_remote_identity(remote: &str) -> anyhow::Result<String> {
    ensure!(
        !remote.is_empty() && !remote.chars().any(char::is_control),
        "repository remote identity is empty or contains control characters"
    );
    if let Ok(url) = url::Url::parse(remote) {
        ensure!(
            url.password().is_none() && url.query().is_none() && url.fragment().is_none(),
            "repository remote identity contains credentials, query, or fragment"
        );
        let host = url
            .host_str()
            .context("repository remote URL has no host")?;
        ensure!(
            matches!(url.scheme(), "git" | "http" | "https" | "ssh"),
            "repository remote URL uses an unsupported scheme"
        );
        let port = url
            .port()
            .map(|port| format!(":{port}"))
            .unwrap_or_default();
        return Ok(format!(
            "{}://{}{}{}",
            url.scheme(),
            host.to_ascii_lowercase(),
            port,
            url.path()
        ));
    }
    if let Some((authority, path)) = remote.split_once(':')
        && authority.contains('@')
    {
        let host = authority.rsplit('@').next().unwrap_or_default();
        ensure!(
            !host.is_empty() && !path.is_empty(),
            "repository SSH remote is incomplete"
        );
        return Ok(format!("ssh://{}/{}", host.to_ascii_lowercase(), path));
    }
    let path = Path::new(remote);
    ensure!(
        path.is_absolute(),
        "repository remote identity is not canonical"
    );
    path_string(path)
}

pub fn repository_state(root: &Path) -> anyhow::Result<RepositoryState> {
    let output = |args: &[&str]| -> anyhow::Result<std::process::Output> {
        Command::new("git")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", "/nonexistent-agent-hub-git-home")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .with_context(|| format!("inspect repository {}", root.display()))
    };
    let head = output(&["rev-parse", "HEAD"])?;
    ensure!(head.status.success(), "repository has no resolvable HEAD");
    let head = String::from_utf8(head.stdout)?.trim().to_string();
    let reference = output(&["symbolic-ref", "-q", "HEAD"])?;
    let reference = reference
        .status
        .success()
        .then(|| String::from_utf8(reference.stdout))
        .transpose()?
        .map(|value| value.trim().to_string());
    let branch = reference
        .as_deref()
        .and_then(|value| value.strip_prefix("refs/heads/"))
        .map(str::to_string);
    let status = output(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    ensure!(status.status.success(), "inspect repository dirty state");
    let mut dirty_paths = BTreeSet::new();
    let fields = status
        .stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        ensure!(field.len() >= 4, "git status emitted a malformed entry");
        dirty_paths.insert(
            String::from_utf8(field[3..].to_vec()).context("repository path is not UTF-8")?,
        );
        if matches!(field[0], b'R' | b'C') || matches!(field[1], b'R' | b'C') {
            index += 1;
            ensure!(index < fields.len(), "git status omitted a rename source");
            dirty_paths.insert(
                String::from_utf8(fields[index].to_vec())
                    .context("repository rename source is not UTF-8")?,
            );
        }
        index += 1;
    }
    Ok(RepositoryState {
        head: Some(head),
        reference,
        branch: branch.clone(),
        detached: branch.is_none(),
        dirty_paths: dirty_paths.into_iter().collect(),
    })
}

fn path_string(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_string)
        .context("attended paths must be valid UTF-8")
}
