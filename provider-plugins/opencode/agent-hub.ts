/**
 * Agent Hub opencode plugin.
 *
 * Routes every permission.ask decision through agent-hub-gateway so that
 * opencode's tool approvals run through the same rule engine and server
 * escalation path as Claude Code and Cursor.
 *
 * Environment variables (set in the shell that launches opencode):
 *   AGENT_HUB_GATEWAY   path to the gateway binary (default: "agent-hub-gateway")
 *   AGENT_HUB_SERVER    server URL (default: "http://localhost:8080")
 *   AGENT_HUB_TOKEN     bearer token for server auth (default: none)
 *   AGENT_HUB_CONFIG    path to tools.json rule config (optional)
 *
 * Installation: add to .opencode/config.json:
 *   { "plugin": ["file:///path/to/opencode-plugin"] }
 */

import type { Hooks, Plugin, PluginInput } from "@opencode-ai/plugin"
import type { PermissionRequest } from "@opencode-ai/sdk/v2"
import path from "path"
import fs from "fs"
import { spawn, spawnSync, type ChildProcess } from "child_process"
import type {
  OpenCodeHookInput,
  OpenCodeHookOutput,
  OpenCodeTool,
  SessionStatus,
  StatusReport,
} from "./generated/gateway-types"

// ---------------------------------------------------------------------------
// Hook output type — extracted from Hooks["permission.ask"] so `apply()`
// stays in sync with the SDK definition automatically.
// ---------------------------------------------------------------------------

// The published @opencode-ai/plugin types the hook output as { status },
// but opencode's runtime also reads `message?: string` from the output
// (see opencode/packages/opencode/src/permission/index.ts).  Extend the
// SDK-derived type until the published types catch up.
type PermissionOutputBase = NonNullable<Hooks["permission.ask"]> extends
  (input: any, output: infer O) => any ? O : never
type PermissionOutput = PermissionOutputBase & { message?: string }

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
// Permission name → gateway tool name (OpenCodeTool wire format).
// The gateway's OpenCodeTool enum accepts both lowercase and PascalCase.
// Values here use PascalCase to match the opencode tool.execute.before format.
// ---------------------------------------------------------------------------

const PERM_TO_TOOL: Record<string, OpenCodeTool> = {
  bash: "Bash",
  edit: "Write",
  read: "Read",
  webfetch: "WebFetch",
}

// ---------------------------------------------------------------------------
// Gateway subprocess tracking — kill on session abort
// ---------------------------------------------------------------------------

const procs: Map<string, Set<ChildProcess>> = new Map()

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
// permission.ask for a new session triggers an immediate "active" report.
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
process.on("SIGINT", onExit)
process.on("SIGTERM", onExit)

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
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const server = process.env.AGENT_HUB_SERVER ?? "http://localhost:8080"
  const token = process.env.AGENT_HUB_TOKEN ?? ""

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
    ["status-report", "--server", server, "--token", token],
    { stdio: ["pipe", "ignore", "pipe"] },
  )

  proc.stdin.end(payload)
  proc.stderr?.on("data", (chunk: Buffer) => {
    log("INFO", "status-report stderr", { output: chunk.toString().trim() })
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
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const server = process.env.AGENT_HUB_SERVER ?? "http://localhost:8080"
  const token = process.env.AGENT_HUB_TOKEN ?? ""

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
      ["status-report", "--server", server, "--token", token],
      { input: payload, stdio: ["pipe", "ignore", "pipe"], timeout: 5000 },
    )
  } catch (err: unknown) {
    log("WARN", "sync status report failed", { err: String(err) })
  }
}

// ---------------------------------------------------------------------------
// Single-pattern gateway call
// ---------------------------------------------------------------------------

/** Gateway result extends the generated OpenCodeHookOutput with plugin-only
 *  fields that are synthesized from exit codes (not part of the JSON wire format). */
interface GatewayResult extends OpenCodeHookOutput {
  ask?: boolean   // true when gateway spawn fails (binary not found — fall back to human)
}

/** Validate that a parsed JSON value conforms to the OpenCodeHookOutput shape.
 *  The gateway is an external subprocess — we cannot trust its stdout blindly. */
function isValidGatewayOutput(v: unknown): v is OpenCodeHookOutput {
  if (typeof v !== "object" || v === null) return false
  const obj = v as Record<string, unknown>
  if (typeof obj.allowed !== "boolean") return false
  if (obj.reason !== undefined && obj.reason !== null && typeof obj.reason !== "string") return false
  return true
}

async function callGateway(sid: string, payload: OpenCodeHookInput): Promise<GatewayResult> {
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const server = process.env.AGENT_HUB_SERVER
  const token = process.env.AGENT_HUB_TOKEN
  const config = process.env.AGENT_HUB_CONFIG

  const args: string[] = ["approval", "--opencode"]
  args.push("--server", server ?? "http://localhost:8080")
  args.push("--token", token ?? "")
  if (config) args.push("--config", config)

  log("INFO", "callGateway", { bin, config: config ?? "(none)" })

  return new Promise((resolve) => {
    const proc = spawn(bin, args, { stdio: ["pipe", "pipe", "pipe"] })
    track(sid, proc)

    let stdout = ""
    let stderr = ""
    proc.stdout.on("data", (chunk: Buffer) => { stdout += chunk.toString() })
    proc.stderr.on("data", (chunk: Buffer) => { stderr += chunk.toString() })

    proc.on("close", (code: number | null) => {
      untrack(sid, proc)
      if (stderr) log("INFO", "gateway stderr", { output: stderr.trim() })
      // Exit 1 = server unreachable or approval timed out — deny, do not fall back to opencode ask.
      if (code === 1) {
        log("WARN", "gateway denied (server unreachable or approval timed out)", { reason: stderr.trim() })
        return resolve({ allowed: false, reason: stderr.trim() || "gateway: server unreachable or timed out" })
      }
      // Exit 2 = fail-closed (bad input, config error, policy denial).
      if (code !== 0) return resolve({ allowed: false, reason: "gateway: fail-closed" })
      try {
        const parsed: unknown = JSON.parse(stdout.trim())
        if (!isValidGatewayOutput(parsed)) {
          log("ERROR", "gateway output failed validation", { stdout: stdout.trim() })
          return resolve({ allowed: false, reason: "gateway: invalid output shape" })
        }
        resolve(parsed)
      } catch {
        log("ERROR", "gateway invalid response", { stdout: stdout.trim() })
        resolve({ allowed: false, reason: "gateway: invalid response" })
      }
    })

    proc.on("error", (err: Error) => {
      untrack(sid, proc)
      log("ERROR", "gateway spawn error — falling back to ask", { err: err.message, bin })
      resolve({ allowed: false, ask: true, reason: err.message })
    })

    proc.stdin.end(JSON.stringify(payload))
  })
}

// ---------------------------------------------------------------------------
// Build a gateway payload for a single tool_input value
// ---------------------------------------------------------------------------

function makePayload(
  sessionId: string,
  tool: OpenCodeTool,
  toolInput: Record<string, unknown>,
  cwd: string,
  title?: string,
): OpenCodeHookInput {
  return {
    session_id: sessionId,
    tool_name: tool,
    tool_input: toolInput,
    cwd,
    workspace_roots: [cwd],
    hook_event_name: "permission.ask",
    session_title: title ?? null,
  }
}

// Resolve a path value against base, but only if it is relative.
function resolvePath(value: string, base: string): string {
  return path.isAbsolute(value) ? value : path.resolve(base, value)
}

// Apply a GatewayResult to the hook output object.
function apply(r: GatewayResult, output: PermissionOutput) {
  output.status = r.ask ? "ask" : r.allowed ? "allow" : "deny"
  if (r.reason && !r.allowed && !r.ask) output.message = r.reason
}

// ---------------------------------------------------------------------------
// Session title cache — avoids repeated SDK calls for the same session.
// Stores the LLM-generated title once it's available; null means we haven't
// fetched a non-default title yet and should retry next time.
// ---------------------------------------------------------------------------

const DEFAULT_TITLE_RE = /^(New session - |Child session - )\d{4}-\d{2}-\d{2}T/

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

export const id = "agent-hub"

const server: Plugin = async (input: PluginInput): Promise<Hooks> => {
  const client = input.client
  const { directory, worktree } = input

  return {
    "permission.ask": async (_info, output) => {
      // The published @opencode-ai/plugin types permission.ask input as the
      // v1 Permission type, but opencode actually sends v2 PermissionRequest
      // at runtime (with `permission`/`patterns` instead of `type`/`pattern`).
      // This cast is safe and necessary until the published types catch up.
      const info = _info as unknown as PermissionRequest
      log("INFO", "permission.ask handler entered", { permission: info.permission ?? "?" })
      // external_directory is opencode-internal — the in_workspace distinction
      // is already handled by the gateway's in_workspace flag on file rules.
      // Auto-approve it here; the subsequent read/write permission will be
      // sent to the gateway with the correct path for rule evaluation.
      if (info.permission === "external_directory") {
        output.status = "allow"
        return
      }

      const tool = PERM_TO_TOOL[info.permission] ?? info.permission
      const sid = info.sessionID

      // First tool call for this session — immediately register it on the
      // server so it shows up in the session list right away, rather than
      // waiting for a debounced session.status event or an approval escalation.
      if (!registeredSessions.has(sid)) {
        registeredSessions.add(sid)
        spawnStatusReport(sid, "active")
      }

      log("INFO", "about to call fetchTitle", { sid, tool })
      const title = await fetchTitle(client, sid, directory)
      log("INFO", "fetchTitle returned", { sid, title: title ?? "(none)" })

      // -----------------------------------------------------------------
      // bash: use the raw unparsed command from metadata rather than the
      // per-subcommand tree-sitter fragments in patterns.
      // -----------------------------------------------------------------
      if (info.permission === "bash" && typeof info.metadata.command === "string") {
        const result = await callGateway(
          sid, makePayload(sid, tool, { command: info.metadata.command }, directory, title),
        )
        apply(result, output)
        return
      }

      // -----------------------------------------------------------------
      // edit (write.ts / edit.ts / apply_patch.ts):
      //   metadata.filepath is the absolute path (scalar, for write/edit).
      //   metadata.files is an array of per-file objects (for apply_patch).
      //   patterns contains worktree-relative paths as fallback.
      // -----------------------------------------------------------------
      if (info.permission === "edit") {
        // apply_patch: rich per-file metadata
        if (Array.isArray(info.metadata.files)) {
          for (const entry of info.metadata.files) {
            const filePath = typeof entry === "object" && entry !== null &&
              "filePath" in entry && typeof (entry as Record<string, unknown>).filePath === "string"
              ? (entry as Record<string, unknown>).filePath as string
              : undefined
            if (!filePath) continue
            const result = await callGateway(
              sid, makePayload(sid, tool, { path: filePath }, directory, title),
            )
            if (!result.allowed) {
              apply(result, output)
              return
            }
          }
          output.status = "allow"
          return
        }

        // write/edit: scalar filepath in metadata (already absolute)
        if (typeof info.metadata.filepath === "string") {
          const result = await callGateway(
            sid, makePayload(sid, tool, { path: info.metadata.filepath }, directory, title),
          )
          apply(result, output)
          return
        }

        // Fallback: patterns entries are worktree-relative — resolve against worktree
        for (const pat of info.patterns) {
          const result = await callGateway(
            sid, makePayload(sid, tool, { path: resolvePath(pat, worktree) }, directory, title),
          )
          if (!result.allowed) {
            apply(result, output)
            return
          }
        }
        output.status = "allow"
        return
      }

      // -----------------------------------------------------------------
      // read: patterns[0] is already an absolute path.
      // -----------------------------------------------------------------
      if (info.permission === "read") {
        const result = await callGateway(
          sid, makePayload(sid, tool, { path: info.patterns[0] ?? "" }, directory, title),
        )
        apply(result, output)
        return
      }

      // -----------------------------------------------------------------
      // webfetch: metadata.url is the canonical arg the gateway expects.
      // -----------------------------------------------------------------
      if (info.permission === "webfetch" && typeof info.metadata.url === "string") {
        const result = await callGateway(
          sid, makePayload(sid, tool, { url: info.metadata.url }, directory, title),
        )
        apply(result, output)
        return
      }

      // -----------------------------------------------------------------
      // grep: patterns[0] is the regex; metadata may carry path/directory.
      // The gateway requires `pattern`; `path` is optional.
      // -----------------------------------------------------------------
      if (info.permission === "grep") {
        log("INFO", "grep permission info", {
          patterns: JSON.stringify(info.patterns),
          metadata: JSON.stringify(info.metadata),
        })
        const toolInput: Record<string, string> = {}
        if (info.patterns[0]) toolInput.pattern = info.patterns[0]
        // Forward path/directory from metadata if present
        if (typeof info.metadata.path === "string") {
          toolInput.path = resolvePath(info.metadata.path, directory)
        } else if (typeof info.metadata.directory === "string") {
          toolInput.path = resolvePath(info.metadata.directory, directory)
        }
        const result = await callGateway(sid, makePayload(sid, tool, toolInput, directory, title))
        apply(result, output)
        return
      }

      // -----------------------------------------------------------------
      // glob: patterns[0] is the glob pattern; metadata may carry
      // path/directory. The gateway needs `path` (target directory).
      // -----------------------------------------------------------------
      if (info.permission === "glob") {
        log("INFO", "glob permission info", {
          patterns: JSON.stringify(info.patterns),
          metadata: JSON.stringify(info.metadata),
        })
        const toolInput: Record<string, string> = {}
        // Forward path/directory from metadata if present
        if (typeof info.metadata.path === "string") {
          toolInput.path = resolvePath(info.metadata.path, directory)
        } else if (typeof info.metadata.directory === "string") {
          toolInput.path = resolvePath(info.metadata.directory, directory)
        } else {
          // Fall back to cwd as the target directory
          toolInput.path = directory
        }
        const result = await callGateway(sid, makePayload(sid, tool, toolInput, directory, title))
        apply(result, output)
        return
      }

      // -----------------------------------------------------------------
      // All other tools (websearch, codesearch, list, task, todowrite,
      // lsp, skill, ...):
      // Single call with empty tool_input. get_matchable_args returns []
      // for tools not in TOOL_DEFS and the rule engine falls to the
      // configured default (typically "ask", escalating to the server).
      // -----------------------------------------------------------------
      log("INFO", "fallback permission info", {
        permission: info.permission,
        patterns: JSON.stringify(info.patterns),
        metadata: JSON.stringify(info.metadata),
      })
      const result = await callGateway(sid, makePayload(sid, tool, {}, directory, title))
      apply(result, output)
    },

    event: async ({ event }) => {
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
      }
    },
  }
}

export default { id, server }
