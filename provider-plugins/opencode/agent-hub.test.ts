import { afterAll, beforeEach, describe, expect, mock, test } from "bun:test"
import { EventEmitter } from "events"
import { mkdirSync, rmSync } from "node:fs"
import path from "node:path"
import type { OpenCodeHookInput } from "./generated/gateway-types"

let approvalPayloads: OpenCodeHookInput[] = []
type ProcessFixture = {
  code?: number
  stdout?: string
  stderr?: string
  spawnError?: string
  readError?: string
  writeError?: string
  signal?: NodeJS.Signals
  hang?: boolean
}
let approvalGatewayResults: ProcessFixture[] = []
let questionGatewayResults: ProcessFixture[] = []
let statusGatewayResults: ProcessFixture[] = []
let statusGatewayCalls = 0
let questionReplies: Array<{ requestID: string; directory: string; answers: string[][] }> = []
let questionRejections: Array<{ requestID: string; directory: string }> = []
let questionReplyResults: Array<Record<string, unknown>> = []
let questionRejectResults: Array<Record<string, unknown>> = []
let sessionUpdateResults: Array<Record<string, unknown> | Error> = []

const logRoot = path.resolve("../../target", `opencode-plugin-test-${crypto.randomUUID()}`)
mkdirSync(path.join(logRoot, "opencode", "log"), { recursive: true })
process.env.XDG_DATA_HOME = logRoot

afterAll(() => rmSync(logRoot, { recursive: true, force: true }))

mock.module("@opencode-ai/sdk/v2", () => ({
  OpencodeClient: class {
    session = {
      update: async () => {
        const result = sessionUpdateResults.shift()
        if (result instanceof Error) throw result
        return result ?? {
          data: { permission: [{ permission: "*", pattern: "*", action: "allow" }] },
        }
      },
    }
    question = {
      reply: async (input: { requestID: string; directory: string; answers: string[][] }) => {
        questionReplies.push(input)
        return questionReplyResults.shift() ?? {}
      },
      reject: async (input: { requestID: string; directory: string }) => {
        questionRejections.push(input)
        return questionRejectResults.shift() ?? {}
      },
    }
  },
}))

mock.module("child_process", () => ({
  spawn: (_bin: string, args: string[]) => {
    const proc = new EventEmitter() as EventEmitter & {
      stdin: EventEmitter & { end: (payload: string, callback?: (error?: Error | null) => void) => void }
      stdout: EventEmitter
      stderr: EventEmitter
      kill: (signal?: NodeJS.Signals) => boolean
    }
    proc.stdout = new EventEmitter()
    proc.stderr = new EventEmitter()
    let closed = false
    proc.kill = (signal = "SIGTERM") => {
      if (!closed) queueMicrotask(() => {
        if (closed) return
        closed = true
        proc.emit("close", null, signal)
      })
      return true
    }
    proc.stdin = Object.assign(new EventEmitter(), {
      end: (payload: string, callback?: (error?: Error | null) => void) => {
        let result: ProcessFixture
        if (args[0] === "approval") {
          approvalPayloads.push(JSON.parse(payload) as OpenCodeHookInput)
          result = approvalGatewayResults.shift() ?? { stdout: '{"allowed":true}' }
        } else if (args[0] === "question") {
          result = questionGatewayResults.shift() ?? { stdout: '{"answers":[]}' }
        } else if (args[0] === "status-report") {
          statusGatewayCalls += 1
          result = statusGatewayResults.shift() ?? { code: 0 }
        } else {
          result = { code: 0 }
        }
        if (result.writeError) callback?.(new Error(result.writeError))
        else callback?.()
        if (result.hang || result.writeError) return
        queueMicrotask(() => {
          if (closed) return
          if (result.spawnError) {
            proc.emit("error", new Error(result.spawnError))
            return
          }
          if (result.stdout) proc.stdout.emit("data", Buffer.from(result.stdout))
          if (result.stderr) proc.stderr.emit("data", Buffer.from(result.stderr))
          if (result.readError) {
            proc.stdout.emit("error", new Error(result.readError))
            return
          }
          closed = true
          proc.emit("close", result.signal ? null : result.code ?? 0, result.signal ?? null)
        })
      },
    })
    return proc
  },
  spawnSync: () => ({ status: 0, signal: null, stderr: Buffer.alloc(0) }),
}))

async function loadPlugin() {
  const module = await import(`./agent-hub.ts?test=${Date.now()}-${Math.random()}`)
  return (input: any) => module.createPluginInstance(input, { registerProcessHandlers: false })
}

async function initializedHooks(directory = "/home/user/project") {
  const hooks = await (await loadPlugin())(pluginInput(directory))
  await hooks.config!({ permission: { "*": "ask" }, agent: { review: { permission: { "*": "ask" } } } } as any)
  return hooks
}

async function ownSession(hooks: Awaited<ReturnType<typeof initializedHooks>>, sid = "session-1") {
  await hooks["chat.message"]!({ sessionID: sid } as any, {} as any)
}

function pluginInput(directory: string) {
  return {
    client: {
      session: { get: async () => ({ data: {} }) },
      _client: { getConfig: () => ({ fetch }) },
    },
    directory,
    worktree: directory,
    serverUrl: new URL("http://localhost:4096"),
  } as any
}

function questionAsked(questionCount = 2) {
  return {
    event: {
      type: "question.asked",
      properties: {
        id: "question-1",
        sessionID: "session-1",
        questions: Array.from({ length: questionCount }, (_, index) => ({
          question: `Question ${index + 1}?`,
          header: `Question ${index + 1}`,
          options: [],
          multiple: false,
          custom: true,
        })),
      },
    },
  } as any
}

async function flushQuestionCallback() {
  await new Promise((resolve) => setTimeout(resolve, 0))
}

const {
  DEFAULT_TITLE_RE,
  makeGlobToolInput,
  makePayload,
  makeReadToolInput,
  runGatewayProcess,
} = await import("./agent-hub")

describe("makePayload", () => {
  test("uses directory for cwd and omits workspace_roots", () => {
    const payload = makePayload(
      "ses_abc",
      "Bash",
      { command: "ls" },
      "/home/user/project",
      "/home/user/project",
      "Fix auth bug",
    )

    expect(payload.cwd).toBe("/home/user/project")
    expect(payload).not.toHaveProperty("workspace_roots")
    expect(payload.session_title).toBe("Fix auth bug")
  })

  test("keeps a nested directory distinct from its worktree", () => {
    const payload = makePayload(
      "ses_abc",
      "Read",
      { path: "/home/user/project/README.md" },
      "/home/user/project/packages/api",
      "/home/user/project",
    )

    expect(payload.cwd).toBe("/home/user/project/packages/api")
    expect(payload).not.toHaveProperty("workspace_roots")
    expect(payload.session_title).toBeNull()
  })

  test("omits workspace_roots for the root sentinel", () => {
    const payload = makePayload(
      "ses_abc",
      "Read",
      { path: "/usr/bin/env" },
      "/home/user/project",
      "/",
    )

    expect(payload.cwd).toBe("/home/user/project")
    expect(payload).not.toHaveProperty("workspace_roots")
  })
})

describe("makeReadToolInput", () => {
  test("resolves a nested-directory read against the worktree", () => {
    expect(makeReadToolInput("README.md", "/home/user/project")).toEqual({
      path: "/home/user/project/README.md",
    })
  })

  test("preserves external traversal when resolving against the worktree", () => {
    expect(makeReadToolInput("../shared/config.json", "/home/user/project")).toEqual({
      path: "/home/user/shared/config.json",
    })
  })

  test("resolves root-sentinel patterns against root, not directory", () => {
    expect(makeReadToolInput("usr/bin/env", "/")).toEqual({ path: "/usr/bin/env" })
    expect(makeReadToolInput("home/user/project/README.md", "/")).toEqual({
      path: "/home/user/project/README.md",
    })
  })

  test("preserves an absolute target with the root sentinel", () => {
    expect(makeReadToolInput("/usr/bin/env", "/")).toEqual({ path: "/usr/bin/env" })
  })
})

describe("makeGlobToolInput", () => {
  test("includes the glob pattern and resolves an explicit target path", () => {
    expect(makeGlobToolInput(
      { pattern: "**/*.ts", path: "src" },
      "/home/user/project",
    )).toEqual({
      pattern: "**/*.ts",
      path: "/home/user/project/src",
    })
  })

  test("includes the glob pattern and defaults the target path to directory", () => {
    expect(makeGlobToolInput(
      { pattern: "**/*.ts" },
      "/home/user/project/packages/api",
    )).toEqual({
      pattern: "**/*.ts",
      path: "/home/user/project/packages/api",
    })
  })

  test("resolves the metadata.directory target alias", () => {
    expect(makeGlobToolInput(
      { pattern: "**/*.ts", directory: "src/generated" },
      "/home/user/project",
    )).toEqual({
      pattern: "**/*.ts",
      path: "/home/user/project/src/generated",
    })
  })

  test("preserves an unusual traversal-looking selector as raw context", () => {
    const pattern = "../../{*,.[!.]*}/**/[a-z]?(.ts)"

    expect(makeGlobToolInput({ pattern }, "/home/user/project")).toEqual({
      pattern,
      path: "/home/user/project",
    })
  })
})

describe("gateway cwd forwarding", () => {
  beforeEach(() => {
    approvalPayloads = []
    approvalGatewayResults = []
    sessionUpdateResults = []
  })

  test("forwards one absolute plugin directory unchanged for arbitrary MCP tool", async () => {
    const server = await loadPlugin()
    const directory = "/home/user/project/./crates/.."
    const hooks = await server(pluginInput(directory))
    await hooks.config!({ permission: { "*": "deny" } } as any)
    await ownSession(hooks)

    await hooks["tool.execute.before"]!(
      { sessionID: "session-1", callID: "call-1", tool: "mcp_weather_lookup" },
      { args: {} },
    )

    expect(approvalPayloads).toHaveLength(1)
    expect(approvalPayloads[0]?.tool_name).toBe("mcp_weather_lookup")
    expect(approvalPayloads[0]?.cwd).toBe(directory)
    expect(approvalPayloads[0]).not.toHaveProperty("workspace_roots")
  })

  test("rejects an empty plugin directory", async () => {
    const server = await loadPlugin()

    await expect(server(pluginInput(""))).rejects.toThrow(/directory/i)
    expect(approvalPayloads).toHaveLength(0)
  })

  test("rejects a relative plugin directory", async () => {
    const server = await loadPlugin()

    await expect(server(pluginInput("relative/project"))).rejects.toThrow(/absolute/i)
    expect(approvalPayloads).toHaveLength(0)
  })
})

describe("permission ownership and fail-closed routing", () => {
  beforeEach(() => {
    approvalGatewayResults = []
    sessionUpdateResults = []
    statusGatewayResults = []
    statusGatewayCalls = 0
  })

  test("config installs deny fallbacks for top-level and every configured agent", async () => {
    const hooks = await (await loadPlugin())(pluginInput("/home/user/project"))
    const config = {
      permission: { "*": "ask" },
      agent: { build: {}, review: { permission: { bash: "allow" } } },
    } as any

    await hooks.config!(config)

    expect(config.permission).toEqual({ "*": "deny" })
    expect(config.agent.build.permission).toEqual({ "*": "deny" })
    expect(config.agent.review.permission).toEqual({ "*": "deny" })
  })

  test("rejects tools for sessions without proven permission ownership", async () => {
    const hooks = await initializedHooks()
    await expect(hooks["tool.execute.before"]!(
      { sessionID: "unowned", callID: "call-unowned", tool: "bash" },
      { args: { command: "touch denied" } },
    )).rejects.toThrow(/has not transferred permission ownership/)
  })

  test.each([
    ["session update error", { error: { message: "update failed" } }],
    ["session update missing proof", { data: { permission: [] } }],
    ["session update rejection", new Error("update rejected")],
  ])("%s never establishes ownership", async (_name, response) => {
    sessionUpdateResults.push(response)
    const hooks = await initializedHooks()
    await expect(ownSession(hooks)).rejects.toThrow(/ownership failed/)
    await expect(hooks["tool.execute.before"]!(
      { sessionID: "session-1", callID: "call-failed-owner", tool: "bash" },
      { args: { command: "touch denied" } },
    )).rejects.toThrow(/has not transferred permission ownership/)
  })

  test("always denies leaked native permission prompts", async () => {
    const hooks = await initializedHooks()
    const output: { status: "ask" | "deny" | "allow" } = { status: "ask" }
    await hooks["permission.ask"]!({ id: "permission-1", sessionID: "session-1" } as any, output)
    expect(output.status).toBe("deny")
  })

  test("multiple plugin instances do not clear or share mutable state", async () => {
    const create = await loadPlugin()
    const first = await create(pluginInput("/home/user/project"))
    await first.config!({ permission: { "*": "deny" } } as any)
    await ownSession(first, "shared-session")

    const second = await create(pluginInput("/home/user/project"))
    await expect(second["tool.execute.before"]!(
      { sessionID: "shared-session", callID: "same-call", tool: "bash" },
      { args: { command: "true" } },
    )).rejects.toThrow(/config hook/)

    await first["tool.execute.before"]!(
      { sessionID: "shared-session", callID: "same-call", tool: "bash" },
      { args: { command: "true" } },
    )

    await second.config!({ permission: { "*": "deny" } } as any)
    await ownSession(second, "shared-session")
    await second["tool.execute.before"]!(
      { sessionID: "shared-session", callID: "same-call", tool: "bash" },
      { args: { command: "true" } },
    )
  })

  test.each([
    ["spawn", { spawnError: "binary missing" }, "spawn"],
    ["read", { readError: "stdout broke" }, "read"],
    ["write", { writeError: "stdin broke" }, "write"],
    ["signal", { signal: "SIGTERM" as NodeJS.Signals }, "signal"],
    ["nonzero", { code: 2, stderr: "invalid config detail" }, "nonzero"],
    ["malformed", { stdout: "not-json" }, "malformed"],
    ["oversized", { stdout: "界".repeat(400_000) }, "oversized"],
    ["schema", { stdout: '{"allowed":"yes"}' }, "schema"],
  ])("returns a bounded explicit denial for %s gateway failure", async (_name, fixture, category) => {
    approvalGatewayResults.push(fixture)
    const hooks = await initializedHooks()
    await ownSession(hooks)
    let message = ""
    try {
      await hooks["tool.execute.before"]!(
        { sessionID: "session-1", callID: `call-${category}`, tool: "bash" },
        { args: { command: "touch denied" } },
      )
    } catch (error) {
      message = (error as Error).message
    }
    expect(message).toContain("Agent Hub denied tool execution because the approval pipeline failed")
    expect(message).toContain(`(${category})`)
    if (category === "nonzero") expect(message).toContain("invalid config detail")
    expect(Buffer.byteLength(message)).toBeLessThanOrEqual(4096)
  })

  test("bounds and preserves an actual operator denial", async () => {
    approvalGatewayResults.push({ stdout: JSON.stringify({ allowed: false, reason: "界".repeat(2000) }) })
    const hooks = await initializedHooks()
    await ownSession(hooks)
    let message = ""
    try {
      await hooks["tool.execute.before"]!(
        { sessionID: "session-1", callID: "call-operator-denial", tool: "bash" },
        { args: { command: "touch denied" } },
      )
    } catch (error) {
      message = (error as Error).message
    }
    expect(message.startsWith("界")).toBe(true)
    expect(Buffer.byteLength(message)).toBeLessThanOrEqual(4096)
  })

  test("process timeout kills, reaps, and resolves once", async () => {
    approvalGatewayResults.push({ hang: true })
    const result = await runGatewayProcess({
      bin: "gateway",
      args: ["approval"],
      input: "{}",
      timeoutMs: 1,
    })
    expect(result).toMatchObject({ ok: false, category: "timeout" })
  })

  test.each([
    ["nonzero", { code: 1, stderr: "status unavailable" }],
    ["oversized", { stderr: "x".repeat(1024 * 1024 + 1) }],
  ])("status %s failure is bounded, non-authoritative, and retried", async (_name, fixture) => {
    statusGatewayResults.push(fixture)
    const hooks = await initializedHooks()
    await ownSession(hooks)
    await hooks["tool.execute.before"]!(
      { sessionID: "session-1", callID: `call-status-1-${_name}`, tool: "bash" },
      { args: { command: "true" } },
    )
    await flushQuestionCallback()
    await hooks["tool.execute.before"]!(
      { sessionID: "session-1", callID: `call-status-2-${_name}`, tool: "bash" },
      { args: { command: "true" } },
    )
    expect(statusGatewayCalls).toBe(2)
  })

})

describe("question gateway outcomes", () => {
  beforeEach(() => {
    questionGatewayResults = []
    questionReplies = []
    questionRejections = []
    questionReplyResults = []
    questionRejectResults = []
  })

  test("explicit rejection calls reject and does not reply", async () => {
    questionGatewayResults.push({ code: 1 })
    const hooks = await initializedHooks()

    await hooks.event!(questionAsked())
    await flushQuestionCallback()

    expect(questionRejections).toEqual([{ requestID: "question-1", directory: "/home/user/project" }])
    expect(questionReplies).toHaveLength(0)
  })

  test("timeout replies with one exact synthetic answer per question", async () => {
    questionGatewayResults.push({ code: 3 })
    const hooks = await initializedHooks()

    await hooks.event!(questionAsked(3))
    await flushQuestionCallback()

    const message = "Agent Hub denied this request because the approval request timed out before an operator decision."
    expect(questionReplies).toHaveLength(1)
    expect(questionReplies[0]?.answers).toEqual([[message], [message], [message]])
    expect(questionRejections).toHaveLength(0)
  })

  test.each([
    ["pipeline exit", { code: 4 }],
    ["fail-closed exit", { code: 2 }],
    ["unexpected exit", { code: 9 }],
    ["spawn failure", { spawnError: "spawn failed" }],
    ["malformed output", { code: 0, stdout: "not json" }],
    ["answer cardinality mismatch", { code: 0, stdout: '{"answers":[["one answer"]]}' }],
    ["oversized output", { code: 0, stdout: "x".repeat(1024 * 1024 + 1) }],
  ])("%s replies with explicit pipeline denial and never rejects", async (_name, result) => {
    questionGatewayResults.push(result)
    const hooks = await initializedHooks()

    await hooks.event!(questionAsked(2))
    await flushQuestionCallback()

    expect(questionReplies).toHaveLength(1)
    expect(questionReplies[0]?.answers).toHaveLength(2)
    expect(questionReplies[0]?.answers[0]?.[0]).toContain("Agent Hub denied tool execution because the approval pipeline failed")
    expect(Buffer.byteLength(questionReplies[0]?.answers[0]?.[0] ?? "")).toBeLessThanOrEqual(4096)
    expect(questionRejections).toHaveLength(0)
  })

  test("unexpected question reply failure returns a visible pipeline error", async () => {
    questionGatewayResults.push({ stdout: '{"answers":[["yes"],["no"]]}' })
    questionReplyResults.push({ error: { message: "reply failed" } })
    const hooks = await initializedHooks()

    await hooks.event!(questionAsked(2))
    await flushQuestionCallback()

    expect(questionReplies).toHaveLength(2)
    expect(questionReplies[1]?.answers[0]?.[0]).toContain("approval pipeline failed")
  })

  test("unexpected question reject failure returns a visible pipeline error", async () => {
    questionGatewayResults.push({ code: 1 })
    questionRejectResults.push({ error: { message: "reject failed" } })
    const hooks = await initializedHooks()

    await hooks.event!(questionAsked(2))
    await flushQuestionCallback()

    expect(questionRejections).toHaveLength(1)
    expect(questionReplies[0]?.answers[0]?.[0]).toContain("approval pipeline failed")
  })

  test("failed visible pipeline reply terminates the question", async () => {
    questionGatewayResults.push({ spawnError: "gateway missing" })
    questionReplyResults.push(
      { error: { message: "reply failed" } },
      { error: { message: "fallback reply failed" } },
    )
    const hooks = await initializedHooks()

    await hooks.event!(questionAsked(2))
    await flushQuestionCallback()

    expect(questionRejections).toEqual([{ requestID: "question-1", directory: "/home/user/project" }])
  })
})

describe("DEFAULT_TITLE_RE", () => {
  test("matches default titles but not generated titles", () => {
    expect(DEFAULT_TITLE_RE.test("New session - 2026-04-01T22:18:18")).toBe(true)
    expect(DEFAULT_TITLE_RE.test("Child session - 2026-04-01T10:00:00")).toBe(true)
    expect(DEFAULT_TITLE_RE.test("Debugging production 500 errors")).toBe(false)
  })
})
