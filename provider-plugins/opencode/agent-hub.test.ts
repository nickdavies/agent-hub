import { afterEach, describe, expect, test } from "bun:test"
import { createOpencodeClient } from "@opencode-ai/sdk"
import type { PluginInput } from "@opencode-ai/plugin"
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "fs"
import os from "os"
import path from "path"
import { createPluginInstance, makePayload, runGatewayProcess } from "./agent-hub"

const cleanup: Array<() => void | Promise<void>> = []
afterEach(async () => { for (const fn of cleanup.splice(0).reverse()) await fn() })

async function fixture(options: { approval?: string; question?: string; status?: string; handlers?: boolean; nested?: boolean; server?: string } = {}) {
  const dir = mkdtempSync(path.join(os.tmpdir(), "agent-hub-plugin-test-"))
  cleanup.push(() => rmSync(dir, { recursive: true, force: true }))
  const gateway = path.join(dir, "gateway")
  const capture = path.join(dir, "calls.jsonl")
  const directory = options.nested ? path.join(dir, "repo/sub") : dir
  const worktree = options.nested ? path.join(dir, "repo") : dir
  mkdirSync(directory, { recursive: true })
  writeFileSync(gateway, `#!/usr/bin/env bun
import fs from "fs";
const payload = await Bun.stdin.text();
fs.appendFileSync(${JSON.stringify(capture)}, JSON.stringify({args:process.argv.slice(2),payload:JSON.parse(payload)})+"\\n");
switch(process.argv[2]) {
case "approval": ${options.approval ?? 'console.log(JSON.stringify({allowed:true}))'}; break;
case "question": ${options.question ?? 'console.log(JSON.stringify({answers:[["Yes"]]}))'}; break;
case "status-report": ${options.status ?? "process.exit(0)"}; break;
}
`)
  chmodSync(gateway, 0o755)
  const original = { ...process.env }
  process.env.AGENT_HUB_GATEWAY = gateway
  process.env.AGENT_HUB_SERVER = options.server ?? "http://localhost:1234"
  process.env.AGENT_HUB_CONFIG = path.join(dir, "policy.json")
  process.env.XDG_DATA_HOME = dir
  delete process.env.AGENT_HUB_TOKEN
  cleanup.push(() => {
    for (const key of Object.keys(process.env)) if (!(key in original)) delete process.env[key]
    Object.assign(process.env, original)
  })
  let permissions: unknown = [{ permission: "read", pattern: "*", action: "deny" }, { permission: "*", pattern: "*", action: "allow" }]
  let ownershipError = false
  let replyError = false
  let rejectError = false
  const requests: Array<{ method: string; url: string; body: any }> = []
  const client = createOpencodeClient({ baseUrl: "http://in-process.invalid", fetch: (async (request: Request) => {
    const url = new URL(request.url)
    const text = request.method === "GET" ? "" : await request.text()
    const body = text ? JSON.parse(text) : undefined
    requests.push({ method: request.method, url: url.pathname, body })
    if (url.pathname.endsWith("/reply") && replyError) return Response.json({ error: "reply unavailable" }, { status: 500 })
    if (url.pathname.endsWith("/reject") && rejectError) return Response.json({ error: "reject unavailable" }, { status: 500 })
    if (request.method === "PATCH") return Response.json(ownershipError ? { error: "unavailable" } : { permission: permissions }, { status: ownershipError ? 500 : 200 })
    return Response.json(request.method === "GET" ? { title: "Useful session title" } : true)
  }) as typeof fetch })
  const input = { client, directory, worktree, serverUrl: new URL("http://unavailable.invalid") } as PluginInput
  const hooks = await createPluginInstance(input, { registerProcessHandlers: options.handlers ?? false })
  cleanup.push(() => hooks.dispose!())
  const before = (callID = "call", args: unknown = { command: "pwd" }, tool = "bash", sessionID = "session") =>
    hooks["tool.execute.before"]!({ callID, tool, sessionID }, { args })
  const after = (callID = "call", args: unknown = { command: "pwd" }, tool = "bash", sessionID = "session") =>
    hooks["tool.execute.after"]!({ callID, args, tool, sessionID }, { title: "", output: "", metadata: {} })
  const own = (sid = "session") => hooks["chat.message"]!({ sessionID: sid }, {} as never)
  const event = (type: string, properties: unknown) => hooks.event!({ event: { type, properties } } as never)
  const calls = () => { try { return readFileSync(capture, "utf8").trim().split("\n").filter(Boolean).map((line) => JSON.parse(line)) } catch { return [] } }
  const ready = async () => { await hooks.config!({}); await own() }
  return { hooks, dir, input, before, after, own, event, ready, calls, requests,
    setPermissions: (value: unknown) => { permissions = value },
    setOwnershipError: () => { ownershipError = true },
    setReplyError: () => { replyError = true },
    setRejectError: () => { rejectError = true } }
}

async function eventually(predicate: () => boolean) {
  for (let i = 0; i < 100; i++) { if (predicate()) return; await Bun.sleep(10) }
  throw new Error("condition not met")
}

describe("session ownership and execution lifecycle", () => {
  test("default-denies global and agent policy, requires proved ownership via in-process client", async () => {
    const f = await fixture()
    await expect(f.before()).rejects.toThrow("permission ownership")
    const config: Record<string, unknown> = { permission: "allow", agent: { build: { permission: "allow" } } }
    await f.hooks.config!(config as never)
    expect(config).toEqual({ permission: { "*": "deny" }, agent: { build: { permission: { "*": "deny" } } } })
    await expect(f.before()).rejects.toThrow("permission ownership")
    await f.own()
    expect(f.requests[0].body).toEqual({ permission: [{ permission: "*", pattern: "*", action: "allow" }] })
    await f.before()
    await f.after()
    const approval = f.calls().find((call) => call.args[0] === "approval")
    expect(approval.payload.tool_input).toEqual({ command: "pwd" })
    expect(approval.payload.tool_match_input).toEqual({ command: "pwd" })
    expect(approval.payload.session_title).toBe("Useful session title")
    expect(approval.args).toEqual(["approval", "--opencode", "--server", "http://localhost:1234", "--config", path.join(f.dir, "policy.json")])
  })
  test("accepts appended structural wildcard rules but revokes ownership on failure", async () => {
    const f = await fixture()
    await f.ready()
    f.setPermissions([{ permission: "*", pattern: "*", action: "allow" }, { permission: "bash", pattern: "*", action: "deny" }])
    await expect(f.own()).rejects.toThrow("final wildcard")
    await expect(f.before()).rejects.toThrow("permission ownership")
    f.setPermissions([{ permission: "*", pattern: "*", action: "allow", extra: true }])
    await f.own()
    f.setOwnershipError()
    await expect(f.own()).rejects.toThrow("ownership failed")
    await expect(f.before()).rejects.toThrow("permission ownership")
  })
  test("rejects malformed ownership responses", async () => {
    const f = await fixture()
    await f.hooks.config!({})
    for (const value of [null, [], [{ permission: "*", action: "allow" }], [{ permission: "*", pattern: "*", action: "maybe" }]]) {
      f.setPermissions(value)
      await expect(f.own()).rejects.toThrow("ownership failed")
    }
  })
  test("leaked native prompts deny even when logging cannot create its file", async () => {
    const f = await fixture()
    const output = { status: "ask" as "ask" | "allow" | "deny" }
    await f.hooks["permission.ask"]!({} as never, output)
    expect(output.status).toBe("deny")
  })
  test("reserves call IDs before awaits and rejects duplicate completion and argument mutation", async () => {
    const f = await fixture({ approval: 'await Bun.sleep(50); console.log("{\\"allowed\\":true}")' })
    await f.ready()
    const pending = f.before()
    await expect(f.before()).rejects.toThrow("reused")
    await pending
    await expect(f.after("call", { command: "other" })).rejects.toThrow("mismatch")
    await expect(f.after()).rejects.toThrow("unauthorized")
    await f.before("next")
    await f.after("next")
    await expect(f.after("next")).rejects.toThrow("duplicate")
    await expect(f.after("unrouted")).rejects.toThrow("unauthorized")
  })
  test("rejects mutation while waiting and permission revocation during routing", async () => {
    const f = await fixture({ approval: 'await Bun.sleep(50); console.log("{\\"allowed\\":true}")' })
    await f.ready()
    const args = { command: "pwd" }
    const pending = f.before("mutated", args)
    args.command = "rm -rf /"
    await expect(pending).rejects.toThrow("changed during approval")
    const revoked = f.before("revoked")
    await f.hooks.config!({})
    await f.own()
    await expect(revoked).rejects.toThrow("changed during approval")
  })
  test("all patch source and destination paths must approve before any execution", async () => {
    const f = await fixture({ approval: 'console.log(JSON.stringify({allowed:!JSON.parse(payload).tool_match_input.path.endsWith("denied"),reason:"blocked destination"}))' })
    await f.ready()
    const args = { patchText: "*** Begin Patch\n*** Update File: source\n*** Move to: denied\n*** Add File: later\n+x\n*** End Patch" }
    await expect(f.before("patch", args, "apply_patch")).rejects.toThrow("blocked destination")
    const calls = f.calls().filter((call) => call.args[0] === "approval")
    expect(calls.map((call) => call.payload.tool_match_input.path)).toEqual([path.join(f.dir, "source"), path.join(f.dir, "denied")])
    expect(calls.every((call) => JSON.stringify(call.payload.tool_input) === JSON.stringify(args))).toBe(true)
    await expect(f.after("patch", args, "apply_patch")).rejects.toThrow("unauthorized")
  })
  test("SDK error and invalid JSON/schema/nonzero never authorize", async () => {
    for (const code of ['console.log("oops")', 'console.log("{\\"allowed\\":\\"yes\\"}")', 'console.log("{\\"allowed\\":true,\\"reason\\":42}")', 'process.exit(2)']) {
      const f = await fixture({ approval: code })
      await f.ready()
      await expect(f.before()).rejects.toThrow("blocked")
      await expect(f.after()).rejects.toThrow("unauthorized")
    }
  })
  test("independent instances do not share ownership and remove only their listeners", async () => {
    const initial = process.listenerCount("exit")
    const f = await fixture({ handlers: true })
    const other = await createPluginInstance(f.input)
    cleanup.push(() => other.dispose!())
    expect(process.listenerCount("exit")).toBe(initial + 2)
    await f.ready()
    await expect(other["tool.execute.before"]!({ sessionID: "session", callID: "call", tool: "bash" }, { args: { command: "pwd" } })).rejects.toThrow("ownership")
    await f.hooks.dispose!()
    await f.hooks.dispose!()
    expect(process.listenerCount("exit")).toBe(initial + 1)
    await expect(f.before()).rejects.toThrow("disposed")
    await other.dispose!()
    expect(process.listenerCount("exit")).toBe(initial)
  })
  test("dispose cancels pending gateway operations", async () => {
    const f = await fixture({ approval: "await Bun.sleep(100000)" })
    await f.ready()
    const pending = f.before()
    await eventually(() => f.calls().some((call) => call.args[0] === "approval"))
    await f.hooks.dispose!()
    await expect(pending).rejects.toThrow("blocked")
  })
  test("abort revokes ownership, kills pending children and prevents after-call acceptance", async () => {
    const f = await fixture({ approval: 'await Bun.sleep(50); console.log("{\\"allowed\\":true}")' })
    await f.ready()
    await f.before("authorized")
    const pending = f.before("waiting")
    await eventually(() => f.calls().filter((call) => call.args[0] === "approval").length === 2)
    await f.event("session.error", { sessionID: "session", error: { name: "MessageAbortedError" } })
    await expect(pending).rejects.toThrow("blocked")
    await expect(f.before("after-abort")).rejects.toThrow("ownership")
    await expect(f.after("authorized")).rejects.toThrow("unauthorized")
    await eventually(() => f.calls().some((call) => call.payload.status === "ended"))
  })
  test("successful multi-path patches route all targets and retain all raw arguments", async () => {
    const f = await fixture()
    await f.ready()
    const args = { patchText: "*** Begin Patch\n*** Update File: source\n*** Move to: destination\n*** Delete File: old\n*** Add File: new\n+x\n*** End Patch" }
    await f.before("patch", args, "apply_patch")
    await f.after("patch", args, "apply_patch")
    const approvals = f.calls().filter((call) => call.args[0] === "approval")
    expect(approvals.map((call) => call.payload.tool_match_input.path)).toEqual(["source", "destination", "old", "new"].map((file) => path.join(f.dir, file)))
    expect(approvals.every((call) => JSON.stringify(call.payload.tool_input) === JSON.stringify(args))).toBe(true)
  })
  test("unknown/MCP executions retain exact tool names and arguments", async () => {
    const f = await fixture()
    await f.ready()
    const args = { nested: { key: [1, "two", false] } }
    await f.before("mcp", args, "MCP_Remote/Lookup")
    const payload = f.calls().find((call) => call.args[0] === "approval").payload
    expect(payload.tool_name).toBe("MCP_Remote/Lookup")
    expect(payload.tool_input).toEqual(args)
    expect(payload.tool_match_input).toEqual(args)
  })
  test("token is conditional and initialization snapshots invocation settings", async () => {
    const f = await fixture()
    process.env.AGENT_HUB_TOKEN = "test-only-token"
    const hooks = await createPluginInstance(f.input, { registerProcessHandlers: false })
    cleanup.push(() => hooks.dispose!())
    delete process.env.AGENT_HUB_TOKEN
    process.env.AGENT_HUB_GATEWAY = "/nonexistent/new-setting"
    await hooks.config!({})
    await hooks["chat.message"]!({ sessionID: "session" }, {} as never)
    await hooks["tool.execute.before"]!({ sessionID: "session", callID: "token", tool: "bash" }, { args: { command: "pwd" } })
    expect(f.calls().find((call) => call.args[0] === "approval").args).toContain("test-only-token")
  })
  test("title caching and debounced retry status remain instance-local", async () => {
    const f = await fixture()
    await f.ready()
    await f.before("one")
    await f.before("two")
    expect(f.requests.filter((request) => request.method === "GET")).toHaveLength(1)
    await f.event("session.status", { sessionID: "session", status: { type: "idle" } })
    await f.event("session.status", { sessionID: "session", status: { type: "retry" } })
    await Bun.sleep(2100)
    await eventually(() => f.calls().filter((call) => call.args[0] === "status-report").length === 2)
    const reports = f.calls().filter((call) => call.args[0] === "status-report")
    expect(reports.at(-1).payload).toMatchObject({ status: "active", display_name: "Useful session title", cwd: f.dir })
    await f.event("session.idle", { sessionID: "session" })
    await f.hooks.dispose!()
    await Bun.sleep(2100)
    expect(f.calls().filter((call) => call.args[0] === "status-report")).toHaveLength(2)
  })
  test("failed initial status registration retries on later tool activity", async () => {
    const f = await fixture({ status: "process.exit(1)" })
    await f.ready()
    await f.before("one")
    await Bun.sleep(50)
    await f.before("two")
    await eventually(() => f.calls().filter((call) => call.args[0] === "status-report").length === 2)
  })
})

const question = { id: "question", sessionID: "session", questions: [{ question: "Continue?", header: "Continue", options: [{ label: "Yes", description: "Continue" }] }] }
describe("questions", () => {
  test("proxies answers over the in-process SDK", async () => {
    const f = await fixture()
    await f.event("question.asked", question)
    expect(f.requests.find((request) => request.url.endsWith("/reply"))?.body).toEqual({ answers: [["Yes"]] })
  })
  test("HTTP unavailable or malformed results become explicit pipeline failures", async () => {
    const f = await fixture({ question: 'console.error("HTTP unavailable"); process.exit(4)' })
    await f.event("question.asked", question)
    expect(f.requests.find((request) => request.url.endsWith("/reply"))?.body.answers[0][0]).toContain("pipeline failed")
  })
  test("rejection, timeout and malformed question outcomes preserve fail-closed semantics", async () => {
    const rejected = await fixture({ question: "process.exit(1)" })
    await rejected.event("question.asked", question)
    expect(rejected.requests.map((request) => request.url)).toEqual(["/question/question/reject"])
    const timeout = await fixture({ question: "process.exit(3)" })
    await timeout.event("question.asked", question)
    expect(timeout.requests[0].body.answers[0][0]).toContain("timed out")
    for (const output of ["not JSON", "null", '{"answers":[[42]]}', '{"answers":[]}']) {
      const f = await fixture({ question: `console.log(${JSON.stringify(output)})` })
      await f.event("question.asked", question)
      expect(f.requests[0].body.answers[0][0]).toContain("pipeline failed")
    }
  })
  test("failed normal and synthetic replies fall back to rejecting the question", async () => {
    const f = await fixture()
    f.setReplyError()
    await f.event("question.asked", question)
    expect(f.requests.filter((request) => request.url.endsWith("/reply"))).toHaveLength(2)
    expect(f.requests.some((request) => request.url.endsWith("/reject"))).toBe(true)
  })
  test("terminal SDK failures surface rather than leaving detached rejections", async () => {
    const f = await fixture()
    f.setReplyError(); f.setRejectError()
    await expect(f.event("question.asked", question)).rejects.toThrow("question.reject failed")
  })
  test("local answers and rejection cancel the gateway without a duplicate SDK reply", async () => {
    for (const type of ["question.replied", "question.rejected"]) {
      const f = await fixture({ question: "await Bun.sleep(100000)" })
      const pending = f.event("question.asked", question)
      await eventually(() => f.calls().some((call) => call.args[0] === "question"))
      await f.event(type, { requestID: "question", sessionID: "session" })
      await pending
      expect(f.requests).toHaveLength(0)
    }
  })
})

describe("bounded subprocess IO", () => {
  const run = (code: string, timeoutMs = 2000) => runGatewayProcess({ bin: process.execPath, args: ["-e", code], input: "{}", timeoutMs })
  test("successful stdout remains distinct from stderr", async () => {
    expect(await run('console.error("diagnostic"); console.log("{\\"allowed\\":true}")')).toEqual({ ok: true, stdout: '{"allowed":true}\n' })
  })
  test("nonzero, missing binary, deadline and oversized pipes are explicit failures", async () => {
    expect(await run("process.exit(2)")).toMatchObject({ ok: false, category: "nonzero", code: 2 })
    expect(await runGatewayProcess({ bin: "/nonexistent/agent-hub-gateway", args: [], input: "{}", timeoutMs: 500 })).toMatchObject({ ok: false, category: "spawn" })
    expect(await run("setInterval(()=>{},1000)", 20)).toMatchObject({ ok: false, category: "timeout" })
    for (const stream of ["stdout", "stderr"]) expect(await run(`process.${stream}.write("x".repeat(2*1024*1024))`)).toMatchObject({ ok: false, category: "oversized" })
  })
})

test("one payload signature preserves raw and normalized input separately", () => {
  expect(makePayload("session", "Read", { filePath: "file" }, { path: "/project/file" }, "/project")).toEqual({
    session_id: "session", tool_name: "Read", tool_input: { filePath: "file" }, tool_match_input: { path: "/project/file" },
    cwd: "/project", hook_event_name: "tool.execute.before", session_title: null,
  })
})

describe("offline provider contracts with the actual main gateway", () => {
  const gateway = path.resolve(import.meta.dir, "../../target/debug/agent-hub-gateway")

  async function realGatewayFixture() {
    if (!existsSync(gateway)) throw new Error("Offline gateway contract tests require: cargo build -p agent-hub-gateway (from repository root)")
    // Only approval invokes the real binary. Existing fake status/question commands
    // stay inert so these policy contracts cannot make even status HTTP requests.
    const f = await fixture({ nested: true, server: "http://127.0.0.1:9", approval: `
      const child = Bun.spawn([${JSON.stringify(gateway)}, ...process.argv.slice(2)], {
        stdin: Buffer.from(payload), stdout: "inherit", stderr: "inherit"
      });
      process.exit(await child.exited)
    ` })
    writeFileSync(path.join(f.dir, "policy.json"), JSON.stringify({ version: 1, default: "deny", rules: [
      { tools: ["Read", "Write", "MultiEdit", "Glob", "Grep", "List"], action: "allow", in_paths: [f.input.directory] },
    ] }))
    await f.ready()
    return f
  }

  test("native edits remain Write operations accepted by the real gateway, including empty replacement strings", async () => {
    const f = await realGatewayFixture()
    let i = 0
    for (const [oldString, newString] of [["old", "new"], ["", "new"], ["old", ""], ["", ""]]) {
      const args = { filePath: path.join(f.input.directory, "file"), oldString, newString, replaceAll: true }
      const id = `edit-${i++}`
      await f.before(id, args, "edit")
      await f.after(id, args, "edit")
      const payload = f.calls().filter((call) => call.args[0] === "approval").at(-1).payload
      expect(payload.tool_name).toBe("Write")
      expect(payload.tool_input).toEqual(args)
      expect(payload.tool_match_input).toEqual({ path: path.join(f.input.directory, "file") })
      expect(payload.tool_match_input).not.toHaveProperty("content")
    }
    const args = { filePath: path.join(f.dir, "outside/secret"), oldString: "old", newString: "new" }
    await expect(f.before("denied-edit", args, "edit")).rejects.toThrow("blocked")
    await expect(f.after("denied-edit", args, "edit")).rejects.toThrow("unauthorized")
    expect(f.calls().filter((call) => call.args[0] === "approval").at(-1).payload.tool_match_input.path).toBe(args.filePath)
  })

  test("both Write wire spellings are accepted as known Write tools by the actual gateway", async () => {
    const f = await realGatewayFixture()
    for (const spelling of ["Write", "write"]) {
      const payload = makePayload("session", spelling, { filePath: "file", oldString: "old", newString: "new" },
        { path: path.join(f.input.directory, "file") }, f.input.directory)
      const result = await runGatewayProcess({ bin: gateway,
        args: ["approval", "--opencode", "--server", "http://127.0.0.1:9", "--config", path.join(f.dir, "policy.json")],
        input: JSON.stringify(payload), timeoutMs: 2000 })
      expect(result.ok).toBe(true)
      if (!result.ok) throw new Error(result.detail)
      expect(JSON.parse(result.stdout)).toMatchObject({ allowed: true })
    }
  })

  test("malformed native edits fail before any gateway authorization", async () => {
    const f = await realGatewayFixture()
    let i = 0
    for (const args of [{ filePath: "file" }, { filePath: "file", oldString: 1, newString: "new" }, { filePath: "file", oldString: "old", newString: null }]) {
      await expect(f.before(`malformed-${i++}`, args, "edit")).rejects.toThrow("oldString and newString")
    }
    expect(f.calls().filter((call) => call.args[0] === "approval")).toHaveLength(0)
  })

})
