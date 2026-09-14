import { constants } from "node:fs"
import { access, stat } from "node:fs/promises"
import { isAbsolute, join } from "node:path"
import type { ToolContext } from "@opencode-ai/plugin"
import { tool } from "@opencode-ai/plugin"
import type { z } from "zod"
import type { AgentHubRunIntentV1 } from "./generated/gateway-types"

const schema = tool.schema
const MAX_STDOUT_BYTES = 1024 * 1024
const MAX_STDERR_BYTES = 64 * 1024
const MAX_OPENCODE_ID_BYTES = 1024

const boundedString = (maximum: number) =>
  schema
    .string()
    .min(1)
    .max(maximum)
    .refine((value) => new TextEncoder().encode(value).byteLength <= maximum, { message: `must not exceed ${maximum} bytes` })
const aliasSchema = boundedString(63).regex(/^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/)
const absolutePathSchema = boundedString(1024).refine((path) => isAbsolute(path) && !path.includes("\0"), {
  message: "path must be absolute",
})
const workspaceSchema = schema
  .object({
    alias: aliasSchema,
    path: absolutePathSchema,
    access: schema.enum(["read_only", "read_write"]),
  })
  .strict()
const contextRootSchema = schema
  .object({
    alias: aliasSchema,
    path: absolutePathSchema,
  })
  .strict()

export const agentHubRunArgs = {
  schema: schema.literal("agent_hub_run_intent_v1"),
  environment: boundedString(128),
  image_family: schema.literal("job-code"),
  network_class: schema.enum(["none", "llm-only", "build", "guarded-web"]),
  network_resources: schema
    .array(
      schema.enum([
        "llm",
        "github",
        "go_registry",
        "crates_registry",
        "pypi_registry",
        "npm_registry",
        "guarded_web",
      ]),
    )
    .max(16)
    .default([]),
  tool_approval: schema.enum(["auto", "dippy"]),
  overlays: schema.array(boundedString(128)).max(16).default([]),
  timeout_secs: schema.number().int().min(1).max(86_400),
  prompt: boundedString(128 * 1024),
  workspaces: schema.array(workspaceSchema).min(1).max(16),
  context_roots: schema.array(contextRootSchema).max(16).default([]),
  primary_workspace: aliasSchema,
  workspace_export_root: absolutePathSchema.nullable().optional(),
}

export const requestSchema = schema
  .object(agentHubRunArgs)
  .strict()
  .superRefine((input, context) => {
    const aliases = [...input.workspaces, ...input.context_roots].map(({ alias }) => alias)
    const paths = [...input.workspaces, ...input.context_roots].map(({ path }) => path)
    if (new Set(aliases).size !== aliases.length || new Set(paths).size !== paths.length) {
      context.addIssue({ code: "custom", message: "workspace and context aliases and paths must be unique" })
    }
    if (!input.workspaces.some(({ alias }) => alias === input.primary_workspace)) {
      context.addIssue({ code: "custom", message: "primary workspace is not requested" })
    }
    if (
      new Set(input.network_resources).size !== input.network_resources.length ||
      new Set(input.overlays).size !== input.overlays.length
    ) {
      context.addIssue({ code: "custom", message: "network resources and overlays must be unique" })
    }
    if (input.workspace_export_root) {
      const matches = input.workspaces.filter(
        ({ access, path }) => access === "read_write" && join(path, ".agent-hub/runs") === input.workspace_export_root,
      )
      if (matches.length !== 1) {
        context.addIssue({
          code: "custom",
          message: "workspace export root must be exactly .agent-hub/runs under one read-write workspace",
        })
      }
    }
  })
export type AgentHubRunRequest = z.infer<typeof requestSchema>

const nonblankString = schema.string().refine((value) => value.trim().length > 0, { message: "must not be blank" })
const agentResultSchema = schema
  .object({
    schema: schema.literal("agent_result_v1"),
    complete: schema.boolean(),
    summary: nonblankString,
    acceptance_results: schema.array(
      schema
        .object({
          criterion: nonblankString,
          passed: schema.boolean(),
          evidence: nonblankString,
        })
        .strict(),
    ),
    blockers: schema.array(nonblankString),
    artifact_references: schema.array(
      schema.object({ path: nonblankString, description: nonblankString }).strict(),
    ),
  })
  .strict()
  .superRefine((value, context) => {
    if (value.complete && (value.blockers.length > 0 || value.acceptance_results.some((result) => !result.passed))) {
      context.addIssue({ code: "custom", message: "complete agent result contradicts blockers or failed acceptance results" })
    }
  })

const runReceiptSchema = schema
  .object({
    schema: schema.literal("agent_hub_run_decision_v1"),
    invocation_id: schema.uuid(),
    canonical_digest: boundedString(64).length(64),
    request_digest: boundedString(64).length(64),
    decision: schema.enum(["allow", "deny"]),
    authority: schema.enum(["automatic", "human"]),
    approval_id: schema.uuid(),
    reason: boundedString(4096).nullable().optional(),
    decided_at: schema.iso.datetime(),
    expires_at: schema.iso.datetime(),
    checksum: schema.string(),
  })
  .strict()

const approvalReceiptSchema = schema
  .object({
    schema: schema.literal("agent_hub_run_approval_receipt_v1"),
    status: schema.enum(["issued", "unavailable"]),
    receipt: runReceiptSchema.nullable().optional(),
    error: boundedString(4096).nullable().optional(),
  })
  .strict()
  .superRefine((value, context) => {
    const valid =
      (value.status === "issued" && value.receipt != null && value.error == null) ||
      (value.status === "unavailable" && value.receipt == null && value.error != null)
    if (!valid) context.addIssue({ code: "custom", message: "approval receipt state is inconsistent" })
  })

const runStatusSchema = schema.enum([
  "preparing",
  "launch_pending",
  "consumed",
  "running",
  "succeeded",
  "failed",
  "cancelled",
  "timed_out",
  "cleanup_failed",
  "cleaned",
])
const retryClassSchema = schema.enum(["prelaunch", "provider_service_unavailable"])
const permissionOutcomeSchema = schema.enum([
  "not_applicable",
  "no_denials",
  "denied",
  "cancelled",
  "readiness_failed",
])
const processOutcomeSchema = schema.enum([
  "not_started",
  "succeeded",
  "failed",
  "cancelled",
  "timed_out",
  "output_limit_exceeded",
])
const agentOutcomeSchema = schema.enum([
  "not_produced",
  "succeeded",
  "incomplete",
  "missing",
  "malformed",
  "contradictory",
])
const executionOutcomeSchema = schema.enum(["pending", "succeeded", "failed", "cancelled", "timed_out"])
const resultOutcomeSchema = schema.enum(["unknown", "pending", "succeeded", "failed"])
const cleanupOutcomeSchema = schema.enum(["pending", "succeeded", "failed"])
const boundedArtifactReferenceSchema = schema
  .object({
    path: schema.string(),
    available: schema.boolean(),
    limit_bytes: schema.number().int().nonnegative(),
    returned_bytes: schema.number().int().nonnegative(),
    total_bytes: schema.number().int().nonnegative(),
    truncated: schema.boolean(),
  })
  .strict()
const parsedDocumentSchema = schema
  .object({
    reference: boundedArtifactReferenceSchema,
    document: schema.json().nullable(),
    parse_error: schema.string().nullable(),
  })
  .strict()

export const localResultV2Schema = schema
  .object({
    schema: schema.literal("local_result_v2"),
    run_id: schema.string(),
    launch_state: schema.enum([
      "proposed",
      "authorized",
      "preparing",
      "launch_pending",
      "consumed",
      "running",
      "terminal",
    ]),
    status: runStatusSchema,
    error: schema.string().nullable(),
    agent_final: agentResultSchema.nullable(),
    process_outcome: processOutcomeSchema,
    agent_outcome: agentOutcomeSchema,
    execution_outcome: executionOutcomeSchema,
    result_outcome: resultOutcomeSchema,
    permission_outcome: permissionOutcomeSchema,
    cleanup_outcome: cleanupOutcomeSchema,
    exit_code: schema.number().int().nullable(),
    retry_class: retryClassSchema.nullable(),
    retry_eligible: schema.boolean(),
    paths: schema.record(schema.string(), schema.string()),
    result: parsedDocumentSchema,
    artifacts: parsedDocumentSchema,
    repositories: parsedDocumentSchema,
    cleanup: parsedDocumentSchema,
    stdout: boundedArtifactReferenceSchema,
    stderr: boundedArtifactReferenceSchema,
    transcript: boundedArtifactReferenceSchema,
  })
  .strict()

const localResultEnvelopeSchema = schema
  .object({ schema: schema.literal("local_result_v2") })
  .loose()

export const hostRunOutputSchema = schema
  .object({
    schema: schema.literal("agent_hub_run_output_v1"),
    invocation_id: schema.uuid(),
    approval_receipt: approvalReceiptSchema,
    local_result_v2: localResultEnvelopeSchema,
    error: boundedString(4096).nullable().optional(),
  })
  .strict()

interface CommandOutput {
  stdout: string
  stderr: string
  exitCode: number
}

export interface AgentHubRuntime {
  env: Record<string, string | undefined>
  assertExecutable(path: string): Promise<void>
  execute(
    binary: string,
    argv: string[],
    stdin: string,
    signal: AbortSignal,
    trustedIdentity: { sessionID: string; toolCallID: string },
  ): Promise<CommandOutput>
}

function validateOpenCodeID(value: string, label: string): string {
  if (!value || value.includes("\0") || new TextEncoder().encode(value).byteLength > MAX_OPENCODE_ID_BYTES) {
    throw new Error(`OpenCode ${label} is not a valid canonical identifier`)
  }
  return value
}

export function openCodeToolCallIdentity(context: ToolContext): { sessionID: string; callID: string } {
  const callID = (context as ToolContext & { callID?: unknown }).callID
  if (typeof callID !== "string") throw new Error("Agent Hub tool invocation lacks a trusted OpenCode tool call ID")
  return {
    sessionID: validateOpenCodeID(context.sessionID, "session ID"),
    callID: validateOpenCodeID(callID, "tool call ID"),
  }
}

export async function readBounded(
  stream: ReadableStream<Uint8Array>,
  maximum: number,
  label: string,
  terminate: () => void,
): Promise<string> {
  const reader = stream.getReader()
  const chunks: Uint8Array[] = []
  let length = 0
  let overflow: Error | undefined
  try {
    while (true) {
      const { done, value } = await reader.read()
      if (done) break
      // Drain shutdown output without retaining it or blocking host cleanup on a full pipe.
      if (overflow) continue
      length += value.byteLength
      if (length > maximum) {
        overflow = new Error(`Agent Hub ${label} exceeded ${maximum} bytes`)
        terminate()
        continue
      }
      chunks.push(value)
    }
  } finally {
    reader.releaseLock()
  }
  if (overflow) throw overflow
  const bytes = new Uint8Array(length)
  let offset = 0
  for (const chunk of chunks) {
    bytes.set(chunk, offset)
    offset += chunk.byteLength
  }
  return new TextDecoder("utf-8", { fatal: true }).decode(bytes)
}

export const defaultRuntime: AgentHubRuntime = {
  env: process.env,
  async assertExecutable(path) {
    try {
      const metadata = await stat(path)
      if (!metadata.isFile()) throw new Error("not a file")
      await access(path, constants.X_OK)
    } catch (error) {
      throw new Error(`Agent Hub binary is not an executable file: ${path}`, { cause: error })
    }
  },
  async execute(binary, argv, stdin, signal, trustedIdentity) {
    if (signal.aborted) throw new Error("Agent Hub run was cancelled before launch")
    const child = Bun.spawn([binary, ...argv], {
      stdin: new TextEncoder().encode(stdin),
      stdout: "pipe",
      stderr: "pipe",
      env: {
        ...process.env,
        AGENT_HUB_TRUSTED_SESSION_ID: trustedIdentity.sessionID,
        AGENT_HUB_TRUSTED_TOOL_CALL_ID: trustedIdentity.toolCallID,
      },
    })
    const terminate = () => child.kill("SIGTERM")
    const onAbort = () => terminate()
    signal.addEventListener("abort", onAbort, { once: true })
    const pending = [
      readBounded(child.stdout, MAX_STDOUT_BYTES, "stdout", terminate),
      readBounded(child.stderr, MAX_STDERR_BYTES, "stderr", terminate),
      child.exited,
    ] as const
    try {
      if (signal.aborted) terminate()
      const [stdout, stderr, exitCode] = await Promise.all(pending)
      return { stdout, stderr, exitCode }
    } catch (error) {
      terminate()
      // The host owns sandbox cleanup; do not return while its process is still running.
      await Promise.allSettled(pending)
      throw error
    } finally {
      signal.removeEventListener("abort", onAbort)
    }
  },
}

export function canonicalIntent(input: AgentHubRunRequest): AgentHubRunIntentV1 {
  const intent = {
    schema: input.schema,
    environment: input.environment,
    image_family: input.image_family,
    network_class: input.network_class,
    network_resources: input.network_resources,
    tool_approval: input.tool_approval,
    overlays: input.overlays,
    timeout_secs: input.timeout_secs,
    prompt: input.prompt,
    workspaces: [input.workspaces[0]!, ...input.workspaces.slice(1)],
    context_roots: input.context_roots,
    primary_workspace: input.primary_workspace,
  } as unknown as AgentHubRunIntentV1
  if (input.workspace_export_root) intent.workspace_export_root = input.workspace_export_root
  return intent
}

function parseOutput(stdout: string, stderr: string, exitCode: number): string {
  let value: unknown
  try {
    value = JSON.parse(stdout)
  } catch (error) {
    throw new Error("Agent Hub returned invalid JSON", { cause: error })
  }
  const output = hostRunOutputSchema.parse(value)
  if (
    output.approval_receipt.status === "issued" &&
    output.approval_receipt.receipt?.invocation_id !== output.invocation_id
  ) {
    throw new Error("Agent Hub approval receipt does not match the output invocation")
  }
  if (exitCode === 0) {
    if (output.error != null || output.approval_receipt.status !== "issued") {
      throw new Error("Agent Hub returned success without an issued approval receipt")
    }
    if (output.approval_receipt.receipt?.decision !== "allow") {
      throw new Error("Agent Hub returned success without an allow decision")
    }
    const result = localResultV2Schema.parse(output.local_result_v2)
    if (
      result.status !== "succeeded" ||
      result.launch_state !== "terminal" ||
      result.exit_code !== 0 ||
      result.error !== null ||
      (result.permission_outcome !== "no_denials" && result.permission_outcome !== "not_applicable") ||
      result.process_outcome !== "succeeded" ||
      result.agent_outcome !== "succeeded" ||
      result.execution_outcome !== "succeeded" ||
      result.result_outcome !== "succeeded" ||
      result.cleanup_outcome !== "succeeded" ||
      result.retry_eligible ||
      result.retry_class !== null ||
      !result.agent_final?.complete
    ) {
      throw new Error("Agent Hub returned a failed, incomplete, or unclean run with a zero exit status")
    }
  } else {
    const result = output.local_result_v2
    if (result.status === "succeeded" || (
      result.execution_outcome === "succeeded" && result.result_outcome === "succeeded" && result.cleanup_outcome === "succeeded"
    )) {
      throw new Error("Agent Hub returned a success result contradicting its non-zero exit status")
    }
    if (!output.error && output.approval_receipt.receipt?.decision !== "deny") {
      const terminal = localResultV2Schema.safeParse(result)
      const terminalFailure = output.approval_receipt.receipt?.decision === "allow" && terminal.success
        && terminal.data.launch_state === "terminal"
        && ["failed", "cancelled", "timed_out", "cleanup_failed", "cleaned"].includes(terminal.data.status)
        && (["failed", "cancelled", "timed_out"].includes(terminal.data.execution_outcome)
          || terminal.data.result_outcome === "failed" || terminal.data.cleanup_outcome === "failed")
      if (!terminalFailure) {
        throw new Error(`Agent Hub exited ${exitCode} without a structured failure: ${stderr.trim() || "no diagnostic"}`)
      }
    }
  }
  return JSON.stringify(output)
}

export function createAgentHubRunTool(runtime: AgentHubRuntime = defaultRuntime) {
  return tool({
    description: "Run one capability-scoped sandbox through Agent Hub's trusted host policy and lifecycle implementation.",
    args: agentHubRunArgs,
    async execute(input, context: ToolContext) {
      const trustedIdentity = openCodeToolCallIdentity(context)
      const intent = canonicalIntent(requestSchema.parse(input))
      const binary = runtime.env.AGENT_HUB_BIN
      if (!binary || !isAbsolute(binary)) {
        throw new Error("AGENT_HUB_BIN must name the fixed absolute Agent Hub executable")
      }
      await runtime.assertExecutable(binary)
      context.metadata({ title: `Agent Hub: ${intent.environment}`, metadata: { intent } })
      const output = await runtime.execute(
        binary,
        ["local", "run", "--request-stdin"],
        JSON.stringify(intent),
        context.abort,
        { sessionID: trustedIdentity.sessionID, toolCallID: trustedIdentity.callID },
      )
      return parseOutput(output.stdout, output.stderr, output.exitCode)
    },
  })
}
