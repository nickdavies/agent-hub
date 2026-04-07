// AUTO-GENERATED — do not edit by hand.
// Source: protocol crate JSON schemas → json-schema-to-typescript
//
// Regenerate with:
//   cargo run -p protocol --bin generate_schemas -- provider-plugins/opencode/generated
//   bun run generate-types.ts

/**
 * Opaque session identifier.
 *
 * Wraps a `String` with `#[serde(transparent)]` so the JSON wire format
 * stays a bare string — no migration needed for TypeScript plugins, shell
 * scripts, or persisted state files.
 */
export type SessionId = string
/**
 * Tool name as sent by OpenCode (lowercase). Known: bash, edit, glob, grep, multiedit, read, task, todowrite, webfetch, write.
 */
export type OpenCodeTool = string

/**
 * Input from the opencode hook (stdin JSON for both tool.execute.before and permission.ask).
 */
export interface OpenCodeHookInput {
  cwd?: string
  hook_event_name?: string
  session_id: SessionId
  /**
   * Session title from opencode (may be absent).
   */
  session_title?: string | null
  tool_input?: {
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
 *   1 = rejected, cancelled, timed out, or server unreachable
 *   2 = fail-closed (bad input / internal error)
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
  questions: QuestionInfo[]
  session_display_name: string
  session_id: SessionId
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
       */
      answers: string[][]
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
export const OPENCODE_TOOL_NAMES = ["bash", "edit", "glob", "grep", "multiedit", "read", "task", "todowrite", "webfetch", "write"] as const

export type KnownOpenCodeTool = (typeof OPENCODE_TOOL_NAMES)[number]
