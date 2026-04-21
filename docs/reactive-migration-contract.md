# Reactive Migration Contract (Prototype -> Planned Model)

This note documents the additive migration boundary introduced for reactive-agents planning.

## Current prototype model (still runtime source of truth)

- `WorkItem`
- `Job`
- legacy workflow schema (`[workflow]` with `entry_step` + `steps`)

The server runtime and persistence continue to operate on these prototype records.

## Planned-form contract views (additive, non-breaking)

New protocol view types:

- `PlannedWorkItemView`
- `PlannedAgentJobView`
- `PlannedEnvironmentSpec`
- `PlannedPermissionSet`
- `PlannedRepoClaim`

Mapping helpers:

- `planned_work_item_view_from_prototype(&WorkItem)`
- `planned_agent_job_view_from_prototype(&Job)`

These are one-way compatibility adapters from prototype records into planned-form shape.

## Explicit defaults / gaps in mappings

- `PlannedPermissionSet` is currently defaulted from prototype data:
  - `can_read_workspace = true`
  - `can_write_workspace = true`
  - `can_run_shell = true`
- Prototype does not yet persist explicit permission scopes.
- Planned-form state naming uses prototype fields:
  - `WorkItem.current_step -> PlannedWorkItemView.current_state_key`
  - `Job.step_id -> PlannedAgentJobView.state_key`

## Workflow schema versions

### Legacy (v1)

- Existing schema remains default and executable by current runtime.

### Reactive (v2)

- New schema identifier: `workflow.schema = "reactive_v2"`
- New model adds:
  - explicit state kinds: `auto`, `agent`, `human`, `terminal`
  - event-driven transitions
  - cycles are allowed (validation does not reject transition loops)

Current status:

- v2 workflows are parsed + validated.
- v2 workflows are cataloged separately.
- v2 workflows have a thin executable runtime slice:
  - `auto` (requires `start` transition), `agent`, `human`, `terminal`
  - one active state at a time
  - event-driven transitions via existing `fire_event` API mapping

Reactive terminal semantics in this thin slice:

- terminal states may declare optional `terminal_outcome`:
  - `succeeded`
  - `failed`
- entering a `terminal` state maps to work-item outcome using `terminal_outcome`.
- if `terminal_outcome` is omitted, behavior remains backward-compatible and defaults to
  `Succeeded`.
- cancel behavior is unchanged in this slice (`cancel` remains an implicit runtime behavior for
  non-terminal states and sets work item state to `Cancelled`).

Reactive event closure + validation notes:

- Runtime transition matching is now based on closed typed `WorkflowEventKind` values.
- Reactive transition events are canonicalized to typed event values at parse/deserialization time.
- Unknown reactive transition event strings are rejected at the parse boundary during workflow load
  (surfaced as explicit catalog parse errors), with no runtime legacy-string fallback.
- Compatibility aliases are accepted for common legacy names:
  - `approve` -> `human_approved`
  - `request_changes` -> `human_changes_requested`
  - `retry` -> `retry_requested`
  - `unblock` -> `human_unblocked`
  - `submit` -> `submit`
- Canonical names are still preferred in workflow files (for example, `start`, `job_succeeded`,
  `job_failed`, `human_approved`, `human_changes_requested`, `retry_requested`,
  `human_unblocked`, `submit`, `cancel`).

Reactive state-transition semantic hardening (validator-first):

- Per-state transition event keys must be unique (duplicate events in one state's `on` table are
  rejected to avoid first-match ambiguity).
- Transition events must be compatible with state kind:
  - `auto`: `start` only
  - `agent`: `job_succeeded`, `job_failed` only
  - `human`: `submit`, `human_approved`, `human_changes_requested`, `human_unblocked`,
    `retry_requested` only
  - `terminal`: no transitions
- Explicit reactive `cancel` transitions are rejected at validation time because cancel remains an
  implicit runtime behavior for non-terminal states.

## Version-aware loading/validation behavior

- Server workflow catalog:
  - loads legacy and reactive docs
  - validates both in version-aware mode
  - cross-workflow child validation remains legacy-only
- CLI workflow validation:
  - parses docs in version-aware mode
  - validates legacy and reactive definitions independently

## Non-goals of this slice

- No runtime rewrite to full reactive semantics.
- No persistence/API breaking changes to prototype structs.

## Task adapter MVP slice

Server orchestration now consumes external task records through an additive `TaskAdapter` boundary.

- Trait: `TaskAdapter`
  - `pull_tasks()` returns adapter task records for sync
  - `persist_work_item()` writes internal/prototype work item updates back to adapter storage
- Concrete MVP adapter: `TaskFileStore` implements `TaskAdapter`
  - one JSON file per task remains the storage shape
  - current local/dev file-backed flow is preserved

Sync behavior remains deterministic and compatible with current persistence/runtime:

- Unknown external task IDs are inserted as internal work items.
- Existing internal items are updated only when external `updated_at` is newer.

### Task-tool driven management extensions

The file adapter now uses a task-native record shape for human management instead of raw work-item passthrough.

Task-native fields include (MVP):

- `title`
- `description`
- optional `workflow_id`
- `triage_status`
- `pending_human_action`
- `current_human_gate`
- `workflow_phase`
- `blocked_reason`
- `allowed_actions`
- `context`
- `repo`
- `updated_at`
- `requested_action` + `action_revision`

Behavior:

- Missing workflow does not fail creation; task is projected as `needs_workflow_selection`.
- Missing workflow now creates a first-class internal triage placeholder (`workflow_id="__triage__"`,
  `current_step="triage"`) so workflow selection is a runtime path, not just metadata.
- Explicit task actions are treated as intents (not full-record overwrite), including:
  - choose workflow
  - submit/start
  - approve
  - request changes
  - unblock
  - retry
  - cancel

Action dedupe/clearing notes:

- Adapter sync applies actions only when `action_revision` is newer than
  `context.task_adapter.last_action_revision`.
- Applied actions are treated as one-shot intents; projection clears `requested_action`.

Human-gate semantics (MVP):

- Task-driven human actions map to richer workflow events:
  - `approve` -> `human_approved`
  - `request_changes` -> `human_changes_requested`
  - `unblock` -> `human_unblocked`
  - `retry` -> `retry_requested`
- The runtime stores `context.task_human_gate_state` for lightweight gate-state projection.
- Default reactive human state now projects to `awaiting_human_action` (instead of
  `workflow_selected`) for semantic clarity.
- Submit while still in triage (`workflow_id="__triage__"`) is rejected with workflow-invalid
  rather than silently ignored.
- `choose_workflow` on non-triage items is currently ignored with debug logging.

Allowed-action projection (MVP):

- Tasks now expose deterministic `allowed_actions` for task-tool UX.
- Examples:
  - triage item: `choose_workflow`, `cancel`
  - human-gate item: narrowed by `human_gate_state` to avoid stale/inapplicable actions
  - pending item: `cancel` (submit retained as compatibility bridge action, not projected by default)

## Additive valid-next-events contract (authoritative, computed)

Task/work-item projection now exposes additive `valid_next_events` as a server-computed, current-state
contract for what workflow events are currently legal to fire.

- Source of truth is runtime/workflow-derived and **not persisted** as canonical state.
- External event ingress (`POST /work-items/:id/fire`) now enforces this contract and rejects
  out-of-state events with a workflow-invalid error that includes the current authoritative
  `valid_next_events` set.
- Reactive workflows:
  - derived from the current reactive state's declared `on` transitions
  - plus `cancel` for non-terminal reactive states (matching runtime behavior)
- Legacy/triage workflows:
  - derived from currently enforced runtime/event rules in orchestration
  - triage remains explicit (`submit` / `cancel` from fire_event plus dedicated
    `choose_workflow`; `workflow_chosen` is no longer exposed on generic fire_event ingress)
  - legacy human-gate states are now gate-state-aware (for example, `changes_requested` narrows to
    `human_unblocked`/`retry_requested` + `cancel`, instead of exposing the full human event set)
- Unsupported reactive parent/child behavior remains unchanged.

Ingress compatibility notes:

- Public `fire_event` ingress now accepts only externally fireable operator events
  (`submit`, human-gate events, `retry_requested`, `cancel`).
- System/internal progression events (`start`, `job_*`, child completion/failure,
  `needs_triage`, `workflow_chosen`) are no longer accepted/exposed via public fire_event.
- Internal progression paths keep existing behavior through trusted/internal ingress
  (job completion advancement, child progression, adapter-applied internal intents,
  GitHub webhook bridge).
- `choose_workflow` remains on its dedicated orchestration path and is not part of generic
  `fire_event` ingress validation.

Projection alignment:

- Task projection includes additive `valid_next_events` on task records.
- `allowed_actions` are now derived as a human/operator subset of `valid_next_events` where practical.
- `allowed_actions` remains intentionally narrower than events (system-only events are not surfaced as
  clickable operator actions).
- Work-item detail UI renders `valid_next_events` read-only for operator/debug clarity.

Deterministic triage rule-selection (MVP):

- Manual-triage authority is now derived from either:
  - task/work-item label `needs-triage` (authoritative)
  - legacy compatibility flag `context.needs_triage=true`
- When manual triage is required, triage remains non-dispatchable and `choose_workflow` is
  withheld from projected `allowed_actions` and triage `valid_next_events` (cancel-only).
- The dedicated `choose_workflow` path also rejects selection while manual triage is still active;
  removing the label/legacy flag is required before workflow selection can proceed.
- Removing `needs-triage` (and with no legacy `needs_triage=true`) deterministically re-runs
  workflow selection on adapter update so triage items can leave `__triage__` automatically when
  a workflow is now deterministically selectable.
- `triage_reason` (when present) is projected as `blocked_reason` for operator clarity.

## Canonical runtime vertical slice (docker-first)

The server runtime now persists and consumes a canonical runtime spec for jobs:

- `CanonicalRuntimeSpec`
  - `environment: PlannedEnvironmentSpec`
  - `permissions: PlannedPermissionSet`

Typed environment/permission tiers:

- Environment tiers:
  - `docker` (primary thin-slice path)
  - `raw` (compatibility tier)
  - `kubernetes` (typed stub)
- Permission tiers:
  - `docker_default`
  - `raw_compatible`
  - `kubernetes_stub`

Deterministic tier mapping used by runtime:

- Docker jobs -> `docker_default` permissions (network disabled in this slice)
- Raw jobs -> `raw_compatible` permissions (network enabled for compatibility)

Compatibility note:

- Legacy job columns (`execution_mode`, `container_image`, `timeout_secs`) remain in DB/API.
- Runtime also writes `runtime_spec_json` for canonical migration.
- Read path prefers canonical runtime spec when present, preserving compatibility when absent.

Canonical runtime as active execution contract (current vertical slice):

- Server now projects canonical runtime spec into each created job's `context` under
  `canonical_runtime_spec`.
- Worker dispatch matching (`supported_modes`) prefers canonical runtime environment from context
  when available.
- Local-agent execution mode/timeout/container image selection prefers canonical runtime context,
  with legacy job fields as fallback.
- Local-agent exposes permission hints to executors via env vars:
  - `AGENT_PERMISSION_TIER`
  - `AGENT_CAN_ACCESS_NETWORK`

Permission hint boundary note:

- The above permission env vars are currently advisory metadata only.
- They are **not** enforcement boundaries in this slice.
- TODO(runtime-permissions): add explicit runtime/sandbox enforcement where required.

This keeps legacy job runtime fields compatible while shifting active execution decisions to the
canonical runtime contract for the covered path.

## First-class task/work-item state (MVP)

Work items now carry first-class task/operator-visible fields beyond context bags, including:

- `external_task_id`
- `external_task_source`
- `title`
- `description`
- `labels`
- `priority`
- `projected_state`
- `triage_status`
- `human_gate`
- `human_gate_state`

Task projection should prefer these explicit work-item fields, with context fallback only where
legacy values still exist.

In particular, `human_gate` on the work item is now the primary projection source for pending
human action, with context fallback retained only for compatibility with older records.

## Local-agent repo/job lifecycle hygiene (MVP)

To improve daily operator usability and avoid stale workspace accumulation, local-agent now uses
attempt-scoped repo workspace semantics:

- Job/attempt-scoped worktree pathing:
  - `<data_dir>/worktrees/<work_item_id>/<job_id>-a<attempt>`
- Job/attempt-scoped branch naming for repo-backed jobs:
  - `<base_branch>--j<job_id>-a<attempt>`
  - where `<base_branch>` is `repo.branch_name` if set, otherwise `agent-hub/<work_item_id>`

Cleanup behavior after each job execution attempt:

- Repo-backed jobs:
  - remove job worktree from mirror via `git worktree remove --force`
  - run `git worktree prune`
  - best-effort delete of attempt branch ref (`git branch -D <attempt-branch>`)
- Non-repo jobs:
  - remove local attempt workspace directory

Mirror refresh loop behavior (bounded fetch frequency):

- Mirror fetch uses a local stamp file and only refreshes when stale.
- Current minimum refresh interval: 30s.
- This keeps mirrors fresh for repeated jobs while reducing noisy per-job fetch overhead.

## Additive structured transcript contract (thin slice)

Local-agent now emits and reports additive structured transcript artifacts without changing existing
transcript behavior:

- worker always writes `.agent-context.json` under `AGENT_TRANSCRIPT_DIR`
- worker now exposes advisory env vars:
  - `AGENT_AGENT_CONTEXT_PATH`
  - `AGENT_TRANSCRIPT_JSONL_PATH`
  - `AGENT_JOB_EVENTS_JSONL_PATH`
  - `AGENT_STDOUT_LOG_PATH`
  - `AGENT_STDERR_LOG_PATH`
  - `AGENT_RUN_JSON_PATH`
- agent-written `transcript.jsonl` remains optional and opt-in; worker does not require it

`agent_context_v1` `.agent-context.json` now includes additive standard worker-local transcript
paths under `paths`:

- existing: `workspace_dir`, `transcript_dir`, `context_json`, `output_json`,
  `transcript_jsonl`, `prompt_user`, `prompt_system`
- additive: `agent_context_json`, `job_events_jsonl`, `stdout_log`, `stderr_log`, `run_json`
- for docker jobs, these `paths.*` entries are normalized to container-visible `/workspace` and
  `/transcript` paths so they match the advisory `AGENT_*_PATH` env vars rather than host paths

`agent_context_v1` now also carries an additive workflow input contract under
`workflow.inputs` so agents get a predictable server-authored bundle:

- `workflow.inputs.valid_next_events`
  - authoritative current public/operator fireable workflow events for the active work item
  - sourced from orchestration (`valid_next_events_for_work_item`) and serialized as snake_case
    event names
- `workflow.inputs.job_artifact_paths`
  - projected prior-step structured transcript artifact path aliases from work-item context
  - metadata/alias only (path existence or mount reachability is not guaranteed)
- `workflow.inputs.job_artifact_metadata`
  - projected prior-step structured transcript artifact metadata from work-item context
- intentionally excluded in this slice: `workflow.inputs.job_artifact_data`

Artifact/result contract additions (additive only):

- `ArtifactSummary.structured_transcript_artifacts[]` now records known structured artifacts with:
  - `key` (for example, `agent_context`, `run_json`, `transcript_jsonl`, `job_events`)
  - `path`
  - optional `bytes`
  - optional `schema` (currently emitted for `.agent-context.json` as `agent_context_v1`,
    and for `run.json` as `job_run_v1`)
  - optional `record_count` (line count, currently emitted when JSONL artifacts are present)
  - optional `inline_json` (currently populated for `.agent-context.json` and `run.json` when
    schema is recognized)

Worker-owned job event transcript artifact in this slice:

- local-agent always writes `job-events.jsonl` in transcript dir
- one JSON line is appended per emitted job event using the `protocol::JobEvent` shape
- this local artifact is transport-independent (present for HTTP and gRPC worker modes)
- artifact metadata is reported as:
  - `key = "job_events"`
  - `schema = "protocol::JobEvent"`
  - `bytes` + `record_count` derived from file metadata
  - `inline_json = [ ... ]` bounded tail preview (small last-N parsed `protocol::JobEvent` records)
    with malformed JSONL lines skipped and a total inline byte budget applied

Current behavior boundaries for this slice:

- existing `stdout.log`, `stderr.log`, `output.json`, and transcript directory behavior is unchanged
- no file-retrieval/download API changes are included
- UI now surfaces path/bytes plus optional schema/record_count metadata on job detail

## Additive work-item context projection for structured transcript artifacts (follow-up slice)

When a job reaches terminal state and its result is merged into work-item context, server now
also projects known structured transcript artifact data into stable additive context namespaces:

- `job_artifact_paths.<step_id>.<artifact_key> = <path>`
  - example keys include `agent_context`
  - optional artifacts such as `transcript_jsonl` are projected only when present
- `job_artifact_metadata.<step_id>.<artifact_key>.*`
  - includes additive metadata fields derived from result artifacts (`path`, optional `bytes`,
    optional `schema`, optional `record_count`)
- `job_artifact_data.<step_id>.<artifact_key> = <json>`
  - currently projected for:
    - `agent_context` artifacts with schema `agent_context_v1`
    - `run_json` artifacts with schema `job_run_v1`
    - `transcript_jsonl` when inline tail data is present (bounded raw line-string array)
  - `job_events` now projects a bounded inline tail preview (not full transcript)

Compatibility and behavior constraints for this slice:

- existing `job_outputs` and `job_results` merge behavior remains unchanged
- jobs without structured transcript artifacts preserve prior behavior; no new required fields
- child-result propagation includes `child_results.<parent_step>.job_artifact_paths` and
  `child_results.<parent_step>.job_artifact_metadata` for consistency
- child-result propagation also includes `child_results.<parent_step>.job_artifact_data`
  for symmetry
- this remains metadata projection only (no file retrieval APIs)
- hardening: workflow step/state ids must not contain `.` to avoid ambiguity with dot-path
  interpolation aliases used by `job_outputs`, `job_results`, `job_artifact_paths`, and
  `job_artifact_metadata`

## Work-item structured I/O inspector (read-only, additive)

Operator/workflow-author UI now includes a read-only structured I/O inspector on the work-item
detail page, sourced from existing `work_item.context` projection data (no protocol changes):

- `job_outputs`
- `job_results`
- `job_artifact_paths`
- `job_artifact_metadata`
- `job_artifact_data`
- `child_results`

Inspector behavior constraints for this slice:

- alias-first presentation with copyable interpolation examples (`{{alias.path}}`)
- metadata-only display (`alias`, interpolation form, JSON value kind)
- focused view that does not dump unrelated raw context by default
- no artifact retrieval/download APIs added

## Job detail structured artifact alias + preview slice (Phase 3, low risk)

Job detail UI now exposes the projected `job_artifact_data` alias alongside existing structured
artifact metadata aliases and adds a bounded inline preview for known schema-gated inline payloads.

- Additive aliases shown per structured transcript artifact:
  - `job_artifact_paths.<step_id>.<artifact_key>`
  - `job_artifact_metadata.<step_id>.<artifact_key>`
  - `job_artifact_data.<step_id>.<artifact_key>`
- Inline preview is currently gated to known structured schemas (`agent_context_v1`,
  `job_run_v1`, `protocol::JobEvent`) plus a strict bounded raw-line preview for
  `transcript_jsonl`; previews are rendered collapsed by default.
- `job_events` preview uses bounded inline tail data only; this does not add full transcript
  retrieval.
- Scope remains UI/server projection only (no retrieval/download endpoint changes, no protocol churn).

## Work-item detail structured artifact preview slice (Phase 3, low risk)

Work-item detail UI now includes a read-only **Structured artifact previews** section to surface
schema-aware inline previews for known projected structured artifacts.

- Data source includes:
  - `work_item.context.job_artifact_data`
  - `work_item.context.child_results.<parent_step>.job_artifact_data`
- Preview is schema-gated to the same known artifacts as job detail:
  - `agent_context` with `agent_context_v1`
  - `run_json` with `job_run_v1`
  - `job_events` with `protocol::JobEvent` (bounded inline tail only)
  - `transcript_jsonl` with strict bounded raw-line arrays only (no schema requirement)
- Preview formatting and truncation reuse existing bounded server behavior.
- Section surfaces useful aliases for workflow authors/operators:
  - `job_artifact_data.<step_id>.<artifact_key>`
  - `job_artifact_metadata.<step_id>.<artifact_key>`
  - `child_results.<parent_step>.job_artifact_data.<child_step>.<artifact_key>`
  - `child_results.<parent_step>.job_artifact_metadata.<child_step>.<artifact_key>`
- `transcript_jsonl` preview is bounded and read-only, and only renders safe raw-line arrays.
- server-side transcript preview safety bounds are intentionally independent from producer-side
  tail capture limits and may be tighter; do not assume they should be kept identical.
- `job_events` preview is bounded to inline tail data and does not imply full transcript access.
- Metadata schema fallback is applied to both top-level and child-result previews.

Scope/compatibility constraints remain unchanged for this slice:

- additive UI/server behavior only
- no retrieval/download endpoint changes
- existing structured-I/O alias inspector remains intact

## Work-item detail structured value preview slice (Phase 3, low risk)

Work-item detail UI now includes a read-only **Structured value previews** section to surface
bounded inline previews for projected structured values already present in context.

- Data source is namespace-scoped to projected structured I/O only:
  - `job_outputs`
  - `job_results`
  - `child_results.<parent_step>.job_outputs`
  - `child_results.<parent_step>.job_results`
- Previews are additive UI/server-only rendering over existing context projection data.
- Each row includes explicit alias + interpolation examples for workflow authoring:
  - `<alias>` and `{{<alias>}}`
  - examples include `job_outputs.<step_id>` and
    `child_results.<parent_step>.job_results.<child_step>`
- Inline previews are rendered collapsed by default and use the existing shared truncation
  behavior (`... (preview truncated)` suffix on bounded output).
- Very large JSON values may be omitted from preview entirely when they exceed the server-side
  pretty-print budget, to avoid excessive in-memory expansion during rendering.
- The section intentionally avoids dumping arbitrary raw context and preserves existing
  structured artifact preview sections unchanged.

Scope/compatibility constraints for this slice:

- additive UI/server behavior only
- no retrieval/download endpoint changes
- no local-agent/protocol contract changes

## Job detail structured value preview slice (Phase 3, low risk)

Job detail UI now includes a read-only **Structured value previews** section for the current job,
using only existing job result data already persisted on the job record.

- Data source is restricted to the current job’s projected aliases:
  - `job_outputs.<step_id>` from `job.result.output_json`
  - `job_results.<step_id>` from serialized `job.result`
- Each row includes alias + interpolation examples for workflow authoring:
  - `<alias>` and `{{<alias>}}`
  - examples include `job_outputs.<step_id>` and `job_results.<step_id>`
- Inline previews are collapsed and bounded using existing server-side preview helpers and
  truncation behavior.
- Existing job detail **Structured transcript artifacts** section remains unchanged.

Scope/compatibility constraints for this slice:

- additive UI/server behavior only
- no retrieval/download endpoint changes
- no local-agent/protocol contract changes

## Work-item detail projected artifact path/metadata preview slice (Phase 3, low risk)

Work-item detail UI now includes a read-only **Projected artifact aliases + metadata previews**
section for bounded preview of projected artifact path and metadata namespaces already present in
`work_item.context`.

- Data source is restricted to projected artifact namespaces:
  - `job_artifact_paths`
  - `job_artifact_metadata`
  - `child_results.<parent_step>.job_artifact_paths`
  - `child_results.<parent_step>.job_artifact_metadata`
- Each row includes explicit alias + interpolation examples for workflow authoring:
  - `<alias>` and `{{<alias>}}`
  - examples include `job_artifact_paths.<step_id>` and
    `child_results.<parent_step>.job_artifact_metadata.<child_step>`
- Inline previews are collapsed and bounded using existing shared structured-value preview
  behavior (JSON byte budget + truncation suffix).
- Section is explicitly metadata/path projection only and does **not** imply artifact path
  retrievability or add retrieval/download endpoints.

Scope/compatibility constraints for this slice:

- additive UI/server behavior only
- no retrieval/download endpoint changes
- no local-agent/protocol contract changes

## Conventional artifact summary projection slice (Phase 3, low risk)

Server projection now maps legacy `ArtifactSummary` path/byte fields into the existing additive
artifact projection namespaces for each terminal job step.

- Additional projected path aliases:
  - `job_artifact_paths.<step_id>.stdout_log`
  - `job_artifact_paths.<step_id>.stderr_log`
  - `job_artifact_paths.<step_id>.output_json`
  - `job_artifact_paths.<step_id>.transcript_dir`
- Matching metadata aliases are projected under `job_artifact_metadata.<step_id>.<key>` with:
  - required `path`
  - optional `bytes` where meaningful (`stdout_log`, `stderr_log`, `output_json`)
- Projection remains sparse/additive:
  - keys are included only when non-empty values are present
  - `transcript_dir` intentionally omits `bytes`
  - no new fields are projected into `job_artifact_data`

Job detail UI now includes an additive read-only **Conventional projected artifacts** section that
surfaces these aliases and metadata/path values alongside existing structured artifact sections.

Scope/compatibility constraints for this slice:

- additive server/UI behavior only
- additive job-scoped retrieval endpoint changes (see below)
- no local-agent/protocol contract changes
- existing structured transcript artifact behavior remains unchanged

## Job-scoped artifact retrieval slice (Phase 3, low/medium risk)

Server now exposes read-only job-scoped retrieval for a constrained set of known artifact keys:

- `GET /orchestration/jobs/{id}/artifacts/{key}`
- optional attachment mode: `?download=1`

Supported keys in this slice:

- conventional: `stdout_log`, `stderr_log`, `output_json`
- structured: `agent_context`, `run_json`, `job_events`, `transcript_jsonl`

Security and compatibility boundaries:

- no request-supplied filesystem path is accepted
- artifact path is resolved from persisted job result metadata only
- unknown keys return `400` (`unsupported artifact key`)
- known keys without persisted metadata path return `404`
- missing/stale/unreadable files return `404`
- `transcript_dir` remains metadata-only and is intentionally not retrievable as a directory
- no DB schema changes
- no local-agent contract changes

UI behavior in this slice:

- job detail page now includes a read-only **Artifact retrieval** section
- each available key shows `Open` and `Download` links routed through the job-scoped endpoint
- work-item detail pages remain unchanged
