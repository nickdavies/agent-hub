use crate::local::{
    ApprovedRoots, RequestedContextRoot, RequestedWorkspace, WorkspaceAccess,
    canonicalize_existing_ancestor, reject_protected_mounts, resolve_mounts,
};
use std::path::Path;

#[cfg(unix)]
#[test]
fn missing_protected_path_resolves_symlinked_existing_ancestor() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/test-tmp")
        .join(format!("protected-missing-{}", uuid::Uuid::new_v4()));
    let real = root.join("real");
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, root.join("alias")).unwrap();
    let resolved = canonicalize_existing_ancestor(&root.join("alias/missing/state")).unwrap();
    assert_eq!(resolved, real.canonicalize().unwrap().join("missing/state"));
    assert!(canonicalize_existing_ancestor(&root.join("alias/../escape")).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn protected_control_plane_rejects_equal_parent_descendant_and_symlink_mounts() {
    let root = std::env::temp_dir().join(format!(
        "agent-hub-protected-paths-{}",
        uuid::Uuid::new_v4()
    ));
    let trusted = root.join("deployment/config");
    let child = trusted.join("credentials");
    let state = root.join("state");
    let bin = root.join("deployment/bin");
    let ordinary = root.join("worktrees/task");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&ordinary).unwrap();
    let executable = bin.join("agent-hub");
    let credential = child.join("token");
    std::fs::write(&executable, "binary").unwrap();
    std::fs::write(&credential, "secret").unwrap();
    let root_path = root.canonicalize().unwrap();
    let trusted = trusted.canonicalize().unwrap();
    let child = child.canonicalize().unwrap();
    let state = state.canonicalize().unwrap();
    let bin = bin.canonicalize().unwrap();
    let executable = executable.canonicalize().unwrap();
    let credential = credential.canonicalize().unwrap();
    let ordinary = ordinary.canonicalize().unwrap();

    for mount in [&root_path, &trusted, &child] {
        assert!(reject_protected_mounts([mount.as_path()], [trusted.as_path()]).is_err());
    }
    let protected = [
        trusted.as_path(),
        state.as_path(),
        executable.as_path(),
        credential.as_path(),
    ];
    for mount in [&state, &bin, &child] {
        assert!(reject_protected_mounts([mount.as_path()], protected).is_err());
    }
    assert!(reject_protected_mounts([ordinary.as_path()], protected).is_ok());

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&ordinary, root.join("workspace-link")).unwrap();
        assert!(
            reject_protected_mounts([root.join("workspace-link").as_path()], [trusted.as_path()])
                .is_err()
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

fn initialize_repository(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(path.join("tracked.txt"), "starting\n").unwrap();
    git(&["add", "tracked.txt"]);
    git(&[
        "-c",
        "user.name=Agent Hub Test",
        "-c",
        "user.email=agent-hub@example.com",
        "commit",
        "-q",
        "-m",
        "initial",
    ]);
}

fn approved_root(identity: &str, path: &std::path::Path) -> ApprovedRoots {
    ApprovedRoots::new(config::RootRegistryConfig {
        roots: vec![config::RootConfig {
            identity: identity.into(),
            path: path.to_string_lossy().into_owned(),
            roles: vec![
                config::RootRole::WorkspaceRo,
                config::RootRole::WorkspaceRw,
                config::RootRole::ContextRo,
                config::RootRole::GitMetadata,
            ],
        }],
    })
    .unwrap()
}

#[test]
fn mount_resolution_is_repeatable_and_rejects_reserved_shadowing() {
    let temp =
        std::env::temp_dir().join(format!("agent-hub-attended-paths-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    initialize_repository(&temp.join("repo"));
    let roots = approved_root("test", &temp);
    let requested = vec![RequestedWorkspace {
        alias: "primary".into(),
        path: temp.join("repo"),
        access: WorkspaceAccess::ReadWrite,
    }];
    let first = resolve_mounts(&roots, &requested, &[], "primary").unwrap();
    let second = resolve_mounts(&roots, &requested, &[], "primary").unwrap();
    assert_eq!(first, second);

    let shadow = vec![
        RequestedWorkspace {
            alias: "primary".into(),
            path: temp.join("repo"),
            access: WorkspaceAccess::ReadWrite,
        },
        RequestedWorkspace {
            alias: "artifacts".into(),
            path: temp.join("repo"),
            access: WorkspaceAccess::ReadOnly,
        },
    ];
    assert!(resolve_mounts(&roots, &shadow, &[], "primary").is_err());
    std::fs::remove_dir_all(temp).unwrap();
}

#[test]
fn context_roots_must_be_read_only_and_approved() {
    let root =
        std::env::temp_dir().join(format!("agent-hub-attended-context-{}", std::process::id()));
    initialize_repository(&root.join("workspace"));
    std::fs::create_dir_all(root.join("context/child")).unwrap();
    let roots = approved_root("test", &root);
    let workspaces = vec![RequestedWorkspace {
        alias: "primary".into(),
        path: root.join("workspace"),
        access: WorkspaceAccess::ReadWrite,
    }];
    let contexts = vec![RequestedContextRoot {
        alias: "code".into(),
        path: root.join("context"),
        access: WorkspaceAccess::ReadOnly,
    }];
    let resolved = resolve_mounts(&roots, &workspaces, &contexts, "primary").unwrap();
    assert!(
        resolved.context_roots[0]
            .container_path
            .starts_with("/context/")
    );
    let contexts = vec![RequestedContextRoot {
        alias: "code-child".into(),
        path: root.join("context/child"),
        access: WorkspaceAccess::ReadOnly,
    }];
    assert!(resolve_mounts(&roots, &workspaces, &contexts, "primary").is_ok());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn mount_resolution_rejects_implicit_modes_symlinks_and_overlap() {
    let root = std::env::temp_dir().join(format!(
        "agent-hub-attended-strict-paths-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(root.join("first/child")).unwrap();
    let roots = approved_root("test", &root);
    let context = RequestedContextRoot {
        alias: "reference".into(),
        path: root.join("first"),
        access: WorkspaceAccess::ReadWrite,
    };
    assert!(resolve_mounts(&roots, &[], &[context], "").is_err());

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("first"), root.join("link")).unwrap();
        let workspace = RequestedWorkspace {
            alias: "primary".into(),
            path: root.join("link"),
            access: WorkspaceAccess::ReadWrite,
        };
        assert!(resolve_mounts(&roots, &[workspace], &[], "primary").is_err());
    }

    let workspaces = vec![RequestedWorkspace {
        alias: "primary".into(),
        path: root.join("first"),
        access: WorkspaceAccess::ReadWrite,
    }];
    let contexts = vec![RequestedContextRoot {
        alias: "reference".into(),
        path: root.join("first/child"),
        access: WorkspaceAccess::ReadOnly,
    }];
    assert!(resolve_mounts(&roots, &workspaces, &contexts, "primary").is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn multi_workspace_git_state_binds_exact_roots_refs_access_and_dirty_paths() {
    let root = std::env::temp_dir().join(format!(
        "agent-hub-attended-repositories-{}",
        uuid::Uuid::new_v4()
    ));
    let alpha = root.join("alpha");
    let zeta = root.join("zeta");
    initialize_repository(&alpha);
    initialize_repository(&zeta);
    for repository in [&alpha, &zeta] {
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };
        git(&["status", "--short"]);
    }
    std::fs::write(alpha.join("tracked.txt"), "dirty\n").unwrap();
    std::fs::write(alpha.join("untracked.txt"), "dirty\n").unwrap();

    let roots = approved_root("test", &root);
    let resolved = resolve_mounts(
        &roots,
        &[
            RequestedWorkspace {
                alias: "zeta".into(),
                path: zeta.clone(),
                access: WorkspaceAccess::ReadOnly,
            },
            RequestedWorkspace {
                alias: "alpha".into(),
                path: alpha.clone(),
                access: WorkspaceAccess::ReadWrite,
            },
        ],
        &[],
        "alpha",
    )
    .unwrap();

    assert_eq!(resolved.repositories[0].workspace_alias, "alpha");
    assert_eq!(
        resolved.repositories[0].repository_root,
        alpha.to_str().unwrap()
    );
    assert_eq!(resolved.repositories[0].access, WorkspaceAccess::ReadWrite);
    assert_eq!(
        resolved.repositories[0].starting.branch.as_deref(),
        Some("main")
    );
    assert!(!resolved.repositories[0].starting.detached);
    assert_eq!(
        resolved.repositories[0].starting.dirty_paths,
        ["tracked.txt", "untracked.txt"]
    );
    assert_eq!(resolved.repositories[1].access, WorkspaceAccess::ReadOnly);
    std::fs::create_dir(alpha.join("nested")).unwrap();
    assert!(
        resolve_mounts(
            &roots,
            &[RequestedWorkspace {
                alias: "nested".into(),
                path: alpha.join("nested"),
                access: WorkspaceAccess::ReadWrite,
            }],
            &[],
            "nested",
        )
        .is_err()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn two_linked_worktrees_share_objects_but_bind_isolated_git_views_and_state() {
    let root = std::env::temp_dir().join(format!(
        "agent-hub-attended-linked-worktrees-{}",
        uuid::Uuid::new_v4()
    ));
    let repository = root.join("repository");
    let primary = root.join("primary");
    let sibling = root.join("sibling");
    initialize_repository(&repository);
    let git = |working_tree: &std::path::Path, args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(working_tree)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(
        &repository,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "attended-primary",
            primary.to_str().unwrap(),
        ],
    );
    git(
        &repository,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "attended-sibling",
            sibling.to_str().unwrap(),
        ],
    );
    std::fs::write(primary.join("tracked.txt"), "primary dirty\n").unwrap();

    let resolved = resolve_mounts(
        &approved_root("linked", &root),
        &[
            RequestedWorkspace {
                alias: "primary".into(),
                path: primary.clone(),
                access: WorkspaceAccess::ReadWrite,
            },
            RequestedWorkspace {
                alias: "sibling".into(),
                path: sibling.clone(),
                access: WorkspaceAccess::ReadOnly,
            },
        ],
        &[],
        "primary",
    )
    .unwrap();

    assert_eq!(resolved.git_views.len(), 2);
    assert!(resolved.git_views.iter().all(|view| {
        view.repository_type == protocol::GitRepositoryType::Linked && view.object_stores.len() == 1
    }));
    assert_eq!(
        resolved.git_views[0].object_stores[0].identity,
        resolved.git_views[1].object_stores[0].identity
    );
    assert_ne!(resolved.git_views[0].git_dir, resolved.git_views[1].git_dir);
    assert_eq!(resolved.repositories[0].workspace_alias, "primary");
    assert_eq!(
        resolved.repositories[0].starting.dirty_paths,
        ["tracked.txt"]
    );
    assert_eq!(resolved.repositories[1].workspace_alias, "sibling");
    assert!(resolved.repositories[1].starting.dirty_paths.is_empty());
    assert_eq!(resolved.workspaces[0].access, WorkspaceAccess::ReadWrite);
    assert_eq!(resolved.workspaces[1].access, WorkspaceAccess::ReadOnly);

    std::fs::remove_dir_all(root).unwrap();
}
