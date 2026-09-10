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
roots only in `#[cfg(test)]` binaries. Production has no filesystem durability waiver. SQLite is the
sole authority for internal action/recovery receipts; private file sidecars are immutable
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

The canonical public work-item event endpoint is
`POST /api/v1/orchestration/work-items/{id}/events`. The singular `/event` form is retained as a
compatibility alias and invokes the same handler. Cancellation responses include the work item and
the durable cancellation identity/outcome. A running job leaves the work item in `cancelling` until
its bound worker proves process and container absence with the matching fenced acknowledgement.
Pending cancellation commands survive server restart. An authenticated worker replacement rebinds
them to the replacement session with a higher recovery generation; the old session is invalidated
and cannot acknowledge. Worker startup reconciles persisted, ownership-labelled OpenCode and
generic Docker containers before polling, then handles recovery cancellation commands before new
work. Failed absence checks remain durable as `containment_failed` with `cleanup_error` and never
terminalize the job or work item.
Cancellation assignment identities are derived and verified by the protocol crate. Recovery rebind
creates a new session-bound delivery assignment while retaining the original runtime assignment for
container reconciliation. Process records persist PID, process group, Linux boot ID, and `/proc`
start time; restart cleanup refuses to signal a reused PID. Process and container evidence are
separate typed values (`not_applicable`, `verified_absent`, or `failed`). Generic Docker uses the
same durable create-submission markers and quarantine rules as OpenCode. Provider-action revision
application plans durable cancellation commands in its idempotent SQLite transaction rather than
terminalizing running jobs.
Every orchestration SQLite connection uses WAL, foreign keys, a bounded busy timeout, and
`synchronous=FULL`. Cancellation intent and acknowledgement commit before their HTTP/gRPC success
response; restart tests reopen the database after both boundaries and verify the immutable command,
receipt, outbox, and terminal result. This protects committed state across process termination and
host power loss when the reviewed filesystem honors fsync.
Each attempt has a fsynced monotonic phase record from `initialized` through workspace, credential,
bundle, Docker create/start, running, post-exit, reporting, cleanup, and `completed`. Invalid phase
regressions hard-fail; `containment_failed` advances only through explicit restart cleanup recovery.
Repeatable external effects use a separate fsynced append-only `external-effects.jsonl` journal.
Records have contiguous sequence numbers, stable operation IDs, immutable assignment/job/attempt and
resource identities, lifecycle phase, and started/completed/ambiguous/cleanup-failed/reconciled
status. Exact duplicate records are idempotent; conflicting duplicates, malformed records,
truncation, sequence gaps, unsupported effects, and missing ownership evidence quarantine or fail
containment. Completed resource-creation records remain unresolved until their exact cleanup is
recorded, so lifecycle phases never regress for Docker exec, inspect, removal, or retry operations.
Pure local observations—such as polling, validating, or reading an `output.json` snapshot for
redaction—do not create cleanup-journal records. Artifact writes, fsyncs, atomic redaction
replacements, markers, removals, helper processes, network calls, and containment inspections remain
journaled.
All of these records, process identities, ownership files, and create-quarantine markers live under
worker-private `$WORKER_DATA/attempts/<runtime-assignment-id>/` with mode `0700`; files are mode
`0600`, no-follow, owner/link/type checked, atomically replaced and fsynced. This directory is never
mounted into a job. `$ARTIFACT_ROOT/transcripts/<job-id>/` contains only redacted, non-authoritative
artifacts, so hostile writes or deletions under `/transcript` cannot affect restart recovery.
Container-facing OpenCode config, XDG state/data/cache/home, access-only auth, and MCP sockets live
in the assignment-bound sibling `$WORKER_DATA/runtime-attempts/<runtime-assignment-id>/`. Only this
exact runtime subtree may be mounted. It is removed before acknowledgement; private authority stays
until terminal report and journal closure.
Linux process cleanup enumerates exact process-group members, opens pidfds after boot-ID/start-time
verification, and sends TERM/KILL through pidfds while repeatedly discovering new members. The
OpenCode plugin declares a pinned TypeScript compiler because an isolated frozen install otherwise
cannot execute its checked-in `typecheck` script.

### Durable-cancellation test API

Execution fault injection is compiled only into the `local-agent` test binary. One central hook
covers every durable lifecycle transition and operation-journal status boundary.

- `AGENT_HUB_TEST_EXECUTION_FAULT=error:<boundary>` returns an injected error.
- `AGENT_HUB_TEST_EXECUTION_FAULT=abort:<boundary>` aborts the process.
- `AGENT_HUB_TEST_EXECUTION_FAULT=pause:<boundary>` writes the selected boundary plus a newline to
  `AGENT_HUB_TEST_EXECUTION_FAULT_READY`, then waits for
  `AGENT_HUB_TEST_EXECUTION_FAULT_RELEASE` to exist.
- Lifecycle boundaries are `lifecycle:<phase>:before|after`, where `<phase>` is one of
  `initialized`, `workspace_preparing`, `workspace_prepared`, `credentials_resolving`,
  `credentials_resolved`, `bundle_materializing`, `bundle_materialized`,
  `docker_create_submitting`, `docker_create_submitted`, `docker_created`,
  `docker_start_submitting`, `docker_started`, `running`, `post_exit`, `cleanup_running`,
  `terminal_reporting`, `containment_failed`, or `completed`.
- Journal boundaries are `operation:<operation-kind>:<status>:before|after`, where status is
  `started`, `completed`, `ambiguous`, `cleanup_failed`, or `reconciled`.

The subprocess hook helper contract is:

```bash
AGENT_HUB_EXECUTION_FAULT_HELPER=1 \
AGENT_HUB_EXECUTION_FAULT_BOUNDARY='lifecycle:running:before' \
AGENT_HUB_TEST_EXECUTION_FAULT='abort:lifecycle:running:before' \
cargo test -p local-agent --bin local-agent \
  tests::subprocess_execution_fault_helper -- \
  --ignored --exact --test-threads=1 --nocapture
```

For pause tests, additionally set absolute isolated ready/release paths. Remove both files before
starting the helper, wait for the ready file, inspect or restart the server/worker, and create the
release file. Never share these control paths between jobs.

An ignored production-path E2E should start the file adapter and gRPC server with isolated absolute
`TASK_DIR`, `TASK_STATE_DIR`, database, artifact, and worker-data paths, then launch the worker with:

```bash
TASK_ADAPTER=file TASK_DIR="$TASK_DIR" TASK_STATE_DIR="$TASK_STATE_DIR" \
cargo run -p agent-hub-server --bin agent-hub-server

cargo run -p local-agent --bin local-agent -- \
  --transport grpc --grpc-endpoint http://127.0.0.1:50051 \
  --worker-id "$WORKER_ID" --hostname localhost \
  --data-dir "$WORKER_DATA" --artifact-store-root "$ARTIFACT_ROOT"
```

Restart means terminating and re-running the same command against the same isolated durable paths;
worker replacement uses the same worker ID and a newly registered session. The harness must inspect:

- `$WORKER_DATA/attempts/<runtime-assignment-id>/attempt-phase.json`
- `$WORKER_DATA/attempts/<runtime-assignment-id>/external-effects.jsonl`
- `$WORKER_DATA/attempts/<runtime-assignment-id>/process-runtime.json`
- `$WORKER_DATA/attempts/<runtime-assignment-id>/opencode/`
- `$WORKER_DATA/attempts/<runtime-assignment-id>/docker/`
- `$WORKER_DATA/attempts/reconciliation/external-effects.jsonl`
- `/tmp/agent-hub-mcp/*.sock`
- ownership-labelled `agent-hub-opencode-*` and `agent-hub-job-*` containers
- the server SQLite cancellation row and file-adapter checkpoint under `$TASK_STATE_DIR`

Success requires terminal cancellation acknowledgement only after all applicable operation records
are completed or reconciled, no process-group member, socket, credential file, XDG/bundle state, or
owned container remains, and no work item remains in `cancelling`. Unsupported operations, missing
ownership evidence, malformed journals, and cleanup failures must remain `containment_failed` and be
retryable after the evidence or cleanup failure is repaired.

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

Builds `agent-hub/job-base:latest`, `agent-hub/job-code:2026-09-09`, and
`agent-hub/job-infra:latest` from Dockerfiles in `../agent-hub-configs/images/`.
The supported coding runner is OpenCode. The job-code image is tagged
`agent-hub/job-code:2026-09-09`; its base image, Debian snapshot, and every
downloaded tool version/checksum are fixed in the Dockerfile. The build accepts
no artifact URL or digest inputs. After building job-code, the CLI runs the image's
shared toolchain preflight with no network, mounts, or credentials, a read-only root,
non-root runtime identity, and tmpfs-backed state. Local-agent runs the same contract
against the resolved image ID before starting each configured job-code execution.

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
- Coordinate an Agent Hub release with an exact `agent-hub-configs` commit. That
  config commit must pin `[image.build_contexts.agent-hub-source].commit` to the
  released 40-character Agent Hub commit and its `archive_sha256` to that commit's
  verified archive. Commit both sides before cutover and record the pair in the
  release evidence. Do not use a branch, tag, or `latest`: config loading and build
  context materialization hard-fail non-canonical or unresolved commit identities.
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

This procedure applies only to the orchestration API server and its database. The
standalone approvals server has a separate file rollback procedure in
`deploy/approvals-server/README.md`.

New databases start with the v4 fence. Schema upgrades preserve an existing lower
fence; startup never raises it. Lift a preserved fence only after deploying
compatible workers:

```bash
agent-hub-server set-authorization-protocol-mode --database "$ORCHESTRATION_DB" --max-version 4
```

Before rolling back to a v3-only build, stop dispatch, drain or explicitly
quarantine every active v4 job, record, authorization obligation, recovery item,
and reactivatable workflow, and invalidate v4-capable sessions. The rollback is
prohibited until this preflight succeeds:

```bash
agent-hub-server rollback-preflight-v3 --database "$ORCHESTRATION_DB"
```

Before rolling back to a v2-only build, perform the same drain for v3 and v4 work.
The rollback is prohibited until this preflight succeeds:

```bash
agent-hub-server rollback-preflight-v2 --database "$ORCHESTRATION_DB"
```

Before rolling back again to a v1-only build, perform the same drain for v2, v3,
and v4 work and run:

```bash
agent-hub-server rollback-preflight-v1 --database "$ORCHESTRATION_DB"
```

All checks reject reactivatable workflows because a lower-version server could
mint or resume unsupported work. Do not rewrite stored authorization bytes between
versions or reuse a higher-version session with a lower-version server.

Each successful preflight commits its durable lower-version fence. A compatible server may lift it
only through the explicit command; ordinary startup preserves it:

```bash
agent-hub-server set-authorization-protocol-mode --database "$ORCHESTRATION_DB" --max-version 2
```
