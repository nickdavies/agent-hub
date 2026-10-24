import { expect, test } from "bun:test"
import { normalizeToolExecution as normalize } from "./permission-normalization"

test("native file tools resolve paths without changing raw args", () => {
  for (const name of ["read", "write", "edit"]) {
    const args = { filePath: "../file", content: "body", offset: 2, oldString: "old", newString: "new" }
    const result = normalize(name, args, "/project/nested")
    expect(result.rawArgs).toEqual(args)
    expect(result.rawArgs).not.toBe(args)
    expect(result.matchInputs).toEqual([{ path: "/project/file" }])
  }
  expect(normalize("read", { filePath: "/external/file" }, "/project").matchInputs).toEqual([{ path: "/external/file" }])
})
test("native edit retains deployed Write identity and validates raw replacement strings including empty ones", () => {
  for (const [oldString, newString] of [["", "new"], ["old", ""], ["", ""], ["old\0", "new\0"]]) {
    const args = { filePath: "file", oldString, newString }
    expect(normalize("edit", args, "/project/sub")).toEqual({ tool: "Write", rawArgs: args, matchInputs: [{ path: "/project/sub/file" }] })
  }
  for (const args of [{ filePath: "file" }, { filePath: "file", oldString: 3, newString: "new" }, { filePath: "file", oldString: "old", newString: null }]) {
    expect(() => normalize("edit", args, "/project/sub")).toThrow("oldString and newString")
  }
})
test("patch normalizes every source, destination, add and delete path", () => {
  const patchText = "*** Begin Patch\r\n*** Update File: src\r\n*** Move to: /external/dest\r\n*** Delete File: old\r\n*** Add File: new\r\n+x\r\n*** End Patch"
  expect(normalize("apply_patch", { patchText }, "/project").matchInputs).toEqual([
    { path: "/project/src" }, { path: "/external/dest" }, { path: "/project/old" }, { path: "/project/new" },
  ])
})
test("multiedit sends the complete batch against one path", () => {
  const raw = { filePath: "file", edits: [{ oldString: "one", newString: "two" }, { oldString: "three", newString: "four" }] }
  const result = normalize("multiedit", raw, "/project")
  expect(result.tool).toBe("MultiEdit")
  expect(result.rawArgs).toEqual(raw)
  expect(result.matchInputs).toEqual([{ path: "/project/file", edits: [{ old_string: "one", new_string: "two" }, { old_string: "three", new_string: "four" }] }])
})
test("search selectors are not treated as filesystem paths; List uses canonical path", () => {
  expect(normalize("glob", { pattern: "**/*.ts" }, "/project").matchInputs).toEqual([{ target_directory: "/project" }])
  expect(normalize("glob", { pattern: "**/*.ts", path: "../external" }, "/project").matchInputs).toEqual([{ target_directory: "/external" }])
  expect(normalize("grep", { pattern: "../not-a-path", path: "src" }, "/project").matchInputs).toEqual([{ path: "/project/src", pattern: "../not-a-path" }])
  expect(normalize("list", { path: "src" }, "/project").matchInputs).toEqual([{ path: "/project/src" }])
  expect(normalize("external_directory", { path: "../outside" }, "/project").matchInputs).toEqual([{ path: "/outside" }])
})
test("canonical scalar and metadata tools retain exact raw input", () => {
  const cases: Array<[string, Record<string, unknown>, Record<string, unknown>]> = [
    ["bash", { command: "pwd", description: "where" }, { command: "pwd" }],
    ["webfetch", { url: "https://example.com" }, { url: "https://example.com" }],
    ["websearch", { query: "hello" }, { query: "hello" }],
    ["codesearch", { query: "hello" }, { query: "hello" }],
    ["code_search", { query: "hello" }, { query: "hello" }],
    ["skill", { name: "local-workspaces" }, { name: "local-workspaces", metadata: { name: "local-workspaces" } }],
    ["lsp", { operation: "references", filePath: "src" }, { name: "references", metadata: { operation: "references", filePath: "/project/src" } }],
    ["question", { question: "hi" }, { patterns: [], metadata: { question: "hi" } }],
    ["doom_loop", {}, { patterns: [], metadata: {} }],
    ["task", { prompt: "hi" }, { prompt: "hi" }],
    ["todowrite", { todos: [] }, { todos: [] }],
  ]
  for (const [name, raw, canonical] of cases) {
    const result = normalize(name, raw, "/project")
    expect(result.rawArgs).toEqual(raw)
    expect(result.matchInputs).toEqual([canonical])
  }
})
test("unknown tools preserve exact case-sensitive MCP names and structured arguments", () => {
  const raw = { nested: { path: "remote/not-local", flags: [true, 3] } }
  const result = normalize("mcp_Example/RemoteLookup", raw, "/project")
  expect(result).toEqual({ tool: "mcp_Example/RemoteLookup", rawArgs: raw, matchInputs: [raw] })
})
test("malformed native arguments fail before spawning a gateway", () => {
  for (const [name, args] of [["read", {}], ["read", { filePath: "\0" }], ["bash", { command: 1 }], ["multiedit", { filePath: "f", edits: [{}] }], ["apply_patch", { patchText: "not a patch" }]] as const) {
    expect(() => normalize(name, args, "/project")).toThrow()
  }
  expect(() => normalize("unknown", [], "/project")).toThrow()
})
