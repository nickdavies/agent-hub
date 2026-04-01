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
import type { OpencodeClient } from "@opencode-ai/sdk"
import path from "path"
import fs from "fs"
import { spawn } from "child_process"

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

let last = Date.now()
function log(level: "INFO " | "WARN " | "ERROR", msg: string, extra?: Record<string, string>) {
  const now = Date.now()
  const diff = now - last
  last = now
  const ts = new Date(now).toISOString().split(".")[0]
  const kv = Object.entries({ service: "agent-hub-plugin", ...extra })
    .map(([k, v]) => `${k}=${v}`)
    .join(" ")
  fs.appendFileSync(logfile, `${level} ${ts} +${diff}ms ${kv} ${msg}\n`)
}

// ---------------------------------------------------------------------------
// Permission name → canonical gateway tool name
// Must stay in sync with TOOL_NAME_MAP in gateway/src/providers/opencode.rs
// ---------------------------------------------------------------------------

const PERM_TO_TOOL: Record<string, string> = {
  bash: "Bash",
  edit: "Write",
  read: "Read",
  webfetch: "WebFetch",
}

// ---------------------------------------------------------------------------
// Single-pattern gateway call
// ---------------------------------------------------------------------------

interface GatewayResult {
  allowed: boolean
  ask?: boolean   // true when gateway exited 1 (server unreachable — fall back to human)
  reason?: string
}

async function callGateway(payload: object): Promise<GatewayResult> {
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const server = process.env.AGENT_HUB_SERVER
  const token = process.env.AGENT_HUB_TOKEN
  const config = process.env.AGENT_HUB_CONFIG

  const args: string[] = ["--opencode"]
  args.push("--server", server ?? "http://localhost:8080")
  args.push("--token", token ?? "")
  if (config) args.push("--config", config)

  log("INFO ", "callGateway", { bin, config: config ?? "(none)" })

  return new Promise((resolve) => {
    const proc = spawn(bin, args, { stdio: ["pipe", "pipe", "pipe"] })

    let stdout = ""
    let stderr = ""
    proc.stdout.on("data", (chunk: Buffer) => { stdout += chunk.toString() })
    proc.stderr.on("data", (chunk: Buffer) => { stderr += chunk.toString() })

    proc.on("close", (code: number | null) => {
      if (stderr) log("INFO ", "gateway stderr", { output: stderr.trim() })
      // Exit 1 = server unreachable or approval timed out — deny, do not fall back to opencode ask.
      if (code === 1) {
        log("WARN ", "gateway denied (server unreachable or approval timed out)", { reason: stderr.trim() })
        return resolve({ allowed: false, reason: stderr.trim() || "gateway: server unreachable or timed out" })
      }
      // Exit 2 = fail-closed (bad input, config error, policy denial).
      if (code !== 0) return resolve({ allowed: false, reason: "gateway: fail-closed" })
      try {
        resolve(JSON.parse(stdout.trim()) as GatewayResult)
      } catch {
        log("ERROR", "gateway invalid response", { stdout: stdout.trim() })
        resolve({ allowed: false, reason: "gateway: invalid response" })
      }
    })

    proc.on("error", (err: Error) => {
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
  tool: string,
  toolInput: Record<string, string>,
  cwd: string,
  title?: string,
): object {
  const payload: Record<string, unknown> = {
    session_id: sessionId,
    tool_name: tool,
    tool_input: toolInput,
    cwd,
    workspace_roots: [cwd],
    hook_event_name: "permission.ask",
  }
  if (title) payload.session_title = title
  return payload
}

// Resolve a path value against base, but only if it is relative.
function resolvePath(value: string, base: string): string {
  return path.isAbsolute(value) ? value : path.resolve(base, value)
}

// Apply a GatewayResult to the hook output object.
function apply(r: GatewayResult, output: { status: "allow" | "deny" | "ask"; message?: string }) {
  output.status = r.ask ? "ask" : r.allowed ? "allow" : "deny"
  if (r.reason && !r.allowed && !r.ask) output.message = r.reason
}

// ---------------------------------------------------------------------------
// Session title cache — avoids repeated SDK calls for the same session.
// Stores the LLM-generated title once it's available; null means we haven't
// fetched a non-default title yet and should retry next time.
// ---------------------------------------------------------------------------

const DEFAULT_TITLE_RE = /^(New session - |Child session - )\d{4}-\d{2}-\d{2}T/

type V1Client = OpencodeClient

const titles: Map<string, string> = new Map()

async function fetchTitle(
  client: V1Client,
  sid: string,
  dir: string,
): Promise<string | undefined> {
  const cached = titles.get(sid)
  if (cached) {
    log("INFO ", "fetchTitle cache hit", { sid, title: cached })
    return cached
  }

  log("INFO ", "fetchTitle calling session.get", { sid, directory: dir })
  try {
    // The plugin client is v1 — uses { path: { id }, query: { directory } }
    // and URL template /session/{id}.
    const res = await client.session.get({
      path: { id: sid },
      query: { directory: dir },
    })
    log("INFO ", "fetchTitle raw response", { sid, keys: Object.keys(res).join(","), hasData: String(!!res.data) })
    const title = res.data?.title
    log("INFO ", "fetchTitle got response", { sid, title: title ?? "(none)", hasData: String(!!res.data) })
    if (title && !DEFAULT_TITLE_RE.test(title)) {
      titles.set(sid, title)
      log("INFO ", "fetchTitle cached title", { sid, title })
      return title
    }
    log("INFO ", "fetchTitle title is default/empty, skipping cache", { sid })
    return undefined
  } catch (err) {
    log("WARN ", "failed to fetch session title", { sid, err: String(err) })
    return undefined
  }
}

// ---------------------------------------------------------------------------
// Plugin export
// ---------------------------------------------------------------------------

export const id = "agent-hub"

const server: Plugin = async (input: PluginInput): Promise<Hooks> => {
  // The runtime client is a v1 client (opencode imports from @opencode-ai/sdk, not /v2).
  const client = input.client as unknown as V1Client
  const { directory, worktree } = input

  return {
    "permission.ask": async (_info, output) => {
      log("INFO ", "permission.ask handler entered [v2]", { permission: (_info as any).permission ?? "?" })
      // The published Permission type from @opencode-ai/sdk still uses the old
      // field names (type/pattern). Cast to PermissionRequest which matches what
      // opencode actually sends at runtime (permission/patterns).
      const info = _info as unknown as PermissionRequest
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
      log("INFO ", "about to call fetchTitle", { sid, tool })
      const title = await fetchTitle(client, sid, directory)
      log("INFO ", "fetchTitle returned", { sid, title: title ?? "(none)" })

      // -----------------------------------------------------------------
      // bash: use the raw unparsed command from metadata rather than the
      // per-subcommand tree-sitter fragments in patterns.
      // -----------------------------------------------------------------
      if (info.permission === "bash" && typeof info.metadata.command === "string") {
        const result = await callGateway(
          makePayload(sid, tool, { command: info.metadata.command }, directory, title),
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
          for (const file of info.metadata.files as Array<{ filePath: string }>) {
            const result = await callGateway(
              makePayload(sid, tool, { path: file.filePath }, directory, title),
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
            makePayload(sid, tool, { path: info.metadata.filepath }, directory, title),
          )
          apply(result, output)
          return
        }

        // Fallback: patterns entries are worktree-relative — resolve against worktree
        for (const pat of info.patterns) {
          const result = await callGateway(
            makePayload(sid, tool, { path: resolvePath(pat, worktree) }, directory, title),
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
          makePayload(sid, tool, { path: info.patterns[0] ?? "" }, directory, title),
        )
        apply(result, output)
        return
      }

      // -----------------------------------------------------------------
      // webfetch: metadata.url is the canonical arg the gateway expects.
      // -----------------------------------------------------------------
      if (info.permission === "webfetch" && typeof info.metadata.url === "string") {
        const result = await callGateway(
          makePayload(sid, tool, { url: info.metadata.url }, directory, title),
        )
        apply(result, output)
        return
      }

      // -----------------------------------------------------------------
      // All other tools (glob, grep, websearch, codesearch, list, task,
      // todowrite, external_directory, lsp, skill, ...):
      // Single call with empty tool_input. get_matchable_args returns []
      // for tools not in TOOL_DEFS and the rule engine falls to the
      // configured default (typically "ask", escalating to the server).
      // We do NOT loop over info.pattern — those are opencode-internal
      // representations unrelated to what the gateway expects.
      // -----------------------------------------------------------------
      const result = await callGateway(makePayload(sid, tool, {}, directory, title))
      apply(result, output)
    },
  }
}

export default { id, server }
