import path from "path"
import type { OpenCodeTool } from "./generated/gateway-types"

export interface NormalizedExecution {
  tool: OpenCodeTool
  rawArgs: Record<string, unknown>
  matchInputs: Record<string, unknown>[]
  resolvedPaths: string[]
  postPolicyValidationError?: string
}

const TOOL_NAMES: Record<string, OpenCodeTool> = {
  apply_patch: "Write",
  bash: "Bash",
  codesearch: "CodeSearch",
  code_search: "CodeSearch",
  doom_loop: "DoomLoop",
  edit: "Edit",
  external_directory: "ExternalDirectory",
  glob: "Glob",
  grep: "Grep",
  list: "List",
  lsp: "Lsp",
  multiedit: "MultiEdit",
  question: "Question",
  read: "Read",
  skill: "Skill",
  task: "Task",
  todowrite: "TodoWrite",
  webfetch: "WebFetch",
  websearch: "WebSearch",
  write: "Write",
}

export const STRUCTURED_OUTPUT_TOOLS = new Set(["structured_output", "structuredoutput", "StructuredOutput"])

function objectArgs(args: unknown): Record<string, unknown> {
  if (!args || typeof args !== "object" || Array.isArray(args)) {
    throw new Error("OpenCode tool arguments must be an object")
  }
  return structuredClone(args as Record<string, unknown>)
}

function requiredString(args: Record<string, unknown>, key: string, toolName: string): string {
  const value = args[key]
  if (typeof value !== "string" || value.length === 0 || value.includes("\0")) {
    throw new Error(`OpenCode ${toolName}.${key} must be a non-empty string`)
  }
  return value
}

function optionalString(args: Record<string, unknown>, key: string, toolName: string): string | undefined {
  const value = args[key]
  if (value === undefined) return undefined
  if (typeof value !== "string" || value.includes("\0")) {
    throw new Error(`OpenCode ${toolName}.${key} must be a string`)
  }
  return value
}

function absolute(value: string, base: string): string {
  return path.isAbsolute(value) ? path.normalize(value) : path.resolve(base, value)
}

function patchPaths(patchText: string, directory: string): string[] {
  const result: string[] = []
  for (const line of patchText.replaceAll("\r\n", "\n").split("\n")) {
    const match = line.match(/^\*\*\* (?:Add|Update|Delete) File: (.+)$/) ?? line.match(/^\*\*\* Move to: (.+)$/)
    if (!match) continue
    const value = match[1]
    if (!value || value.includes("\0")) throw new Error("OpenCode apply_patch contains an invalid path")
    result.push(absolute(value, directory))
  }
  if (result.length === 0) throw new Error("OpenCode apply_patch contains no file operations")
  return [...new Set(result)]
}

function unknownPaths(
  args: Record<string, unknown>,
  directory: string,
): { paths: string[]; error?: string } {
  const result: string[] = []
  let error: string | undefined
  const visit = (value: unknown, key?: string) => {
    if (Array.isArray(value)) {
      for (const item of value) visit(item, key)
      return
    }
    if (value && typeof value === "object") {
      for (const [nestedKey, nested] of Object.entries(value)) visit(nested, nestedKey)
      return
    }
    if (!key || !/(?:^|_)(?:file|directory|cwd|root)?path$/i.test(key)) return
    if (typeof value !== "string" || value.length === 0 || value.includes("\0")) {
      error ??= `OpenCode unknown tool path-bearing argument '${key}' is malformed`
      return
    }
    result.push(absolute(value, directory))
  }
  visit(args)
  return { paths: [...new Set(result)], error }
}

export function normalizeToolExecution(
  toolName: string,
  args: unknown,
  directory = process.cwd(),
  worktree = directory,
): NormalizedExecution {
  const rawArgs = objectArgs(args)
  const tool = TOOL_NAMES[toolName] ?? toolName
  const one = (matchInput: Record<string, unknown>, resolvedPaths: string[] = []): NormalizedExecution => ({
    tool,
    rawArgs,
    matchInputs: [matchInput],
    resolvedPaths,
  })

  if (toolName === "read") {
    const filePath = absolute(requiredString(rawArgs, "filePath", toolName), worktree)
    return one({ path: filePath }, [filePath])
  }
  if (toolName === "write") {
    const filePath = absolute(requiredString(rawArgs, "filePath", toolName), worktree)
    return one({ path: filePath }, [filePath])
  }
  if (toolName === "edit") {
    const filePath = absolute(requiredString(rawArgs, "filePath", toolName), worktree)
    return one({ path: filePath }, [filePath])
  }
  if (toolName === "apply_patch") {
    const paths = patchPaths(requiredString(rawArgs, "patchText", toolName), worktree)
    return { tool, rawArgs, matchInputs: paths.map((filePath) => ({ path: filePath })), resolvedPaths: paths }
  }
  if (toolName === "multiedit") {
    const filePath = absolute(requiredString(rawArgs, "filePath", toolName), worktree)
    if (!Array.isArray(rawArgs.edits)) throw new Error("OpenCode multiedit.edits must be an array")
    const edits = rawArgs.edits.map((entry, index) => {
      const edit = objectArgs(entry)
      const oldString = edit.oldString
      const newString = edit.newString
      if (typeof oldString !== "string" || typeof newString !== "string") {
        throw new Error(`OpenCode multiedit.edits[${index}] has malformed strings`)
      }
      return { old_string: oldString, new_string: newString }
    })
    return one({ path: filePath, edits }, [filePath])
  }
  if (toolName === "external_directory") {
    const target = absolute(requiredString(rawArgs, "path", toolName), directory)
    return one({ path: target }, [target])
  }
  if (toolName === "glob") {
    const target = absolute(optionalString(rawArgs, "path", toolName) ?? directory, directory)
    return one({ target_directory: target }, [target])
  }
  if (toolName === "list") {
    const target = absolute(requiredString(rawArgs, "path", toolName), directory)
    return one({ path: target }, [target])
  }
  if (toolName === "grep") {
    const target = absolute(optionalString(rawArgs, "path", toolName) ?? directory, directory)
    return one({ path: target, pattern: requiredString(rawArgs, "pattern", toolName) }, [target])
  }
  if (toolName === "bash") return one({ command: requiredString(rawArgs, "command", toolName) })
  if (toolName === "webfetch") return one({ url: requiredString(rawArgs, "url", toolName) })
  if (toolName === "websearch" || toolName === "codesearch" || toolName === "code_search") {
    return one({ query: requiredString(rawArgs, "query", toolName) })
  }
  if (toolName === "skill") return one({ name: requiredString(rawArgs, "name", toolName), metadata: rawArgs })
  if (toolName === "lsp") {
    const operation = requiredString(rawArgs, "operation", toolName)
    const filePath = optionalString(rawArgs, "filePath", toolName)
    const resolved = filePath ? [absolute(filePath, directory)] : []
    return one({ name: operation, metadata: { ...rawArgs, ...(resolved[0] ? { filePath: resolved[0] } : {}) } }, resolved)
  }
  if (toolName === "doom_loop" || toolName === "question") {
    return one({ patterns: [], metadata: rawArgs })
  }
  if (toolName === "task" || toolName === "todowrite") return one(rawArgs)

  const validation = unknownPaths(rawArgs, directory)
  return { ...one(rawArgs, validation.paths), postPolicyValidationError: validation.error }
}

export function isStructuredOutputTool(toolName: string): boolean {
  return STRUCTURED_OUTPUT_TOOLS.has(toolName)
}
