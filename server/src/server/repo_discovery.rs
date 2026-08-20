use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::process::Command;

#[cfg(test)]
use anyhow::Context;

use super::config::RepoDiscoveryConfig;
#[cfg(test)]
use super::config::RepoDiscoveryStrategy;

#[derive(Debug, Clone)]
pub struct DiscoveredRepo {
    pub name: String,
    pub local_path: String,
    pub remote_url: Option<String>,
}

pub fn discover_repositories_from_config(
    config: Option<&RepoDiscoveryConfig>,
    mounts: &[agent_hub_config::TriageRepositoryMount],
) -> anyhow::Result<Vec<DiscoveredRepo>> {
    let Some(config) = config else {
        return Ok(Vec::new());
    };

    if !config.enabled.unwrap_or(false) {
        return Ok(Vec::new());
    }

    let mut repos = mounts
        .iter()
        .map(|mount| build_discovered_repo(&mount.host_path))
        .collect::<Vec<_>>();

    repos.sort_by(|a, b| a.local_path.cmp(&b.local_path));
    repos.dedup_by(|a, b| a.local_path == b.local_path);

    if repos.is_empty() {
        tracing::warn!(
            "system.repo_discovery is enabled but no repositories were discovered from configured directory strategies"
        );
    }

    Ok(repos)
}

#[cfg(test)]
fn discover_repositories_from_directory_strategy(
    strategy: &RepoDiscoveryStrategy,
) -> anyhow::Result<Vec<DiscoveredRepo>> {
    let raw_path = strategy
        .path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("directory repo discovery strategy requires a non-empty path")
        })?;
    let root = expand_home(raw_path)?;

    if !root.exists() {
        tracing::warn!(
            path = %root.display(),
            "repo discovery directory does not exist; skipping"
        );
        return Ok(Vec::new());
    }

    if !root.is_dir() {
        tracing::warn!(
            path = %root.display(),
            "repo discovery path is not a directory; skipping"
        );
        return Ok(Vec::new());
    }

    let mut discovered = Vec::new();
    walk_for_repositories(&root, &root, strategy, &mut discovered)?;
    Ok(discovered)
}

#[cfg(test)]
fn walk_for_repositories(
    root: &Path,
    current: &Path,
    strategy: &RepoDiscoveryStrategy,
    discovered: &mut Vec<DiscoveredRepo>,
) -> anyhow::Result<()> {
    if current.join(".git").exists() {
        if repo_matches(root, current, strategy) {
            discovered.push(build_discovered_repo(current));
        }
        return Ok(());
    }

    let entries = std::fs::read_dir(current).with_context(|| {
        format!(
            "failed to read repo discovery directory {}",
            current.display()
        )
    })?;

    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "failed to read directory entry while scanning {}",
                current.display()
            )
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == ".git")
        {
            continue;
        }

        walk_for_repositories(root, &path, strategy, discovered)?;
    }

    Ok(())
}

#[cfg(test)]
fn repo_matches(root: &Path, repo_path: &Path, strategy: &RepoDiscoveryStrategy) -> bool {
    let rel = repo_path
        .strip_prefix(root)
        .unwrap_or(repo_path)
        .to_string_lossy()
        .replace('\\', "/");

    let include_match = if strategy.include.is_empty() {
        true
    } else {
        strategy
            .include
            .iter()
            .map(|pattern| normalize_pattern(pattern))
            .any(|pattern| glob_like_match(&pattern, &rel))
    };
    if !include_match {
        return false;
    }

    !strategy
        .exclude
        .iter()
        .map(|pattern| normalize_pattern(pattern))
        .any(|pattern| glob_like_match(&pattern, &rel))
}

#[cfg(test)]
fn normalize_pattern(pattern: &str) -> String {
    pattern
        .trim()
        .trim_matches('/')
        .replace('\\', "/")
        .to_string()
}

#[cfg(test)]
fn glob_like_match(pattern: &str, value: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }

    let pattern_tokens = tokenize_pattern(pattern);
    let value_tokens = tokenize_path(value);
    match_segments(&pattern_tokens, &value_tokens)
}

#[cfg(test)]
fn tokenize_pattern(pattern: &str) -> Vec<&str> {
    pattern
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

#[cfg(test)]
fn tokenize_path(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

#[cfg(test)]
fn match_segments(pattern: &[&str], value: &[&str]) -> bool {
    if pattern.is_empty() {
        return value.is_empty();
    }

    if pattern[0] == "**" {
        if pattern.len() == 1 {
            return true;
        }

        for skip in 0..=value.len() {
            if match_segments(&pattern[1..], &value[skip..]) {
                return true;
            }
        }
        return false;
    }

    if value.is_empty() {
        return false;
    }

    if !match_segment(pattern[0], value[0]) {
        return false;
    }

    match_segments(&pattern[1..], &value[1..])
}

#[cfg(test)]
fn match_segment(pattern: &str, value: &str) -> bool {
    let pattern_chars = pattern.chars().collect::<Vec<_>>();
    let value_chars = value.chars().collect::<Vec<_>>();
    let mut p = 0usize;
    let mut v = 0usize;
    let mut star_idx: Option<usize> = None;
    let mut match_idx = 0usize;

    while v < value_chars.len() {
        if p < pattern_chars.len()
            && (pattern_chars[p] == '?' || pattern_chars[p] == value_chars[v])
        {
            p += 1;
            v += 1;
        } else if p < pattern_chars.len() && pattern_chars[p] == '*' {
            star_idx = Some(p);
            match_idx = v;
            p += 1;
        } else if let Some(star) = star_idx {
            p = star + 1;
            match_idx += 1;
            v = match_idx;
        } else {
            return false;
        }
    }

    while p < pattern_chars.len() && pattern_chars[p] == '*' {
        p += 1;
    }

    p == pattern_chars.len()
}

fn build_discovered_repo(repo_path: &Path) -> DiscoveredRepo {
    let local_path = repo_path.to_string_lossy().into_owned();
    let name = repo_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| local_path.clone());
    let remote_url = git_origin_remote_url(repo_path);

    DiscoveredRepo {
        name,
        local_path,
        remote_url,
    }
}

fn git_origin_remote_url(repo_path: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("remote")
        .arg("get-url")
        .arg("origin")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let remote = String::from_utf8(output.stdout).ok()?;
    let trimmed = remote.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
fn expand_home(path: &str) -> anyhow::Result<PathBuf> {
    if path == "~" {
        return home_dir();
    }

    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }

    Ok(PathBuf::from(path))
}

#[cfg(test)]
fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("could not resolve HOME directory for repo discovery"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_git_worktree_root_is_discovered_without_recursive_parent_scan() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-discovery-worktree-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "gitdir: /synthetic/worktree\n").unwrap();
        let strategy = RepoDiscoveryStrategy {
            strategy_type: "directory".to_string(),
            path: Some(root.to_string_lossy().into_owned()),
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let discovered = discover_repositories_from_directory_strategy(&strategy).unwrap();
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].local_path, root.to_string_lossy());
        std::fs::remove_dir_all(root).unwrap();
    }
}
