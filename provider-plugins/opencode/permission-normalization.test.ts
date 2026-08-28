import { describe, expect, test } from "bun:test"
import { isStructuredOutputTool, normalizeToolExecution } from "./permission-normalization"

describe("OpenCode 1.18.18 execution-hook normalization", () => {
  test("preserves raw camelCase file arguments and resolves canonical paths", () => {
    const cases: Array<[string, Record<string, unknown>, string, Record<string, unknown>]> = [
      ["read", { filePath: "src/a.ts", offset: 2, limit: 3 }, "Read", { path: "/workspace/src/a.ts" }],
      ["write", { filePath: "src/a.ts", content: "body" }, "Write", { path: "/workspace/src/a.ts" }],
      ["edit", { filePath: "src/a.ts", oldString: "old", newString: "new", replaceAll: true }, "Edit", {
        path: "/workspace/src/a.ts",
      }],
    ]
    for (const [name, args, tool, match] of cases) {
      const normalized = normalizeToolExecution(name, args, "/workspace")
      expect(normalized.tool).toBe(tool)
      expect(normalized.rawArgs).toEqual(args)
      expect(normalized.matchInputs).toEqual([match])
      expect(normalized.resolvedPaths).toEqual(["/workspace/src/a.ts"])
    }
  })

  test("extracts every apply_patch source and move destination", () => {
    const patchText = "*** Begin Patch\n*** Add File: a.txt\n+x\n*** Update File: b.txt\n*** Move to: moved/b.txt\n@@\n-old\n+new\n*** Delete File: c.txt\n*** End Patch"
    const normalized = normalizeToolExecution("apply_patch", { patchText }, "/workspace")
    expect(normalized.rawArgs).toEqual({ patchText })
    expect(normalized.matchInputs).toEqual([
      { path: "/workspace/a.txt" },
      { path: "/workspace/b.txt" },
      { path: "/workspace/moved/b.txt" },
      { path: "/workspace/c.txt" },
    ])
  })

  test("normalizes filesystem, process, network, subagent, and UI built-ins", () => {
    const cases: Array<[string, Record<string, unknown>, string]> = [
      ["glob", { pattern: "**/*.ts", path: "src" }, "Glob"],
      ["list", { path: "src", ignore: ["target"] }, "List"],
      ["grep", { pattern: "needle", path: "src", include: "*.ts" }, "Grep"],
      ["multiedit", { filePath: "src/a.ts", edits: [{ oldString: "a", newString: "b" }] }, "MultiEdit"],
      ["external_directory", { path: "src" }, "ExternalDirectory"],
      ["bash", { command: "git status", timeout: 1000, description: "status" }, "Bash"],
      ["task", { description: "work", prompt: "do it", subagentType: "general" }, "Task"],
      ["skill", { name: "local-workspaces" }, "Skill"],
      ["lsp", { operation: "hover", filePath: "src/a.ts", line: 1, character: 2 }, "Lsp"],
      ["question", { questions: [{ question: "Continue?" }] }, "Question"],
      ["webfetch", { url: "https://example.test", format: "markdown", timeout: 10 }, "WebFetch"],
      ["websearch", { query: "query", numResults: 3 }, "WebSearch"],
      ["codesearch", { query: "symbol", tokensNum: 500 }, "CodeSearch"],
      ["doom_loop", { tool: "bash", args: { command: "false" } }, "DoomLoop"],
      ["todowrite", { todos: [] }, "TodoWrite"],
      ["plan_enter", {}, "plan_enter"],
      ["plan_exit", {}, "plan_exit"],
      ["execute", { code: "await tools.read({filePath: 'a'})" }, "execute"],
      ["invalid", { tool: "missing", error: "bad" }, "invalid"],
    ]
    for (const [name, args, tool] of cases) {
      const normalized = normalizeToolExecution(name, args, "/workspace")
      expect(normalized.tool).toBe(tool)
      expect(normalized.rawArgs).toEqual(args)
      expect(normalized.matchInputs).toHaveLength(1)
    }
  })

  test("passes MCP, plugin, custom, and future tools through as typed Unknown inputs", () => {
    for (const name of ["mcp_database_query", "plugin_deploy", "custom_tool", "future_builtin"]) {
      const args = { query: "select 1", filePath: "review/input.sql", extra: { retained: true } }
      const normalized = normalizeToolExecution(name, args, "/workspace")
      expect(normalized.tool).toBe(name)
      expect(normalized.rawArgs).toEqual(args)
      expect(normalized.matchInputs).toEqual([args])
      expect(normalized.resolvedPaths).toEqual(["/workspace/review/input.sql"])
    }
  })

  test("does not pre-deny unknowns and resolves only valid path-bearing arguments", () => {
    const malformed = normalizeToolExecution("mcp_custom", { filePath: 42 }, "/workspace")
    expect(malformed.resolvedPaths).toEqual([])
    expect(malformed.postPolicyValidationError).toContain("path-bearing")
    expect(normalizeToolExecution("mcp_custom", { opaque: 42 }, "/workspace").tool).toBe("mcp_custom")
  })

  test("classifies only StructuredOutput as transcript-only", () => {
    expect(isStructuredOutputTool("StructuredOutput")).toBe(true)
    expect(isStructuredOutputTool("structured_output")).toBe(true)
    expect(isStructuredOutputTool("read")).toBe(false)
    expect(isStructuredOutputTool("future_builtin")).toBe(false)
  })
})
