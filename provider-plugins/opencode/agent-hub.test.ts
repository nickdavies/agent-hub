import { afterAll, beforeEach, describe, expect, mock, spyOn, test } from "bun:test"
import fs from "fs"
import { defaultRuntime, requestSchema } from "./agent-hub-run"
import { EventEmitter } from "events"
import { mkdirSync, rmSync } from "node:fs"
import path from "node:path"
import { createHash } from "node:crypto"
import type { OpenCodeHookInput } from "./generated/gateway-types"
import { AgentHubCatalogService, catalogRequestSchema, validateCatalog } from "./agent-hub-catalog"

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

function testCatalog() {
  const value = {
    schema: "agent_hub_catalog_v1",
    fingerprint: "",
    environments: [{
      name: "code-auto",
      description: "Code safely",
      image_family: "job-code",
      network_options: [{ network_class: "none", resources: [] }],
      tool_approvals: ["auto"],
      overlays: [],
      max_timeout_secs: 600,
    }],
    images: [{ family: "job-code", description: "Job image", tool_facts: { status: "unavailable" } }],
    overlays: [],
    guidance: {
      limits: { max_prompt_bytes: 131072, max_workspaces: 16, max_context_roots: 16, max_overlays: 16 },
      outputs: {
        structured_result: true, artifact_manifest: true, repository_result: true, cleanup_result: true,
        stdout_bytes: 16777216, stderr_bytes: 16777216, transcript_bytes: 33554432, artifact_bytes: 4194304,
      },
      workspace: {
        workspace_required: true, primary_workspace_required: true, context_roots_read_only: true,
        workspace_export_supported: true, workspace_export_is_untrusted: true,
      },
    },
  }
  value.fingerprint = createHash("sha256").update("agent-hub:catalog:v1\0").update(JSON.stringify(value)).digest("hex")
  return validateCatalog(value)
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
    approvalPayloads = []
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
    ["session update error with proof", { error: { message: "update failed" }, data: { permission: [{ permission: "*", pattern: "*", action: "allow" }] } }],
    ["session update missing data", {}],
    ["session update missing permissions", { data: {} }],
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

  test.each([
    ["seeded child denies", [
      { permission: "todowrite", pattern: "*", action: "deny" },
      { permission: "task", pattern: "*", action: "deny" },
      { permission: "read", pattern: "*", action: "ask" },
      { permission: "*", pattern: "*", action: "allow" },
    ]],
    ["reordered rule fields", [{ action: "allow", pattern: "*", permission: "*" }]],
  ])("accepts %s and still routes tools through the gateway", async (_name, permission) => {
    sessionUpdateResults.push({ data: { permission } })
    const hooks = await initializedHooks()
    await ownSession(hooks)
    approvalGatewayResults.push({ stdout: '{"allowed":false,"reason":"gateway denied"}' })
    await expect(hooks["tool.execute.before"]!(
      { sessionID: "session-1", callID: "call-owned", tool: "bash" },
      { args: { command: "touch denied" } },
    )).rejects.toThrow("gateway denied")
  })

  test("repeated chat messages accept accumulated wildcard allows", async () => {
    const hooks = await initializedHooks()
    const permission = [{ permission: "task", pattern: "*", action: "deny" }]
    for (let index = 0; index < 3; index++) {
      permission.push({ permission: "*", pattern: "*", action: "allow" })
      sessionUpdateResults.push({ data: { permission: [...permission] } })
      await ownSession(hooks)
      await hooks["tool.execute.before"]!(
        { sessionID: "session-1", callID: `call-repeat-${index}`, tool: "bash" },
        { args: { command: "true" } },
      )
    }
  })

  test.each([
    ["null ruleset", null],
    ["non-array ruleset", { permission: "*", pattern: "*", action: "allow" }],
    ["null rule", [null, { permission: "*", pattern: "*", action: "allow" }]],
    ["array rule", [[], { permission: "*", pattern: "*", action: "allow" }]],
    ["missing permission", [{ pattern: "*", action: "deny" }, { permission: "*", pattern: "*", action: "allow" }]],
    ["non-string pattern", [{ permission: "bash", pattern: 42, action: "deny" }, { permission: "*", pattern: "*", action: "allow" }]],
    ["invalid action", [{ permission: "bash", pattern: "*", action: "invalid" }, { permission: "*", pattern: "*", action: "allow" }]],
    ["non-wildcard permission", [{ permission: "bash", pattern: "*", action: "allow" }]],
    ["non-wildcard pattern", [{ permission: "*", pattern: "git *", action: "allow" }]],
    ["trailing deny", [{ permission: "*", pattern: "*", action: "allow" }, { permission: "bash", pattern: "*", action: "deny" }]],
    ["trailing ask", [{ permission: "*", pattern: "*", action: "allow" }, { permission: "*", pattern: "*", action: "ask" }]],
  ])("rejects %s and invalidates prior ownership", async (_name, permission) => {
    const hooks = await initializedHooks()
    await ownSession(hooks)
    sessionUpdateResults.push({ data: { permission } })
    await expect(ownSession(hooks)).rejects.toThrow(/ownership failed/)
    await expect(hooks["tool.execute.before"]!(
      { sessionID: "session-1", callID: "call-invalidated-owner", tool: "bash" },
      { args: { command: "touch denied" } },
    )).rejects.toThrow(/has not transferred permission ownership/)
  })

  test("always denies leaked native permission prompts", async () => {
    const hooks = await initializedHooks()
    const output: { status: "ask" | "deny" | "allow" } = { status: "ask" }
    const logger = spyOn(fs, "appendFileSync").mockImplementation(() => { throw new Error("log unavailable") })
    try {
      await hooks["permission.ask"]!({ id: "permission-1", sessionID: "session-1" } as any, output)
      expect(output.status).toBe("deny")
      expect(logger).not.toHaveBeenCalled()
    } finally {
      logger.mockRestore()
    }
  })

  test("host launch requires gateway authorization bound to the exact session, call, and structured input", async () => {
    const hooks = await initializedHooks()
    await ownSession(hooks)
    const launch = hooks.tool?.agent_hub_run
    expect(launch).toBeDefined()
    const args = requestSchema.parse({
      schema: "agent_hub_run_intent_v1", environment: "code-auto", image_family: "job-code",
      network_class: "none", tool_approval: "auto", timeout_secs: 60, prompt: "test",
      workspaces: [{ alias: "source", path: "/work/source", access: "read_write" }], primary_workspace: "source",
    })
    const context = {
      sessionID: "session-1", callID: "host-call", abort: new AbortController().signal,
      metadata: () => {}, ask: async () => { throw new Error("native prompt must not run") },
    } as any
    const priorBinary = process.env.AGENT_HUB_BIN
    process.env.AGENT_HUB_BIN = "/fixed/agent-hub"
    const executable = spyOn(defaultRuntime, "assertExecutable").mockResolvedValue(undefined)
    const execute = spyOn(defaultRuntime, "execute").mockRejectedValue(new Error("host reached"))
    try {
      await expect(launch!.execute(args, context)).rejects.toThrow(/authorization/)
      expect(executable).not.toHaveBeenCalled()
      approvalGatewayResults = [{ stdout: '{"allowed":false,"reason":"operator denied launch"}' }]
      await expect(hooks["tool.execute.before"]!(
        { sessionID: context.sessionID, callID: context.callID, tool: "agent_hub_run" }, { args },
      )).rejects.toThrow("operator denied launch")
      await expect(launch!.execute(args, context)).rejects.toThrow(/authorization/)
      expect(execute).not.toHaveBeenCalled()

      context.callID = "transport-failed-host-call"
      approvalGatewayResults = [{ spawnError: "gateway unavailable" }]
      await expect(hooks["tool.execute.before"]!(
        { sessionID: context.sessionID, callID: context.callID, tool: "agent_hub_run" }, { args },
      )).rejects.toThrow(/pipeline failed/)
      await expect(launch!.execute(args, context)).rejects.toThrow(/authorization/)
      expect(executable).not.toHaveBeenCalled()
      expect(execute).not.toHaveBeenCalled()

      context.callID = "allowed-host-call"
      await hooks["tool.execute.before"]!(
        { sessionID: context.sessionID, callID: context.callID, tool: "agent_hub_run" }, { args },
      )
      expect(approvalPayloads.at(-1)?.tool_name).toBe("agent_hub_run")
      expect(approvalPayloads.at(-1)?.tool_input).toEqual(args)
      expect(approvalPayloads.at(-1)?.tool_match_input).toEqual(args)
      await expect(launch!.execute({ ...args, prompt: "changed" }, context)).rejects.toThrow(/authorization/)
      await expect(launch!.execute(args, { ...context, sessionID: "other" })).rejects.toThrow(/authorization/)
      await expect(launch!.execute(args, { ...context, callID: "other" })).rejects.toThrow(/authorization/)
      sessionUpdateResults = [{ error: "ownership lost" }]
      await expect(ownSession(hooks)).rejects.toThrow(/ownership failed/)
      await expect(launch!.execute(args, context)).rejects.toThrow(/authorization/)
      expect(executable).not.toHaveBeenCalled()
      expect(execute).not.toHaveBeenCalled()
      await ownSession(hooks)
      await expect(launch!.execute(args, context)).rejects.toThrow("host reached")
      expect(execute).toHaveBeenCalledTimes(1)
      await expect(launch!.execute(args, context)).rejects.toThrow(/authorization/)
      expect(execute).toHaveBeenCalledTimes(1)
    } finally {
      executable.mockRestore()
      execute.mockRestore()
      if (priorBinary === undefined) delete process.env.AGENT_HUB_BIN
      else process.env.AGENT_HUB_BIN = priorBinary
      await hooks.dispose?.()
    }
  })

  test("registers catalog only in HTTP mode and menu lookup uses only toolID without gateway routing", async () => {
    const lookup = spyOn(AgentHubCatalogService.prototype, "get").mockResolvedValue(testCatalog())
    const hooks = await initializedHooks()
    try {
      expect(Object.keys(hooks.tool ?? {}).sort()).toEqual(["agent_hub_catalog", "agent_hub_run"])
      const output = { description: "generic", args: {} } as any
      await hooks["tool.definition"]!({ toolID: "agent_hub_run" } as any, output)
      expect(output.description).toContain('"code-auto"')
      expect(approvalPayloads).toHaveLength(0)
      expect(lookup).toHaveBeenCalledTimes(1)

      await hooks["tool.definition"]!({ toolID: "bash" } as any, output)
      expect(lookup).toHaveBeenCalledTimes(1)
    } finally {
      lookup.mockRestore()
      await hooks.dispose?.()
    }
  })

  test("catalog detail stays behind ownership and gateway authorization and consumes allow once before cache access", async () => {
    const lookup = spyOn(AgentHubCatalogService.prototype, "get").mockResolvedValue(testCatalog())
    const hooks = await initializedHooks()
    await ownSession(hooks)
    const detail = hooks.tool?.agent_hub_catalog
    const args = catalogRequestSchema.parse({
      operation: "detail",
      sections: ["environments"], selectors: [{ kind: "environment", name: "code-auto" }],
    })
    const context = {
      sessionID: "session-1", callID: "catalog-no-route", abort: new AbortController().signal,
      metadata: () => {}, ask: async () => { throw new Error("native prompt must not run") },
    } as any
    try {
      await expect(detail!.execute(args, context)).rejects.toThrow(/authorization/)
      expect(lookup).not.toHaveBeenCalled()

      context.callID = "catalog-denied"
      approvalGatewayResults.push({ stdout: '{"allowed":false,"reason":"catalog denied"}' })
      await expect(hooks["tool.execute.before"]!(
        { sessionID: context.sessionID, callID: context.callID, tool: "agent_hub_catalog" }, { args },
      )).rejects.toThrow("catalog denied")
      await expect(detail!.execute(args, context)).rejects.toThrow(/authorization/)
      expect(lookup).not.toHaveBeenCalled()

      context.callID = "catalog-allowed"
      await hooks["tool.execute.before"]!(
        { sessionID: context.sessionID, callID: context.callID, tool: "agent_hub_catalog" }, { args },
      )
      expect(approvalPayloads.at(-1)?.tool_name).toBe("agent_hub_catalog")
      expect(JSON.parse(String(await detail!.execute(args, context))).environments[0].name).toBe("code-auto")
      expect(lookup).toHaveBeenCalledTimes(1)
      expect(lookup).toHaveBeenCalledWith(context.abort)
      await expect(detail!.execute(args, context)).rejects.toThrow(/already consumed/)
      expect(lookup).toHaveBeenCalledTimes(1)
    } finally {
      lookup.mockRestore()
      await hooks.dispose?.()
    }
  })

  test("menu hook replaces generic text with explicit catalog-unavailable text", async () => {
    const lookup = spyOn(AgentHubCatalogService.prototype, "get").mockRejectedValue(new Error("missing trusted catalog"))
    const hooks = await initializedHooks()
    try {
      const output = { description: "generic", args: {} } as any
      await hooks["tool.definition"]!({ toolID: "agent_hub_run" } as any, output)
      expect(output.description).toContain("catalog unavailable")
      expect(output.description).not.toContain("missing trusted catalog")
      expect(output.description).toContain("No cached or fallback capability menu")
      expect(output.description).not.toBe("generic")
    } finally {
      lookup.mockRestore()
      await hooks.dispose?.()
    }
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
