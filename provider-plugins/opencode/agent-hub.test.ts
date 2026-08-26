import { beforeEach, describe, expect, mock, test } from "bun:test"
import { EventEmitter } from "events"
import type { OpenCodeHookInput } from "./generated/gateway-types"

let approvalPayloads: OpenCodeHookInput[] = []
let questionGatewayResults: Array<{
  code?: number
  stdout?: string
  stderr?: string
  spawnError?: string
}> = []
let questionReplies: Array<{ requestID: string; directory: string; answers: string[][] }> = []
let questionRejections: Array<{ requestID: string; directory: string }> = []

mock.module("fs", () => ({
  default: { appendFileSync: () => {} },
  appendFileSync: () => {},
}))

mock.module("@opencode-ai/sdk/v2", () => ({
  createOpencodeClient: () => ({
    question: {
      reply: async (input: { requestID: string; directory: string; answers: string[][] }) => {
        questionReplies.push(input)
        return {}
      },
      reject: async (input: { requestID: string; directory: string }) => {
        questionRejections.push(input)
        return {}
      },
    },
  }),
}))

mock.module("child_process", () => ({
  spawn: (_bin: string, args: string[]) => {
    const proc = new EventEmitter() as EventEmitter & {
      stdin: { end: (payload: string) => void }
      stdout: EventEmitter
      stderr: EventEmitter
      kill: () => boolean
    }
    proc.stdout = new EventEmitter()
    proc.stderr = new EventEmitter()
    proc.kill = () => true
    proc.stdin = {
      end: (payload: string) => {
        if (args[0] === "approval") {
          approvalPayloads.push(JSON.parse(payload) as OpenCodeHookInput)
          queueMicrotask(() => {
            proc.stdout.emit("data", Buffer.from('{"allowed":true}'))
            proc.emit("close", 0)
          })
        }
        if (args[0] === "question") {
          const result = questionGatewayResults.shift()
          if (!result) throw new Error("missing question gateway test result")
          queueMicrotask(() => {
            if (result.spawnError) {
              proc.emit("error", new Error(result.spawnError))
              return
            }
            if (result.stdout) proc.stdout.emit("data", Buffer.from(result.stdout))
            if (result.stderr) proc.stderr.emit("data", Buffer.from(result.stderr))
            proc.emit("close", result.code ?? 0)
          })
        }
      },
    }
    return proc
  },
  spawnSync: () => ({ status: 0 }),
}))

async function loadPlugin() {
  const { default: plugin } = await import(`./agent-hub.ts?test=${Date.now()}-${Math.random()}`)
  return plugin.server
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

function arbitraryMcpPermission() {
  return {
    permission: "mcp_weather_lookup",
    patterns: [],
    metadata: {},
    sessionID: "session-1",
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
  })

  test("forwards one absolute plugin directory unchanged for arbitrary MCP permission", async () => {
    const server = await loadPlugin()
    const directory = "/home/user/project/./crates/.."
    const hooks = await server(pluginInput(directory))
    const output = { status: "ask" as const }

    await hooks["permission.ask"]!(arbitraryMcpPermission(), output)

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

describe("question gateway outcomes", () => {
  beforeEach(() => {
    questionGatewayResults = []
    questionReplies = []
    questionRejections = []
  })

  test("explicit rejection calls reject and does not reply", async () => {
    questionGatewayResults.push({ code: 1 })
    const hooks = await (await loadPlugin())(pluginInput("/home/user/project"))

    await hooks.event!(questionAsked())
    await flushQuestionCallback()

    expect(questionRejections).toEqual([{ requestID: "question-1", directory: "/home/user/project" }])
    expect(questionReplies).toHaveLength(0)
  })

  test("timeout replies with one exact synthetic answer per question", async () => {
    questionGatewayResults.push({ code: 3 })
    const hooks = await (await loadPlugin())(pluginInput("/home/user/project"))

    await hooks.event!(questionAsked(3))
    await flushQuestionCallback()

    const message = "Your approval request timed out because the operator was away. This is not a denial. Feel free to retry if you still need this."
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
  ])("%s replies with neutral pipeline errors and never rejects", async (_name, result) => {
    questionGatewayResults.push(result)
    const hooks = await (await loadPlugin())(pluginInput("/home/user/project"))

    await hooks.event!(questionAsked(2))
    await flushQuestionCallback()

    const message = "There was an error in the approvals pipeline. This is a null answer: it is neither an approval nor a rejection of your intended action. Do not take this as a signal that your intended path is unwanted. Try again; if the error recurs, abort and report it to the operator."
    expect(questionReplies).toHaveLength(1)
    expect(questionReplies[0]?.answers).toEqual([[message], [message]])
    expect(questionRejections).toHaveLength(0)
  })
})

describe("DEFAULT_TITLE_RE", () => {
  test("matches default titles but not generated titles", () => {
    expect(DEFAULT_TITLE_RE.test("New session - 2026-04-01T22:18:18")).toBe(true)
    expect(DEFAULT_TITLE_RE.test("Child session - 2026-04-01T10:00:00")).toBe(true)
    expect(DEFAULT_TITLE_RE.test("Debugging production 500 errors")).toBe(false)
  })
})
