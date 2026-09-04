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
