import path from "path"
import type { OpenCodeTool } from "./generated/gateway-types"

export interface NormalizedExecution {
  tool: OpenCodeTool
  rawArgs: Record<string, unknown>
  matchInputs: Record<string, unknown>[]
}

const TOOL_NAMES: Record<string, OpenCodeTool> = {
  apply_patch: "Write", bash: "Bash", codesearch: "CodeSearch", code_search: "CodeSearch",
  doom_loop: "DoomLoop", edit: "Write", external_directory: "ExternalDirectory",
  glob: "Glob", grep: "Grep", list: "List", lsp: "Lsp", multiedit: "MultiEdit",
  question: "Question", read: "Read", skill: "Skill", task: "Task", todowrite: "TodoWrite",
  webfetch: "WebFetch", websearch: "WebSearch", write: "Write",
}

function objectArgs(args: unknown): Record<string, unknown> {
  if (!args || typeof args !== "object" || Array.isArray(args)) {
    throw new Error("OpenCode tool arguments must be an object")
  }
  return structuredClone(args as Record<string, unknown>)
}

function stringArg(args: Record<string, unknown>, key: string, optional = false): string | undefined {
  const value = args[key]
  if (value === undefined && optional) return undefined
  if (typeof value !== "string" || !value.length || value.includes("\0")) {
    throw new Error(`OpenCode argument '${key}' must be a non-empty string`)
  }
  return value
}

export function normalizeToolExecution(
  toolName: string,
  args: unknown,
  directory: string,
  worktree = directory,
): NormalizedExecution {
  const rawArgs = objectArgs(args)
  const tool = TOOL_NAMES[toolName] ?? toolName
  const required = (key: string) => stringArg(rawArgs, key)!
  const absolute = (value: string, base = directory) => path.resolve(base, value)
  const one = (input: Record<string, unknown>): NormalizedExecution => ({ tool, rawArgs, matchInputs: [input] })

  if (["read", "write", "edit", "multiedit"].includes(toolName)) {
    const target = absolute(required("filePath"), worktree)
    // Preserve the deployed Write policy identity. Replacements are fragments,
    // not whole-file content, and empty replacement strings are valid inputs.
    if (toolName === "edit" && (typeof rawArgs.oldString !== "string" || typeof rawArgs.newString !== "string")) {
      throw new Error("OpenCode edit requires oldString and newString strings")
    }
    if (toolName !== "multiedit") return one({ path: target })
    if (!Array.isArray(rawArgs.edits)) throw new Error("OpenCode multiedit.edits must be an array")
    const edits = rawArgs.edits.map((value) => {
      const edit = objectArgs(value)
      if (typeof edit.oldString !== "string" || typeof edit.newString !== "string") {
        throw new Error("OpenCode multiedit has malformed edit strings")
      }
      return { old_string: edit.oldString, new_string: edit.newString }
    })
    return one({ path: target, edits })
  }
  if (toolName === "apply_patch") {
    const paths = new Set<string>()
    const prefixes = ["*** Add File:", "*** Update File:", "*** Delete File:", "*** Move to:"]
    for (const line of required("patchText").replaceAll("\r\n", "\n").split("\n")) {
      // Match the pinned host's startsWith(prefix) + slice(...).trim() semantics.
      // In particular, no space after ':' is required, and extra spaces are not
      // part of the path. Never silently skip a recognized operation header.
      const prefix = prefixes.find((candidate) => line.startsWith(candidate))
      if (!prefix) continue
      const target = line.slice(prefix.length).trim()
      if (!target || target.includes("\0")) throw new Error("OpenCode apply_patch contains an invalid path")
      paths.add(absolute(target, worktree))
    }
    if (!paths.size) throw new Error("OpenCode apply_patch contains no file operations")
    return { tool, rawArgs, matchInputs: [...paths].map((target) => ({ path: target })) }
  }
  if (toolName === "external_directory" || toolName === "list") return one({ path: absolute(required("path")) })
  if (toolName === "glob" || toolName === "grep") {
    const target = absolute(stringArg(rawArgs, "path", true) ?? directory)
    return toolName === "glob"
      ? one({ target_directory: target })
      : one({ path: target, pattern: required("pattern") })
  }
  if (toolName === "bash") return one({ command: required("command") })
  if (toolName === "webfetch") return one({ url: required("url") })
  if (["websearch", "codesearch", "code_search"].includes(toolName)) return one({ query: required("query") })
  if (toolName === "skill") return one({ name: required("name"), metadata: rawArgs })
  if (toolName === "lsp") {
    const filePath = stringArg(rawArgs, "filePath", true)
    return one({ name: required("operation"), metadata: { ...rawArgs, ...(filePath ? { filePath: absolute(filePath) } : {}) } })
  }
  if (toolName === "doom_loop" || toolName === "question") return one({ patterns: [], metadata: rawArgs })
  // Unknown/MCP tools retain their exact names and arguments for structured rules.
  return one(rawArgs)
}
