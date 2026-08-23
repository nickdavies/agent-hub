# Local orchestration dev (Phase 1)

> **Current milestone:** only exact `TASK_ADAPTER=file` is supported. Vikunja and distributed/NFS
> operation are intentionally unsupported; this is a single-host local-smoke boundary.

File mode requires two separate paths:

- `TASK_DIR` contains human-editable task projections only.
- `TASK_STATE_DIR` is a distinct server-private directory under the server data root. It must be a
  regular, non-symlink directory with owner-only mode `0700`. It contains authoritative
  checkpoints, terminal evidence, provider locks, and action/recovery state and must not be exposed
  through `TASK_DIR`.

Both directories must canonicalize to disjoint paths owned by the same user on the same reviewed
local filesystem. Production accepts Linux ext4, btrfs, and XFS. tmpfs, NFS/NFS4, FUSE, CIFS/SMB,
overlayfs, remote filesystems, and unknown filesystem types are rejected because authoritative
evidence must survive reboot and power loss. Tests may use tmpfs/overlay beneath their isolated temp
roots. `AGENT_HUB_ALLOW_EPHEMERAL_FILE_ADAPTER_FS=1` is an explicit durability waiver usable
wherever it is set and emits a startup warning; c390 and normal production deployments must leave
it unset. SQLite is
the sole authority for internal action/recovery receipts; private file sidecars are immutable
provider-facing checkpoint and terminal-delivery evidence.

The Rust test binary exposes only an ignored, environment-gated subprocess helper named
`server::task_files::tests::subprocess_file_adapter_helper`. Testers invoke it with
`AGENT_HUB_FILE_HELPER=1`, isolated `TASK_DIR`/`TASK_STATE_DIR`, and
`AGENT_HUB_FILE_HELPER_OP=ensure|hold-lock`. Test-only durability hooks use
`AGENT_HUB_TEST_FILE_FAULT=error|abort|pause:<boundary>` for `before_file_fsync`,
`after_file_fsync`, `after_rename`, `before_directory_fsync`, `after_directory_fsync`, and
`while_provider_lock_held`; optional ready/release paths coordinate subprocess termination. These
hooks are compiled out of production builds.

Durable replacement owns only these temporary basenames: `<uuid>.json.<nonce-uuid>.tmp`,
`<uuid>.checkpoint.json.<nonce-uuid>.tmp`, and
`terminal-<ascii-idempotency-key>.<nonce-uuid>.tmp`. Startup and task snapshot pulls inspect them
only while holding `provider.lock`. A candidate must be directly in the expected directory, regular,
non-symlinked, single-link, same-owner/device, and at least one hour old. Unknown names and unsafe
file types are ignored without being followed or deleted; more than 128 valid owned candidates fail
closed. The provider lock excludes a current writer, so no PID marker
is needed; fresh files are always preserved. Deletion uses `unlinkat` against an `O_NOFOLLOW`
directory descriptor followed by directory fsync.

`TASK_ADAPTER=vikunja` and unknown values fail before adapter construction, database recovery,
network access, or listener binding. The checked-in Vikunja adapter is groundwork for a separate
task, not a reachable production path in this milestone.

This setup runs:
- `agent-hub-server` in Docker compose
- `vikunja` in Docker compose
- `local-agent` as a host process

## 1) Prepare local env

Create `.env` from `env.example` and set at least:

- `VIKUNJA_BOT_USERNAME`
- `VIKUNJA_BOT_EMAIL`
- `VIKUNJA_BOT_PASSWORD`
- `VIKUNJA_JWT_SECRET`
- one read-only token environment variable for every `[[system.scm_hosts]]`
  entry's `read_token_env` (authoritative polling)
- `AGENT_HUB_GITHUB_WEBHOOK_SECRET` for signed webhook ingestion; when absent
  or blank, the webhook route is unavailable and never accepts unsigned events

The local setup uses a shared artifact store directory mounted into the server container at `/artifacts` and used by local-agent on host via `ARTIFACT_STORE_ROOT`.

## 2) Build execution image

```bash
make build-job-images
```

Builds `agent-hub/job-base:latest`, `agent-hub/job-code:2026-08-18`, and
`agent-hub/job-infra:latest` from Dockerfiles in `../agent-hub-configs/images/`.
The supported coding runner is OpenCode. The job-code image is tagged
`agent-hub/job-code:2026-08-18`; its base image, Debian snapshot, and every
downloaded tool version/checksum are fixed in the Dockerfile. The build accepts
no artifact URL or digest inputs.

## 3) Start services + bootstrap Vikunja

```bash
make bootstrap-vikunja
```

This will:
- start compose stack (`vikunja`, `server`)
- wait for Vikunja API
- create/ensure bot user
- create/ensure project
- print `VIKUNJA_TOKEN` and `VIKUNJA_PROJECT_ID` values to put in `.env`

After updating `.env`, restart server:

```bash
make dev-up
```

## 4) Run local-agent on host

```bash
ARTIFACT_STORE_ROOT=./dev-data/artifacts \
CONFIG_ROOT=../agent-hub-configs \
cargo run -p local-agent -- --worker-id local-dev --hostname localhost --server http://127.0.0.1:8180
```

## 5) GitHub polling mode

Enable via env:

- `GITHUB_POLLING_ENABLED=true`
- `GITHUB_POLL_INTERVAL_SECS=30`
- the host-specific read-only variables named by `system.scm_hosts[*].read_token_env`

When enabled, the server polls active work items with known PR URLs. Polling is
read-only and is one of the two authoritative sources (with validated webhooks)
allowed to emit `pr_merged`.

Managed GitHub mutation jobs use separate host-specific read/write providers in
`local-agent.json`, for example `github:github.com:read_write` and
`github:github.snooguts.net:read_write`. Each provider must emit only
`GITHUB_TOKEN`. Its host must have an exact `github_api_base_urls` entry matching
the immutable capability issued from the server SCM registry. These credentials
are never made available to agent or command jobs.

## Workflow and config boundaries

- `CONFIG_ROOT` must point to the same config checkout for server, CLI build, and
  local-agent startup.
- Installed workflow IDs come from `[system.workflows].installed`; the current
  local set is `simple-code-change` and `github-pr`.
- Workflow directories use those exact IDs and contain one `workflow.toml` each.
- Managed states perform narrowly authorized GitHub mutations. Polling observes
  SCM state and advances workflows; it does not execute managed mutations.
- Only `timeout_secs` is accepted in reactive workflow execution records.

## Notes

- Artifact paths in job metadata are now stored relative to artifact root (for local-agent-produced artifacts), so server no longer depends on arbitrary host absolute paths.
- `simple-code-change` uses the configured `job-code` image.
- `VIKUNJA_BASE_URL` should be the Vikunja service root (for local compose: `http://vikunja:3456` inside the server container), not the `/api/v1` path.
## Authorization protocol rollback

New databases start with the v3 fence. Schema upgrades preserve an existing v1 or v2 fence; startup
never raises it. Lift a preserved fence only after deploying compatible workers:

```bash
agent-hub-server set-authorization-protocol-mode --database "$ORCHESTRATION_DB" --max-version 3
```

Before rolling back to a v2-only build, stop dispatch, drain or explicitly quarantine every active
v3 job, record, authorization obligation, and recovery item, and invalidate v3-capable sessions.
The rollback is prohibited until this preflight succeeds:

```bash
agent-hub-server rollback-preflight-v2 --database "$ORCHESTRATION_DB"
```

Before rolling back again to a v1-only build, perform the same drain for v2 and v3 work and run:

```bash
agent-hub-server rollback-preflight-v1 --database "$ORCHESTRATION_DB"
```

Both checks also reject reactivatable workflows because a lower-version server could mint or resume
unsupported work. Do not rewrite stored authorization bytes between versions or reuse a
higher-version session with a lower-version server.

Each successful preflight commits its durable lower-version fence. A compatible server may lift it
only through the explicit command; ordinary startup preserves it:

```bash
agent-hub-server set-authorization-protocol-mode --database "$ORCHESTRATION_DB" --max-version 2
```
