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

  test("preserves unknown tool metadata and patterns", () => {
    expect(normalizePermission({
      permission: "mcp_database_query",
      patterns: ["production", "read-only"],
      metadata: { query: "select 1", limit: 10 },
    }, "/w", "/w")).toEqual([{
      tool: "mcp_database_query",
      toolInput: {
        patterns: ["production", "read-only"],
        metadata: { query: "select 1", limit: 10 },
      },
    }])
  })
})
