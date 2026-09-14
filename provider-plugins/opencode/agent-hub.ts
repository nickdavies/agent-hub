/**
 * Agent Hub opencode plugin.
 *
 * Built against the OpenCode 1.18.26 plugin SDK to route supervised OpenCode
 * 1.18.18 capability executions through agent-hub-gateway.
 *
 * Environment variables (set in the shell that launches opencode):
 *   AGENT_HUB_GATEWAY   path to the gateway binary (default: "agent-hub-gateway")
 *   AGENT_HUB_APPROVAL_MODE  attended_socket for explicit sandbox mode
 *   AGENT_HUB_ATTENDED_APPROVAL_SOCKET  immutable per-run relay socket
 *   AGENT_HUB_ATTENDED_APPROVAL_BINDING  immutable attended run binding JSON
 *   AGENT_HUB_ATTENDED_RUN_DEADLINE  attended run RFC 3339 deadline
 *   AGENT_HUB_SERVER    server URL (default: "http://localhost:8080")
 *   AGENT_HUB_TOKEN     bearer token for server auth (default: none)
 *   AGENT_HUB_CONFIG    path to agent-hub-permissions.json rule config (optional)
 *
 * Installation: add to opencode.json:
 *   { "plugin": ["file:///path/to/provider-plugins/opencode/index.ts"] }
 */

import type { Hooks, Plugin, PluginInput } from "@opencode-ai/plugin"
import type { Event as EventV2 } from "@opencode-ai/sdk/v2"
import { OpencodeClient as OpencodeClientV2 } from "@opencode-ai/sdk/v2"
import path from "path"
import fs from "fs"
import { createHash, randomUUID } from "crypto"
import { spawn, spawnSync, type ChildProcess } from "child_process"
import type {
  OpenCodeHookInput,
  OpenCodeHookOutput,
  OpenCodeTool,
  QuestionGatewayOutput,
  QuestionProxyRequest,
  SessionStatus,
  StatusReport,
} from "./generated/gateway-types"
import { approvalTransport, gatewayApprovalInvocation, gatewaySupervisionInvocation } from "./approval-transport"
import { isStructuredOutputTool, normalizeToolExecution } from "./permission-normalization"
import { createAgentHubRunTool, openCodeToolCallIdentity, requestSchema } from "./agent-hub-run"

const APPROVAL_TRANSPORT = approvalTransport(process.env)
const SESSION_ALLOW_RULES = [{ permission: "*", pattern: "*", action: "allow" as const }]
// Process exit must await every instance, not just the first signal listener's launches.
const processShutdowns = new Set<() => Promise<void>>()
let processShutdown: Promise<void> | undefined

function exitAfterShutdown(code: number): Promise<void> {
  processShutdown ??= (async () => {
    const results = await Promise.allSettled([...processShutdowns].map((shutdown) => shutdown()))
    const failures = results.filter((result) => result.status === "rejected")
    for (const failure of failures) console.error("Agent Hub shutdown failed:", failure.reason)
    process.exit(failures.length > 0 ? 1 : code)
  })()
  return processShutdown
}

type ExecutionStatus = "routing" | "authorized" | "completed" | "denied" | "transcript_only"
interface ExecutionRecord {
  call_id: string
  session_id: string
  tool_name: string
  runtime_tool_name: string
  raw_args_digest: string
  canonical_match_digests: string[]
  resolved_paths: string[]
  status: ExecutionStatus
  host_launch_started?: boolean
  requests: { requestID: string; canonicalDigest: string }[]
}

interface PluginInstanceState {
  readinessRecorded: boolean
  configInitialized: boolean
  permissionOwnedSessions: Set<string>
  executionRecords: Map<string, ExecutionRecord>
  procs: Map<string, Set<ChildProcess>>
  questionGatewayProcs: Map<string, ChildProcess>
  locallyAnsweredQuestions: Set<string>
  statusTimers: Map<string, NodeJS.Timeout>
  registeredSessions: Set<string>
  pendingSessionRegistrations: Set<string>
  exitHandled: boolean
  titles: Map<string, string>
  lastLogTime: number
}

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical)
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value as Record<string, unknown>)
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([key, nested]) => [key, canonical(nested)]))
  }
  return value
}

function digest(domain: string, value: unknown): string {
  return createHash("sha256").update(`${domain}\0`).update(JSON.stringify(canonical(value))).digest("hex")
}

function initializeRunEvidence(state: PluginInstanceState) {
  state.executionRecords.clear()
}

function recordExecution(state: PluginInstanceState, record: ExecutionRecord) {
  state.executionRecords.set(record.call_id, record)
}

function recordPermissionDenied(_reason?: string) {}

// ---------------------------------------------------------------------------
// Minimal structured logger — writes to the same dev.log opencode uses.
// Format matches opencode's Log.create output: LEVEL  ISO +Xms key=value msg
// ---------------------------------------------------------------------------

const logfile = path.join(
  process.env.XDG_DATA_HOME ?? path.join(process.env.HOME ?? "~", ".local", "share"),
  "opencode",
  "log",
  "dev.log",
)

type LogLevel = "INFO" | "WARN" | "ERROR"

function log(state: PluginInstanceState, level: LogLevel, msg: string, extra?: Record<string, string>) {
  const now = Date.now()
  const diff = now - state.lastLogTime
  state.lastLogTime = now
  const ts = new Date(now).toISOString().split(".")[0]
  const kv = Object.entries({ service: "agent-hub-plugin", ...extra })
    .map(([k, v]) => `${k}=${v}`)
    .join(" ")
  const pad = level === "INFO" || level === "WARN" ? " " : ""
  fs.appendFileSync(logfile, `${level}${pad} ${ts} +${diff}ms ${kv} ${msg}\n`)
}

// ---------------------------------------------------------------------------
// Gateway subprocess tracking — kill on session abort
// ---------------------------------------------------------------------------

// Per-question gateway process tracking — keyed by opencode question requestId.
// Used to detect when the OpenCode TUI answers a question locally (i.e. before
// the agent-hub gateway gets a chance to answer it), so we can cancel the
// in-flight gateway and clean up the pending question on the server.
// Set of requestIds that were answered locally in the OpenCode TUI before the
// gateway had a chance to answer.  When the gateway process exits (due to
// SIGTERM from our kill) with code 1, the callGatewayQuestion callback checks
// this set and skips the question.reject call to avoid a double-resolve error.
const GATEWAY_OUTPUT_LIMIT_BYTES = 1024 * 1024
const DENIAL_LIMIT_BYTES = 4096
const GATEWAY_OPERATION_TIMEOUT_MS = 86_410_000
const SHORT_GATEWAY_OPERATION_TIMEOUT_MS = 15_000

type GatewayFailureCategory = "spawn" | "read" | "write" | "timeout" | "signal" | "nonzero" | "malformed" | "oversized" | "schema"
type GatewayProcessResult =
  | { ok: true; stdout: string; stderr: string }
  | { ok: false; category: GatewayFailureCategory; detail: string }

function boundedUtf8(value: string, limit = DENIAL_LIMIT_BYTES): string {
  if (Buffer.byteLength(value) <= limit) return value
  let result = ""
  let bytes = 0
  for (const character of value) {
    const size = Buffer.byteLength(character)
    if (bytes + size > limit) break
    result += character
    bytes += size
  }
  return result
}

function pipelineDenial(category: GatewayFailureCategory, detail: string): string {
  return boundedUtf8(`Agent Hub denied tool execution because the approval pipeline failed (${category}): ${detail || "no detail available"}`)
}

export function runGatewayProcess(options: {
  bin: string
  args: string[]
  input: string
  timeoutMs: number
  env?: NodeJS.ProcessEnv
  onSpawn?: (proc: ChildProcess) => void
}): Promise<GatewayProcessResult> {
  return new Promise((resolve) => {
    let proc: ChildProcess
    try {
      proc = spawn(options.bin, options.args, { stdio: ["pipe", "pipe", "pipe"], env: options.env })
    } catch (error) {
      resolve({ ok: false, category: "spawn", detail: boundedUtf8(String(error)) })
      return
    }

    options.onSpawn?.(proc)
    let settled = false
    let pendingFailure: Exclude<GatewayProcessResult, { ok: true }> | undefined
    let stdoutBytes = 0
    let stderrBytes = 0
    const stdout: Buffer[] = []
    const stderr: Buffer[] = []

    const finish = (result: GatewayProcessResult) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      resolve(result.ok ? result : { ...result, detail: boundedUtf8(result.detail) })
    }
    const failAndReap = (category: GatewayFailureCategory, detail: string) => {
      if (pendingFailure || settled) return
      pendingFailure = { ok: false, category, detail }
      try { proc.kill("SIGKILL") } catch (error) {
        finish({ ok: false, category, detail: `${detail}; kill failed: ${String(error)}` })
      }
    }
    const capture = (stream: "stdout" | "stderr", chunk: Buffer | string) => {
      const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk)
      const current = stream === "stdout" ? stdoutBytes : stderrBytes
      if (current + bytes.byteLength > GATEWAY_OUTPUT_LIMIT_BYTES) {
        failAndReap("oversized", `${stream} exceeded ${GATEWAY_OUTPUT_LIMIT_BYTES} bytes`)
        return
      }
      if (stream === "stdout") {
        stdoutBytes += bytes.byteLength
        stdout.push(bytes)
      } else {
        stderrBytes += bytes.byteLength
        stderr.push(bytes)
      }
    }

    const timer = setTimeout(() => {
      failAndReap("timeout", `gateway exceeded ${options.timeoutMs} ms`)
    }, options.timeoutMs)
    proc.stdout?.on("data", (chunk: Buffer | string) => capture("stdout", chunk))
    proc.stderr?.on("data", (chunk: Buffer | string) => capture("stderr", chunk))
    proc.stdout?.on("error", (error: Error) => failAndReap("read", `stdout: ${error.message}`))
    proc.stderr?.on("error", (error: Error) => failAndReap("read", `stderr: ${error.message}`))
    proc.stdin?.on("error", (error: Error) => failAndReap("write", error.message))
    proc.on("error", (error: Error) => finish({ ok: false, category: "spawn", detail: error.message }))
    proc.on("close", (code: number | null, signal: NodeJS.Signals | null) => {
      if (pendingFailure) return finish(pendingFailure)
      const stderrText = Buffer.concat(stderr).toString("utf8").trim()
      if (signal) return finish({ ok: false, category: "signal", detail: `gateway terminated by ${signal}${stderrText ? `: ${stderrText}` : ""}` })
      if (code !== 0) return finish({ ok: false, category: "nonzero", detail: `gateway exited ${String(code)}${stderrText ? `: ${stderrText}` : ""}` })
      finish({
        ok: true,
        stdout: Buffer.concat(stdout).toString("utf8"),
        stderr: Buffer.concat(stderr).toString("utf8"),
      })
    })
    if (!proc.stdin) {
      failAndReap("write", "gateway stdin was unavailable")
      return
    }
    proc.stdin.end(options.input, (error?: Error | null) => {
      if (error) failAndReap("write", error.message)
    })
  })
}

function track(state: PluginInstanceState, sid: string, proc: ChildProcess) {
  let set = state.procs.get(sid)
  if (!set) {
    set = new Set()
    state.procs.set(sid, set)
  }
  set.add(proc)
}

function untrack(state: PluginInstanceState, sid: string, proc: ChildProcess) {
  const set = state.procs.get(sid)
  if (!set) return
  set.delete(proc)
  if (set.size === 0) state.procs.delete(sid)
}

function killGateways(state: PluginInstanceState, sid: string) {
  const set = state.procs.get(sid)
  if (!set || set.size === 0) return
  log(state, "INFO", "killing gateway subprocesses on abort", { sid, count: String(set.size) })
  for (const proc of set) {
    try { proc.kill("SIGTERM") } catch (err: unknown) {
      log(state, "WARN", "failed to kill gateway subprocess", { sid, err: String(err) })
    }
  }
}

// ---------------------------------------------------------------------------
// Status reporting — fire-and-forget via gateway status-report subcommand
// ---------------------------------------------------------------------------

const DEBOUNCE_MS = 2000

// Track sessions we've already registered on the server so the first
// tool execution for a new session triggers an immediate "active" report.
// On process exit, report all tracked sessions as ended so the dashboard
// doesn't leave stale "active" / "idle" entries after opencode shuts down.
// Uses spawnSync so the reports are delivered before the process terminates —
// async spawn() in an exit handler is unreliable because stdin writes may not
// flush before the parent dies.
function onExit(state: PluginInstanceState) {
  if (state.exitHandled) return
  state.exitHandled = true
  for (const sid of state.registeredSessions) {
    // Cancel any pending debounced reports — we want "ended" immediately.
    const pending = state.statusTimers.get(sid)
    if (pending) clearTimeout(pending)
    state.statusTimers.delete(sid)
    spawnStatusReportSync(state, sid, "ended")
  }
  state.registeredSessions.clear()
}

function reportStatus(
  state: PluginInstanceState,
  sid: string,
  status: SessionStatus,
  opts?: { waitingReason?: string; displayName?: string },
) {
  // "ended" should fire immediately — no debounce
  if (status === "ended") {
    const existing = state.statusTimers.get(sid)
    if (existing) clearTimeout(existing)
    state.statusTimers.delete(sid)
    spawnStatusReport(state, sid, status, opts)
    return
  }

  const existing = state.statusTimers.get(sid)
  if (existing) clearTimeout(existing)

  const timer = setTimeout(() => {
    state.statusTimers.delete(sid)
    spawnStatusReport(state, sid, status, opts)
  }, DEBOUNCE_MS)

  state.statusTimers.set(sid, timer)
}

function spawnStatusReport(
  state: PluginInstanceState,
  sid: string,
  status: SessionStatus,
  opts?: { waitingReason?: string; displayName?: string },
) {
  if (APPROVAL_TRANSPORT.kind === "attended_socket") return
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const server = process.env.AGENT_HUB_SERVER ?? "http://127.0.0.1:8080"
  const token = process.env.AGENT_HUB_TOKEN

  const report: StatusReport = {
    session_id: sid,
    cwd: process.cwd(),
    status,
    waiting_reason: opts?.waitingReason ?? null,
    display_name: opts?.displayName ?? null,
    editor_type: "opencode",
  }
  const payload = JSON.stringify(report)

  log(state, "INFO", "spawnStatusReport", { sid, status })

  void runGatewayProcess({
    bin,
    args: ["status-report", "--server", server, ...(token ? ["--token", token] : [])],
    input: payload,
    timeoutMs: SHORT_GATEWAY_OPERATION_TIMEOUT_MS,
  }).then((result) => {
    state.pendingSessionRegistrations.delete(sid)
    if (!result.ok) {
      state.registeredSessions.delete(sid)
      log(state, "WARN", "status report failed; later activity will retry", {
        sid,
        category: result.category,
        detail: result.detail,
      })
      return
    }
    if (status === "active") state.registeredSessions.add(sid)
    if (result.stderr) log(state, "INFO", "status-report stderr", { output: boundedUtf8(result.stderr.trim()) })
  })
}

/** Synchronous variant used only during process exit to ensure the report
 *  is delivered before the Node.js process terminates. */
function spawnStatusReportSync(
  state: PluginInstanceState,
  sid: string,
  status: SessionStatus,
  opts?: { waitingReason?: string; displayName?: string },
) {
  if (APPROVAL_TRANSPORT.kind === "attended_socket") return
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const server = process.env.AGENT_HUB_SERVER ?? "http://127.0.0.1:8080"
  const token = process.env.AGENT_HUB_TOKEN

  const report: StatusReport = {
    session_id: sid,
    cwd: process.cwd(),
    status,
    waiting_reason: opts?.waitingReason ?? null,
    display_name: opts?.displayName ?? null,
    editor_type: "opencode",
  }
  const payload = JSON.stringify(report)

  log(state, "INFO", "spawnStatusReportSync", { sid, status })

  try {
    const result = spawnSync(
      bin,
      ["status-report", "--server", server, ...(token ? ["--token", token] : [])],
      {
        input: payload,
        stdio: ["pipe", "ignore", "pipe"],
        timeout: 5000,
        maxBuffer: GATEWAY_OUTPUT_LIMIT_BYTES,
      },
    )
    const stderr = boundedUtf8(result.stderr?.toString().trim() ?? "")
    if (result.error || result.status !== 0 || result.signal) {
      log(state, "WARN", "sync status report failed", {
        status: String(result.status),
        signal: String(result.signal),
        detail: boundedUtf8(result.error?.message || stderr || "unknown failure"),
      })
    } else if (stderr) {
      log(state, "INFO", "sync status-report stderr", { output: stderr })
    }
  } catch (err: unknown) {
    log(state, "WARN", "sync status report failed", { err: boundedUtf8(String(err)) })
  }
}

// ---------------------------------------------------------------------------
// Single-pattern gateway call
// ---------------------------------------------------------------------------

type GatewayResult = OpenCodeHookOutput
class ExplicitDenial extends Error {}

export const APPROVAL_TIMEOUT_MESSAGE = "Agent Hub denied this request because the approval request timed out before an operator decision."
export const APPROVAL_PIPELINE_ERROR_MESSAGE = "Agent Hub denied this request because the approval pipeline failed."

/** Validate that a parsed JSON value conforms to the OpenCodeHookOutput shape.
 *  The gateway is an external subprocess — we cannot trust its stdout blindly. */
function isValidGatewayOutput(v: unknown): v is OpenCodeHookOutput {
  if (typeof v !== "object" || v === null) return false
  const obj = v as Record<string, unknown>
  if (typeof obj.allowed !== "boolean") return false
  if (obj.reason !== undefined && obj.reason !== null && typeof obj.reason !== "string") return false
  return true
}

type ExecutionBinding = Readonly<{ callID: string; requestID: string; rawDigest: string; canonicalDigest: string }>

async function callGateway(state: PluginInstanceState, sid: string, payload: OpenCodeHookInput, execution: ExecutionBinding): Promise<GatewayResult> {
  return callGatewayOnce(state, sid, payload, execution)
}

async function callGatewayOnce(state: PluginInstanceState, sid: string, payload: OpenCodeHookInput, execution: ExecutionBinding): Promise<GatewayResult> {
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const config = process.env.AGENT_HUB_CONFIG

  const invocation = gatewayApprovalInvocation(APPROVAL_TRANSPORT, process.env, execution)
  const args = invocation.args
  if (config) args.push("--config", config)

  log(state, "INFO", "callGateway", { bin, config: config ?? "(none)", transport: APPROVAL_TRANSPORT.kind })

  let childProcess: ChildProcess | undefined
  const result = await runGatewayProcess({
    bin,
    args,
    env: invocation.env as NodeJS.ProcessEnv,
    input: JSON.stringify(payload),
    timeoutMs: GATEWAY_OPERATION_TIMEOUT_MS,
    onSpawn: (spawned) => {
      childProcess = spawned
      track(state, sid, spawned)
    },
  })
  if (childProcess) untrack(state, sid, childProcess)
  if (!result.ok) {
    log(state, "ERROR", "gateway approval failed", { category: result.category, detail: result.detail })
    return { allowed: false, reason: pipelineDenial(result.category, result.detail) }
  }
  if (result.stderr) log(state, "INFO", "gateway stderr", { output: boundedUtf8(result.stderr.trim()) })
  let parsed: unknown
  try {
    parsed = JSON.parse(result.stdout.trim())
  } catch (error) {
    const detail = boundedUtf8(`${String(error)}; stdout=${result.stdout.trim()}`)
    log(state, "ERROR", "gateway invalid response", { detail })
    return { allowed: false, reason: pipelineDenial("malformed", detail) }
  }
  if (!isValidGatewayOutput(parsed)) {
    const detail = boundedUtf8(`stdout=${result.stdout.trim()}`)
    log(state, "ERROR", "gateway output failed validation", { detail })
    return { allowed: false, reason: pipelineDenial("schema", detail) }
  }
  return parsed.allowed
    ? parsed
    : { allowed: false, reason: boundedUtf8(parsed.reason ?? "Agent Hub denied tool execution") }
}

async function reportSupervision(state: PluginInstanceState, transition: Record<string, unknown>): Promise<void> {
  if (APPROVAL_TRANSPORT.kind !== "attended_socket") return
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const invocation = gatewaySupervisionInvocation(APPROVAL_TRANSPORT, process.env)
  const result = await runGatewayProcess({
    bin,
    args: invocation.args,
    env: invocation.env as NodeJS.ProcessEnv,
    timeoutMs: SHORT_GATEWAY_OPERATION_TIMEOUT_MS,
    input: JSON.stringify({
      version: "attended_gateway_transition_v1",
      ...JSON.parse(APPROVAL_TRANSPORT.binding),
      ...transition,
    }),
  })
  if (!result.ok) throw new Error(pipelineDenial(result.category, result.detail))
}

function executionTransition(record: ExecutionRecord, request: ExecutionRecord["requests"][number], transition: string) {
  return {
    transition,
    request_id: request.requestID,
    call_id: record.call_id,
    session_id: record.session_id,
    tool_name: record.tool_name,
    raw_args_digest: record.raw_args_digest,
    canonical_args_digest: request.canonicalDigest,
    plugin_nonce: null,
  }
}

// ---------------------------------------------------------------------------
// Question proxy — blocking gateway call for question.asked events
// ---------------------------------------------------------------------------

/** Result from the gateway `question` subcommand. */
type QuestionGatewayResult =
  | { answers: string[][] }
  | { outcome: "rejected" | "timeout" }
  | { outcome: "pipeline-error"; category: GatewayFailureCategory; detail: string }

function isValidQuestionAnswers(value: unknown): value is string[][] {
  return Array.isArray(value) && value.every(
    (answer) => Array.isArray(answer) && answer.every((item) => typeof item === "string"),
  )
}

async function callGatewayQuestion(
  state: PluginInstanceState,
  sid: string,
  payload: QuestionProxyRequest,
): Promise<QuestionGatewayResult> {
  if (APPROVAL_TRANSPORT.kind === "attended_socket") {
    log(state, "ERROR", "question gateway unavailable for approval transport", {
      requestId: payload.question_request_id,
      transport: APPROVAL_TRANSPORT.kind,
    })
    return { outcome: "pipeline-error", category: "schema", detail: "questions are unavailable in attended_socket mode" }
  }
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const server = process.env.AGENT_HUB_SERVER
  const token = process.env.AGENT_HUB_TOKEN

  const args: string[] = ["question", "--opencode"]
  args.push("--server", server ?? "http://127.0.0.1:8080")
  if (token) args.push("--token", token)

  log(state, "INFO", "callGatewayQuestion", { bin, requestId: payload.question_request_id })

  const requestId = payload.question_request_id

  const result = await runGatewayProcess({
    bin,
    args,
    input: JSON.stringify(payload),
    timeoutMs: GATEWAY_OPERATION_TIMEOUT_MS,
    onSpawn: (proc) => {
      track(state, sid, proc)
      state.questionGatewayProcs.set(requestId, proc)
    },
  })
  const childProcess = state.questionGatewayProcs.get(requestId)
  if (childProcess) untrack(state, sid, childProcess)
  state.questionGatewayProcs.delete(requestId)
  if (!result.ok) {
    const exit = /^gateway exited (\d+)/.exec(result.detail)?.[1]
    if (result.category === "nonzero" && exit === "1") return { outcome: "rejected" }
    if (result.category === "nonzero" && exit === "3") return { outcome: "timeout" }
    return { outcome: "pipeline-error", category: result.category, detail: result.detail }
  }
  let parsed: QuestionGatewayOutput
  try {
    parsed = JSON.parse(result.stdout.trim()) as QuestionGatewayOutput
  } catch (error) {
    return { outcome: "pipeline-error", category: "malformed", detail: boundedUtf8(`${String(error)}; stdout=${result.stdout.trim()}`) }
  }
  if (!isValidQuestionAnswers(parsed.answers) || parsed.answers.length !== payload.questions.length) {
    return { outcome: "pipeline-error", category: "schema", detail: boundedUtf8(`stdout=${result.stdout.trim()}`) }
  }
  return { answers: parsed.answers }
}

// ---------------------------------------------------------------------------
// Build a gateway payload for a single tool_input value
// ---------------------------------------------------------------------------

export function makePayload(
  sessionId: string,
  tool: OpenCodeTool,
  rawArgs: Record<string, unknown>,
  matchInputOrDirectory: Record<string, unknown> | string,
  directoryOrWorktree: string,
  _worktreeOrTitle?: string,
  title?: string,
): OpenCodeHookInput {
  const legacyCall = typeof matchInputOrDirectory === "string"
  const directory = legacyCall ? matchInputOrDirectory : directoryOrWorktree
  const matchInput = legacyCall ? undefined : matchInputOrDirectory
  const sessionTitle = legacyCall ? _worktreeOrTitle : title
  return {
    session_id: sessionId,
    tool_name: tool,
    tool_input: rawArgs,
    ...(matchInput ? { tool_match_input: matchInput } : {}),
    cwd: directory,
    hook_event_name: "tool.execute.before",
    session_title: sessionTitle ?? null,
  }
}

// Resolve a path value against base, but only if it is relative.
function resolvePath(value: string, base: string): string {
  return path.isAbsolute(value) ? value : path.resolve(base, value)
}

export function makeReadToolInput(pattern: string, worktree: string): Record<string, string> {
  return { path: resolvePath(pattern, worktree) }
}

export function makeGlobToolInput(
  metadata: Record<string, unknown>,
  directory: string,
): Record<string, string> {
  const toolInput: Record<string, string> = {}
  if (typeof metadata.pattern === "string") toolInput.pattern = metadata.pattern
  if (typeof metadata.path === "string") {
    toolInput.path = resolvePath(metadata.path, directory)
  } else if (typeof metadata.directory === "string") {
    toolInput.path = resolvePath(metadata.directory, directory)
  } else {
    toolInput.path = directory
  }
  return toolInput
}

function validatePluginDirectory(directory: string) {
  if (directory.length === 0) throw new Error("Plugin directory must not be empty")
  if (!path.isAbsolute(directory)) throw new Error("Plugin directory must be absolute")
}
// ---------------------------------------------------------------------------
// Session title cache — avoids repeated SDK calls for the same session.
// Stores the LLM-generated title once it's available; null means we haven't
// fetched a non-default title yet and should retry next time.
// ---------------------------------------------------------------------------

export const DEFAULT_TITLE_RE = /^(New session - |Child session - )\d{4}-\d{2}-\d{2}T/

/** The plugin receives a v1 SDK client. Extract the type from PluginInput
 *  so we track upstream changes automatically. */
type PluginClient = PluginInput["client"]

async function fetchTitle(
  state: PluginInstanceState,
  client: PluginClient,
  sid: string,
  dir: string,
): Promise<string | undefined> {
  const cached = state.titles.get(sid)
  if (cached) {
    log(state, "INFO", "fetchTitle cache hit", { sid, title: cached })
    return cached
  }

  log(state, "INFO", "fetchTitle calling session.get", { sid, directory: dir })
  try {
    // The plugin client is v1 — uses { path: { id }, query: { directory } }
    // and URL template /session/{id}.
    const res = await client.session.get({
      path: { id: sid },
      query: { directory: dir },
    })
    log(state, "INFO", "fetchTitle raw response", { sid, keys: Object.keys(res).join(","), hasData: String(!!res.data) })
    const title = res.data?.title
    log(state, "INFO", "fetchTitle got response", { sid, title: title ?? "(none)", hasData: String(!!res.data) })
    if (title && !DEFAULT_TITLE_RE.test(title)) {
      state.titles.set(sid, title)
      log(state, "INFO", "fetchTitle cached title", { sid, title })
      return title
    }
    log(state, "INFO", "fetchTitle title is default/empty, skipping cache", { sid })
    return undefined
  } catch (err) {
    log(state, "WARN", "failed to fetch session title", { sid, err: String(err) })
    return undefined
  }
}

// ---------------------------------------------------------------------------
// Plugin export
// ---------------------------------------------------------------------------

const id = "agent-hub"

export async function createPluginInstance(
  input: PluginInput,
  options: { registerProcessHandlers?: boolean } = {},
): Promise<Hooks> {
  if (processShutdown) throw new Error("Agent Hub plugin cannot initialize during process shutdown")
  const client = input.client
  const { directory, worktree } = input
  validatePluginDirectory(directory)
  const inProcessClient = (client as any)?._client
  if (!inProcessClient) {
    throw new Error("OpenCode runtime does not expose the in-process plugin client")
  }
  const clientV2 = new OpencodeClientV2({ client: inProcessClient })
  const state: PluginInstanceState = {
    readinessRecorded: false,
    configInitialized: false,
    permissionOwnedSessions: new Set(),
    executionRecords: new Map(),
    procs: new Map(),
    questionGatewayProcs: new Map(),
    locallyAnsweredQuestions: new Set(),
    statusTimers: new Map(),
    registeredSessions: new Set(),
    pendingSessionRegistrations: new Set(),
    exitHandled: false,
    titles: new Map(),
    lastLogTime: Date.now(),
  }
  initializeRunEvidence(state)

  const shutdown = new AbortController()
  const hostLaunches = new Set<Promise<unknown>>()
  let disposal: Promise<void> | undefined
  function dispose(): Promise<void> {
    disposal ??= (async () => {
      shutdown.abort()
      // Tool callers retain their outcomes; settlement alone does not attest sandbox cleanup success.
      await Promise.allSettled([...hostLaunches])
      if (options.registerProcessHandlers !== false) {
        process.off("exit", exit)
        process.off("SIGINT", sigint)
        process.off("SIGTERM", sigterm)
        processShutdowns.delete(stopForExit)
      }
      for (const timer of state.statusTimers.values()) clearTimeout(timer)
      state.statusTimers.clear()
      for (const sid of state.procs.keys()) killGateways(state, sid)
    })()
    return disposal
  }
  const stopForExit = async () => {
    await dispose()
    onExit(state)
  }
  const exit = () => onExit(state)
  const sigint = () => { void exitAfterShutdown(130) }
  const sigterm = () => { void exitAfterShutdown(143) }
  if (options.registerProcessHandlers !== false) {
    processShutdowns.add(stopForExit)
    process.on("exit", exit)
    process.on("SIGINT", sigint)
    process.on("SIGTERM", sigterm)
  }

  async function routeTool(
    sid: string,
    callID: string,
    toolName: string,
    args: Record<string, unknown>,
  ) {
    if (shutdown.signal.aborted) throw new ExplicitDenial("Agent Hub denied tool execution during plugin shutdown")
    if (!state.configInitialized) {
      const reason = "OpenCode executed a tool before the Agent Hub config hook took permission ownership"
      recordPermissionDenied(reason)
      throw new ExplicitDenial(reason)
    }
    if (!state.permissionOwnedSessions.has(sid)) {
      const reason = `Agent Hub denied tool execution because session '${sid}' has not transferred permission ownership to the approval pipeline`
      recordPermissionDenied(reason)
      throw new ExplicitDenial(boundedUtf8(reason))
    }
    if (APPROVAL_TRANSPORT.kind === "attended_socket" && !state.readinessRecorded) {
      throw new ExplicitDenial("Agent Hub denied tool execution because host supervision was not ready")
    }
    if (state.executionRecords.has(callID)) {
      const reason = `OpenCode reused tool call ID '${callID}'`
      recordPermissionDenied(reason)
      throw new ExplicitDenial(reason)
    }
    if (!state.registeredSessions.has(sid) && !state.pendingSessionRegistrations.has(sid)) {
      state.pendingSessionRegistrations.add(sid)
      spawnStatusReport(state, sid, "active")
    }

    const title = await fetchTitle(state, client, sid, directory)
    let request
    try {
      request = normalizeToolExecution(toolName, args, directory, worktree)
    } catch (error) {
      const reason = String(error)
      log(state, "ERROR", "tool normalization failed closed", { err: reason })
      recordPermissionDenied(reason)
      throw new ExplicitDenial(pipelineDenial("schema", reason))
    }

    const record: ExecutionRecord = {
      call_id: callID,
      session_id: sid,
      tool_name: request.tool,
      runtime_tool_name: toolName,
      raw_args_digest: digest("agent-hub:opencode-raw-args:v1", request.rawArgs),
      canonical_match_digests: request.matchInputs.map((input) => digest("agent-hub:opencode-match-input:v1", input)),
      resolved_paths: request.resolvedPaths,
      status: "routing",
      requests: [],
    }
    recordExecution(state, record)
    for (const matchInput of request.matchInputs) {
      const requestID = randomUUID()
      const canonicalDigest = digest("agent-hub:opencode-match-input:v1", matchInput)
      const execution = {
        callID,
        requestID,
        rawDigest: record.raw_args_digest,
        canonicalDigest,
      }
      record.requests.push({ requestID, canonicalDigest })
      await reportSupervision(state, executionTransition(record, record.requests.at(-1)!, "created"))
      const result = await callGateway(
        state,
        sid,
        makePayload(sid, request.tool, request.rawArgs, matchInput, directory, worktree, title),
        execution,
      )
      if (!result.allowed) {
        const reason = result.reason ?? "Agent Hub denied tool execution"
        for (const prior of record.requests.slice(0, -1)) {
          await reportSupervision(state, executionTransition(record, prior, "failed"))
        }
        record.status = "denied"
        recordExecution(state, record)
        recordPermissionDenied(reason)
        throw new ExplicitDenial(reason)
      }
    }
    if (request.postPolicyValidationError) {
      for (const routed of record.requests) {
        await reportSupervision(state, executionTransition(record, routed, "failed"))
      }
      record.status = "denied"
      recordExecution(state, record)
      recordPermissionDenied(request.postPolicyValidationError)
      throw new ExplicitDenial(request.postPolicyValidationError)
    }
    record.status = "authorized"
    recordExecution(state, record)
  }

  const hostRunTool = createAgentHubRunTool()
  return {
    ...(APPROVAL_TRANSPORT.kind === "http" ? {
      tool: {
        agent_hub_run: {
          ...hostRunTool,
          async execute(args, context) {
            const { callID } = openCodeToolCallIdentity(context)
            const record = state.executionRecords.get(callID)
            if (shutdown.signal.aborted || !record || record.status !== "authorized" || record.host_launch_started
              || !state.permissionOwnedSessions.has(context.sessionID)
              || record.session_id !== context.sessionID || record.runtime_tool_name !== "agent_hub_run"
              || record.raw_args_digest !== digest("agent-hub:opencode-raw-args:v1", args)) {
              throw new Error("Agent Hub denied host launch because matching gateway authorization is missing or already consumed")
            }
            record.host_launch_started = true
            const launch = hostRunTool.execute(requestSchema.parse(args), {
              ...context,
              abort: AbortSignal.any([context.abort, shutdown.signal]),
            })
            hostLaunches.add(launch)
            try {
              return await launch
            } finally {
              hostLaunches.delete(launch)
            }
          },
        },
      },
    } : {}),
    config: async (config) => {
      const mutable = config as any
      mutable.permission = { "*": "deny" }
      for (const agent of Object.values(mutable.agent ?? {}) as any[]) {
        agent.permission = { "*": "deny" }
      }
      state.configInitialized = true
    },

    "chat.message": async (info) => {
      state.permissionOwnedSessions.delete(info.sessionID)
      try {
        const response = await clientV2.session.update({
          sessionID: info.sessionID,
          directory,
          permission: SESSION_ALLOW_RULES,
        })
        if (response.error) {
          throw new Error(`session.update returned ${JSON.stringify(response.error)}`)
        }
        const permissions = response.data?.permission
        const validRules = Array.isArray(permissions) && permissions.every((rule) =>
          rule !== null
          && typeof rule === "object"
          && !Array.isArray(rule)
          && typeof rule.permission === "string"
          && typeof rule.pattern === "string"
          && (rule.action === "allow" || rule.action === "ask" || rule.action === "deny"))
        // Session updates append rules; OpenCode uses the last matching rule.
        const lastRule = validRules ? permissions.at(-1) : undefined
        if (lastRule?.permission !== "*" || lastRule.pattern !== "*" || lastRule.action !== "allow") {
          throw new Error("session.update response did not prove the requested permission rules")
        }
      } catch (error) {
        const reason = boundedUtf8(`OpenCode session permission ownership failed: ${String(error)}`)
        log(state, "ERROR", "session permission ownership failed", { sid: info.sessionID, detail: reason })
        throw new Error(reason)
      }
      state.permissionOwnedSessions.add(info.sessionID)
      if (APPROVAL_TRANSPORT.kind === "attended_socket" && !state.readinessRecorded) {
        await reportSupervision(state, {
          transition: "plugin_ready",
          request_id: null,
          call_id: null,
          session_id: null,
          tool_name: null,
          raw_args_digest: null,
          canonical_args_digest: null,
          plugin_nonce: APPROVAL_TRANSPORT.nonce,
        })
        state.readinessRecorded = true
      }
    },

    "tool.execute.before": async (info, output) => {
      try {
        if (isStructuredOutputTool(info.tool)) {
          if (!state.configInitialized
            || !state.permissionOwnedSessions.has(info.sessionID)
            || (APPROVAL_TRANSPORT.kind === "attended_socket" && !state.readinessRecorded)
            || state.executionRecords.has(info.callID)) {
            const reason = `invalid StructuredOutput execution evidence for tool call '${info.callID}'`
            recordPermissionDenied(reason)
            throw new ExplicitDenial(reason)
          }
          const record: ExecutionRecord = {
            call_id: info.callID,
            session_id: info.sessionID,
            tool_name: info.tool,
            runtime_tool_name: info.tool,
            raw_args_digest: digest("agent-hub:opencode-raw-args:v1", output.args),
            canonical_match_digests: [],
            resolved_paths: [],
            status: "transcript_only",
            requests: [],
          }
          await reportSupervision(state, {
            transition: "transcript_only",
            request_id: null,
            call_id: record.call_id,
            session_id: record.session_id,
            tool_name: record.tool_name,
            raw_args_digest: record.raw_args_digest,
            canonical_args_digest: null,
            plugin_nonce: null,
          })
          recordExecution(state, record)
          return
        }
        await routeTool(info.sessionID, info.callID, info.tool, output.args as Record<string, unknown>)
      } catch (error) {
        const message = error instanceof ExplicitDenial
          ? boundedUtf8(error.message)
          : pipelineDenial("schema", String(error))
        recordPermissionDenied(message)
        throw new Error(message)
      }
    },

    "permission.ask": async (info, output) => {
      const message = boundedUtf8(`Agent Hub denied leaked native permission prompt '${info.id}' for session '${info.sessionID}'; the approval pipeline must own every tool decision`)
      output.status = "deny"
      recordPermissionDenied(message)
    },

    "tool.execute.after": async (info) => {
      const record = state.executionRecords.get(info.callID)
      if (!record) {
        const reason = `OpenCode completed unrouted tool call '${info.callID}'`
        recordPermissionDenied(reason)
        throw new Error(reason)
      }
      const afterDigest = digest("agent-hub:opencode-raw-args:v1", info.args)
      if (record.session_id !== info.sessionID || record.runtime_tool_name !== info.tool || record.raw_args_digest !== afterDigest) {
        if (record.status === "authorized") {
          for (const request of record.requests) {
            await reportSupervision(state, executionTransition(record, request, "failed"))
          }
        }
        const reason = `OpenCode before/after evidence mismatch for tool call '${info.callID}'`
        recordPermissionDenied(reason)
        throw new Error(reason)
      }
      if (record.status === "transcript_only") return
      if (record.status !== "authorized") {
        const reason = `OpenCode completed unauthorized tool call '${info.callID}'`
        recordPermissionDenied(reason)
        throw new Error(reason)
      }
      for (const request of record.requests) {
        await reportSupervision(state, executionTransition(record, request, "completed"))
      }
      record.status = "completed"
      recordExecution(state, record)
    },

    event: async ({ event: _event }) => {
      // The published @opencode-ai/plugin types the event hook using the v1
      // SDK Event union, which doesn't include question.* variants. Cast to
      // the v2 Event type which does; the v2 bus is what fires at runtime.
      const event = _event as unknown as EventV2

      // session.status — map opencode statuses to agent-hub statuses
      if (event.type === "session.status") {
        const sid = event.properties.sessionID
        // opencode emits "idle", "busy", "retry"; map to agent-hub terms
        const mapped = event.properties.status.type === "idle" ? "idle" as const : "active" as const
        reportStatus(state, sid, mapped)
        return
      }

      // session.idle — dedicated idle event emitted when the session is
      // waiting for user input. This is separate from session.status and
      // is the primary signal that the agent has finished working.
      if (event.type === "session.idle") {
        const sid = event.properties.sessionID
        reportStatus(state, sid, "idle")
        return
      }

      // session.error — abort / error; kill gateways and report ended
      if (event.type === "session.error") {
        if (event.properties.error?.name !== "MessageAbortedError") return
        const sid = event.properties.sessionID
        if (sid) {
          state.registeredSessions.delete(sid)
          killGateways(state, sid)
          reportStatus(state, sid, "ended")
        }
        return
      }

      // question.asked — AI is blocked waiting for operator input.
      // Proxy the question through the gateway so the operator can answer it
      // from the web UI or TUI. Report Waiting immediately (bypass debounce).
      if (event.type === "question.asked") {
        const sid = event.properties.sessionID
        const requestId = event.properties.id
        const questions = event.properties.questions
        const firstQuestion = questions[0]
        if (!firstQuestion) throw new Error("question.asked event contained no questions")
        const header = firstQuestion.header

        // Immediately report Waiting (bypass debounce — time-sensitive)
        const existing = state.statusTimers.get(sid)
        if (existing) clearTimeout(existing)
        state.statusTimers.delete(sid)
        spawnStatusReport(state, sid, "waiting", { waitingReason: `Plan question: ${header}` })

        // Build the gateway request
        const proxyReq: QuestionProxyRequest = {
          id: `${sid}-${requestId}`,
          session_id: sid,
          session_display_name: state.titles.get(sid) ?? sid,
          cwd: directory,
          question_request_id: requestId,
          questions: [
            {
              question: firstQuestion.question,
              header: firstQuestion.header,
              options: firstQuestion.options.map((o) => ({ label: o.label, description: o.description })),
              multiple: firstQuestion.multiple,
              custom: firstQuestion.custom,
            },
            ...questions.slice(1).map((q) => ({
              question: q.question,
              header: q.header,
              options: q.options.map((o) => ({ label: o.label, description: o.description })),
              multiple: q.multiple,
              custom: q.custom,
            })),
          ],
          provider: "opencode",
        }

        // Proxy through gateway (async — the event hook is fire-and-forget from
        // opencode's perspective, but question.ask() awaits the Deferred internally
        // so the reply can arrive at any time).
        const handleGatewayResult = async (result: QuestionGatewayResult) => {
          // If the question was answered locally in the TUI while the gateway
          // was in-flight, we killed the gateway (SIGTERM) which causes it to
          // exit without an answer. Skip the gateway callback here because the
          // question is already resolved and a second response would fail.
          if (state.locallyAnsweredQuestions.has(requestId)) {
            log(state, "INFO", "question already answered locally, skipping gateway callback", { sid, requestId })
            state.locallyAnsweredQuestions.delete(requestId)
            return
          }
          if ("answers" in result) {
            log(state, "INFO", "question answered via gateway, calling question.reply", { sid, requestId })
            const resp = await clientV2.question.reply({
              requestID: requestId,
              directory,
              answers: result.answers,
            })
            if (resp.error) {
              throw new Error(`question.reply failed: ${JSON.stringify(resp.error)}`)
            }
            return
          }

          if (result.outcome === "rejected") {
            log(state, "INFO", "question rejected via gateway, calling question.reject", { sid, requestId })
            const resp = await clientV2.question.reject({
              requestID: requestId,
              directory,
            })
            if (resp.error) {
              throw new Error(`question.reject failed: ${JSON.stringify(resp.error)}`)
            }
            return
          }

          const message = result.outcome === "pipeline-error"
            ? pipelineDenial(result.category, result.detail)
            : APPROVAL_TIMEOUT_MESSAGE
          const resp = await clientV2.question.reply({
            requestID: requestId,
            directory,
            answers: questions.map(() => [message]),
          })
          if (resp.error) {
            throw new Error(`synthetic question.reply failed: ${JSON.stringify(resp.error)}`)
          }
        }

        callGatewayQuestion(state, sid, proxyReq).then(handleGatewayResult).catch(async (err: unknown) => {
          const detail = boundedUtf8(String(err))
          log(state, "ERROR", "question approval pipeline callback failed", { sid, requestId, detail })
          try {
            const response = await clientV2.question.reply({
              requestID: requestId,
              directory,
              answers: questions.map(() => [pipelineDenial("schema", detail)]),
            })
            if (response.error) throw new Error(JSON.stringify(response.error))
          } catch (replyError) {
            log(state, "ERROR", "question pipeline error reply failed; rejecting question", {
              sid,
              requestId,
              detail: boundedUtf8(String(replyError)),
            })
            try {
              const response = await clientV2.question.reject({ requestID: requestId, directory })
              if (response.error) throw new Error(JSON.stringify(response.error))
            } catch (terminationError) {
              const termination = boundedUtf8(`Agent Hub could neither answer nor terminate question '${requestId}' after an approval pipeline failure: ${String(terminationError)}`)
              log(state, "ERROR", "question pipeline termination failed", { sid, requestId, detail: termination })
              throw new Error(termination)
            }
          }
        })

        return
      }

      // question.replied — question was answered (locally or via proxy).
      // Restore active status so the dashboard reflects the agent is running again.
      // If a gateway process is still in-flight for this requestId, the question was
      // answered locally in the OpenCode TUI — kill the gateway so it cancels the
      // pending question on the agent-hub server.
      if (event.type === "question.replied") {
        const reqId = event.properties.requestID
        const gwProc = state.questionGatewayProcs.get(reqId)
        if (gwProc) {
          log(state, "INFO", "question answered locally, cancelling agent-hub gateway", { reqId })
          state.locallyAnsweredQuestions.add(reqId)
          try { gwProc.kill("SIGTERM") } catch (err: unknown) {
            log(state, "WARN", "failed to kill question gateway", { reqId, err: String(err) })
          }
        }
        reportStatus(state, event.properties.sessionID, "active")
        return
      }

      // question.rejected — question was dismissed in the OpenCode TUI. Same
      // cleanup: kill the in-flight gateway if present so the server entry is cancelled.
      if (event.type === "question.rejected") {
        const reqId = event.properties.requestID
        const gwProc = state.questionGatewayProcs.get(reqId)
        if (gwProc) {
          log(state, "INFO", "question rejected locally, cancelling agent-hub gateway", { reqId })
          state.locallyAnsweredQuestions.add(reqId)
          try { gwProc.kill("SIGTERM") } catch (err: unknown) {
            log(state, "WARN", "failed to kill question gateway", { reqId, err: String(err) })
          }
        }
        reportStatus(state, event.properties.sessionID, "active")
        return
      }
    },
    dispose,
  }
}

export const server: Plugin = (input) => createPluginInstance(input)

export default server
