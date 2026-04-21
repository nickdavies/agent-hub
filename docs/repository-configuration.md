# Repository configuration for code-change workflows

This document explains how `simple-code-change` determines repository settings for implementation jobs.

## 1) How the system discovers repositories

Repository discovery is driven by triage output (`output.json`) from the `triage_investigation` state.

- Triage is expected to emit:
  - `repo.repo_url`
  - `repo.base_ref`
  - optional `repo.branch_name`
- On success, downstream jobs receive these values through standard agent environment variables and runtime repo claims.

Minimal triage repo output example:

```json
{
  "next_event": "job_succeeded",
  "repo": {
    "repo_url": "https://github.com/owner/repo",
    "base_ref": "origin/main",
    "branch_name": "agent-hub/1234/fix-cache-invalidation"
  }
}
```

## 2) Private repository access

For GitHub-hosted repos, the `gh` toolchain uses host-provided GitHub CLI authentication injected into the execution environment.

For SSH-based repositories, mount SSH keys through credential provider configuration. Without mounted SSH credentials, cloning/fetch/push for SSH remotes will fail.

## 3) Branch naming

By default, agent workflows should use descriptive target branches in the form:

- `agent-hub/{work-item-id}/{descriptive-name}`

How to customize:

- Set `repo.branch_name` during triage output to request a specific branch base/name.
- Implementation jobs can create/switch to a branch that preserves this convention while adding task-specific detail.

## 4) Allowed repositories

Current behavior: workflows can operate on any repository that the configured GitHub token (or equivalent credentials) can access.

Operational guidance:

- Scope tokens to least privilege where possible.
- Future hardening can add explicit repo allowlists in workflow/server config.

## 5) Base branch selection

Base branch is selected from triage output `repo.base_ref`, typically one of:

- `origin/main`
- `origin/master`

This base ref is propagated as `AGENT_BASE_REF` and is used by downstream review/PR logic.

## 6) Example local-agent config for private repo SSH access

```json
{
  "triage_repo_dirs": ["~/code", "~/workspaces"],
  "credential_providers": {
    "ssh-keys": {
      "type": "file",
      "name": "ssh-keys",
      "host_path": "~/.ssh",
      "container_path": "/home/agent/.ssh",
      "read_only": true
    }
  }
}
```

`triage_repo_dirs` is optional and defaults to `[]`. When set in the local-agent config
(`~/.config/agent-hub/local-agent.json`, or a path passed via `--local-agent-config`), local-agent
expands `~` to `$HOME` and mounts each existing directory read-only into job containers at
`/repos/<dirname>`.
