# Host-owned one-step run v1

`agent-hub local run --request-stdin` accepts one `AgentHubRunIntentV1` JSON
object. The public intent contains only requested capabilities: environment and
image-family selectors, network class and resources, tool-approval mode,
overlays, timeout, prompt, and workspace/context paths. It cannot name a config
root, image digest, provider command, gateway binary, policy file, endpoint,
credential, invocation identity, or installation provenance.

The trusted host adapter loads `~/.config/agent-hub/host-run.json`. The strict
`agent_hub_run_host_v1` object fixes the canonical config-root device/inode,
immutable image identities, and a non-empty list of config artifacts. It also
pins the absolute host executable, gateway, policy, and reviewed OpenCode plugin
installation using path, SHA-256, device, inode, and a role-bound source
identity of the form `<role>:sha256:<digest>`. Each config pin instead uses
`config:<canonical-relative-path>:sha256:<digest>`. The pin set must exactly
cover every regular file below the authoritative config root; duplicate paths,
path aliases, omissions, extras, and digest mismatches fail closed. Plugin
installation provenance is host configuration, never public tool input. A
manual `--host-config` is available, but `local run` permits automatic rules
only when its open nofollow file identity equals the trusted default.

Each executable or file entry has this shape:

```json
{
  "path": "/absolute/canonical/path",
  "sha256": "<64 lowercase hex characters>",
  "source_identity": "runtime:agent-hub-gateway:sha256:<same digest>",
  "device": 30,
  "inode": 12345
}
```

The host object contains `config_root`, `config_root_identity`,
`config_artifacts`, `self_binary`, `gateway`, `policy`,
`opencode_plugin_install`, and `image_digests`. Missing pins hard-fail.

The host performs these steps in order:

1. Validate the lowered bounds (128 KiB prompt, 16 mounts per class, 16
   selectors, and 1 KiB paths), canonicalize every requested directory, and
   verify all installation pins.
2. Resolve config provenance, environment ceilings, image identity, runner,
   network policy, overlays, and retry limits into one canonical attended V3
   artifact. This performs no execution or executable run-state operation.
3. Encode canonical bytes once as canonical base64 in
   `AgentHubRunResolvedV1`. The gateway verifies those bytes, independently
   derives `AgentHubRunPolicyProjectionV1`, rejects a host projection mismatch,
   and matches policy only against that projection. Canonical bytes are not
   copied into match input; the raw resolved request remains the human-review
   and audit input.
4. Validate an `AgentHubRunDecisionV1` that echoes the schema, invocation UUID,
   canonical digest, request digest, authority, approval ID, decision timestamp,
   and expiry. `ask` is resolved by the gateway to a terminal human allow/deny.
   Each gateway process supplies a canonical UUID in
   `X-Agent-Hub-Waiter-Id` when registering and as the bounded `waiter_id`
   query parameter while polling. On SIGTERM or SIGINT it posts the same UUID
   to `/api/v1/approvals/{id}/waiters/cancel` and waits for the bounded server
   confirmation before exiting. The server cancels the approval only after its
   last registered waiter leaves, so equivalent invocation retries share the
   decision without sharing cancellation. Reviewer cancellation through
   `/resolve` remains global. Clients without waiter IDs retain the legacy
   orphan-eviction behavior.
5. Persist the authority-bound decision with a private domain-separated
   checksum under the invocation UUID. The checksum is local integrity
   evidence, not a gateway signature.
6. On allow, revalidate all host pins and workspace identities, atomically
   create or verify the digest-keyed immutable canonical artifact, then create
   mutable authorization and attempt state keyed by invocation UUID.
7. Reverify the decision, host pins, and workspace identities before each
   bounded attempt. The host derives a stable invocation UUID from trusted
   OpenCode session/tool-call context. A retry or repeated delivery reconciles
   the exact invocation and receipt. Manual calls generate an ID and may safely
   resume it with `--internal-invocation-id`.

Image inspection, preflight, credentials, proxies, executable run state,
workspace writes, and agent startup occur only after a valid allow decision.
The invocation-scoped resolved request may be journaled before the decision so
an interrupted approval can be retried idempotently. This private recovery
journal is not authorization and causes none of those capability side effects.
Pending approvals survive a server restart, but active waiter memberships are
transient and are re-established by registration or the next long poll.
`local inspect` is a preparation-only diagnostic and cannot approve or execute.
The public intent does not expose propose/authorize or receipt/digest bypass
fields. SIGTERM and SIGINT request owned Agent Hub cancellation through the
permission wait and execution, terminate and reap an outstanding gateway child,
and wait for bounded cleanup and a terminal envelope. A completion/signal race
reports durable run state; it never rewrites durable success as cancelled.

Every terminal stdout document is `AgentHubRunOutputV1`. It always contains a
versioned `AgentHubRunApprovalReceiptV1` slot (issued or unavailable), a bounded
`local_result_v2` aggregate, the stable invocation ID, and an optional error.
The CLI writes this document before returning nonzero for deny, failure, or
cancellation. The complete envelope is bounded to 1 MiB; each embedded local
result document is bounded to 128 KiB.

`agent-hub local preview --request-stdin` performs canonical preparation and
gateway policy evaluation only. It returns the independently derived projection
and matched `allow`, `ask`, or `deny` action without approval registration,
mutable run state, or execution.

The canonical Rust contracts include `AgentHubRunIntentV1`,
`AgentHubRunResolvedV1`, `AgentHubRunPolicyProjectionV1`,
`AgentHubRunDecisionV1`, and `AgentHubRunOutputV1`. Generated public schemas and
TypeScript definitions are under `provider-plugins/opencode/generated`.

The OpenCode package directly pins both `@opencode-ai/plugin` and the imported
`@opencode-ai/sdk` to 1.18.18 so install provenance cannot silently select a
different SDK revision.
