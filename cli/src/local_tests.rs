use crate::local::{
    ApprovedRoots, RequestedContextRoot, RequestedWorkspace, WorkspaceAccess, resolve_mounts,
};

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
