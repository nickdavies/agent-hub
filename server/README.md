# agent-hub-server

`agent-hub-server` owns orchestration and its HTTP/gRPC APIs. It persists work
items, workflows, jobs, workers, leases, cancellation, artifacts, and SCM state,
and serves the orchestration admin UI.

Approval, question, interactive-session, and notification lifecycles are owned
by `agent-hub-approvals-server`. This binary does not expose those routes or
persist that state.

## Run

```bash
cargo run -p agent-hub-server -- serve
```

Required runtime configuration depends on the selected task and orchestration
adapters. The common settings are:

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `TASK_STATE_DIR` | yes | none | Private file-adapter state |
| `AUTH_MODE` | no | `token` | `token` or local-only `none` |
| `AGENT_HUB_TOKENS` | token mode | none | Comma-separated API bearer tokens |
| `LISTEN_ADDR` | no | mode-dependent | HTTP listener |
| `GRPC_LISTEN_ADDR` | no | disabled | Worker lifecycle gRPC listener |
| `CONFIG_ROOT` | no | `./config` | Authoritative deployment config |
| `WORKFLOWS_DIR` | no | `workflows` | Installed workflow catalog |
| `ORCHESTRATION_DB` | no | `orchestration.sqlite3` | Orchestration SQLite database |
| `ARTIFACT_STORE_ROOT` | no | `local-agent-data` | Artifact root |
| `AGENT_HUB_APPROVALS_SERVER` | no | disabled | Approval service HTTP origin |
| `AGENT_HUB_APPROVALS_TOKEN` | with authenticated approval service | none | Approval service bearer token |

When `AGENT_HUB_APPROVALS_SERVER` is configured, the server constructs the
shared `agent-hub-approval-client` adapter and projects that endpoint into jobs
that use gateway approvals. A token without an endpoint is rejected.

The public health endpoint is `GET /health`. Orchestration HTTP APIs are under
`/api/v1/orchestration`, the worker lifecycle API is available over configured
gRPC, and the admin UI starts at `/orchestration`.
