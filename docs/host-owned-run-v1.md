# Host-owned one-step run and capability boundary

`agent-hub local run --request-stdin` continues to accept the public,
plugin-compatible `AgentHubRunIntentV1`. It contains requested capabilities only:
environment and image family, network class/resources, approval mode, overlays,
timeout, prompt, and workspace/context mounts. It cannot select a backend config,
executable, policy, provider command, arbitrary image reference, credential,
invocation identity, or digest.

## Trusted backend configuration

The host reads the strict `agent_hub_run_host_v2` object at
`~/.config/agent-hub/host-run.json`:

```json
{
  "schema": "agent_hub_run_host_v2",
  "config_root": "/home/operator/.local/share/agent-hub/deployments/current/config",
  "gateway": { "path": "/home/operator/.local/share/agent-hub/deployments/current/bin/agent-hub-gateway" },
  "policy": { "path": "/home/operator/.local/share/agent-hub/deployments/current/config/permissions.json" },
  "managed_images": {
    "job-code": "agent-hub/job-code:stable"
  },
  "backend_metadata_revision": "2026-09-17.1"
}
```

Paths must be absolute, canonical, existing regular files/directories with the
declared role. Unknown fields and the old draft schema hard-fail; there is no
missing-config fallback. The backend config no longer asks a human to maintain
binary hashes/inodes, whole-config inventory hashes, source commits, or image ID
pins. Artifact checksums remain appropriate build/install evidence, but are not
live permission criteria. The selected config must still be the nofollow-opened
trusted default identity for automatic runs.

`managed_images` is an exact family-to-reference map. A reference may be a tag.
Before approval the host resolves it once through Docker to an immutable
`sha256:` image ID, verifies that the family reference agrees with the selected
environment/image manifest, records both in the canonical execution audit, and
uses only the captured ID for every Docker create and retry. An agent-provided
reference is never accepted, and a tag is never re-resolved after approval.

## Permission scope versus execution audit

The internal `AgentHubRunPolicyProjectionV2` / `agent_hub_run_policy_projection_v2`
is the normalized permission
scope. Its domain-separated permission-scope digest includes the prompt binding,
environment/image **family**, effective network grants, approval mode,
mount paths and access, selected overlays and credential capabilities/targets,
export root, timeout/retry policy, CPU/memory/PID limits, and output limits.

It intentionally excludes image ID/tag bytes, executable and provider locations,
config serialization/provenance hashes, root inodes, and build versions.
Effective network grants contain canonical host/pattern/port strings for each
typed resource, the selected provider for provider-specific LLM egress, and an
exclusive guarded-web mode. Host lists are sorted sets; ordered policy rules
remain ordered. Operators continue to request human-facing resource aliases and
do not maintain host digests. Thus an equivalent image rebuild or trusted
implementation relocation changes the full
canonical execution digest but not the permission-scope digest. A network,
mount-access, overlay target/privilege, or limit change changes the scope.

The gateway matches policy against projection v2. For `ask`, it sends the scope
digest in generic `ApprovalRequest.request_digest`; the approval service's strict
`agent_hub_run_resolved` reviewer independently derives and verifies it. The
generic approval interface remains opaque for other tools. Exact full-request
comparison still protects registration idempotency and replay handling. For this
special tool the reviewer also reconstructs and compares the invocation/session,
display name, cwd, provider, event, workspace roots, and context envelope.

The existing canonical digest remains the exact execution/audit identity and
keeps its existing meaning in `AgentHubRunDecisionV1`, receipts, persisted state,
and public `AgentHubRunOutputV1`. Public intent, terminal output, and receipt
shapes are unchanged. The plugin derives the invocation UUID from trusted
OpenCode session/call IDs and validates both output and receipt against it. It
also verifies canonical UUIDs, lowercase digests, timestamp ordering, automatic
approval identity, and the domain-separated canonical receipt checksum. Receipt
validity is measured at trusted call start, so a result authorized before expiry
remains valid when a legitimate long-running execution completes later. No scope
digest is added to those public objects and there
is no fallback that sends a canonical digest to an older strict reviewer.

## Protocol capability query

`agent-hub local capabilities` is a bounded, read-only compatibility check. It
opens the gateway path from the selected `host-run.json` with the same pinned
descriptor rules used by host-run execution, invokes
`agent-hub-gateway capabilities`, and performs an authenticated
`GET /api/v1/capabilities` against the configured approval endpoint through the
existing Rustls HTTP client. The
client rejects redirects and invalid endpoint schemes or embedded credentials,
applies a 10-second timeout, and caps each response at 4 KiB.

Both peers return the strict `AgentHubCapabilitiesV1` interface manifest:

```json
{"schema":"agent_hub_capabilities_v1","capabilities":["agent_hub_run_v1"]}
```

Capability identifiers are bounded, sorted, and unique. The CLI validates both
manifests, computes and prints their deterministic intersection, and succeeds
only when that intersection contains the required `agent_hub_run_v1` interface.
This is interface compatibility, not a boolean readiness or health claim. The
approval-service route is inside the existing authenticated
`/api/v1` router; `/health` stays public. This query does not prepare a run,
evaluate policy, register approval state, probe runtime readiness, or change the
public `AgentHubRunIntentV1` and `AgentHubRunOutputV1` contracts.

## Protected control-plane paths

After canonicalization and before launch, every requested read-only or read-write
mount is checked for equal, ancestor, or descendant intersection with paths
derived by the host from:

- the host config and authoritative config root;
- the current Agent Hub executable, gateway executable, and policy;
- the authoritative state root;
- selected credential files and absolute/resolved credential provider commands.

An approved broad root does not override this deny boundary. Intentionally
brokered credential delivery remains separate from public mounts. Checks run
during preparation and again before each attempt, in addition to existing root
identity, symlink, Git-view, receipt, expiry, and TOCTOU checks. Read-only mode or
same-UID file permissions are not treated as sufficient isolation. Installed
files with unexpected hard-link aliases fail closed. Root-owned, non-group/world
writable system executables are the explicit exception because distribution
packaging may legitimately hard-link them. Credential source paths, including
`~/` OAuth paths, use the same trusted-HOME normalization as the executor. A
missing state path is resolved from its canonical existing ancestor, so a
symlinked ancestor cannot hide an intersection. Installed
backend files should live outside ordinary workspaces, for example below
`~/.local/share/agent-hub/deployments`; a source checkout may remain an ordinary
workspace, but it must not be the installed trusted copy.

## Read-only catalog

Catalog reads a bounded authoritative config snapshot and optional published
tool-facts manifest. It never probes Docker, credentials, networks, or runtime
readiness. Absent optional facts are explicitly unavailable; declared missing,
malformed, unsupported-schema, or checksum-inconsistent facts fail closed. The
host config, each file, aggregate bytes, file/directory count, depth, and watch
count are bounded before unbounded allocation. Descriptor metadata is checked
before and after each limited read, pathnames are rebound to the opened inode,
and the inventory is rewalked; a mixed generation fails closed for both public
catalog and internal cache output.
Managed aliases must exactly cover configured families. Public output excludes
references, image IDs, cache paths, protected paths, provider details, and scope
protocol internals. The private cache identity is v2 and combines
`backend_metadata_revision` with bounded file/directory metadata watches; input
changes invalidate rather than serve stale catalog data.

Published tool facts are qualification metadata, not a live Docker readiness
probe. When facts are present, protected image metadata and the facts manifest
must agree on the managed image reference, qualified `sha256:` image identity,
and `backend_metadata_revision`; the declared manifest checksum must also match.
Changing a managed reference or publication revision makes old facts fail closed
until phase two atomically republishes all fields. The qualified identity may
remain in generated metadata and audit records but is not a user policy field;
`runtime_readiness_checked` remains false.

## Paired deployment order for phase two

This is an internal paired protocol/config upgrade, not a live activation.
Phase-two packaging must install immutable trusted copies outside workspaces,
write a complete v2 host config atomically, and roll out in this order:

1. approval service that verifies the complete special envelope and scope digest;
2. matching gateway that sends projection v2 with effective grants;
3. CLI/runtime plus v2 host config, managed aliases, and one atomically published
   tool-facts generation (`image_reference`, qualified `image_identity`,
   `backend_metadata_revision`, and manifest checksum/content);
4. public plugin with trusted invocation/receipt verification and catalog cache
   identity v2 support.

Do not point an upgraded CLI at an old gateway/reviewer and do not silently fall
back. Generated artifact checksums belong in installation/qualification evidence.
Image IDs may be retained internally for audit, reproduction, qualification cache,
and freezing Docker selection, but must not become human policy fields again.
