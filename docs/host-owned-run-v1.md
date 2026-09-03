# Host-owned one-step run v1

`agent-hub local run --request-stdin` accepts one `AgentHubRunIntentV1` JSON
object. The public intent contains only requested capabilities: environment and
image-family selectors, network class and resources, tool-approval mode,
overlays, timeout, prompt, and workspace/context paths. It cannot name a config
root, image digest, provider command, gateway binary, policy file, endpoint, or
credential.

The trusted host adapter loads
`~/.config/agent-hub/host-run.json`. That private configuration fixes the
canonical Agent Hub config root, absolute gateway executable, gateway policy
path, and immutable image identities by environment. A manual `--host-config`
is available for inspection, but `local run` permits automatic rules only when
its canonical file identity equals the trusted default.

The host performs these steps in order:

1. Validate intent bounds and canonicalize every requested directory.
2. Resolve config provenance, environment ceilings, image identity, runner,
   network policy, overlays, and retry limits into one canonical attended V3
   artifact. This performs no execution or mutable-state operation.
3. Submit the single `AgentHubRunResolvedV1` event to the configured absolute
   gateway path. Gateway failure, malformed output, or binding mismatch fails
   closed; there is no bypass or fallback.
4. Validate an `AgentHubRunDecisionV1` that echoes the schema, invocation UUID,
   canonical digest, request digest, authority, approval ID, decision timestamp,
   and expiry. `ask` is resolved by the gateway to a terminal human allow/deny.
5. Persist the authority-bound decision with a private domain-separated
   checksum under the invocation UUID. The checksum is local integrity
   evidence, not a gateway signature.
6. On allow, revalidate workspace identities, atomically create or verify the
   digest-keyed immutable canonical artifact, then create mutable authorization
   and attempt state keyed by invocation UUID.
7. Reverify the decision and workspace identities before each bounded attempt.
   Retries for one invocation reuse its decision; repeated identical launches
   receive independent invocation UUIDs and mutable state.

Image inspection, preflight, credentials, proxies, run state, workspace writes,
and agent startup occur only after a valid allow decision. `local inspect` is a
preparation-only diagnostic and cannot approve or execute. The public CLI does
not expose propose/authorize or receipt/digest bypass arguments.

The canonical Rust contracts are `AgentHubRunIntentV1`,
`AgentHubRunResolvedV1`, and `AgentHubRunDecisionV1`. The generated public
schemas and TypeScript definitions for all three contracts are under
`provider-plugins/opencode/generated`.
Actual OpenCode host-tool registration belongs in the host configuration
repository and must be added only after cross-repository integration.
