# One-step attended operator guide

`agent-hub local run` is the host-owned attended interface. It resolves host
configuration, requests a gateway decision, records the decision receipt, and
executes an allowed request. There is no normal proposal or separate authorize
command.

## Run and inspect

Pass one `agent_hub_run_intent_v1` document on standard input:

```sh
agent-hub local run --request-stdin < agent-hub-run.json
```

The intent contains requested environment, image family, network capabilities,
tool approval mode, overlays, timeout, prompt, workspace/context mounts, primary
workspace, and an optional `workspace_export_root`. Config roots, exact image
digests, provider commands, and gateway transport remain host-owned. The trusted
host config and gateway decision are validated before execution side effects.

Use `local inspect --request-stdin` to debug canonical resolution without asking
for permission or executing. Inspection grants no authority.

Each invocation has a distinct receipt and invocation-keyed authorization
lineage. Retryable prelaunch and provider-unavailable failures reuse that allow
receipt only within its reviewed limits. Repeating the same intent starts a new
invocation and requires a new gateway decision.

## Results and cleanup

The run command prints its decision receipt and, when allowed, the final run
record. Inspect retained state with:

```sh
agent-hub local list
agent-hub local status --run-id RUN_UUID
agent-hub local result --run-id RUN_UUID
agent-hub local artifacts --run-id RUN_UUID
agent-hub local repositories --run-id RUN_UUID
agent-hub local cleanup-result --run-id RUN_UUID
agent-hub local cleanup --run-id RUN_UUID
```

New run records use `attended_result_v4`. `result_outcome` records result
publication independently from process, agent, execution, permission, and
cleanup outcomes. Frozen V3 documents remain readable without adding fields;
their result evidence is `unknown`. `local result` emits `local_result_v2` and
bounded copies of the result, artifact, repository, and cleanup documents.

Cleanup removes only host-owned runtime resources after proving containment and
retains an immutable receipt. Automatic runs do not create an approval relay,
and their cleanup does not require one. Supervised cleanup uses only the exact
host relay identity recorded for that run. A published receipt is authoritative
if cleanup was interrupted before the convenience execution index was updated.

## Optional workspace export

`workspace_export_root` may be omitted. When present, it must be the exact
absolute `RW_WORKSPACE/.agent-hub/runs` path under one requested read-write
workspace. Existing path components must be real directories, not symlinks.
The selected workspace identity and export contract are bound into attended V3
canonical bytes; V1 and V2 bytes remain unchanged.

The host atomically publishes allowlisted, bounded output to
`ROOT/RUN_UUID/`. The export is non-authoritative and untrusted: no
authorization, retry, cleanup, result, ownership, credential, or effect state is
read back from it. Export paths are excluded from reviewed Git dirty state so a
prior export cannot alter a later invocation's canonical input. Publication is
deferred while an attempt remains retryable and finalized after cleanup.

The manifest records included checksums and omission reasons. Credential values
are scrubbed before publication; free-text output is omitted when complete
redaction cannot be proved. Receipts, owner markers, and private effect ledgers
are never exported.

## State

The private state root defaults to `~/.local/state/agent-hub/attended` and may be
overridden with `AGENT_HUB_ATTENDED_STATE`. Important entries are:

```text
agent-hub-run/invocations/INVOCATION_UUID/receipt.json
agent-hub-run/canonical/DIGEST.json
authorizations/records/INVOCATION_UUID.json
attempt-journals/RUN_UUID.json
executions/RUN_UUID.json
runs/RUN_UUID/
receipts/RUN_UUID.json
locks/INVOCATION_UUID.lock
locks/cleanup-RUN_UUID.lock
```

Host supervision transitions remain in the private run directory and cleanup
receipt as Dippy audit evidence; they are not workspace authority.
