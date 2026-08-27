import net from "node:net"
import crypto from "node:crypto"
import path from "node:path"

const socketPath = process.env.AGENT_HUB_ATTENDED_APPROVAL_SOCKET
const binding = JSON.parse(process.env.AGENT_HUB_ATTENDED_APPROVAL_BINDING ?? "null")
const runDeadline = process.env.AGENT_HUB_ATTENDED_RUN_DEADLINE
const MAX_FRAME_BYTES = 1024 * 1024
const DECISION_KEYS = [
  "approval_bundle_digest", "authorization_digest", "deadline", "decision",
  "environment_bundle_digest", "image_digest", "network_digest", "reason",
  "request_id", "run_epoch", "tool_input_digest", "version",
]

function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical)
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.keys(value).sort().map((key) => [key, canonical(value[key])]))
  }
  return value
}

function canonicalBytes(value) {
  return Buffer.from(JSON.stringify(canonical(value)))
}

function digest(input) {
  return crypto.createHash("sha256")
    .update("agent-hub:attended-tool-input:v1\0")
    .update(canonicalBytes(input))
    .digest("hex")
}

function resolvePath(value, base) {
  return path.isAbsolute(value) ? value : path.resolve(base, value)
}

function toolRequests(info, directory, worktree) {
  const metadata = info.metadata ?? {}
  const patterns = info.patterns ?? []
  const tool = ({ bash: "Bash", edit: "Write", read: "Read", webfetch: "WebFetch" })[info.permission] ?? info.permission
  if (info.permission === "bash" && typeof metadata.command === "string") return [[tool, { command: metadata.command }]]
  if (info.permission === "edit") {
    if (Array.isArray(metadata.files)) {
      return metadata.files.flatMap((entry) => entry && typeof entry.filePath === "string" ? [[tool, { path: entry.filePath }]] : [])
    }
    if (typeof metadata.filepath === "string") return [[tool, { path: metadata.filepath }]]
    return patterns.map((pattern) => [tool, { path: resolvePath(pattern, worktree) }])
  }
  if (info.permission === "read") return [[tool, { path: patterns[0] ?? "" }]]
  if (info.permission === "webfetch" && typeof metadata.url === "string") return [[tool, { url: metadata.url }]]
  if (info.permission === "grep") {
    const input = {}
    if (patterns[0]) input.pattern = patterns[0]
    if (typeof metadata.path === "string") input.path = resolvePath(metadata.path, directory)
    else if (typeof metadata.directory === "string") input.path = resolvePath(metadata.directory, directory)
    return [[tool, input]]
  }
  if (info.permission === "glob") {
    const value = typeof metadata.path === "string" ? metadata.path : metadata.directory
    return [[tool, { path: typeof value === "string" ? resolvePath(value, directory) : directory }]]
  }
  return [[tool, {}]]
}

function requestFor(info, tool, toolInput, directory) {
  const requestId = crypto.randomUUID()
  return {
    version: "attended_gateway_request_v1",
    ...binding,
    request_id: requestId,
    approval: {
      id: requestId,
      session_id: info.sessionID,
      session_display_name: `Attended session ${info.sessionID}`,
      cwd: directory,
      tool_name: tool,
      tool_input: toolInput,
      provider: "opencode",
      request_type: "tool_use",
      context: {
        workspace_roots: [directory],
        hook_event_name: "permission.ask",
        extra: null,
      },
    },
    tool_input_digest: digest(toolInput),
    deadline: runDeadline,
  }
}

function exactDecision(decision, request) {
  if (Object.keys(decision).sort().join("\0") !== DECISION_KEYS.join("\0")) return false
  return decision.version === "attended_gateway_decision_v1" &&
    decision.run_epoch === request.run_epoch &&
    decision.authorization_digest === request.authorization_digest &&
    decision.environment_bundle_digest === request.environment_bundle_digest &&
    decision.image_digest === request.image_digest &&
    decision.network_digest === request.network_digest &&
    decision.approval_bundle_digest === request.approval_bundle_digest &&
    decision.request_id === request.request_id &&
    decision.tool_input_digest === request.tool_input_digest &&
    decision.deadline === request.deadline &&
    (decision.decision === "allow" || decision.decision === "deny") &&
    (decision.reason === null || typeof decision.reason === "string")
}

function relay(request) {
  return new Promise((resolve) => {
    if (!socketPath || !binding || !runDeadline) {
      resolve({ decision: "deny", reason: "attended_relay_not_configured" })
      return
    }
    const client = net.createConnection({ path: socketPath })
    let bytes = Buffer.alloc(0)
    let expected = null
    let finished = false
    const deny = (reason) => {
      if (finished) return
      finished = true
      client.destroy()
      resolve({ decision: "deny", reason })
    }
    client.setTimeout(60_000, () => deny("attended_relay_timeout"))
    client.on("connect", () => {
      const payload = canonicalBytes(request)
      if (payload.length > MAX_FRAME_BYTES) return deny("attended_request_too_large")
      const header = Buffer.alloc(4)
      header.writeUInt32BE(payload.length)
      client.write(Buffer.concat([header, payload]))
    })
    client.on("data", (chunk) => {
      bytes = Buffer.concat([bytes, chunk])
      if (bytes.length > MAX_FRAME_BYTES + 4) return deny("attended_response_too_large")
      if (expected === null && bytes.length >= 4) {
        expected = bytes.readUInt32BE(0)
        if (expected < 1 || expected > MAX_FRAME_BYTES) return deny("attended_response_invalid_length")
      }
      if (expected !== null && bytes.length === expected + 4) {
        try {
          const payload = bytes.subarray(4).toString("utf8")
          const decision = JSON.parse(payload)
          if (canonicalBytes(decision).toString("utf8") !== payload || !exactDecision(decision, request)) {
            return deny("attended_response_mismatch")
          }
          finished = true
          client.end()
          resolve(decision)
        } catch {
          deny("attended_response_invalid")
        }
      }
    })
    client.on("error", () => deny("attended_relay_unavailable"))
    client.on("close", () => {
      if (!finished) deny("attended_relay_disconnected")
    })
  })
}

export const AttendedApprovalPlugin = async ({ directory, worktree }) => ({
  "permission.ask": async (info, output) => {
    if (info.permission === "external_directory") {
      output.status = "allow"
      return
    }
    for (const [tool, toolInput] of toolRequests(info, directory, worktree)) {
      const result = await relay(requestFor(info, tool, toolInput, directory))
      if (result.decision !== "allow") {
        output.status = "deny"
        output.message = result.reason ?? "approval_denied"
        return
      }
    }
    output.status = "allow"
  },
})

export default AttendedApprovalPlugin
