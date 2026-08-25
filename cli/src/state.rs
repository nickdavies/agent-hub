use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde::{Serialize, de::DeserializeOwned};

#[derive(Debug, Clone)]
pub struct StateStore {
    root: PathBuf,
}

pub struct StateLock(File);

impl Drop for StateLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

impl StateStore {
    pub fn open(root: PathBuf) -> anyhow::Result<Self> {
        ensure!(!root.as_os_str().is_empty(), "state root path is empty");
        ensure!(root.is_absolute(), "state root path must be absolute");
        if root.exists() {
            verify_owned_directory(&root)?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create_json<T: Serialize>(&self, relative: &Path, value: &T) -> anyhow::Result<()> {
        self.ensure_root()?;
        let path = self.checked_path(relative)?;
        ensure_parent(&self.root, &path)?;
        ensure!(
            !path.exists(),
            "state file already exists: {}",
            path.display()
        );
        let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec_pretty(value)?;
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        let published = std::fs::hard_link(&temporary, &path)
            .with_context(|| format!("atomically publish {}", path.display()));
        let removed = std::fs::remove_file(&temporary);
        published?;
        removed?;
        sync_parent(&path)?;
        Ok(())
    }

    pub fn lock(&self, relative: &Path) -> anyhow::Result<StateLock> {
        self.ensure_root()?;
        let path = self.checked_path(relative)?;
        ensure_parent(&self.root, &path)?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(&path)?;
        set_mode(&path, 0o600)?;
        verify_owned_file(&path)?;
        sync_parent(&path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if result != 0 {
                return Err(std::io::Error::last_os_error()).context("lock attended state");
            }
        }
        Ok(StateLock(file))
    }

    pub fn replace_json<T: Serialize>(&self, relative: &Path, value: &T) -> anyhow::Result<()> {
        self.ensure_root()?;
        let path = self.checked_path(relative)?;
        ensure_parent(&self.root, &path)?;
        let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec_pretty(value)?;
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        sync_parent(&path)?;
        Ok(())
    }

    pub fn read_json<T: DeserializeOwned>(&self, relative: &Path) -> anyhow::Result<T> {
        let path = self.checked_path(relative)?;
        serde_json::from_reader(open_owned_file(&path)?)
            .with_context(|| format!("parse {}", path.display()))
    }

    pub fn rename(&self, from: &Path, to: &Path) -> anyhow::Result<()> {
        self.ensure_root()?;
        let from = self.checked_path(from)?;
        let to = self.checked_path(to)?;
        verify_owned_file(&from)?;
        ensure!(
            !to.exists(),
            "state destination already exists: {}",
            to.display()
        );
        ensure_parent(&self.root, &to)?;
        std::fs::rename(&from, &to)?;
        sync_parent(&from)?;
        if from.parent() != to.parent() {
            sync_parent(&to)?;
        }
        Ok(())
    }

    pub fn path(&self, relative: &Path) -> anyhow::Result<PathBuf> {
        self.checked_path(relative)
    }

    pub fn list_json<T: DeserializeOwned>(&self, relative: &Path) -> anyhow::Result<Vec<T>> {
        let directory = self.checked_path(relative)?;
        if !directory.exists() {
            return Ok(Vec::new());
        }
        verify_owned_directory(&directory)?;
        let mut paths = std::fs::read_dir(&directory)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?;
        paths.sort();
        paths
            .into_iter()
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
            .map(|path| {
                serde_json::from_reader(open_owned_file(&path)?)
                    .with_context(|| format!("parse {}", path.display()))
            })
            .collect()
    }

    fn checked_path(&self, relative: &Path) -> anyhow::Result<PathBuf> {
        ensure!(!relative.is_absolute(), "state path must be relative");
        ensure!(
            relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
            "state path is not canonical"
        );
        Ok(self.root.join(relative))
    }

    fn ensure_root(&self) -> anyhow::Result<()> {
        if self.root.exists() {
            return verify_owned_directory(&self.root);
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(&self.root)
            .with_context(|| format!("create attended state root {}", self.root.display()))?;
        set_mode(&self.root, 0o700)?;
        sync_parent(&self.root)?;
        verify_owned_directory(&self.root)
    }
}

fn ensure_parent(root: &Path, path: &Path) -> anyhow::Result<()> {
    let parent = path.parent().context("state path has no parent")?;
    if !parent.exists() {
        let relative = parent
            .strip_prefix(root)
            .context("state parent escaped root")?;
        let mut current = root.to_path_buf();
        for component in relative.components() {
            current.push(component);
            if !current.exists() {
                std::fs::create_dir(&current)?;
                set_mode(&current, 0o700)?;
                sync_parent(&current)?;
            }
        }
    }
    let relative = parent
        .strip_prefix(root)
        .context("state parent escaped root")?;
    let mut current = root.to_path_buf();
    verify_owned_directory(&current)?;
    for component in relative.components() {
        current.push(component);
        verify_owned_directory(&current)?;
    }
    Ok(())
}

#[cfg(unix)]
fn open_owned_file(path: &Path) -> anyhow::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    verify_file_metadata(&file.metadata()?)?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_owned_file(path: &Path) -> anyhow::Result<File> {
    verify_owned_file(path)?;
    Ok(File::open(path)?)
}

#[cfg(unix)]
fn verify_owned_directory(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "state directory is not a real directory"
    );
    ensure!(
        metadata.uid() == std::fs::metadata("/proc/self")?.uid(),
        "state directory is not owned by the current user"
    );
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "state directory is accessible by another user"
    );
    Ok(())
}

#[cfg(not(unix))]
fn verify_owned_directory(path: &Path) -> anyhow::Result<()> {
    ensure!(path.is_dir(), "state directory is not a directory");
    Ok(())
}

#[cfg(unix)]
fn verify_owned_file(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    verify_file_metadata(&metadata)
}

#[cfg(unix)]
fn verify_file_metadata(metadata: &std::fs::Metadata) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "state file is not a real file"
    );
    ensure!(
        metadata.uid() == std::fs::metadata("/proc/self")?.uid(),
        "state file is not owned by the current user"
    );
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "state file is accessible by another user"
    );
    Ok(())
}

#[cfg(not(unix))]
fn verify_owned_file(path: &Path) -> anyhow::Result<()> {
    ensure!(path.is_file(), "state file is not a file");
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> anyhow::Result<()> {
    Ok(())
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    File::open(path.parent().context("state path has no parent")?)?.sync_all()?;
    Ok(())
}
