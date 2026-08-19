# Local orchestration dev (Phase 1)

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
