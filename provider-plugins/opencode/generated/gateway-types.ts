// AUTO-GENERATED — do not edit by hand.
// Source: protocol crate JSON schemas → json-schema-to-typescript
//
// Regenerate with:
//   cargo run -p protocol --bin generate_schemas -- provider-plugins/opencode/generated
//   bun run generate-types.ts

export type AgentHubRunDecisionAuthority = "automatic" | "human"
export type AgentHubRunDecision = "allow" | "deny"

/**
 * Versioned gateway decision. Every request binding is echoed and validated by
 * the host before the private receipt is created.
 */
export interface AgentHubRunDecisionV1 {
  approval_id: string
  authority: AgentHubRunDecisionAuthority
  canonical_digest: string
  decided_at: string
  decision: AgentHubRunDecision
  expires_at: string
  invocation_id: string
  reason?: string | null
  request_digest: string
  schema: string
}

export type AgentHubRunImageFamily = "job-code"
export type AgentHubRunNetworkClass = "none" | "llm-only" | "build" | "guarded-web"
export type NetworkResourceKind =
  | "llm"
  | "github"
  | "go_registry"
  | "crates_registry"
  | "pypi_registry"
  | "npm_registry"
  | "guarded_web"
export type AgentHubRunIntentSchema = "agent_hub_run_intent_v1"
export type AgentHubRunToolApproval = "auto" | "dippy"
export type WorkspaceAccess = "read_only" | "read_write"

/**
 * Public tool input. It contains requested capabilities only; config identity,
 * image identity, provider commands, and gateway transport are host-owned.
 */
export interface AgentHubRunIntentV1 {
  /**
   * @maxItems 16
   */
  context_roots?:
    | []
    | [AgentHubRunContextRootV1]
    | [AgentHubRunContextRootV1, AgentHubRunContextRootV1]
    | [AgentHubRunContextRootV1, AgentHubRunContextRootV1, AgentHubRunContextRootV1]
    | [AgentHubRunContextRootV1, AgentHubRunContextRootV1, AgentHubRunContextRootV1, AgentHubRunContextRootV1]
    | [
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1
      ]
    | [
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1
      ]
    | [
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1
      ]
    | [
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1
      ]
    | [
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1
      ]
    | [
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1
      ]
    | [
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1
      ]
    | [
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1
      ]
    | [
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1
      ]
    | [
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1
      ]
    | [
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1
      ]
    | [
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1,
        AgentHubRunContextRootV1
      ]
  environment: string
  image_family: AgentHubRunImageFamily
  network_class: AgentHubRunNetworkClass
  /**
   * @maxItems 16
   */
  network_resources?:
    | []
    | [NetworkResourceKind]
    | [NetworkResourceKind, NetworkResourceKind]
    | [NetworkResourceKind, NetworkResourceKind, NetworkResourceKind]
    | [NetworkResourceKind, NetworkResourceKind, NetworkResourceKind, NetworkResourceKind]
    | [NetworkResourceKind, NetworkResourceKind, NetworkResourceKind, NetworkResourceKind, NetworkResourceKind]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
  /**
   * @maxItems 16
   */
  overlays?:
    | []
    | [string]
    | [string, string]
    | [string, string, string]
    | [string, string, string, string]
    | [string, string, string, string, string]
    | [string, string, string, string, string, string]
    | [string, string, string, string, string, string, string]
    | [string, string, string, string, string, string, string, string]
    | [string, string, string, string, string, string, string, string, string]
    | [string, string, string, string, string, string, string, string, string, string]
    | [string, string, string, string, string, string, string, string, string, string, string]
    | [string, string, string, string, string, string, string, string, string, string, string, string]
    | [string, string, string, string, string, string, string, string, string, string, string, string, string]
    | [string, string, string, string, string, string, string, string, string, string, string, string, string, string]
    | [
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string
      ]
    | [
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string,
        string
      ]
  primary_workspace: string
  prompt: string
  schema: AgentHubRunIntentSchema
  timeout_secs: number
  tool_approval: AgentHubRunToolApproval
  workspace_export_root?: string | null
  /**
   * @minItems 1
   * @maxItems 16
   */
  workspaces:
    | [AgentHubRunWorkspaceV1]
    | [AgentHubRunWorkspaceV1, AgentHubRunWorkspaceV1]
    | [AgentHubRunWorkspaceV1, AgentHubRunWorkspaceV1, AgentHubRunWorkspaceV1]
    | [AgentHubRunWorkspaceV1, AgentHubRunWorkspaceV1, AgentHubRunWorkspaceV1, AgentHubRunWorkspaceV1]
    | [
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1
      ]
    | [
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1
      ]
    | [
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1
      ]
    | [
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1
      ]
    | [
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1
      ]
    | [
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1
      ]
    | [
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1
      ]
    | [
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1
      ]
    | [
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1
      ]
    | [
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1
      ]
    | [
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1
      ]
    | [
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1,
        AgentHubRunWorkspaceV1
      ]
}
export interface AgentHubRunContextRootV1 {
  alias: string
  path: string
}
export interface AgentHubRunWorkspaceV1 {
  access: WorkspaceAccess
  alias: string
  path: string
}

export type AgentHubRunApprovalReceiptStatus = "issued" | "unavailable"

/**
 * Plugin-consumable terminal envelope. `local_result_v2` is deliberately a
 * bounded JSON value so the CLI's established aggregate remains the source of
 * truth without coupling protocol compatibility to local storage internals.
 */
export interface AgentHubRunOutputV1 {
  approval_receipt: AgentHubRunApprovalReceiptV1
  error?: string | null
  invocation_id: string
  local_result_v2: unknown
  schema: string
}
/**
 * Stable approval evidence slot present on every one-step terminal envelope.
 */
export interface AgentHubRunApprovalReceiptV1 {
  error?: string | null
  receipt?: AgentHubRunReceiptV1 | null
  schema: string
  status: AgentHubRunApprovalReceiptStatus
}
/**
 * Authority-bound gateway decision plus a private integrity checksum. The
 * checksum is local integrity evidence, not a gateway signature.
 */
export interface AgentHubRunReceiptV1 {
  approval_id: string
  authority: AgentHubRunDecisionAuthority
  canonical_digest: string
  checksum: string
  decided_at: string
  decision: AgentHubRunDecision
  expires_at: string
  invocation_id: string
  reason?: string | null
  request_digest: string
  schema: string
}

/**
 * Strict policy surface derived from a verified canonical execution. Policy
 * matching never receives the canonical transport or public intent fields.
 */
export interface AgentHubRunPolicyProjectionV1 {
  /**
   * @maxItems 64
   */
  capabilities: AgentHubRunCapabilityProjectionV1[]
  /**
   * @maxItems 16
   */
  context_roots:
    | []
    | [AgentHubRunRootProjectionV1]
    | [AgentHubRunRootProjectionV1, AgentHubRunRootProjectionV1]
    | [AgentHubRunRootProjectionV1, AgentHubRunRootProjectionV1, AgentHubRunRootProjectionV1]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
  environment: string
  image_family: string
  image_id: string
  network_class: string
  /**
   * @maxItems 16
   */
  network_resources:
    | []
    | [NetworkResourceKind]
    | [NetworkResourceKind, NetworkResourceKind]
    | [NetworkResourceKind, NetworkResourceKind, NetworkResourceKind]
    | [NetworkResourceKind, NetworkResourceKind, NetworkResourceKind, NetworkResourceKind]
    | [NetworkResourceKind, NetworkResourceKind, NetworkResourceKind, NetworkResourceKind, NetworkResourceKind]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
    | [
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind,
        NetworkResourceKind
      ]
  output: AttendedOutputContract
  /**
   * @maxItems 16
   */
  overlays:
    | []
    | [AgentHubRunCapabilityProjectionV1]
    | [AgentHubRunCapabilityProjectionV1, AgentHubRunCapabilityProjectionV1]
    | [AgentHubRunCapabilityProjectionV1, AgentHubRunCapabilityProjectionV1, AgentHubRunCapabilityProjectionV1]
    | [
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1
      ]
    | [
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1
      ]
    | [
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1
      ]
    | [
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1
      ]
    | [
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1
      ]
    | [
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1
      ]
    | [
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1
      ]
    | [
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1
      ]
    | [
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1
      ]
    | [
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1
      ]
    | [
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1
      ]
    | [
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1
      ]
    | [
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1,
        AgentHubRunCapabilityProjectionV1
      ]
  prompt_digest: string
  retry: AttendedRetryConstraints
  review: AgentHubRunReviewProjectionV1
  schema: string
  timeout_secs: number
  tool_approval: string
  /**
   * @minItems 1
   * @maxItems 16
   */
  workspaces:
    | [AgentHubRunRootProjectionV1]
    | [AgentHubRunRootProjectionV1, AgentHubRunRootProjectionV1]
    | [AgentHubRunRootProjectionV1, AgentHubRunRootProjectionV1, AgentHubRunRootProjectionV1]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
    | [
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1,
        AgentHubRunRootProjectionV1
      ]
}
export interface AgentHubRunCapabilityProjectionV1 {
  access: string
  name: string
}
export interface AgentHubRunRootProjectionV1 {
  access: WorkspaceAccess
  alias: string
  path: string
  root_identity: string
}
export interface AttendedOutputContract {
  artifacts: string
  cleanup: string
  limits: AttendedOutputLimits
  repositories: string
  result: string
  transcript: string
}
export interface AttendedOutputLimits {
  artifact_bytes: number
  stderr_bytes: number
  stdout_bytes: number
  transcript_bytes: number
}
export interface AttendedRetryConstraints {
  max_attempts: number
  max_prelaunch_retries: number
  max_provider_unavailable_retries: number
}
export interface AgentHubRunReviewProjectionV1 {
  approval_bundle_digest: string
  config_digest: string
  environment_bundle_digest: string
  primary_workspace: string
  workspace_export_root?: string | null
}

/**
 * Exact canonical execution submitted to the gateway.
 */
export interface AgentHubRunResolvedV1 {
  canonical_digest: string
  canonical_execution: AgentHubRunCanonicalArtifactV1
  expires_at: string
  invocation_id: string
  policy_projection: AgentHubRunPolicyProjectionV1
  request_digest: string
  requested_at: string
  schema: string
}
export interface AgentHubRunCanonicalArtifactV1 {
  canonical_bytes_base64: string
  digest: string
  version: number
}

/**
 * Opaque session identifier.
 *
 * Wraps a `String` with `#[serde(transparent)]` so the JSON wire format
 * stays a bare string — no migration needed for TypeScript plugins, shell
 * scripts, or persisted state files.
 */
export type SessionId = string
/**
 * Tool name as sent by OpenCode (lowercase). Known: bash, codesearch, doom_loop, edit, external_directory, glob, grep, list, lsp, multiedit, question, read, skill, task, todowrite, webfetch, websearch, write.
 */
export type OpenCodeTool = string

/**
 * Input from the OpenCode execution hook.
 */
export interface OpenCodeHookInput {
  cwd: string
  hook_event_name?: string
  session_id: SessionId
  /**
   * Session title from opencode (may be absent).
   */
  session_title?: string | null
  tool_input?: {
    [k: string]: unknown
  }
  /**
   * Canonical fields used only for typed matching. The original tool input
   * remains intact for review, delegation, and attended request digests.
   */
  tool_match_input?: {
    [k: string]: unknown
  }
  tool_name: OpenCodeTool
  workspace_roots?: string[] | null
}

/**
 * Output to opencode (stdout JSON).
 */
export interface OpenCodeHookOutput {
  allowed: boolean
  reason?: string | null
}

/**
 * The editor/agent that owns a session.
 */
export type Provider = "claude" | "cursor" | "opencode" | "unknown"

/**
 * JSON written to stdout by `agent-hub-gateway question` on exit 0.
 *
 * Exit codes:
 *   0 = answered   — stdout contains this struct
 *   1 = explicitly rejected
 *   2 = fail-closed malformed input or local failure
 *   3 = timed out because the operator was away
 *   4 = approvals-pipeline failure
 */
export interface QuestionGatewayOutput {
  /**
   * One array of selected labels per question, in input order.
   */
  answers: string[][]
}

/**
 * Request body sent by the gateway when a question needs proxying.
 */
export interface QuestionProxyRequest {
  cwd: string
  /**
   * Gateway-generated UUID used as an idempotency key.
   */
  id: string
  /**
   * Provider that originated the question (always "opencode" for now).
   */
  provider: string
  /**
   * The opencode-native question ID (e.g. "que_01j...").
   */
  question_request_id: string
  /**
   * @minItems 1
   * @maxItems 32
   */
  questions: [QuestionInfo, ...QuestionInfo[]]
  session_display_name: string
  session_id: string
}
/**
 * One question with its choices.
 */
export interface QuestionInfo {
  /**
   * If true (default), a free-text "Type your own answer" input is shown.
   */
  custom?: boolean | null
  /**
   * Short label (≤30 chars) shown as the tab/step heading.
   */
  header: string
  /**
   * If true, multiple options may be selected simultaneously.
   */
  multiple?: boolean | null
  /**
   * @maxItems 64
   */
  options: QuestionOption[]
  question: string
}
/**
 * A single selectable option within a question.
 */
export interface QuestionOption {
  description: string
  label: string
}

/**
 * Immediate response returned to the gateway after registering a question.
 */
export type QuestionProxyResponse = {
  id: string
} & (
  | {
      type: "pending"
    }
  | {
      /**
       * One `Answer` (array of selected labels) per question, in order.
       *
       * @minItems 1
       * @maxItems 32
       */
      answers: [[string, ...string[]], ...[string, ...string[]][]]
      type: "answered"
    }
  | {
      reason?: string | null
      type: "rejected"
    }
  | {
      type: "cancelled"
    }
)

/**
 * Decision choices when resolving a question.
 */
export type QuestionDecision = "answer" | "reject" | "cancel"

/**
 * Request body for POST /api/v1/questions/{id}/resolve.
 */
export interface QuestionResolveRequest {
  /**
   * Required when `decision == Answer`. One array of selected labels per question.
   */
  answers?: [[string, ...string[]], ...[string, ...string[]][]] | null
  decision: QuestionDecision
  /**
   * Optional reason shown in the dashboard (used with `Reject` or `Cancel`).
   */
  reason?: string | null
}

/**
 * Stored session status as reported by the client.
 */
export type SessionStatus = "active" | "idle" | "waiting" | "ended"

/**
 * POST /api/v1/hooks/status — request body sent by the gateway (status-report subcommand).
 */
export interface StatusReport {
  cwd: string
  display_name?: string | null
  editor_type?: Provider | null
  session_id: SessionId
  status: SessionStatus
  waiting_reason?: string | null
}

/**
 * Known OpenCodeTool values. The gateway accepts any string (for unknown/MCP
 * tools), but these are the recognized tool names.
 */
export const OPENCODE_TOOL_NAMES = ["bash", "codesearch", "doom_loop", "edit", "external_directory", "glob", "grep", "list", "lsp", "multiedit", "read", "question", "skill", "task", "todowrite", "webfetch", "websearch", "write"] as const

export type KnownOpenCodeTool = (typeof OPENCODE_TOOL_NAMES)[number]
