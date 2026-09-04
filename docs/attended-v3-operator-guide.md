# Attended V3 operator guide

`agent-hub local` is the attended V3 operator interface. It separates review,
authorization, execution, inspection, and explicit cleanup. Authorization is
one-use except for the retry classes and limits included in the reviewed V3
artifact; display and recovery state do not grant authority.

## Canonical lifecycle

1. Create a private proposal. The prompt is accepted only on standard input.

   ```sh
   agent-hub local propose \
     --config-root /config \
     --environment code-supervised \
     --image-family job-code \
     --network-class build \
     --network-resource llm \
     --tool-approval dippy \
     --overlay github:github.com:read_write \
     --workspace primary=/workspace/repo:rw \
     --primary-workspace primary \
     --context-root code=/home/nick.davies/code:ro \
      --timeout-secs 3600 \
      --prompt-stdin \
      --workspace-export-root /workspace/repo/.agent-hub/runs \
      --output /secure/authorization.json < /secure/prompt
   ```

   Review all displayed selectors, roots, Git identities, overlay bindings,
   prompt, retry limits, and digest. Run the exact shell-escaped command printed
   below `Authorize:`. Proposal creation resolves and records authority but does
   not authorize execution.

2. `local authorize` reopens the canonical private artifact, revalidates all
   reviewed inputs, and persists an authorization receipt. Its JSON includes
   distinct UUIDs in `authorization_id` and `receipt_id`, the digest,
   `initially_consumable`, the optional `workspace_export_root`, and an exact
   `canonical_run_command`. `initially_consumable` describes the new
   authorization, not retry eligibility for a terminal attempt. Run the command
   without reordering repeatable resource or overlay selectors.

3. `local run` consumes the receipt and executes in the foreground. It prints
   an `attended_result_v3` run record even when the run is unsuccessful, then
   exits nonzero if execution or cleanup was unsuccessful. Save the canonical
   `run_id` UUID for inspection. Diagnostic retrieval succeeding does not imply
   that the run succeeded.

4. Inspect with:

   ```sh
   agent-hub local list
   agent-hub local status --run-id RUN_UUID
   agent-hub local result --run-id RUN_UUID
   agent-hub local artifacts --run-id RUN_UUID
   agent-hub local repositories --run-id RUN_UUID
   agent-hub local cleanup-result --run-id RUN_UUID
   ```

   `list` and `status` return run records. `result` returns `local_result_v1`,
   combining typed outcomes, the structured agent result, bounded documents,
   and locations. `artifacts`, `repositories`, and `cleanup-result` return a
   bounded reference plus a parsed document (or a `parse_error`). Use
   `--limit-bytes` to change the read bound. `status` and `result` exit nonzero
   when execution or cleanup failed; `cleanup-result` considers only cleanup.

5. Explicitly remove owned runtime resources:

   ```sh
   agent-hub local cleanup --run-id RUN_UUID
   ```

   Cleanup has an outcome independent of execution. A failed agent may have
   successful cleanup, and a successful agent may have failed cleanup. Retry
   cleanup after correcting its actionable error. Successful cleanup removes
   the runtime payload but retains an immutable receipt; all document commands
   transparently read retained values from it.

## IDs, attempts, and retry eligibility

All IDs are canonical lowercase hyphenated UUIDs. A run record deliberately
keeps these identities separate:

* `authorization_id`: authorization decision identity.
* `receipt_id`: persisted authorization receipt identity.
* `run_id` (also `id`): one execution attempt's lookup identity.
* `attempt_id`: lineage identity for that attempt.
* `owner_nonce`: ownership fencing identity.

`attempt_number`, `parent_attempt_id`, and `attempt_ancestry` describe the
lineage. Do not substitute an attempt, authorization, or receipt UUID where a
command requires `--run-id`.

`retry_eligible` describes the current attempt only. A retry also requires
complete resource reconciliation, unchanged reviewed inputs, and room under
all reviewed limits. Automatic approval reviews at most three attempts, one
prelaunch retry, and one provider-unavailable retry. Dippy reviews at most four
attempts, two prelaunch retries, and one provider-unavailable retry. Provider
unavailability requires bounded host evidence before an assistant/tool result
or workspace mutation. Other failures consume authorization fail-closed.

## Optional workspace-local export

`--workspace-export-root` is optional and may be defaulted by an operator skill
in the future. When present, it must be the exact canonical
`RW_WORKSPACE/.agent-hub/runs` path under one reviewed read-write workspace.
Existing `.agent-hub` and `runs` components must be real directories; symlinks,
path escapes, nested alternatives, and read-only workspaces are rejected. The
path is bound into the V3 authorization bytes and revalidated before execution.
The contract also binds the selected workspace alias and host directory
device/inode identity. Publication walks every absolute path component through
no-follow directory descriptors and verifies that reviewed identity before
creating the reserved child directories.

The host atomically publishes to `ROOT/RUN_UUID/` using no-follow directory
operations. The export is explicitly **non-authoritative and untrusted**: agent
or workspace processes can alter or remove it, and the host never reads it for
authorization, retry, cleanup, or result decisions. Export failure is reported
in the private run record but does not change execution or cleanup outcomes.

The allowlist is `status.json`, bounded `stdout.log`, `stderr.log`, and
`transcript.jsonl`, `agent-result.json`, `repositories.json`, `artifacts.json`,
`cleanup.json`, and `manifest.json`. Free-text outputs may contain task or
workspace content and must not be treated as secret-free. Before publication,
the host scrubs the exact credential values materialized for that attempt. It
omits free-text output when that redaction proof is unavailable or the bounded
source cannot be read completely. The manifest records included checksums and
explicit omission reasons; files outside its current scope are removed from the
allowlist. Credential/effect state, owner markers, authorization state, and
approval or cleanup receipts are never export inputs.
The manifest is published last and republished after cleanup. Publication is
deferred while an attempt remains retry-eligible so host output cannot change
the reviewed workspace identity before the retry. Explicit cleanup removes
private runtime payloads while retaining an already published workspace export.

## Independent outcomes and structured result

Read these fields independently instead of inferring one from `status`:

* `process_outcome`: process launch/exit, cancellation, timeout, or output cap.
* `agent_outcome`: structured terminal result validity and completeness.
* `execution_outcome`: aggregate execution result.
* `permission_outcome`: supervised permission result.
* `cleanup_outcome`: containment and resource reconciliation result.

`agent_final`, when present, is `agent_result_v1`:

```json
{
  "schema": "agent_result_v1",
  "complete": true,
  "summary": "non-empty summary",
  "acceptance_results": [
    {"criterion": "criterion", "passed": true, "evidence": "evidence"}
  ],
  "blockers": [],
  "artifact_references": [
    {"path": "path", "description": "description"}
  ]
}
```

A complete result cannot contain blockers or failed acceptance results. Process
exit zero does not override a missing, malformed, incomplete, or contradictory
agent result.

## Workspace, context-root, and overlay grammar

* Workspace: `ALIAS=CANONICAL_ABSOLUTE_PATH:rw` or `:ro`.
* Context root: `ALIAS=CANONICAL_ABSOLUTE_PATH:ro` only.
* Alias: 1–63 bytes; lowercase ASCII letters and digits with interior hyphens;
  it must start and end alphanumerically.
* Reserved aliases: `artifacts`, `config`, `context`, `opencode`, and `output`.
* Every host path must already be a canonical real directory under an approved
  root with the requested `workspace_ro`, `workspace_rw`, `context_ro`, and (for
  repositories) `git_metadata` roles.
* Workspace and context paths must not overlap, and aliases are globally unique.
* `--overlay NAME` and `--network-resource KIND` are repeatable. Duplicates are
  rejected. Their canonical order is printed in the authorize/run commands and
  must match exactly. Overlays must exist in the environment allowlist and bind
  to its reviewed registry entry.

Errors include the rejected alias/path/selector or the required canonical form
where it is safe to do so. Correct the proposal input rather than editing the
authorization JSON; editing invalidates its canonical digest.

## State and retained receipts

The default private state root is:

```text
~/.local/state/agent-hub/attended
```

Set `AGENT_HUB_ATTENDED_STATE` to an absolute private directory to override it.
Important locations below the root are:

```text
authorizations/records/DIGEST.json
attempt-journals/RUN_UUID.json
executions/RUN_UUID.json
runs/RUN_UUID/
receipts/RUN_UUID.json
locks/DIGEST.lock
locks/cleanup-RUN_UUID.lock
```

After explicit cleanup, `receipts/RUN_UUID.json` is the durable source for
status and result documents. `local result` reports both the runtime and receipt
locations in `paths`; a receipt location may be reported before it exists so an
operator knows where successful explicit cleanup will retain evidence.
When configured, `local run` and `local status` include `workspace_export`, and
`local result` adds `paths.workspace_export`. The authorization receipt reports
the configured export root before a run UUID exists.
