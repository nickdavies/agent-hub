# Attended approval gateway v1

The foreground `agent-hub local run` process is the sole owner of one Unix
socket below `/run/user/<uid>/agent-hub/<run-epoch>/approval.sock`. Docker bind-mounts
the private `0700` parent read-only at `/agent-hub/approval`; the socket is
`/agent-hub/approval/approval.sock`. The sandbox receives the
socket path and non-secret binding values only. It never receives an approvals
URL or bearer credential.

## Transport

* Unix stream socket, mode `0600`, owned by the foreground process UID, below
  its private canonical run directory.
* One request and one response per connection.
* A four-byte unsigned big-endian payload length followed by canonical compact
  UTF-8 JSON. Object keys are recursively sorted, arrays retain order, and
  floating-point numbers are rejected.
* Maximum request/response frame size: 1,048,576 bytes.
* Unknown JSON fields are rejected.
* A disconnect, malformed frame, timeout, relay loss, or mismatched response is
  a denial. There is no sandbox fallback to direct network approval.

## Request

```json
{
  "version": "attended_gateway_request_v1",
  "run_epoch": "UUID",
  "authorization_digest": "64 lowercase hex",
  "environment_bundle_digest": "64 lowercase hex",
  "image_digest": "sha256: + 64 lowercase hex",
  "network_digest": "64 lowercase hex",
  "approval_bundle_digest": "64 lowercase hex",
  "request_id": "UUID",
  "call_id": "OpenCode call ID",
  "raw_args_digest": "64 lowercase hex",
  "canonical_args_digest": "64 lowercase hex",
  "approval": {
    "id": "the same UUID",
    "session_id": "...",
    "session_display_name": "...",
    "cwd": "/workspace/main",
    "tool_name": "Bash",
    "tool_input": {"command": "..."},
    "provider": "opencode",
    "request_type": "tool_use",
    "context": {"workspace_roots": ["/workspace/main"], "hook_event_name": "permission.ask", "extra": null}
  },
  "tool_input_digest": "64 lowercase hex",
  "deadline": "RFC 3339 UTC timestamp"
}
```

A request ID is single-use within bounded per-run replay state. Requests may be
concurrent and have no ordering field. The deadline must be in the future and no
later than the run deadline. `tool_input_digest` is
SHA-256 over the byte concatenation of
`agent-hub:attended-tool-input:v1\0` and canonical compact JSON (object keys
sorted recursively; arrays retain order) for the exact `tool_input` value.
`raw_args_digest` and `canonical_args_digest` use the domains
`agent-hub:opencode-raw-args:v1\0` and
`agent-hub:opencode-match-input:v1\0` respectively.

## Supervision transitions

The same framed socket accepts `attended_gateway_transition_v1` messages and
returns an exact `attended_gateway_ack_v1`. The plugin first reports
`plugin_ready` with the run nonce after OpenCode permission ownership succeeds.
Each capability then follows one legal monotonic path:

* `created` → `local_allow` or host-recorded `remote_allow` → `completed` or `failed`
* `created` → `local_deny`, host-recorded `remote_deny`, or host-recorded `remote_cancel`

Transitions carry the complete run binding, request ID, call ID, session, tool,
and exact raw and canonical argument digests. Duplicate, conflicting, omitted,
or out-of-order transitions fail the relay. A sandbox cannot report remote
decisions. `StructuredOutput` is reported separately as `transcript_only`; it
is an explicit non-capability exclusion and has no request or canonical digest.

The host writes accepted transitions to a private `0600`, `O_NOFOLLOW`, bounded,
append-only ledger under the host run directory. This ledger is never mounted
into the sandbox. Run success requires readiness and a terminal legal path for
every routed call; sandbox-writable state JSON is not an authority.

The network digest uses domain `agent-hub:attended-network:v1\0` over canonical
JSON containing the authorized network class, resources, and runtime request.
The approval bundle digest uses domain
`agent-hub:attended-approval-bundle:v2\0` over canonical JSON binding the
environment bundle, approval mode, pinned plugin digest, protocol version, and
socket destination.

## Decision

```json
{
  "version": "attended_gateway_decision_v1",
  "run_epoch": "UUID",
  "authorization_digest": "64 lowercase hex",
  "environment_bundle_digest": "64 lowercase hex",
  "image_digest": "sha256: + 64 lowercase hex",
  "network_digest": "64 lowercase hex",
  "approval_bundle_digest": "64 lowercase hex",
  "request_id": "UUID",
  "tool_input_digest": "64 lowercase hex",
  "deadline": "the exact request deadline",
  "decision": "allow | deny",
  "reason": "optional text or null"
}
```

Every echoed field must exactly match the request. Decisions at or after the
deadline, duplicate IDs, and decisions for another run binding are
rejected.

## Host relay behavior

Before registration, the relay rechecks socket type, owner, mode, device and
inode, and uses Docker inspection to prove one exact read-only parent bind
source and destination. It forwards the complete immutable `ApprovalRequest`
to the existing approvals API:

1. `POST /api/v1/hooks/approval`
2. `GET /api/v1/approvals/{id}/wait` until a terminal status or deadline
3. `POST /api/v1/approvals/{id}/resolve` with `{"decision":"cancel"}` for every
   pending request when the run is cancelled or the relay stops

The host supplies optional bearer authentication only over HTTPS or literal
loopback HTTP. Redirects are disabled. Registration, cancellation,
socket bind/chmod/removal, and recovery are recorded in the shared
worker-neutral external-effect journal before mutation. Shutdown is awaited,
the original socket inode is removed and proven absent, pending approvals are
cancelled, and all corresponding effects must reconcile before cleanup can be
reported complete. Connections, reads, concurrent registrations, waits,
cancellation drain, response bodies, and replay state are all hard bounded. A
sandbox disconnect cancels its remote request.

## Authorization compatibility

The relay bundle is bound by `attended_authorization_request_v2` and attended
artifact version 2 under domain `agent-hub:attended-authorization:v2\0`. The V1
request shape, artifact version, canonical bytes, and
`agent-hub:attended-authorization:v1\0` parser remain unchanged.
