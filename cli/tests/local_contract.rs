use agent_hub::local::{
    ApprovedRoots, RequestedContextRoot, RequestedWorkspace, WorkspaceAccess, resolve_mounts,
};

#[test]
fn mount_resolution_is_repeatable_and_rejects_reserved_shadowing() {
    let temp =
        std::env::temp_dir().join(format!("agent-hub-attended-paths-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(temp.join("repo/.git")).unwrap();
    let roots = ApprovedRoots::new(vec![temp.clone()]).unwrap();
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
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    std::fs::create_dir(root.join("context")).unwrap();
    let roots = ApprovedRoots::new(vec![root.clone()]).unwrap();
    let workspaces = vec![RequestedWorkspace {
        alias: "primary".into(),
        path: root.join("workspace"),
        access: WorkspaceAccess::ReadWrite,
    }];
    let contexts = vec![RequestedContextRoot {
        alias: "reference".into(),
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
        path: "/home/nick.davies/code/achilles".into(),
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
    let roots = ApprovedRoots::new(vec![root.clone()]).unwrap();
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
