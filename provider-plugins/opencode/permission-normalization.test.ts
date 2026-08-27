import { describe, expect, test } from "bun:test"
import { normalizePermission } from "./permission-normalization"

describe("OpenCode permission normalization", () => {
  test("normalizes every apply_patch file without dropping malformed entries", () => {
    expect(normalizePermission({
      permission: "edit",
      patterns: [],
      metadata: { files: [{ filePath: "a.txt" }, { filePath: "/workspace/b.txt" }] },
    }, "/workspace", "/workspace")).toEqual([
      { tool: "Write", toolInput: { path: "/workspace/a.txt" } },
      { tool: "Write", toolInput: { path: "/workspace/b.txt" } },
    ])
    expect(() => normalizePermission({
      permission: "edit",
      patterns: [],
      metadata: { files: [{ filePath: "a.txt" }, {}] },
    }, "/workspace", "/workspace")).toThrow("file 1")
  })

  test("normalizes all mature path, shell, and network permissions", () => {
    expect(normalizePermission({ permission: "bash", patterns: ["ignored"], metadata: { command: "git status" } }, "/w", "/w"))
      .toEqual([{ tool: "Bash", toolInput: { command: "git status" } }])
    expect(normalizePermission({ permission: "read", patterns: ["a", "b"], metadata: {} }, "/w", "/w"))
      .toHaveLength(2)
    expect(normalizePermission({ permission: "grep", patterns: ["needle"], metadata: { directory: "src" } }, "/w", "/w"))
      .toEqual([{ tool: "Grep", toolInput: { pattern: "needle", path: "/w/src" } }])
    expect(normalizePermission({ permission: "glob", patterns: ["**/*.ts"], metadata: {} }, "/w", "/w"))
      .toEqual([{ tool: "Glob", toolInput: { path: "/w", patterns: ["**/*.ts"] } }])
    expect(normalizePermission({ permission: "webfetch", patterns: [], metadata: { url: "https://example.test" } }, "/w", "/w"))
      .toEqual([{ tool: "WebFetch", toolInput: { url: "https://example.test" } }])
  })

  test("maps every supported permission and fails closed on omissions", () => {
    for (const permission of ["read", "list"]) {
      expect(normalizePermission({ permission, patterns: ["src"], metadata: {} }, "/w", "/w"))
        .toEqual([{ tool: permission === "read" ? "Read" : "List", toolInput: { path: "/w/src" } }])
    }
    expect(normalizePermission({ permission: "external_directory", patterns: ["src"], metadata: {} }, "/w", "/w"))
      .toEqual([{ tool: "ExternalDirectory", toolInput: { path: "/w/src" } }])
    expect(() => normalizePermission({ permission: "external_directory", patterns: ["/etc"], metadata: {} }, "/w", "/w"))
      .toThrow("outside")
    expect(normalizePermission({ permission: "task", patterns: ["agent"], metadata: { prompt: "work" } }, "/w", "/w")[0].tool)
      .toBe("Task")
    expect(normalizePermission({ permission: "todowrite", patterns: [], metadata: {} }, "/w", "/w")[0].tool)
      .toBe("TodoWrite")
    expect(normalizePermission({ permission: "question", patterns: [], metadata: {} }, "/w", "/w")[0].tool)
      .toBe("Question")
    expect(normalizePermission({ permission: "doom_loop", patterns: [], metadata: {} }, "/w", "/w")[0].tool)
      .toBe("DoomLoop")
    expect(normalizePermission({ permission: "lsp", patterns: ["hover"], metadata: {} }, "/w", "/w"))
      .toEqual([{ tool: "Lsp", toolInput: { name: "hover", metadata: {} } }])
    expect(normalizePermission({ permission: "skill", patterns: ["local-workspaces"], metadata: {} }, "/w", "/w"))
      .toEqual([{ tool: "Skill", toolInput: { name: "local-workspaces", metadata: {} } }])
    expect(normalizePermission({ permission: "websearch", patterns: ["query"], metadata: {} }, "/w", "/w"))
      .toEqual([{ tool: "WebSearch", toolInput: { query: "query" } }])
    expect(normalizePermission({ permission: "codesearch", patterns: [], metadata: { query: "symbol" } }, "/w", "/w"))
      .toEqual([{ tool: "CodeSearch", toolInput: { query: "symbol" } }])
    expect(() => normalizePermission({ permission: "mcp_database_query", patterns: [], metadata: {} }, "/w", "/w"))
      .toThrow("unsupported")
    expect(() => normalizePermission({ permission: "external_directory", patterns: [], metadata: {} }, "/w", "/w"))
      .toThrow("one exact requested path")
  })
})
