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
- `GITHUB_TOKEN` (if you want GitHub polling to work)

The local setup uses a shared artifact store directory mounted into the server container at `/artifacts` and used by local-agent on host via `ARTIFACT_STORE_ROOT`.

## 2) Build execution image

```bash
make build-job-images
```

Builds `agent-hub/job-base:latest`, `agent-hub/job-code:latest`, and
`agent-hub/job-infra:latest` from Dockerfiles in `../agent-hub-configs/images/`.

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
cargo run -p local-agent -- --worker-id local-dev --hostname localhost --server http://127.0.0.1:8180 --mode raw --mode docker
```

## 5) GitHub polling mode

Enable via env:

- `GITHUB_POLLING_ENABLED=true`
- `GITHUB_POLL_INTERVAL_SECS=30`
- `GITHUB_TOKEN=...`

When enabled, the server polls active work items with known PR URLs and synthesizes webhook-equivalent state updates/events.

## Notes

- Artifact paths in job metadata are now stored relative to artifact root (for local-agent-produced artifacts), so server no longer depends on arbitrary host absolute paths.
- `simple_code_change` Docker execution now defaults to `agent-hub/job-code:latest` instead of Alpine fallback.
- `VIKUNJA_BASE_URL` should be the Vikunja service root (for local compose: `http://vikunja:3456` inside the server container), not the `/api/v1` path.
