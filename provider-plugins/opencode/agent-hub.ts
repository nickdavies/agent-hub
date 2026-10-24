/** Generic HTTP gateway integration for the OpenCode 1.18.26 plugin contract. */
import type { Hooks, Plugin, PluginInput } from "@opencode-ai/plugin"
import { OpencodeClient as OpencodeClientV2, type Event as EventV2, type Config as ConfigV2 } from "@opencode-ai/sdk/v2"
import { createHash } from "crypto"
import { spawn, spawnSync, type ChildProcess } from "child_process"
import fs from "fs"
import path from "path"
import type { OpenCodeHookInput, OpenCodeHookOutput, OpenCodeTool, QuestionProxyRequest, SessionStatus, StatusReport } from "./generated/gateway-types"
import { normalizeToolExecution } from "./permission-normalization"

const OUTPUT_LIMIT = 1024 * 1024
const OPERATION_TIMEOUT = 86_410_000
const SHORT_TIMEOUT = 15_000
export const DEFAULT_TITLE_RE = /^(New session - |Child session - )\d{4}-\d{2}-\d{2}T/
export const APPROVAL_TIMEOUT_MESSAGE = "Agent Hub denied this request because approval timed out before an operator decision."
export const APPROVAL_PIPELINE_ERROR_MESSAGE = "Agent Hub denied this request because the approval pipeline failed."

function bounded(value: string): string {
  if (Buffer.byteLength(value) <= 4096) return value
  let result = ""
  for (const character of value) {
    if (Buffer.byteLength(result) + Buffer.byteLength(character) > 4096) break
    result += character
  }
  return result
}

type ProcessResult = { ok: true; stdout: string } | { ok: false; category: string; detail: string; code?: number }

/** Capture both pipes with bounded memory and a deadline; never interpret a failed child as approval. */
export function runGatewayProcess(options: {
  bin: string
  args: string[]
  input: string
  timeoutMs: number
  onSpawn?: (proc: ChildProcess) => void
}): Promise<ProcessResult> {
  return new Promise((resolve) => {
    let proc: ChildProcess
    try {
      proc = spawn(options.bin, options.args, { stdio: ["pipe", "pipe", "pipe"] })
    } catch (error) {
      resolve({ ok: false, category: "spawn", detail: bounded(String(error)) })
      return
    }
    let settled = false
    let failure: ProcessResult | undefined
    const chunks: Buffer[] = []
    const errors: Buffer[] = []
    let stdoutBytes = 0
    let stderrBytes = 0
    const finish = (result: ProcessResult) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      resolve(result)
    }
    const fail = (category: string, detail: string) => {
      if (settled || failure) return
      failure = { ok: false, category, detail: bounded(detail) }
      proc.kill("SIGKILL")
      // A descendant can retain inherited pipes after the direct child dies.
      finish(failure)
    }
    const timer = setTimeout(() => fail("timeout", `gateway exceeded ${options.timeoutMs} ms`), options.timeoutMs)
    const capture = (stream: "stdout" | "stderr", chunk: Buffer) => {
      if (settled) return
      if (stream === "stdout") stdoutBytes += chunk.length
      else stderrBytes += chunk.length
      if (stdoutBytes > OUTPUT_LIMIT || stderrBytes > OUTPUT_LIMIT) return fail("oversized", `${stream} exceeded ${OUTPUT_LIMIT} bytes`)
      ;(stream === "stdout" ? chunks : errors).push(chunk)
    }
    proc.stdout?.on("data", (chunk: Buffer) => capture("stdout", chunk))
    proc.stderr?.on("data", (chunk: Buffer) => capture("stderr", chunk))
    proc.stdout?.on("error", (error) => fail("read", error.message))
    proc.stderr?.on("error", (error) => fail("read", error.message))
    proc.stdin?.on("error", (error) => fail("write", error.message))
    proc.on("error", (error) => finish({ ok: false, category: "spawn", detail: bounded(error.message) }))
    proc.on("close", (code, signal) => {
      if (failure) return finish(failure)
      if (signal) return finish({ ok: false, category: "signal", detail: `gateway terminated by ${signal}` })
      if (code !== 0) return finish({ ok: false, category: "nonzero", code: code ?? undefined, detail: bounded(`gateway exited ${code}: ${Buffer.concat(errors).toString("utf8")}`) })
      finish({ ok: true, stdout: Buffer.concat(chunks).toString("utf8") })
    })
    options.onSpawn?.(proc)
    if (!proc.stdin) return fail("write", "gateway stdin unavailable")
    proc.stdin.end(options.input)
  })
}

export function makePayload(
  sessionId: string,
  tool: OpenCodeTool,
  rawArgs: Record<string, unknown>,
  matchInput: Record<string, unknown>,
  directory: string,
  title?: string,
): OpenCodeHookInput {
  return { session_id: sessionId, tool_name: tool, tool_input: rawArgs, tool_match_input: matchInput,
    cwd: directory, hook_event_name: "tool.execute.before", session_title: title ?? null }
}

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical)
  if (value && typeof value === "object") return Object.fromEntries(Object.entries(value)
    .sort(([a], [b]) => a.localeCompare(b)).map(([key, nested]) => [key, canonical(nested)]))
  return value
}

function digest(value: unknown): string {
  return createHash("sha256").update(JSON.stringify(canonical(value))).digest("hex")
}

interface Execution {
  sessionID: string
  tool: string
  digest: string
  status: "routing" | "authorized" | "denied" | "completed"
}

/** All mutable state, timers, children and process listeners belong to this instance. */
export async function createPluginInstance(input: PluginInput, options: { registerProcessHandlers?: boolean } = {}): Promise<Hooks> {
  const { client, directory } = input
  if (!directory || !path.isAbsolute(directory)) throw new Error("Plugin directory must be absolute")
  // Reuse the host's generated client, including its in-process transport. No TCP server is required.
  // The plugin publishes a v1 client with a protected field; the host's v2 API
  // uses that same generated transport. Isolate this SDK boundary cast here.
  type Transport = NonNullable<ConstructorParameters<typeof OpencodeClientV2>[0]>["client"]
  const transport = (client as unknown as { _client?: Transport })._client
  if (!transport) throw new Error("OpenCode runtime does not expose the in-process plugin client")
  const clientV2 = new OpencodeClientV2({ client: transport })
  const env = { ...process.env }
  const bin = env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const serverArgs = ["--server", env.AGENT_HUB_SERVER ?? "http://localhost:8080", ...(env.AGENT_HUB_TOKEN ? ["--token", env.AGENT_HUB_TOKEN] : [])]
  const owned = new Set<string>()
  const ownershipVersions = new Map<string, number>()
  const records = new Map<string, Execution>()
  const procs = new Map<string, Set<ChildProcess>>()
  const questions = new Map<string, { sid: string; proc?: ChildProcess; cancelled: boolean }>()
  const timers = new Map<string, ReturnType<typeof setTimeout>>()
  const registered = new Set<string>()
  const registering = new Set<string>()
  const titles = new Map<string, string>()
  let initialized = false
  let disposed = false
  const logfile = path.join(env.XDG_DATA_HOME ?? path.join(env.HOME ?? "~", ".local/share"), "opencode/log/dev.log")
  const log = (message: string) => {
    // Diagnostics must never change an approval or cleanup outcome.
    try { fs.appendFileSync(logfile, `WARN ${new Date().toISOString()} service=agent-hub-plugin ${bounded(message)}\n`) } catch {}
  }
  const ensureLive = () => { if (disposed) throw new Error("Agent Hub plugin is disposed") }
  const invoke = async (sid: string, args: string[], payload: unknown, timeoutMs = OPERATION_TIMEOUT,
    onSpawn?: (proc: ChildProcess) => void): Promise<ProcessResult> => {
    if (disposed) return { ok: false, category: "disposed", detail: "plugin disposed" }
    let child: ChildProcess | undefined
    const result = await runGatewayProcess({ bin, args, input: JSON.stringify(payload), timeoutMs, onSpawn: (proc) => {
      child = proc
      let set = procs.get(sid)
      if (!set) procs.set(sid, set = new Set())
      set.add(proc)
      onSpawn?.(proc)
    } })
    if (child) procs.get(sid)?.delete(child)
    if (!procs.get(sid)?.size) procs.delete(sid)
    return result
  }
  const kill = (sid: string) => {
    for (const proc of procs.get(sid) ?? []) proc.kill("SIGKILL")
    for (const question of questions.values()) if (question.sid === sid) question.cancelled = true
  }
  const statusPayload = (sid: string, status: SessionStatus, waitingReason?: string): StatusReport => ({
    session_id: sid, cwd: directory, status, waiting_reason: waitingReason ?? null,
    display_name: titles.get(sid) ?? null, editor_type: "opencode",
  })
  const sendStatus = async (sid: string, status: SessionStatus, reason?: string) => {
    const result = await invoke(sid, ["status-report", ...serverArgs], statusPayload(sid, status, reason), SHORT_TIMEOUT)
    registering.delete(sid)
    if (!disposed && result.ok && status !== "ended") registered.add(sid)
    else registered.delete(sid)
    if (!result.ok) log(`status report failed; later activity will retry: ${result.detail}`)
  }
  const reportStatus = (sid: string, status: SessionStatus) => {
    clearTimeout(timers.get(sid))
    if (disposed) return
    timers.set(sid, setTimeout(() => { timers.delete(sid); void sendStatus(sid, status) }, 2000))
  }
  const fetchTitle = async (sid: string) => {
    if (titles.has(sid)) return titles.get(sid)
    try {
      const response = await client.session.get({ path: { id: sid }, query: { directory } })
      const title = response.error ? undefined : response.data?.title
      if (title && !DEFAULT_TITLE_RE.test(title)) { titles.set(sid, title); return title }
    } catch (error) { log(`session title unavailable: ${error}`) }
    return undefined
  }
  const deny = (reason: string): never => { throw new Error(bounded(reason)) }
  const shutdown = (reportEnded: boolean) => {
    if (disposed) return
    disposed = true
    process.off("exit", exit)
    process.off("SIGINT", sigint)
    process.off("SIGTERM", sigterm)
    for (const timer of timers.values()) clearTimeout(timer)
    timers.clear()
    for (const sid of procs.keys()) kill(sid)
    if (reportEnded) for (const sid of registered) {
      try {
        spawnSync(bin, ["status-report", ...serverArgs], { input: JSON.stringify(statusPayload(sid, "ended")),
          timeout: 5000, maxBuffer: OUTPUT_LIMIT, stdio: ["pipe", "ignore", "pipe"] })
      } catch (error) { log(`exit status report failed: ${error}`) }
    }
    owned.clear(); ownershipVersions.clear(); records.clear(); titles.clear(); registered.clear(); registering.clear()
  }
  const exit = () => shutdown(true)
  const sigint = () => { shutdown(true); process.exit(130) }
  const sigterm = () => { shutdown(true); process.exit(143) }
  if (options.registerProcessHandlers !== false) {
    process.on("exit", exit); process.on("SIGINT", sigint); process.on("SIGTERM", sigterm)
  }

  const proxyQuestion = async (payload: QuestionProxyRequest) => {
    const sid = payload.session_id
    const id = payload.question_request_id
    if (questions.has(id)) throw new Error(`Duplicate question '${id}'`)
    const pending = { sid, cancelled: false, proc: undefined as ChildProcess | undefined }
    questions.set(id, pending)
    const reply = async (answers: string[][]) => {
      const result = await clientV2.question.reply({ requestID: id, directory, answers })
      if (result.error) throw new Error(`question.reply failed: ${JSON.stringify(result.error)}`)
    }
    const reject = async () => {
      const result = await clientV2.question.reject({ requestID: id, directory })
      if (result.error) throw new Error(`question.reject failed: ${JSON.stringify(result.error)}`)
    }
    try {
      const result = await invoke(sid, ["question", "--opencode", ...serverArgs], payload, OPERATION_TIMEOUT, (proc) => { pending.proc = proc })
      if (pending.cancelled || disposed) return
      if (!result.ok) {
        if (result.category === "nonzero" && result.code === 1) return await reject()
        if (result.category === "nonzero" && result.code === 3) return await reply(payload.questions.map(() => [APPROVAL_TIMEOUT_MESSAGE]))
        throw new Error(`${APPROVAL_PIPELINE_ERROR_MESSAGE} ${result.detail}`)
      }
      const parsed: unknown = JSON.parse(result.stdout)
      const answers = (parsed as { answers?: unknown } | null)?.answers
      if (!Array.isArray(answers) || answers.length !== payload.questions.length || !answers.every((answer) =>
        Array.isArray(answer) && answer.every((item) => typeof item === "string"))) throw new Error("Malformed question answers")
      await reply(answers)
    } catch (error) {
      if (pending.cancelled || disposed) return
      const message = bounded(`${APPROVAL_PIPELINE_ERROR_MESSAGE} ${error}`)
      log(message)
      try { await reply(payload.questions.map(() => [message])) } catch (replyError) {
        if (pending.cancelled || disposed) return
        log(`question error reply failed: ${replyError}`)
        await reject()
      }
    } finally { questions.delete(id) }
  }

  return {
    config: async (config) => {
      ensureLive()
      // The hook's published config type is v1; the host consumes v2 rules.
      const runtimeConfig = config as unknown as ConfigV2
      runtimeConfig.permission = { "*": "deny" }
      for (const agent of Object.values(runtimeConfig.agent ?? {})) if (agent) agent.permission = { "*": "deny" }
      owned.clear()
      for (const [sid, version] of ownershipVersions) ownershipVersions.set(sid, version + 1)
      initialized = true
    },
    "chat.message": async (info) => {
      ensureLive()
      const sid = info.sessionID
      owned.delete(sid)
      const version = (ownershipVersions.get(sid) ?? 0) + 1
      ownershipVersions.set(sid, version)
      if (!initialized) return deny("Agent Hub config hook has not taken permission ownership")
      try {
        const response = await clientV2.session.update({ sessionID: sid, directory,
          permission: [{ permission: "*", pattern: "*", action: "allow" }] })
        if (response.error) throw new Error(JSON.stringify(response.error))
        const rules = response.data?.permission
        const valid = Array.isArray(rules) && rules.every((rule) => rule && typeof rule.permission === "string"
          && typeof rule.pattern === "string" && ["allow", "ask", "deny"].includes(rule.action))
        const last = valid ? rules.at(-1) : undefined
        if (last?.permission !== "*" || last.pattern !== "*" || last.action !== "allow") throw new Error("session.update did not prove final wildcard allow rule")
        ensureLive()
        if (ownershipVersions.get(sid) !== version) throw new Error("Session ownership update superseded")
        owned.add(sid)
      } catch (error) { return deny(`OpenCode session permission ownership failed: ${error}`) }
    },
    "permission.ask": async (_info, output) => {
      // Set the decision before diagnostics: a failed logger cannot expose a native approval UI.
      output.status = "deny"
      log("Denied leaked native permission prompt; Agent Hub must own every tool decision")
    },
    "tool.execute.before": async (info, output) => {
      ensureLive()
      if (!initialized || !owned.has(info.sessionID)) return deny("Agent Hub denied tool execution: session has not transferred permission ownership")
      if (records.has(info.callID)) return deny(`OpenCode reused tool call ID '${info.callID}'`)
      // Reserve before the first await, including title fetching and subprocess startup.
      const record: Execution = { sessionID: info.sessionID, tool: info.tool, digest: digest(output.args), status: "routing" }
      const ownershipVersion = ownershipVersions.get(info.sessionID)
      const ensureOwnership = () => {
        ensureLive()
        if (!owned.has(info.sessionID) || ownershipVersions.get(info.sessionID) !== ownershipVersion) return deny("OpenCode session ownership changed during approval")
      }
      records.set(info.callID, record)
      try {
        const request = normalizeToolExecution(info.tool, output.args, directory)
        if (!registered.has(info.sessionID) && !registering.has(info.sessionID)) {
          registering.add(info.sessionID)
          void sendStatus(info.sessionID, "active")
        }
        const title = await fetchTitle(info.sessionID)
        for (const matchInput of request.matchInputs) {
          ensureOwnership()
          const result = await invoke(info.sessionID, ["approval", "--opencode", ...serverArgs,
            ...(env.AGENT_HUB_CONFIG ? ["--config", env.AGENT_HUB_CONFIG] : [])],
          makePayload(info.sessionID, request.tool, request.rawArgs, matchInput, directory, title))
          if (!result.ok) return deny(`${APPROVAL_PIPELINE_ERROR_MESSAGE} (${result.category}): ${result.detail}`)
          const parsed: unknown = JSON.parse(result.stdout)
          if (!parsed || typeof parsed !== "object" || typeof (parsed as OpenCodeHookOutput).allowed !== "boolean") return deny("Malformed gateway approval response")
          const decision = parsed as OpenCodeHookOutput
          if (decision.reason != null && typeof decision.reason !== "string") return deny("Malformed gateway approval reason")
          if (!decision.allowed) return deny(decision.reason ?? "Agent Hub denied tool execution")
        }
        ensureOwnership()
        if (digest(output.args) !== record.digest) return deny("OpenCode tool arguments changed during approval")
        record.status = "authorized"
      } catch (error) {
        record.status = "denied"
        return deny(`Agent Hub blocked tool execution: ${error}`)
      }
    },
    "tool.execute.after": async (info) => {
      ensureLive()
      const record = records.get(info.callID)
      if (!record || record.status !== "authorized" || !owned.has(info.sessionID)) return deny(`OpenCode completed unauthorized or duplicate tool call '${info.callID}'`)
      record.status = "denied"
      if (record.sessionID !== info.sessionID || record.tool !== info.tool || record.digest !== digest(info.args)) return deny(`OpenCode before/after argument mismatch for '${info.callID}'`)
      record.status = "completed"
    },
    event: async ({ event: rawEvent }) => {
      if (disposed) return
      const event = rawEvent as unknown as EventV2
      if (event.type === "session.status") return reportStatus(event.properties.sessionID, event.properties.status.type === "idle" ? "idle" : "active")
      if (event.type === "session.idle") return reportStatus(event.properties.sessionID, "idle")
      if (event.type === "session.error" && event.properties.error?.name === "MessageAbortedError") {
        const sid = event.properties.sessionID
        if (sid) {
          owned.delete(sid); ownershipVersions.set(sid, (ownershipVersions.get(sid) ?? 0) + 1)
          clearTimeout(timers.get(sid)); timers.delete(sid)
          kill(sid)
          void sendStatus(sid, "ended")
        }
      }
      if (event.type === "question.asked") {
        const { sessionID: sid, id, questions: items } = event.properties
        clearTimeout(timers.get(sid)); timers.delete(sid)
        void sendStatus(sid, "waiting", `Plan question: ${items[0]?.header ?? "Plan question"}`)
        // Awaited so terminal SDK failures surface to the host rather than an unhandled detached rejection.
        await proxyQuestion({ id: `${sid}-${id}`, session_id: sid, session_display_name: titles.get(sid) ?? sid,
          cwd: directory, question_request_id: id, questions: items.map((item) => ({ ...item })), provider: "opencode" })
      }
      if (event.type === "question.replied" || event.type === "question.rejected") {
        const pending = questions.get(event.properties.requestID)
        if (pending) { pending.cancelled = true; pending.proc?.kill("SIGKILL") }
        reportStatus(event.properties.sessionID, "active")
      }
    },
    dispose: async () => shutdown(false),
  }
}

const plugin: Plugin = (input) => createPluginInstance(input)
export default plugin
