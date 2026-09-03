use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use agent_hub_executor::{DockerMount, ExecutionCancellationContext};
use anyhow::{Context as _, ensure};
use protocol::{
    GitObjectStoreSpec, GitRepositoryType, GitViewSpec, HostDirectoryIdentity, WorkspaceAccess,
};

const MAX_CONTROL_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 512 * 1024 * 1024;

pub(crate) struct MaterializedGitViews {
    pub(crate) mounts: Vec<DockerMount>,
    pub(crate) home_mount: DockerMount,
    root: PathBuf,
    mount_identity: String,
}

pub(crate) fn inspect_git_view(
    alias: &str,
    workspace: &Path,
    container_path: &str,
    access: WorkspaceAccess,
) -> anyhow::Result<GitViewSpec> {
    let dot_git = workspace.join(".git");
    let dot_git_metadata = std::fs::symlink_metadata(&dot_git)
        .with_context(|| format!("inspect {}", dot_git.display()))?;
    ensure!(
        !dot_git_metadata.file_type().is_symlink(),
        "workspace .git must not be a symlink"
    );
    let (repository_type, git_dir) = if dot_git_metadata.is_dir() {
        (
            GitRepositoryType::Standalone,
            canonical_real_directory(&dot_git)?,
        )
    } else {
        ensure!(
            dot_git_metadata.is_file(),
            "workspace .git must be a directory or bounded regular gitdir file"
        );
        let pointer = read_bounded_regular(&dot_git, MAX_CONTROL_FILE_BYTES, false)?;
        let pointer = std::str::from_utf8(&pointer).context("workspace .git is not UTF-8")?;
        let relative = pointer
            .strip_prefix("gitdir: ")
            .context("workspace .git has no canonical gitdir prefix")?
            .trim_end_matches(['\r', '\n']);
        ensure!(
            !relative.is_empty() && !relative.contains('\n') && !relative.contains('\r'),
            "workspace .git has a malformed gitdir"
        );
        let target = if Path::new(relative).is_absolute() {
            PathBuf::from(relative)
        } else {
            workspace.join(relative)
        };
        (
            GitRepositoryType::Linked,
            canonical_real_directory(&target)?,
        )
    };

    let commondir_path = git_dir.join("commondir");
    let common_dir = match std::fs::symlink_metadata(&commondir_path) {
        Ok(_) => {
            ensure!(
                repository_type == GitRepositoryType::Linked,
                "standalone repositories with commondir are unsupported"
            );
            let bytes = read_bounded_regular(&commondir_path, MAX_CONTROL_FILE_BYTES, false)?;
            let value = std::str::from_utf8(&bytes)
                .context("Git commondir is not UTF-8")?
                .trim_end_matches(['\r', '\n']);
            ensure!(
                !value.is_empty() && !value.contains('\n') && !value.contains('\r'),
                "Git commondir is malformed"
            );
            let target = if Path::new(value).is_absolute() {
                PathBuf::from(value)
            } else {
                git_dir.join(value)
            };
            canonical_real_directory(&target)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                repository_type == GitRepositoryType::Standalone,
                "linked worktree has no commondir"
            );
            git_dir.clone()
        }
        Err(error) => return Err(error.into()),
    };
    if repository_type == GitRepositoryType::Linked {
        let worktrees = common_dir.join("worktrees");
        ensure!(
            git_dir.parent().and_then(Path::parent) == Some(common_dir.as_path())
                && git_dir.parent() == Some(worktrees.as_path()),
            "linked worktree gitdir is not an unambiguous common worktree entry"
        );
        let backpointer =
            read_bounded_regular(&git_dir.join("gitdir"), MAX_CONTROL_FILE_BYTES, false)?;
        let backpointer = std::str::from_utf8(&backpointer)
            .context("linked worktree backpointer is not UTF-8")?
            .trim_end_matches(['\r', '\n']);
        ensure!(
            std::fs::canonicalize(backpointer)? == dot_git,
            "linked worktree backpointer does not identify this workspace"
        );
    }

    reject_submodules(workspace)?;
    let config = read_bounded_regular(&common_dir.join("config"), MAX_CONTROL_FILE_BYTES, false)?;
    let remote = validate_and_sanitize_config(&config)?;
    let objects = canonical_real_directory(&common_dir.join("objects"))?;
    reject_unsupported_object_layout(&objects)?;
    reject_unsupported_repository_layout(&git_dir, &common_dir)?;
    let head_bytes = read_bounded_regular(&git_dir.join("HEAD"), MAX_CONTROL_FILE_BYTES, false)?;
    let head_text = std::str::from_utf8(&head_bytes)
        .context("Git HEAD is not UTF-8")?
        .trim_end_matches(['\r', '\n']);
    let (head, reference) = if let Some(reference) = head_text.strip_prefix("ref: ") {
        validate_reference(reference)?;
        (
            resolve_reference(&common_dir, reference)?,
            Some(reference.to_string()),
        )
    } else {
        ensure!(is_object_id(head_text), "Git HEAD is not a SHA-1 object ID");
        (head_text.to_string(), None)
    };
    let index_path = git_dir.join("index");
    let index_sha256 = match std::fs::symlink_metadata(&index_path) {
        Ok(_) => Some(sha256(&read_bounded_regular(
            &index_path,
            MAX_INDEX_BYTES,
            false,
        )?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    Ok(GitViewSpec {
        workspace_alias: alias.to_string(),
        repository_type,
        workspace_path: path_string(workspace)?,
        workspace_container_path: container_path.to_string(),
        git_dir: directory_identity(&git_dir)?,
        common_dir: directory_identity(&common_dir)?,
        object_stores: vec![GitObjectStoreSpec {
            identity: directory_identity(&objects)?,
            container_path: format!("/agent-hub/git/views/{alias}/objects/0"),
        }],
        head,
        reference,
        index_sha256,
        remote,
        access,
        pointer_container_path: format!("{container_path}/.git"),
        git_dir_container_path: format!("/agent-hub/git/views/{alias}/git"),
    })
}

pub(crate) fn materialize_git_views(
    run_dir: &Path,
    specs: &[GitViewSpec],
    effects: &ExecutionCancellationContext,
) -> anyhow::Result<MaterializedGitViews> {
    effects.check()?;
    let root = run_dir.join("git-views");
    ensure!(
        std::fs::symlink_metadata(&root)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "run-owned Git view root already exists"
    );
    let file_effect =
        effects.begin_external_operation("attended_git_view_files", &path_string(&root)?)?;
    create_private_dir(&root)?;
    let home = root.join("home");
    create_private_dir(&home)?;
    let mut global_config = String::new();
    let mut mounts = Vec::new();
    for expected in specs {
        effects.check()?;
        let actual = inspect_git_view(
            &expected.workspace_alias,
            Path::new(&expected.workspace_path),
            &expected.workspace_container_path,
            expected.access,
        )?;
        ensure!(
            &actual == expected,
            "Git view identity changed after authorization for workspace '{}'",
            expected.workspace_alias
        );
        let view_root = root.join(&expected.workspace_alias);
        let generated_git = view_root.join("git");
        create_private_dir(&view_root)?;
        create_private_dir(&generated_git)?;
        create_private_dir(&generated_git.join("refs"))?;
        create_private_dir(&generated_git.join("refs/heads"))?;
        create_private_dir(&generated_git.join("objects"))?;
        create_private_dir(&generated_git.join("objects/info"))?;
        for index in 0..expected.object_stores.len() {
            create_private_dir(&generated_git.join(format!("objects/{index}")))?;
        }
        write_read_only(
            &view_root.join("dot-git"),
            format!("gitdir: {}\n", expected.git_dir_container_path).as_bytes(),
        )?;
        write_read_only(
            &generated_git.join("HEAD"),
            head_contents(expected).as_bytes(),
        )?;
        if let Some(reference) = &expected.reference {
            let ref_path = generated_git.join(reference);
            ensure_contained_relative(reference)?;
            if let Some(parent) = ref_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            write_read_only(&ref_path, format!("{}\n", expected.head).as_bytes())?;
        }
        copy_optional_index(expected, &generated_git.join("index"))?;
        write_filtered_packed_refs(expected, &generated_git.join("packed-refs"))?;
        let mut alternates = expected
            .object_stores
            .iter()
            .map(|store| store.container_path.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        alternates.push('\n');
        write_read_only(
            &generated_git.join("objects/info/alternates"),
            alternates.as_bytes(),
        )?;
        write_read_only(
            &generated_git.join("config"),
            sanitized_config(expected).as_bytes(),
        )?;
        global_config.push_str(&format!(
            "[safe]\n\tdirectory = {}\n",
            expected.workspace_container_path
        ));
        mounts.push(DockerMount::new(
            if expected.repository_type == GitRepositoryType::Standalone {
                generated_git.clone()
            } else {
                view_root.join("dot-git")
            },
            &expected.pointer_container_path,
            true,
        )?);
        mounts.push(DockerMount::new(
            &generated_git,
            &expected.git_dir_container_path,
            true,
        )?);
        for store in &expected.object_stores {
            verify_directory_identity(&store.identity)?;
            mounts.push(DockerMount::new(
                &store.identity.canonical_path,
                &store.container_path,
                true,
            )?);
        }
    }
    write_read_only(&home.join(".gitconfig"), global_config.as_bytes())?;
    sync_tree(&root)?;
    file_effect.completed()?;
    let home_mount = DockerMount::new(&home, "/agent-hub/git/home", true)?;
    let mount_identity = serde_json::to_string(
        &mounts
            .iter()
            .chain(std::iter::once(&home_mount))
            .map(|mount| {
                serde_json::json!({
                    "host_path": mount.host_path,
                    "container_path": mount.container_path,
                    "read_only": mount.read_only,
                })
            })
            .collect::<Vec<_>>(),
    )?;
    effects
        .begin_external_operation("attended_git_view_mount_plan", &mount_identity)?
        .completed()?;
    Ok(MaterializedGitViews {
        mounts,
        home_mount,
        root,
        mount_identity,
    })
}

impl MaterializedGitViews {
    pub(crate) fn cleanup(self, effects: &ExecutionCancellationContext) -> anyhow::Result<()> {
        remove_owned_tree(&self.root)?;
        effects.reconcile_operation_kind_resource(
            "attended_git_view_files",
            &path_string(&self.root)?,
        )?;
        effects
            .reconcile_operation_kind_resource("attended_git_view_mount_plan", &self.mount_identity)
    }
}

pub(crate) fn recover_git_views(
    run_dir: &Path,
    effects: &ExecutionCancellationContext,
) -> anyhow::Result<()> {
    let root = run_dir.join("git-views");
    if root.exists() {
        remove_owned_tree(&root)?;
    }
    effects.reconcile_operation_kind_resource("attended_git_view_files", &path_string(&root)?)?;
    for identity in effects.unresolved_operation_resources("attended_git_view_mount_plan") {
        effects.reconcile_operation_kind_resource("attended_git_view_mount_plan", &identity)?;
    }
    Ok(())
}

fn copy_optional_index(spec: &GitViewSpec, destination: &Path) -> anyhow::Result<()> {
    let source = Path::new(&spec.git_dir.canonical_path).join("index");
    match &spec.index_sha256 {
        Some(expected) => {
            let bytes = read_bounded_regular(&source, MAX_INDEX_BYTES, false)?;
            ensure!(
                &sha256(&bytes) == expected,
                "Git index changed during materialization"
            );
            write_read_only(destination, &bytes)
        }
        None => {
            ensure!(
                std::fs::symlink_metadata(source)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "Git index appeared during materialization"
            );
            Ok(())
        }
    }
}

fn write_filtered_packed_refs(spec: &GitViewSpec, destination: &Path) -> anyhow::Result<()> {
    let source = Path::new(&spec.common_dir.canonical_path).join("packed-refs");
    let bytes = match std::fs::symlink_metadata(&source) {
        Ok(_) => read_bounded_regular(&source, MAX_CONTROL_FILE_BYTES, false)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let text = std::str::from_utf8(&bytes).context("packed-refs is not UTF-8")?;
    let mut output = String::from("# pack-refs with: peeled fully-peeled sorted\n");
    if let Some(reference) = &spec.reference {
        for line in text.lines() {
            if line
                .split_once(' ')
                .is_some_and(|(_, candidate)| candidate == reference)
            {
                output.push_str(line);
                output.push('\n');
            }
        }
    }
    write_read_only(destination, output.as_bytes())
}

fn sanitized_config(spec: &GitViewSpec) -> String {
    let mut config = format!(
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tworktree = {}\n\tlogallrefupdates = false\n",
        spec.workspace_container_path
    );
    if let Some(remote) = &spec.remote {
        config.push_str(&format!("[remote \"origin\"]\n\turl = {remote}\n"));
    }
    config
}

fn head_contents(spec: &GitViewSpec) -> String {
    spec.reference
        .as_ref()
        .map(|reference| format!("ref: {reference}\n"))
        .unwrap_or_else(|| format!("{}\n", spec.head))
}

fn validate_and_sanitize_config(bytes: &[u8]) -> anyhow::Result<Option<String>> {
    let text = std::str::from_utf8(bytes).context("Git config is not UTF-8")?;
    let mut section = String::new();
    let mut origin_remote = false;
    let mut remote = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            ensure!(line.ends_with(']'), "Git config section is malformed");
            section = line[1..line.len() - 1].trim().to_ascii_lowercase();
            origin_remote = section == "remote \"origin\"";
            ensure!(
                section != "include"
                    && section != "includeif"
                    && !section.starts_with("includeif ")
                    && !section.starts_with("filter ")
                    && !section.starts_with("credential ")
                    && !section.starts_with("alias ")
                    && !section.starts_with("submodule "),
                "Git config contains an executable or unsupported section"
            );
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .context("Git config assignment is malformed")?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        let dangerous = (section == "core"
            && matches!(
                key.as_str(),
                "hookspath"
                    | "fsmonitor"
                    | "sshcommand"
                    | "pager"
                    | "sparsecheckout"
                    | "sparsecheckoutcone"
            ))
            || (section.starts_with("diff ")
                && matches!(key.as_str(), "external" | "textconv" | "command"))
            || section == "credential"
            || section == "alias"
            || section == "extensions"
            || (section.starts_with("remote ")
                && matches!(key.as_str(), "promisor" | "partialclonefilter"));
        ensure!(
            !dangerous,
            "Git config contains executable or unsupported behavior"
        );
        if origin_remote && key == "url" {
            ensure!(remote.is_none(), "Git origin has ambiguous URL identities");
            remote = Some(crate::local::canonical_remote_identity(value)?);
        }
    }
    Ok(remote)
}

fn reject_unsupported_repository_layout(git_dir: &Path, common_dir: &Path) -> anyhow::Result<()> {
    for path in [
        common_dir.join("shallow"),
        common_dir.join("objects/info/grafts"),
        git_dir.join("info/sparse-checkout"),
    ] {
        match std::fs::symlink_metadata(&path) {
            Ok(_) => anyhow::bail!(
                "unsupported Git repository metadata is present: {}",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    for entry in std::fs::read_dir(git_dir)? {
        if entry?
            .file_name()
            .to_string_lossy()
            .starts_with("sharedindex.")
        {
            anyhow::bail!("split Git indexes are unsupported");
        }
    }
    Ok(())
}

fn reject_submodules(workspace: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(workspace.join(".gitmodules")) {
        Ok(_) => anyhow::bail!("submodules are unsupported in attended Git views"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn reject_unsupported_object_layout(objects: &Path) -> anyhow::Result<()> {
    let alternates = objects.join("info/alternates");
    match std::fs::symlink_metadata(&alternates) {
        Ok(_) => {
            let bytes = read_bounded_regular(&alternates, MAX_CONTROL_FILE_BYTES, false)?;
            ensure!(
                bytes.iter().all(u8::is_ascii_whitespace),
                "Git alternates are unsupported because object-store identity would be ambiguous"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let pack = objects.join("pack");
    if pack.is_dir() {
        for entry in std::fs::read_dir(pack)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().ends_with(".promisor") {
                anyhow::bail!("partial-clone object stores are unsupported");
            }
        }
    }
    Ok(())
}

fn resolve_reference(common_dir: &Path, reference: &str) -> anyhow::Result<String> {
    ensure_contained_relative(reference)?;
    let loose = common_dir.join(reference);
    match std::fs::symlink_metadata(&loose) {
        Ok(_) => {
            let bytes = read_bounded_regular(&loose, MAX_CONTROL_FILE_BYTES, false)?;
            let value = std::str::from_utf8(&bytes)
                .context("Git loose ref is not UTF-8")?
                .trim_end_matches(['\r', '\n']);
            ensure!(
                is_object_id(value),
                "Git loose ref is not a SHA-1 object ID"
            );
            return Ok(value.to_string());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let packed = common_dir.join("packed-refs");
    let bytes = read_bounded_regular(&packed, MAX_CONTROL_FILE_BYTES, false)
        .context("Git HEAD ref is neither loose nor packed")?;
    let text = std::str::from_utf8(&bytes).context("packed-refs is not UTF-8")?;
    let matches = text
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, candidate)| *candidate == reference)
        .collect::<Vec<_>>();
    ensure!(
        matches.len() == 1,
        "Git packed ref identity is missing or ambiguous"
    );
    ensure!(
        is_object_id(matches[0].0),
        "Git packed ref is not a SHA-1 object ID"
    );
    Ok(matches[0].0.to_string())
}

fn validate_reference(reference: &str) -> anyhow::Result<()> {
    ensure!(
        reference.starts_with("refs/heads/")
            && !reference.contains("..")
            && !reference.contains("//")
            && !reference.contains("@{")
            && !reference.ends_with('/')
            && !reference
                .split('/')
                .any(|component| component.starts_with('.') || component.ends_with(".lock"))
            && reference
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && !b"~^:?*[\\".contains(&byte)),
        "Git HEAD reference is unsupported"
    );
    Ok(())
}

fn ensure_contained_relative(value: &str) -> anyhow::Result<()> {
    let path = Path::new(value);
    ensure!(!path.is_absolute(), "Git metadata path must be relative");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "Git metadata path escapes its generated view"
    );
    Ok(())
}

fn canonical_real_directory(path: &Path) -> anyhow::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("canonicalize Git directory {}", path.display()))?;
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Git metadata path is not a real directory: {}",
        path.display()
    );
    Ok(canonical)
}

pub(crate) fn directory_identity(path: &Path) -> anyhow::Result<HostDirectoryIdentity> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(metadata.is_dir(), "Git identity is not a directory");
    Ok(HostDirectoryIdentity {
        canonical_path: path_string(path)?,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

pub(crate) fn verify_directory_identity(identity: &HostDirectoryIdentity) -> anyhow::Result<()> {
    ensure!(
        directory_identity(Path::new(&identity.canonical_path))? == *identity,
        "Git host directory identity changed after authorization"
    );
    Ok(())
}

fn read_bounded_regular(path: &Path, maximum: u64, allow_empty: bool) -> anyhow::Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open bounded Git control file {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "Git control path is not a regular file");
    ensure!(
        metadata.len() <= maximum && (allow_empty || metadata.len() > 0),
        "Git control file is empty or exceeds its bound"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "Git control file changed while reading"
    );
    Ok(bytes)
}

fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_read_only(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true).mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o400))?;
    Ok(())
}

fn sync_tree(root: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            sync_tree(&path)?;
        }
    }
    File::open(root)?.sync_all()?;
    Ok(())
}

fn remove_owned_tree(root: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(root)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "run-owned Git view root is not a real directory"
    );
    make_tree_writable(root)?;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

fn make_tree_writable(root: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "run-owned Git view contains a symlink"
        );
        if metadata.is_dir() {
            make_tree_writable(&path)?;
        } else {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn path_string(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_string)
        .context("Git paths must be valid UTF-8")
}

fn sha256(bytes: &[u8]) -> String {
    protocol::sha256_hex(bytes)
}

fn is_object_id(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hub_executor::Cancellation;
    use std::process::Command;

    fn git(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn repository() -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("agent-hub-git-view-{}", uuid::Uuid::new_v4()));
        let repository = root.join("repository");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-q", "-b", "main"]);
        git(&repository, &["config", "user.name", "Agent Hub Test"]);
        git(
            &repository,
            &["config", "user.email", "agent-hub@example.com"],
        );
        std::fs::write(repository.join("tracked.txt"), "one\n").unwrap();
        git(&repository, &["add", "tracked.txt"]);
        git(&repository, &["commit", "-q", "-m", "one"]);
        std::fs::write(repository.join("tracked.txt"), "two\n").unwrap();
        git(&repository, &["commit", "-qam", "two"]);
        (root, repository)
    }

    #[test]
    fn standalone_linked_detached_dirty_and_packed_views_are_bound() {
        let (root, repository) = repository();
        git(&repository, &["pack-refs", "--all"]);
        let sibling = root.join("sibling");
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                sibling.to_str().unwrap(),
            ],
        );
        std::fs::write(repository.join("tracked.txt"), "dirty\n").unwrap();

        let standalone = inspect_git_view(
            "primary",
            &repository,
            "/workspace/primary",
            WorkspaceAccess::ReadWrite,
        )
        .unwrap();
        let linked = inspect_git_view(
            "sibling",
            &sibling,
            "/workspace/sibling",
            WorkspaceAccess::ReadOnly,
        )
        .unwrap();
        assert_eq!(standalone.repository_type, GitRepositoryType::Standalone);
        assert_eq!(standalone.reference.as_deref(), Some("refs/heads/main"));
        assert_eq!(linked.repository_type, GitRepositoryType::Linked);
        assert!(linked.reference.is_none());
        assert_eq!(linked.common_dir, standalone.common_dir);
        assert_eq!(
            linked.object_stores,
            standalone
                .object_stores
                .iter()
                .cloned()
                .map(|mut store| {
                    store.container_path = "/agent-hub/git/views/sibling/objects/0".into();
                    store
                })
                .collect::<Vec<_>>()
        );
        let run = root.join("run");
        std::fs::create_dir(&run).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        let effects =
            ExecutionCancellationContext::new(&run.join("effects"), Cancellation::never()).unwrap();
        let materialized = materialize_git_views(&run, &[linked], &effects).unwrap();
        let generated = run.join("git-views/sibling/git");
        assert!(
            !std::fs::read_to_string(generated.join("packed-refs"))
                .unwrap()
                .contains("refs/heads/main")
        );
        let config = std::fs::read_to_string(generated.join("config")).unwrap();
        assert!(!config.contains("worktrees"));
        assert!(!config.contains(&standalone.common_dir.canonical_path));
        materialized.cleanup(&effects).unwrap();
        effects.finalize().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malicious_metadata_and_config_execution_are_rejected_without_execution() {
        let (root, repository) = repository();
        let marker = root.join("executed");
        std::fs::write(
            repository.join(".git/config"),
            format!(
                "[core]\n\trepositoryformatversion = 0\n\tfsmonitor = touch {}\n",
                marker.display()
            ),
        )
        .unwrap();
        assert!(
            inspect_git_view(
                "primary",
                &repository,
                "/workspace/primary",
                WorkspaceAccess::ReadWrite,
            )
            .is_err()
        );
        assert!(!marker.exists());

        std::fs::write(
            repository.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .unwrap();
        std::fs::create_dir_all(repository.join(".git/objects/info")).unwrap();
        std::fs::write(
            repository.join(".git/objects/info/alternates"),
            root.join("other/objects").to_string_lossy().as_bytes(),
        )
        .unwrap();
        assert!(
            inspect_git_view(
                "primary",
                &repository,
                "/workspace/primary",
                WorkspaceAccess::ReadWrite,
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn symlinked_git_metadata_is_rejected() {
        let (root, repository) = repository();
        let git_dir = root.join("git-dir");
        std::fs::rename(repository.join(".git"), &git_dir).unwrap();
        std::os::unix::fs::symlink(&git_dir, repository.join(".git")).unwrap();
        assert!(
            inspect_git_view(
                "primary",
                &repository,
                "/workspace/primary",
                WorkspaceAccess::ReadOnly,
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_gitdir_and_escaped_commondir_are_rejected() {
        let (root, repository) = repository();
        let sibling = root.join("sibling");
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                sibling.to_str().unwrap(),
            ],
        );
        std::fs::write(sibling.join(".git"), "not-a-gitdir\n").unwrap();
        assert!(
            inspect_git_view(
                "sibling",
                &sibling,
                "/workspace/sibling",
                WorkspaceAccess::ReadOnly,
            )
            .is_err()
        );

        let git_dir = repository.join(".git/worktrees/sibling");
        std::fs::write(
            sibling.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        std::fs::write(git_dir.join("commondir"), root.to_string_lossy().as_bytes()).unwrap();
        assert!(
            inspect_git_view(
                "sibling",
                &sibling,
                "/workspace/sibling",
                WorkspaceAccess::ReadOnly,
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_files_and_mount_plan_are_journal_owned_and_cleaned() {
        let (root, repository) = repository();
        let host_metadata = metadata_snapshot(&repository.join(".git"));
        let spec = inspect_git_view(
            "primary",
            &repository,
            "/workspace/primary",
            WorkspaceAccess::ReadWrite,
        )
        .unwrap();
        let run = root.join("run");
        std::fs::create_dir(&run).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        let effects =
            ExecutionCancellationContext::new(&run.join("effects"), Cancellation::never()).unwrap();
        let materialized = materialize_git_views(&run, &[spec], &effects).unwrap();
        assert!(materialized.mounts.iter().all(|mount| mount.read_only));
        assert!(run.join("git-views/primary/dot-git").is_file());
        assert!(run.join("git-views/primary/git/index").is_file());
        assert_eq!(metadata_snapshot(&repository.join(".git")), host_metadata);
        materialized.cleanup(&effects).unwrap();
        effects.finalize().unwrap();
        assert!(!run.join("git-views").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancelled_materialization_fails_before_creating_a_view() {
        let (root, repository) = repository();
        let spec = inspect_git_view(
            "primary",
            &repository,
            "/workspace/primary",
            WorkspaceAccess::ReadWrite,
        )
        .unwrap();
        let run = root.join("run");
        std::fs::create_dir(&run).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (tx, rx) = tokio::sync::watch::channel(true);
        drop(tx);
        let effects =
            ExecutionCancellationContext::new(&run.join("effects"), Cancellation::new(rx)).unwrap();
        assert!(materialize_git_views(&run, &[spec], &effects).is_err());
        assert!(!run.join("git-views").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires a real Docker daemon and AGENT_HUB_GIT_TEST_IMAGE"]
    fn real_docker_git_view_supports_read_commands_and_denies_metadata_writes() {
        let image = std::env::var("AGENT_HUB_GIT_TEST_IMAGE")
            .expect("AGENT_HUB_GIT_TEST_IMAGE must name a local image with git and sh");
        let (root, repository) = repository();
        let known_sha = git(&repository, &["rev-parse", "HEAD~1"]);
        std::fs::write(repository.join("tracked.txt"), "dirty\n").unwrap();
        let before = metadata_snapshot(&repository.join(".git"));
        let spec = inspect_git_view(
            "primary",
            &repository,
            "/workspace/primary",
            WorkspaceAccess::ReadWrite,
        )
        .unwrap();
        let run = root.join("run");
        std::fs::create_dir(&run).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        let effects =
            ExecutionCancellationContext::new(&run.join("effects"), Cancellation::never()).unwrap();
        let views = materialize_git_views(&run, &[spec], &effects).unwrap();
        let mut args = vec!["run".to_string(), "--rm".into()];
        let workspace_mount = DockerMount::new(&repository, "/workspace/primary", false).unwrap();
        for mount in std::iter::once(&workspace_mount)
            .chain(views.mounts.iter())
            .chain(std::iter::once(&views.home_mount))
        {
            args.extend(["--mount".into(), mount.bind_argument()]);
        }
        for (name, value) in [
            ("HOME", "/agent-hub/git/home"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/agent-hub/git/home/.gitconfig"),
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_PAGER", "cat"),
            ("PAGER", "cat"),
        ] {
            args.extend(["-e".into(), format!("{name}={value}")]);
        }
        args.extend([
            "--workdir".into(),
            "/workspace/primary".into(),
            "--entrypoint".into(),
            "sh".into(),
            image,
            "-ec".into(),
            format!(
                "git status --short; git diff -- tracked.txt; git show {known_sha}; git log -1; git rev-parse HEAD; git branch; git ls-files; git blame tracked.txt; ! git branch forbidden"
            ),
        ]);
        let status = Command::new("docker").args(args).status().unwrap();
        assert!(status.success());
        assert_eq!(metadata_snapshot(&repository.join(".git")), before);
        views.cleanup(&effects).unwrap();
        effects.finalize().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires AGENT_HUB_PRIVATE_GO_DOCKER_INTEGRATION=1, cleanup consent, and a pinned local image and Go version with sh/git/python3/go"]
    fn real_docker_private_go_two_linked_worktree_fixture() {
        run_real_docker_private_go_two_linked_worktree_fixture().unwrap();
    }

    #[cfg(target_os = "linux")]
    fn run_real_docker_private_go_two_linked_worktree_fixture() -> anyhow::Result<()> {
        use agent_hub_executor::credential_provider::{CredentialProvider, resolve_all_for_job};
        use agent_hub_executor::{DockerExecutionPlan, DockerOwnership, NetworkMode};
        use std::collections::BTreeMap;
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Duration;

        anyhow::ensure!(
            std::env::var("AGENT_HUB_PRIVATE_GO_DOCKER_INTEGRATION").as_deref() == Ok("1"),
            "AGENT_HUB_PRIVATE_GO_DOCKER_INTEGRATION=1 is required"
        );
        anyhow::ensure!(
            std::env::var("AGENT_HUB_PRIVATE_GO_ALLOW_CLEANUP").as_deref() == Ok("1"),
            "AGENT_HUB_PRIVATE_GO_ALLOW_CLEANUP=1 is required"
        );
        let image_reference = std::env::var("AGENT_HUB_PRIVATE_GO_TEST_IMAGE")?;
        let go_version = std::env::var("AGENT_HUB_PRIVATE_GO_TEST_GO_VERSION")?;
        anyhow::ensure!(
            image_reference.contains("@sha256:") || image_reference.starts_with("sha256:"),
            "AGENT_HUB_PRIVATE_GO_TEST_IMAGE must be an immutable digest reference"
        );
        anyhow::ensure!(
            go_version.starts_with("go1.") && !go_version.chars().any(char::is_whitespace),
            "AGENT_HUB_PRIVATE_GO_TEST_GO_VERSION must be an exact go1.x version"
        );
        let docker = PathBuf::from("docker");
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target")
            .join(format!(
                "agent-hub-private-go-docker-{}",
                uuid::Uuid::new_v4().simple()
            ));
        std::fs::create_dir(&root)?;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        let namespace = uuid::Uuid::new_v4().simple().to_string();
        let network = format!("agent-hub-private-go-{namespace}");
        let origin = format!("agent-hub-private-go-origin-{namespace}");
        let job = format!("agent-hub-private-go-job-{namespace}");
        let fixture_label = "agent-hub.reddit.com/private-go-fixture";
        let token = format!("synthetic-private-go-{namespace}");
        let token_path = root.join("fixture-token");
        std::fs::write(&token_path, &token)?;
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600))?;

        let command = |args: &[String]| -> anyhow::Result<std::process::Output> {
            Ok(Command::new(&docker).args(args).output()?)
        };
        let docker_ok =
            |args: &[String], operation: &str| -> anyhow::Result<std::process::Output> {
                let output = command(args)?;
                anyhow::ensure!(
                    output.status.success(),
                    "{operation}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(output)
            };
        let image_inspect = command(&[
            "image".into(),
            "inspect".into(),
            "--format={{.Id}}".into(),
            image_reference.clone(),
        ])?;
        anyhow::ensure!(
            image_inspect.status.success(),
            "inspect fixture image: {}",
            String::from_utf8_lossy(&image_inspect.stderr)
        );
        let image_identity = String::from_utf8(image_inspect.stdout)?.trim().to_string();
        docker_ok(
            &[
                "network".into(),
                "create".into(),
                "--internal".into(),
                "--label".into(),
                format!("{fixture_label}={namespace}"),
                network.clone(),
            ],
            "create fixture network",
        )?;
        struct FixtureCleanup {
            docker: PathBuf,
            network: String,
            origin: String,
            job: String,
            armed: bool,
        }
        impl FixtureCleanup {
            fn disarm(&mut self) {
                self.armed = false;
            }
        }
        impl Drop for FixtureCleanup {
            fn drop(&mut self) {
                if !self.armed {
                    return;
                }
                let _ = Command::new(&self.docker)
                    .args(["rm", "-f", &self.job])
                    .output();
                let _ = Command::new(&self.docker)
                    .args(["rm", "-f", &self.origin])
                    .output();
                let _ = Command::new(&self.docker)
                    .args(["network", "rm", &self.network])
                    .output();
            }
        }
        let mut cleanup = FixtureCleanup {
            docker: docker.clone(),
            network: network.clone(),
            origin: origin.clone(),
            job: job.clone(),
            armed: true,
        };

        let operation = (|| -> anyhow::Result<()> {
            let deterministic_git = |tree: &Path, args: &[&str]| -> anyhow::Result<String> {
                let output = Command::new("git")
                    .arg("-C")
                    .arg(tree)
                    .args(args)
                    .env("GIT_AUTHOR_NAME", "Agent Hub Test")
                    .env("GIT_AUTHOR_EMAIL", "agent-hub@example.com")
                    .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
                    .env("GIT_COMMITTER_NAME", "Agent Hub Test")
                    .env("GIT_COMMITTER_EMAIL", "agent-hub@example.com")
                    .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
                    .output()?;
                anyhow::ensure!(
                    output.status.success(),
                    "git {args:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(String::from_utf8(output.stdout)?.trim().to_string())
            };
            let source = root.join("source");
            std::fs::create_dir(&source)?;
            deterministic_git(&source, &["init", "-q", "-b", "main"])?;
            std::fs::write(source.join("seed"), "seed\n")?;
            deterministic_git(&source, &["add", "seed"])?;
            deterministic_git(&source, &["commit", "-q", "-m", "seed"])?;
            let primary = root.join("primary");
            let sibling = root.join("sibling");
            deterministic_git(
                &source,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "fixture-primary",
                    primary.to_str().unwrap(),
                ],
            )?;
            deterministic_git(
                &source,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "fixture-sibling",
                    sibling.to_str().unwrap(),
                ],
            )?;
            std::fs::write(sibling.join("sentinel.txt"), "sibling-isolated\n")?;
            deterministic_git(&sibling, &["add", "sentinel.txt"])?;
            deterministic_git(&sibling, &["commit", "-q", "-m", "sibling sentinel"])?;
            std::fs::write(
                primary.join("go.mod"),
                "module example.com/agent-hub/private-go-consumer\n\ngo 1.23\n\nrequire github.snooguts.net/private/module.git v0.0.1\n",
            )?;
            std::fs::write(
                primary.join("main.go"),
                "package main\nimport private \"github.snooguts.net/private/module.git\"\nfunc main(){println(private.Value)}\n",
            )?;
            deterministic_git(&primary, &["add", "go.mod", "main.go"])?;
            deterministic_git(&primary, &["commit", "-q", "-m", "consumer"])?;
            let primary_head = deterministic_git(&primary, &["rev-parse", "HEAD"])?;
            let sibling_head = deterministic_git(&sibling, &["rev-parse", "HEAD"])?;
            let sibling_before = std::fs::read(sibling.join("sentinel.txt"))?;

            let module_source = root.join("module-source");
            std::fs::create_dir(&module_source)?;
            deterministic_git(&module_source, &["init", "-q", "-b", "main"])?;
            std::fs::write(
                module_source.join("go.mod"),
                "module github.snooguts.net/private/module.git\n\ngo 1.23\n",
            )?;
            std::fs::write(
                module_source.join("module.go"),
                "package module\nconst Value = 42\n",
            )?;
            deterministic_git(&module_source, &["add", "go.mod", "module.go"])?;
            deterministic_git(&module_source, &["commit", "-q", "-m", "private module"])?;
            deterministic_git(&module_source, &["tag", "v0.0.1"])?;
            let http_root = root.join("http");
            std::fs::create_dir(&http_root)?;
            let bare = http_root.join("private/module");
            std::fs::create_dir_all(bare.parent().unwrap())?;
            let clone = std::process::Command::new("git")
                .args(["clone", "--bare", "-q"])
                .arg(&module_source)
                .arg(&bare)
                .status()?;
            anyhow::ensure!(clone.success(), "create bare module fixture");
            deterministic_git(&bare, &["update-server-info"])?;
            let common_metadata_before = metadata_snapshot(&source.join(".git"));
            let evidence = root.join("origin.log");
            std::fs::write(&evidence, b"")?;
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/private_go_http_origin.py");
            let origin_output = docker_ok(
                &[
                    "run".into(),
                    "-d".into(),
                    "--name".into(),
                    origin.clone(),
                    "--label".into(),
                    format!("{fixture_label}={namespace}"),
                    format!("--network={network}"),
                    "--network-alias=github.snooguts.net".into(),
                    "--read-only".into(),
                    "--security-opt=no-new-privileges".into(),
                    "--cap-drop=ALL".into(),
                    "--tmpfs=/tmp:rw,nosuid,nodev,size=16m".into(),
                    "--user".into(),
                    format!("{}:{}", unsafe { libc::geteuid() }, unsafe {
                        libc::getegid()
                    }),
                    format!(
                        "--mount=type=bind,src={},dst=/git,readonly",
                        http_root.display()
                    ),
                    format!(
                        "--mount=type=bind,src={},dst=/evidence.log",
                        evidence.display()
                    ),
                    format!(
                        "--mount=type=bind,src={},dst=/origin.py,readonly",
                        fixture.display()
                    ),
                    format!(
                        "--mount=type=bind,src={},dst=/fixture-token,readonly",
                        token_path.display()
                    ),
                    "-e=FIXTURE_GIT_ROOT=/git".into(),
                    "-e=FIXTURE_EVIDENCE=/evidence.log".into(),
                    "-e=FIXTURE_TOKEN_FILE=/fixture-token".into(),
                    "--entrypoint=python3".into(),
                    image_reference.clone(),
                    "/origin.py".into(),
                ],
                "start authenticated Git HTTP fixture",
            )?;
            anyhow::ensure!(
                !origin_output
                    .stdout
                    .windows(token.len())
                    .any(|bytes| bytes == token.as_bytes())
                    && !origin_output
                        .stderr
                        .windows(token.len())
                        .any(|bytes| bytes == token.as_bytes()),
                "synthetic token escaped through origin startup output"
            );
            let ready_args = [
                "exec".to_string(),
                origin.clone(),
                "python3".into(),
                "-c".into(),
                "import socket; socket.create_connection(('127.0.0.1', 8080), 1).close()".into(),
            ];
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let output = command(&ready_args)?;
                anyhow::ensure!(
                    !output
                        .stdout
                        .windows(token.len())
                        .any(|bytes| bytes == token.as_bytes())
                        && !output
                            .stderr
                            .windows(token.len())
                            .any(|bytes| bytes == token.as_bytes()),
                    "synthetic token escaped through readiness output"
                );
                if output.status.success() {
                    break;
                }
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "authenticated origin did not become ready within 10 seconds"
                );
                std::thread::sleep(Duration::from_millis(100));
            }

            let run_dir = root.join("run");
            let credentials_dir = run_dir.join("credentials");
            std::fs::create_dir_all(&credentials_dir)?;
            let effects =
                ExecutionCancellationContext::new(&run_dir.join("effects"), Cancellation::never())?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let provider = CredentialProvider::GithubToken {
                name: "github:github.snooguts.net:read_write".into(),
                host: "github.snooguts.net".into(),
                api_base_url: "https://github.snooguts.net/api/v3".into(),
                access: "read_write".into(),
                command: "/bin/cat".into(),
                args: vec![token_path.to_string_lossy().into_owned()],
                target: "/agent-hub/github/tokens/github.snooguts.net".into(),
            };
            match &provider {
                CredentialProvider::GithubToken {
                    name,
                    host,
                    api_base_url,
                    access,
                    command,
                    args,
                    target,
                } => anyhow::ensure!(
                    name == "github:github.snooguts.net:read_write"
                        && host == "github.snooguts.net"
                        && api_base_url == "https://github.snooguts.net/api/v3"
                        && access == "read_write"
                        && command == "/bin/cat"
                        && args == &[token_path.to_string_lossy().into_owned()]
                        && target == "/agent-hub/github/tokens/github.snooguts.net",
                    "synthetic GitHub provider is not exact"
                ),
                _ => anyhow::bail!("synthetic GitHub provider has the wrong type"),
            }
            let credentials = runtime.block_on(resolve_all_for_job(
                &[provider],
                Some(std::time::SystemTime::now() + std::time::Duration::from_secs(120)),
                Some(&credentials_dir),
                &effects,
            ))?;
            anyhow::ensure!(credentials.env_vars["GOPRIVATE"] == "github.snooguts.net");
            let specs = vec![
                inspect_git_view(
                    "primary",
                    &primary,
                    "/workspace/primary",
                    WorkspaceAccess::ReadWrite,
                )?,
                inspect_git_view(
                    "sibling",
                    &sibling,
                    "/workspace/sibling",
                    WorkspaceAccess::ReadOnly,
                )?,
            ];
            anyhow::ensure!(
                specs
                    .iter()
                    .all(|view| view.repository_type == GitRepositoryType::Linked)
            );
            let views = materialize_git_views(&run_dir, &specs, &effects)?;
            anyhow::ensure!(views.mounts.iter().all(|mount| mount.read_only));
            let mut mounts = vec![
                DockerMount::new(&primary, "/workspace/primary", false)?,
                DockerMount::new(&sibling, "/workspace/sibling", true)?,
            ];
            mounts.extend(views.mounts.iter().cloned());
            mounts.push(views.home_mount.clone());
            let material = credentials.material()?;
            mounts.extend(material.mounts);
            let mut environment: BTreeMap<String, String> = material.environment;
            environment.extend([
                ("HOME".into(), "/tmp/home".into()),
                (
                    "GIT_CONFIG_SYSTEM".into(),
                    "/agent-hub/git/home/.gitconfig".into(),
                ),
                ("GIT_CONFIG_NOSYSTEM".into(), "0".into()),
                ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
                ("GONOSUMDB".into(), "github.snooguts.net".into()),
                ("GOINSECURE".into(), "github.snooguts.net".into()),
                ("GOPROXY".into(), "direct".into()),
                ("GOTOOLCHAIN".into(), "local".into()),
                ("GOMODCACHE".into(), "/tmp/go-mod".into()),
                ("EXPECTED_GO_VERSION".into(), go_version.clone()),
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                (
                    "GIT_CONFIG_KEY_0".into(),
                    "url.http://github.snooguts.net:8080/.insteadOf".into(),
                ),
                (
                    "GIT_CONFIG_VALUE_0".into(),
                    "https://github.snooguts.net/".into(),
                ),
            ]);
            let script = format!(
                r#"set -eu
mkdir -p "$HOME"
test "$GOPRIVATE" = github.snooguts.net
test "$GONOSUMDB" = github.snooguts.net
test "$GOINSECURE" = github.snooguts.net
test "$GOPROXY" = direct
test "$GOTOOLCHAIN" = local
test "$GOMODCACHE" = /tmp/go-mod
test "$(go env GOVERSION)" = "$EXPECTED_GO_VERSION"
test "$(go list -m)" = example.com/agent-hub/private-go-consumer
git ls-remote https://github.snooguts.net/private/module refs/tags/v0.0.1 >/dev/null
test "$(git rev-parse HEAD)" = '{primary_head}'
test -z "$(git status --short)"
test "$(git -C /workspace/sibling rev-parse HEAD)" = '{sibling_head}'
test -z "$(git -C /workspace/sibling status --short)"
test -z "$(git -C /workspace/sibling diff)"
test "$(cat /workspace/sibling/sentinel.txt)" = sibling-isolated
! touch /workspace/sibling/forbidden
! git branch forbidden
go mod download github.snooguts.net/private/module.git@v0.0.1
gofmt -w main.go
go test ./...
go vet ./...
go build ./...
test -x private-go-consumer
rm private-go-consumer
test "$(git rev-parse HEAD)" = '{primary_head}'
"#
            );
            let plan = DockerExecutionPlan {
                image_identity,
                container_name: job.clone(),
                entrypoint: "sh".into(),
                command_args: vec!["-ceu".into(), script],
                mounts,
                environment,
                network: NetworkMode::Named(network.clone()),
                dns: Vec::new(),
                extra_hosts: Vec::new(),
                ownership: DockerOwnership::new(BTreeMap::from([(
                    fixture_label.into(),
                    namespace.clone(),
                )]))?,
                user: format!("{}:{}", unsafe { libc::geteuid() }, unsafe {
                    libc::getegid()
                }),
                workdir: Some("/workspace/primary".into()),
                memory_limit: "2g".into(),
                cpu_limit: "2".into(),
                pids_limit: 256,
                read_only: true,
                tmpfs: vec!["/tmp:rw,nosuid,nodev,size=512m".into()],
            };
            let plan_debug = format!("{plan:?}");
            anyhow::ensure!(
                !plan_debug.contains(&token),
                "synthetic token escaped into Docker plan"
            );
            anyhow::ensure!(
                plan.environment
                    .values()
                    .all(|value| !value.contains(&token)),
                "synthetic token escaped into Docker environment"
            );
            let plan_args = plan.materialize_args()?;
            anyhow::ensure!(
                plan_args.iter().all(|arg| !arg.contains(&token)),
                "synthetic token escaped into Docker argv"
            );
            let output = command(&plan_args)?;
            anyhow::ensure!(
                !output
                    .stdout
                    .windows(token.len())
                    .any(|bytes| bytes == token.as_bytes())
                    && !output
                        .stderr
                        .windows(token.len())
                        .any(|bytes| bytes == token.as_bytes()),
                "synthetic token escaped through job output"
            );
            anyhow::ensure!(
                output.status.success(),
                "job failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            anyhow::ensure!(git(&primary, &["rev-parse", "HEAD"]) == primary_head);
            anyhow::ensure!(git(&sibling, &["rev-parse", "HEAD"]) == sibling_head);
            anyhow::ensure!(std::fs::read(sibling.join("sentinel.txt"))? == sibling_before);
            anyhow::ensure!(
                std::fs::read_to_string(primary.join("main.go"))?
                    == "package main\n\nimport private \"github.snooguts.net/private/module.git\"\n\nfunc main() { println(private.Value) }\n",
                "gofmt output was not exact"
            );
            let go_sum = std::fs::read_to_string(primary.join("go.sum"))?;
            let go_sum_lines = go_sum.lines().collect::<Vec<_>>();
            anyhow::ensure!(
                go_sum_lines.len() == 2
                    && go_sum_lines[0]
                        .starts_with("github.snooguts.net/private/module.git v0.0.1 h1:")
                    && go_sum_lines[1]
                        .starts_with("github.snooguts.net/private/module.git v0.0.1/go.mod h1:"),
                "go.sum changes were not exact: {go_sum:?}"
            );
            anyhow::ensure!(
                git(&primary, &["status", "--short"]) == "M main.go\n?? go.sum",
                "primary formatted/go.sum changes were not exact"
            );
            anyhow::ensure!(
                metadata_snapshot(&source.join(".git")) == common_metadata_before,
                "shared Git metadata changed"
            );
            let log = std::fs::read_to_string(&evidence)?;
            anyhow::ensure!(log.contains("authorized GET /private/module/"));
            anyhow::ensure!(!log.contains(&token));
            views.cleanup(&effects)?;
            effects.reconcile_owned_credentials(&credentials_dir, None)?;
            std::fs::remove_dir_all(&credentials_dir)?;
            effects.finalize()?;
            fn ensure_tree_has_no_token(path: &Path, token: &[u8]) -> anyhow::Result<()> {
                for entry in std::fs::read_dir(path)? {
                    let path = entry?.path();
                    let metadata = std::fs::symlink_metadata(&path)?;
                    anyhow::ensure!(
                        !metadata.file_type().is_symlink(),
                        "fixture contains an unexpected symlink: {}",
                        path.display()
                    );
                    if metadata.is_dir() {
                        ensure_tree_has_no_token(&path, token)?;
                    } else {
                        let bytes = std::fs::read(&path)?;
                        anyhow::ensure!(
                            !bytes
                                .windows(token.len())
                                .any(|candidate| candidate == token),
                            "synthetic token escaped into {}",
                            path.display()
                        );
                    }
                }
                Ok(())
            }
            std::fs::remove_file(&token_path)?;
            ensure_tree_has_no_token(&run_dir, token.as_bytes())?;
            ensure_tree_has_no_token(&primary, token.as_bytes())?;
            ensure_tree_has_no_token(&sibling, token.as_bytes())?;
            ensure_tree_has_no_token(&module_source, token.as_bytes())?;
            ensure_tree_has_no_token(&http_root, token.as_bytes())?;
            std::fs::remove_dir_all(&run_dir)?;
            anyhow::ensure!(!run_dir.exists(), "fixture run directory leaked");
            Ok(())
        })();

        let strict_cleanup = (|| -> anyhow::Result<()> {
            for (kind, name) in [
                ("container", &job),
                ("container", &origin),
                ("network", &network),
            ] {
                let remove_args = if kind == "container" {
                    vec!["rm".into(), "-f".into(), name.clone()]
                } else {
                    vec!["network".into(), "rm".into(), name.clone()]
                };
                let removed = command(&remove_args)?;
                if !removed.status.success() {
                    let stderr = String::from_utf8_lossy(&removed.stderr).to_ascii_lowercase();
                    let not_found = if kind == "container" {
                        stderr.contains("no such container")
                    } else {
                        stderr.contains("no such network")
                            || stderr.contains("network ") && stderr.contains(" not found")
                    };
                    anyhow::ensure!(
                        not_found,
                        "remove fixture {kind} {name} failed: {}",
                        String::from_utf8_lossy(&removed.stderr)
                    );
                }
                let absent = command(&[kind.into(), "inspect".into(), name.clone()])?;
                anyhow::ensure!(!absent.status.success(), "fixture leaked {kind} {name}");
                let stderr = String::from_utf8_lossy(&absent.stderr).to_ascii_lowercase();
                anyhow::ensure!(
                    if kind == "container" {
                        stderr.contains("no such container") || stderr.contains("no such object")
                    } else {
                        stderr.contains("no such network")
                            || stderr.contains("network ") && stderr.contains(" not found")
                    },
                    "fixture {kind} absence was ambiguous: {}",
                    String::from_utf8_lossy(&absent.stderr)
                );
            }
            let containers_by_label = command(&[
                "ps".into(),
                "-aq".into(),
                "--filter".into(),
                format!("label={fixture_label}={namespace}"),
            ])?;
            let containers_by_name = command(&[
                "ps".into(),
                "-aq".into(),
                "--filter".into(),
                format!("name=^/({job}|{origin})$"),
            ])?;
            let networks_by_label = command(&[
                "network".into(),
                "ls".into(),
                "-q".into(),
                "--filter".into(),
                format!("label={fixture_label}={namespace}"),
            ])?;
            let networks_by_name = command(&[
                "network".into(),
                "ls".into(),
                "-q".into(),
                "--filter".into(),
                format!("name=^{network}$"),
            ])?;
            for (description, output) in [
                ("container label", containers_by_label),
                ("container name", containers_by_name),
                ("network label", networks_by_label),
                ("network name", networks_by_name),
            ] {
                anyhow::ensure!(
                    output.status.success(),
                    "Docker {description} absence query failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                anyhow::ensure!(
                    output.stdout.iter().all(u8::is_ascii_whitespace),
                    "fixture resources remain by {description}"
                );
            }
            Ok(())
        })();
        match (operation, strict_cleanup) {
            (Ok(()), Ok(())) => {
                std::fs::remove_dir_all(&root)?;
                anyhow::ensure!(!root.exists(), "fixture root leaked");
                cleanup.disarm();
                Ok(())
            }
            (Err(error), Ok(())) => {
                Err(error.context(format!("fixture state preserved at {}", root.display())))
            }
            (Ok(()), Err(error)) => {
                Err(error.context(format!("fixture state preserved at {}", root.display())))
            }
            (Err(operation), Err(cleanup_error)) => Err(operation.context(format!(
                "cleanup also failed ({cleanup_error:#}); fixture state preserved at {}",
                root.display()
            ))),
        }
    }

    fn metadata_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn visit(root: &Path, path: &Path, output: &mut Vec<(PathBuf, Vec<u8>)>) {
            let mut entries = std::fs::read_dir(path)
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                if path.is_dir() {
                    visit(root, &path, output);
                } else {
                    output.push((
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        std::fs::read(path).unwrap(),
                    ));
                }
            }
        }
        let mut output = Vec::new();
        visit(root, root, &mut output);
        output
    }
}
