/**
 * Agent Hub opencode plugin.
 *
 * Routes OpenCode 1.18.18 capability executions through agent-hub-gateway so
 * they use the same rule engine and escalation path as Claude Code and Cursor.
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
 * Installation: add to .opencode/config.json:
 *   { "plugin": ["file:///path/to/opencode-plugin"] }
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

const APPROVAL_TRANSPORT = approvalTransport(process.env)
let readinessRecorded = false
let configInitialized = false

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
  requests: { requestID: string; canonicalDigest: string }[]
}

const executionRecords = new Map<string, ExecutionRecord>()

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

function initializeRunEvidence() {
  executionRecords.clear()
}

function recordExecution(record: ExecutionRecord) {
  executionRecords.set(record.call_id, record)
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

let last = Date.now()
function log(level: LogLevel, msg: string, extra?: Record<string, string>) {
  const now = Date.now()
  const diff = now - last
  last = now
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

const procs: Map<string, Set<ChildProcess>> = new Map()

// Per-question gateway process tracking — keyed by opencode question requestId.
// Used to detect when the OpenCode TUI answers a question locally (i.e. before
// the agent-hub gateway gets a chance to answer it), so we can cancel the
// in-flight gateway and clean up the pending question on the server.
const questionGatewayProcs: Map<string, ChildProcess> = new Map()

// Set of requestIds that were answered locally in the OpenCode TUI before the
// gateway had a chance to answer.  When the gateway process exits (due to
// SIGTERM from our kill) with code 1, the callGatewayQuestion callback checks
// this set and skips the question.reject call to avoid a double-resolve error.
const locallyAnsweredQuestions: Set<string> = new Set()
const GATEWAY_OUTPUT_LIMIT_BYTES = 1024 * 1024

function appendBounded(current: string, chunk: Buffer): string | undefined {
  if (Buffer.byteLength(current) + chunk.byteLength > GATEWAY_OUTPUT_LIMIT_BYTES) return undefined
  return current + chunk.toString()
}

function track(sid: string, proc: ChildProcess) {
  let set = procs.get(sid)
  if (!set) {
    set = new Set()
    procs.set(sid, set)
  }
  set.add(proc)
}

function untrack(sid: string, proc: ChildProcess) {
  const set = procs.get(sid)
  if (!set) return
  set.delete(proc)
  if (set.size === 0) procs.delete(sid)
}

function killGateways(sid: string) {
  const set = procs.get(sid)
  if (!set || set.size === 0) return
  log("INFO", "killing gateway subprocesses on abort", { sid, count: String(set.size) })
  for (const proc of set) {
    try { proc.kill("SIGTERM") } catch (err: unknown) {
      log("WARN", "failed to kill gateway subprocess", { sid, err: String(err) })
    }
  }
}

// ---------------------------------------------------------------------------
// Status reporting — fire-and-forget via gateway status-report subcommand
// ---------------------------------------------------------------------------

const statusTimers: Map<string, NodeJS.Timeout> = new Map()
const DEBOUNCE_MS = 2000

// Track sessions we've already registered on the server so the first
// tool execution for a new session triggers an immediate "active" report.
const registeredSessions: Set<string> = new Set()

// On process exit, report all tracked sessions as ended so the dashboard
// doesn't leave stale "active" / "idle" entries after opencode shuts down.
// Uses spawnSync so the reports are delivered before the process terminates —
// async spawn() in an exit handler is unreliable because stdin writes may not
// flush before the parent dies.
let exitHandled = false
function onExit() {
  if (exitHandled) return
  exitHandled = true
  for (const sid of registeredSessions) {
    // Cancel any pending debounced reports — we want "ended" immediately.
    const pending = statusTimers.get(sid)
    if (pending) clearTimeout(pending)
    statusTimers.delete(sid)
    spawnStatusReportSync(sid, "ended")
  }
  registeredSessions.clear()
}
process.on("exit", onExit)
process.on("SIGINT", () => {
  onExit()
  process.exit(130)
})
process.on("SIGTERM", () => {
  onExit()
  process.exit(143)
})

function reportStatus(
  sid: string,
  status: SessionStatus,
  opts?: { waitingReason?: string; displayName?: string },
) {
  // "ended" should fire immediately — no debounce
  if (status === "ended") {
    const existing = statusTimers.get(sid)
    if (existing) clearTimeout(existing)
    statusTimers.delete(sid)
    spawnStatusReport(sid, status, opts)
    return
  }

  const existing = statusTimers.get(sid)
  if (existing) clearTimeout(existing)

  const timer = setTimeout(() => {
    statusTimers.delete(sid)
    spawnStatusReport(sid, status, opts)
  }, DEBOUNCE_MS)

  statusTimers.set(sid, timer)
}

function spawnStatusReport(
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

  log("INFO", "spawnStatusReport", { sid, status })

  const proc = spawn(
    bin,
    ["status-report", "--server", server, ...(token ? ["--token", token] : [])],
    { stdio: ["pipe", "ignore", "pipe"] },
  )

  proc.stdin.end(payload)
  let stderr = ""
  proc.stderr?.on("data", (chunk: Buffer) => {
    const next = appendBounded(stderr, chunk)
    if (next === undefined) {
      proc.kill("SIGKILL")
    } else {
      stderr = next
    }
  })
  proc.on("close", () => {
    if (stderr) log("INFO", "status-report stderr", { output: stderr.trim() })
  })
  proc.on("error", (err: Error) => {
    log("WARN", "status report spawn failed", { err: err.message })
  })
}

/** Synchronous variant used only during process exit to ensure the report
 *  is delivered before the Node.js process terminates. */
function spawnStatusReportSync(
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

  log("INFO", "spawnStatusReportSync", { sid, status })

  try {
    spawnSync(
      bin,
      ["status-report", "--server", server, ...(token ? ["--token", token] : [])],
      {
        input: payload,
        stdio: ["pipe", "ignore", "pipe"],
        timeout: 5000,
        maxBuffer: GATEWAY_OUTPUT_LIMIT_BYTES,
      },
    )
  } catch (err: unknown) {
    log("WARN", "sync status report failed", { err: String(err) })
  }
}

// ---------------------------------------------------------------------------
// Single-pattern gateway call
// ---------------------------------------------------------------------------

type GatewayResult = OpenCodeHookOutput

export const APPROVAL_TIMEOUT_MESSAGE = "Your approval request timed out because the operator was away. This is not a denial. Feel free to retry if you still need this."
export const APPROVAL_PIPELINE_ERROR_MESSAGE = "There was an error in the approvals pipeline. This is a null answer: it is neither an approval nor a rejection of your intended action. Do not take this as a signal that your intended path is unwanted. Try again; if the error recurs, abort and report it to the operator."

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

async function callGateway(sid: string, payload: OpenCodeHookInput, execution: ExecutionBinding): Promise<GatewayResult> {
  return callGatewayOnce(sid, payload, execution)
}

async function callGatewayOnce(sid: string, payload: OpenCodeHookInput, execution: ExecutionBinding): Promise<GatewayResult> {
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const config = process.env.AGENT_HUB_CONFIG

  const invocation = gatewayApprovalInvocation(APPROVAL_TRANSPORT, process.env, execution)
  const args = invocation.args
  if (config) args.push("--config", config)

  log("INFO", "callGateway", { bin, config: config ?? "(none)", transport: APPROVAL_TRANSPORT.kind })

  return new Promise((resolve) => {
    const proc = spawn(bin, args, { stdio: ["pipe", "pipe", "pipe"], env: invocation.env })
    track(sid, proc)

    let stdout = ""
    let stderr = ""
    let outputExceeded = false
    const capture = (current: string, chunk: Buffer): string => {
      const next = appendBounded(current, chunk)
      if (next === undefined) {
        outputExceeded = true
        proc.kill("SIGKILL")
        return current
      }
      return next
    }
    proc.stdout.on("data", (chunk: Buffer) => { stdout = capture(stdout, chunk) })
    proc.stderr.on("data", (chunk: Buffer) => { stderr = capture(stderr, chunk) })

    proc.on("close", (code: number | null) => {
      untrack(sid, proc)
      if (outputExceeded) return resolve({ allowed: false, reason: "gateway: output exceeded limit" })
      if (stderr) log("INFO", "gateway stderr", { output: stderr.trim() })
      // Exit 1 is an approvals-pipeline failure from older gateway versions.
      if (code === 1) {
        log("WARN", "gateway approvals pipeline failure", { reason: stderr.trim() })
        return resolve({ allowed: false, reason: APPROVAL_PIPELINE_ERROR_MESSAGE })
      }
      // Exit 2 = fail-closed local input or config error.
      if (code === 2) return resolve({ allowed: false, reason: "gateway: fail-closed" })
      if (code !== 0) return resolve({ allowed: false, reason: APPROVAL_PIPELINE_ERROR_MESSAGE })
      try {
        const parsed: unknown = JSON.parse(stdout.trim())
        if (!isValidGatewayOutput(parsed)) {
          log("ERROR", "gateway output failed validation", { stdout: stdout.trim() })
          return resolve({ allowed: false, reason: APPROVAL_PIPELINE_ERROR_MESSAGE })
        }
        resolve(parsed)
      } catch {
        log("ERROR", "gateway invalid response", { stdout: stdout.trim() })
        resolve({ allowed: false, reason: APPROVAL_PIPELINE_ERROR_MESSAGE })
      }
    })

    proc.on("error", (err: Error) => {
      untrack(sid, proc)
      log("ERROR", "gateway spawn error", { err: err.message, bin })
      resolve({ allowed: false, reason: APPROVAL_PIPELINE_ERROR_MESSAGE })
    })

    proc.stdin.end(JSON.stringify(payload))
  })
}

async function reportSupervision(transition: Record<string, unknown>): Promise<void> {
  if (APPROVAL_TRANSPORT.kind !== "attended_socket") return
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const invocation = gatewaySupervisionInvocation(APPROVAL_TRANSPORT, process.env)
  await new Promise<void>((resolve, reject) => {
    const proc = spawn(bin, invocation.args, { stdio: ["pipe", "ignore", "pipe"], env: invocation.env })
    let stderr = ""
    let exceeded = false
    proc.stderr.on("data", (chunk: Buffer) => {
      const next = appendBounded(stderr, chunk)
      if (next === undefined) {
        exceeded = true
        proc.kill("SIGKILL")
      } else {
        stderr = next
      }
    })
    proc.on("error", reject)
    proc.on("close", (code) => {
      if (exceeded) return reject(new Error("supervision gateway output exceeded limit"))
      if (code !== 0) return reject(new Error(stderr.trim() || `supervision gateway exited ${code}`))
      resolve()
    })
    proc.stdin.end(JSON.stringify({
      version: "attended_gateway_transition_v1",
      ...JSON.parse(APPROVAL_TRANSPORT.binding),
      ...transition,
    }))
  })
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
  | { outcome: "rejected" | "timeout" | "pipeline-error" }

function isValidQuestionAnswers(value: unknown): value is string[][] {
  return Array.isArray(value) && value.every(
    (answer) => Array.isArray(answer) && answer.every((item) => typeof item === "string"),
  )
}

async function callGatewayQuestion(
  sid: string,
  payload: QuestionProxyRequest,
): Promise<QuestionGatewayResult> {
  if (APPROVAL_TRANSPORT.kind === "attended_socket") {
    return { error: "question relay is unavailable in approval-socket mode" }
  }
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const server = process.env.AGENT_HUB_SERVER
  const token = process.env.AGENT_HUB_TOKEN

  const args: string[] = ["question", "--opencode"]
  args.push("--server", server ?? "http://127.0.0.1:8080")
  if (token) args.push("--token", token)

  log("INFO", "callGatewayQuestion", { bin, requestId: payload.question_request_id })

  const requestId = payload.question_request_id

  return new Promise((resolve) => {
    const proc = spawn(bin, args, { stdio: ["pipe", "pipe", "pipe"] })
    track(sid, proc)
    questionGatewayProcs.set(requestId, proc)

    let stdout = ""
    let stderr = ""
    let outputExceeded = false
    const capture = (current: string, chunk: Buffer): string => {
      const next = appendBounded(current, chunk)
      if (next === undefined) {
        outputExceeded = true
        proc.kill("SIGKILL")
        return current
      }
      return next
    }
    proc.stdout.on("data", (chunk: Buffer) => { stdout = capture(stdout, chunk) })
    proc.stderr.on("data", (chunk: Buffer) => { stderr = capture(stderr, chunk) })

    proc.on("close", (code: number | null) => {
      untrack(sid, proc)
      questionGatewayProcs.delete(requestId)
      if (outputExceeded) return resolve({ error: "gateway: output exceeded limit" })
      if (stderr) log("INFO", "question gateway stderr", { output: stderr.trim() })

      if (code === 0) {
        try {
          const parsed = JSON.parse(stdout.trim()) as QuestionGatewayOutput
          if (!isValidQuestionAnswers(parsed.answers) || parsed.answers.length !== payload.questions.length) {
            log("ERROR", "question gateway output failed validation", { stdout: stdout.trim() })
            resolve({ outcome: "pipeline-error" })
            return
          }
          resolve({ answers: parsed.answers })
        } catch {
          log("ERROR", "question gateway invalid JSON", { stdout: stdout.trim() })
          resolve({ outcome: "pipeline-error" })
        }
        return
      }

      if (code === 1) {
        log("INFO", "question gateway rejected", { reason: stderr.trim() })
        resolve({ outcome: "rejected" })
        return
      }

      if (code === 3) {
        log("INFO", "question gateway timed out", { reason: stderr.trim() })
        resolve({ outcome: "timeout" })
        return
      }

      log("WARN", "question gateway pipeline failure", { code: String(code), stderr: stderr.trim() })
      resolve({ outcome: "pipeline-error" })
    })

    proc.on("error", (err: Error) => {
      untrack(sid, proc)
      questionGatewayProcs.delete(requestId)
      log("ERROR", "question gateway spawn error", { err: err.message, bin })
      resolve({ outcome: "pipeline-error" })
    })

    proc.stdin.end(JSON.stringify(payload))
  })
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

const titles: Map<string, string> = new Map()

async function fetchTitle(
  client: PluginClient,
  sid: string,
  dir: string,
): Promise<string | undefined> {
  const cached = titles.get(sid)
  if (cached) {
    log("INFO", "fetchTitle cache hit", { sid, title: cached })
    return cached
  }

  log("INFO", "fetchTitle calling session.get", { sid, directory: dir })
  try {
    // The plugin client is v1 — uses { path: { id }, query: { directory } }
    // and URL template /session/{id}.
    const res = await client.session.get({
      path: { id: sid },
      query: { directory: dir },
    })
    log("INFO", "fetchTitle raw response", { sid, keys: Object.keys(res).join(","), hasData: String(!!res.data) })
    const title = res.data?.title
    log("INFO", "fetchTitle got response", { sid, title: title ?? "(none)", hasData: String(!!res.data) })
    if (title && !DEFAULT_TITLE_RE.test(title)) {
      titles.set(sid, title)
      log("INFO", "fetchTitle cached title", { sid, title })
      return title
    }
    log("INFO", "fetchTitle title is default/empty, skipping cache", { sid })
    return undefined
  } catch (err) {
    log("WARN", "failed to fetch session title", { sid, err: String(err) })
    return undefined
  }
}

// ---------------------------------------------------------------------------
// Plugin export
// ---------------------------------------------------------------------------

const id = "agent-hub"

const server: Plugin = async (input: PluginInput): Promise<Hooks> => {
  const client = input.client
  const { directory, worktree } = input
  validatePluginDirectory(directory)
  const inProcessClient = (client as any)?._client
  if (!inProcessClient) {
    throw new Error("OpenCode runtime does not expose the in-process plugin client")
  }
  const clientV2 = new OpencodeClientV2({ client: inProcessClient })
  initializeRunEvidence()
  configInitialized = true

  async function routeTool(
    sid: string,
    callID: string,
    toolName: string,
    args: Record<string, unknown>,
  ) {
    if (!configInitialized) {
      const reason = "OpenCode executed a tool before the Agent Hub config hook took permission ownership"
      recordPermissionDenied(reason)
      throw new Error(reason)
    }
    if (APPROVAL_TRANSPORT.kind === "attended_socket" && !readinessRecorded) {
      throw new Error("OpenCode tool execution attempted before host supervision readiness")
    }
    if (executionRecords.has(callID)) {
      const reason = `OpenCode reused tool call ID '${callID}'`
      recordPermissionDenied(reason)
      throw new Error(reason)
    }
    if (!registeredSessions.has(sid)) {
      registeredSessions.add(sid)
      spawnStatusReport(sid, "active")
    }

    const title = await fetchTitle(client, sid, directory)
    let request
    try {
      request = normalizeToolExecution(toolName, args, directory, worktree)
    } catch (error) {
      const reason = String(error)
      log("ERROR", "tool normalization failed closed", { err: reason })
      recordPermissionDenied(reason)
      throw error
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
    recordExecution(record)
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
      await reportSupervision(executionTransition(record, record.requests.at(-1)!, "created"))
      const result = await callGateway(
        sid,
        makePayload(sid, request.tool, request.rawArgs, matchInput, directory, worktree, title),
        execution,
      )
      if (!result.allowed) {
        const reason = result.reason ?? "Agent Hub denied tool execution"
        for (const prior of record.requests.slice(0, -1)) {
          await reportSupervision(executionTransition(record, prior, "failed"))
        }
        record.status = "denied"
        recordExecution(record)
        recordPermissionDenied(reason)
        throw new Error(reason)
      }
    }
    if (request.postPolicyValidationError) {
      for (const routed of record.requests) {
        await reportSupervision(executionTransition(record, routed, "failed"))
      }
      record.status = "denied"
      recordExecution(record)
      recordPermissionDenied(request.postPolicyValidationError)
      throw new Error(request.postPolicyValidationError)
    }
    record.status = "authorized"
    recordExecution(record)
  }

  return {
    "chat.message": async (info) => {
      const response = await clientV2.session.update({
        sessionID: info.sessionID,
        directory,
        permission: [{ permission: "*", pattern: "*", action: "allow" }],
      })
      if (response.error) {
        throw new Error(`OpenCode session permission ownership failed: ${JSON.stringify(response.error)}`)
      }
      if (APPROVAL_TRANSPORT.kind === "attended_socket" && !readinessRecorded) {
        await reportSupervision({
          transition: "plugin_ready",
          request_id: null,
          call_id: null,
          session_id: null,
          tool_name: null,
          raw_args_digest: null,
          canonical_args_digest: null,
          plugin_nonce: APPROVAL_TRANSPORT.nonce,
        })
        readinessRecorded = true
      }
    },

    "tool.execute.before": async (info, output) => {
      if (isStructuredOutputTool(info.tool)) {
        if (!configInitialized
          || (APPROVAL_TRANSPORT.kind === "attended_socket" && !readinessRecorded)
          || executionRecords.has(info.callID)) {
          const reason = `invalid StructuredOutput execution evidence for tool call '${info.callID}'`
          recordPermissionDenied(reason)
          throw new Error(reason)
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
        await reportSupervision({
          transition: "transcript_only",
          request_id: null,
          call_id: record.call_id,
          session_id: record.session_id,
          tool_name: record.tool_name,
          raw_args_digest: record.raw_args_digest,
          canonical_args_digest: null,
          plugin_nonce: null,
        })
        recordExecution(record)
        return
      }
      await routeTool(info.sessionID, info.callID, info.tool, output.args as Record<string, unknown>)
    },

    "tool.execute.after": async (info) => {
      const record = executionRecords.get(info.callID)
      if (!record) {
        const reason = `OpenCode completed unrouted tool call '${info.callID}'`
        recordPermissionDenied(reason)
        throw new Error(reason)
      }
      const afterDigest = digest("agent-hub:opencode-raw-args:v1", info.args)
      if (record.session_id !== info.sessionID || record.runtime_tool_name !== info.tool || record.raw_args_digest !== afterDigest) {
        if (record.status === "authorized") {
          for (const request of record.requests) {
            await reportSupervision(executionTransition(record, request, "failed"))
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
        await reportSupervision(executionTransition(record, request, "completed"))
      }
      record.status = "completed"
      recordExecution(record)
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
        reportStatus(sid, mapped)
        return
      }

      // session.idle — dedicated idle event emitted when the session is
      // waiting for user input. This is separate from session.status and
      // is the primary signal that the agent has finished working.
      if (event.type === "session.idle") {
        const sid = event.properties.sessionID
        reportStatus(sid, "idle")
        return
      }

      // session.error — abort / error; kill gateways and report ended
      if (event.type === "session.error") {
        if (event.properties.error?.name !== "MessageAbortedError") return
        const sid = event.properties.sessionID
        if (sid) {
          registeredSessions.delete(sid)
          killGateways(sid)
          reportStatus(sid, "ended")
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
        const header = questions[0]?.header ?? "Plan question"

        // Immediately report Waiting (bypass debounce — time-sensitive)
        const existing = statusTimers.get(sid)
        if (existing) clearTimeout(existing)
        statusTimers.delete(sid)
        spawnStatusReport(sid, "waiting", { waitingReason: `Plan question: ${header}` })

        // Build the gateway request
        const proxyReq: QuestionProxyRequest = {
          id: `${sid}-${requestId}`,
          session_id: sid,
          session_display_name: titles.get(sid) ?? sid,
          cwd: directory,
          question_request_id: requestId,
          questions: questions.map((q) => ({
            question: q.question,
            header: q.header,
            options: q.options.map((o) => ({ label: o.label, description: o.description })),
            multiple: q.multiple,
            custom: q.custom,
          })),
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
          if (locallyAnsweredQuestions.has(requestId)) {
            log("INFO", "question already answered locally, skipping gateway callback", { sid, requestId })
            locallyAnsweredQuestions.delete(requestId)
            return
          }
          if ("answers" in result) {
            log("INFO", "question answered via gateway, calling question.reply", { sid, requestId })
            const resp = await clientV2.question.reply({
              requestID: requestId,
              directory,
              answers: result.answers,
            })
            if (resp.error) {
              log("WARN", "question.reply failed", { sid, requestId, err: String(resp.error) })
            }
            return
          }

          if (result.outcome === "rejected") {
            log("INFO", "question rejected via gateway, calling question.reject", { sid, requestId })
            const resp = await clientV2.question.reject({
              requestID: requestId,
              directory,
            })
            if (resp.error) {
              log("WARN", "question.reject failed", { sid, requestId, err: String(resp.error) })
            }
            return
          }

          const message = result.outcome === "timeout"
            ? APPROVAL_TIMEOUT_MESSAGE
            : APPROVAL_PIPELINE_ERROR_MESSAGE
          const resp = await clientV2.question.reply({
            requestID: requestId,
            directory,
            answers: questions.map(() => [message]),
          })
          if (resp.error) {
            log("WARN", "synthetic question.reply failed", { sid, requestId, err: String(resp.error) })
          }
        }

        callGatewayQuestion(sid, proxyReq).then(handleGatewayResult).catch(async (err: unknown) => {
          log("WARN", "callGatewayQuestion threw", { sid, requestId, err: String(err) })
          await handleGatewayResult({ outcome: "pipeline-error" })
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
        const gwProc = questionGatewayProcs.get(reqId)
        if (gwProc) {
          log("INFO", "question answered locally, cancelling agent-hub gateway", { reqId })
          locallyAnsweredQuestions.add(reqId)
          try { gwProc.kill("SIGTERM") } catch (err: unknown) {
            log("WARN", "failed to kill question gateway", { reqId, err: String(err) })
          }
        }
        reportStatus(event.properties.sessionID, "active")
        return
      }

      // question.rejected — question was dismissed in the OpenCode TUI. Same
      // cleanup: kill the in-flight gateway if present so the server entry is cancelled.
      if (event.type === "question.rejected") {
        const reqId = event.properties.requestID
        const gwProc = questionGatewayProcs.get(reqId)
        if (gwProc) {
          log("INFO", "question rejected locally, cancelling agent-hub gateway", { reqId })
          locallyAnsweredQuestions.add(reqId)
          try { gwProc.kill("SIGTERM") } catch (err: unknown) {
            log("WARN", "failed to kill question gateway", { reqId, err: String(err) })
          }
        }
        reportStatus(event.properties.sessionID, "active")
        return
      }
    },
  }
}

export default server
