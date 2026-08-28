import path from "path"
import type { OpenCodeTool } from "./generated/gateway-types"

export interface PermissionInfo {
  permission: string
  patterns: string[]
  metadata: Record<string, unknown>
}

export interface NormalizedPermission {
  tool: OpenCodeTool
  toolInput: Record<string, unknown>
}

const TOOL_NAMES: Record<string, OpenCodeTool> = {
  bash: "Bash",
  codesearch: "CodeSearch",
  doom_loop: "DoomLoop",
  edit: "Write",
  external_directory: "ExternalDirectory",
  glob: "Glob",
  grep: "Grep",
  list: "List",
  lsp: "Lsp",
  question: "Question",
  read: "Read",
  skill: "Skill",
  task: "Task",
  todowrite: "TodoWrite",
  webfetch: "WebFetch",
  websearch: "WebSearch",
}

const EXECUTION_TOOL_NAMES: Record<string, OpenCodeTool> = {
  ...TOOL_NAMES,
  apply_patch: "Write",
  write: "Write",
}

export function normalizeToolExecution(
  toolName: string,
  args: Record<string, unknown>,
): NormalizedPermission {
  const tool = EXECUTION_TOOL_NAMES[toolName]
  if (!tool) throw new Error(`unsupported OpenCode tool '${toolName}'`)
  return { tool, toolInput: structuredClone(args) }
}

function absolute(value: string, base: string): string {
  return path.isAbsolute(value) ? value : path.resolve(base, value)
}

function stringMetadata(info: PermissionInfo, key: string): string | undefined {
  const value = info.metadata[key]
  return typeof value === "string" && value.length > 0 ? value : undefined
}

function requireValue(value: string | undefined, description: string): string {
  if (!value) throw new Error(`OpenCode ${description} is missing`)
  return value
}

function within(candidate: string, root: string): boolean {
  const relative = path.relative(root, candidate)
  return relative === "" || (!relative.startsWith(`..${path.sep}`) && relative !== ".." && !path.isAbsolute(relative))
}

export function normalizePermission(
  info: PermissionInfo,
  directory: string,
  worktree: string,
): NormalizedPermission[] {
  const tool = TOOL_NAMES[info.permission]
  if (!tool) throw new Error(`unsupported OpenCode permission '${info.permission}'`)
  if (info.permission === "bash") {
    return [{ tool, toolInput: { command: requireValue(stringMetadata(info, "command"), "bash command") } }]
  }
  if (info.permission === "edit") {
    if (Object.prototype.hasOwnProperty.call(info.metadata, "files")) {
      if (!Array.isArray(info.metadata.files) || info.metadata.files.length === 0) {
        throw new Error("OpenCode apply_patch files are missing")
      }
      return info.metadata.files.map((entry, index) => {
        if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
          throw new Error(`OpenCode apply_patch file ${index} is malformed`)
        }
        const filePath = (entry as Record<string, unknown>).filePath
        if (typeof filePath !== "string" || filePath.length === 0) {
          throw new Error(`OpenCode apply_patch file ${index} has no filePath`)
        }
        return { tool, toolInput: { path: absolute(filePath, worktree) } }
      })
    }
    const filepath = stringMetadata(info, "filepath")
    if (filepath) return [{ tool, toolInput: { path: absolute(filepath, worktree) } }]
    if (info.patterns.length === 0) throw new Error("OpenCode edit path is missing")
    return info.patterns.map((pattern) => ({ tool, toolInput: { path: absolute(pattern, worktree) } }))
  }
  if (info.permission === "read") {
    if (info.patterns.length === 0) throw new Error("OpenCode read path is missing")
    return info.patterns.map((pattern) => ({ tool, toolInput: { path: absolute(pattern, worktree) } }))
  }
  if (["list", "external_directory"].includes(info.permission)) {
    if (info.patterns.length === 0) throw new Error(`OpenCode ${info.permission} path is missing`)
    return info.patterns.map((pattern) => ({ tool, toolInput: { path: absolute(pattern, directory) } }))
  }
  if (info.permission === "external_directory") {
    if (info.patterns.length !== 1) throw new Error("OpenCode external_directory requires one exact requested path")
    const requested = absolute(info.patterns[0], directory)
    const authorizedRoots = [absolute(directory, directory), absolute(worktree, directory)]
    if (!authorizedRoots.some((root) => within(requested, root))) {
      throw new Error("OpenCode external_directory path is outside the authorized workspace/context")
    }
    return [{ tool, toolInput: { path: requested } }]
  }
  if (info.permission === "webfetch") {
    return [{ tool, toolInput: { url: requireValue(stringMetadata(info, "url"), "webfetch URL") } }]
  }
  if (info.permission === "grep") {
    const pattern = requireValue(info.patterns[0], "grep pattern")
    const target = stringMetadata(info, "path") ?? stringMetadata(info, "directory") ?? directory
    return [{ tool, toolInput: { pattern, path: absolute(target, directory) } }]
  }
  if (info.permission === "glob") {
    if (info.patterns.length === 0) throw new Error("OpenCode glob pattern is missing")
    const target = stringMetadata(info, "path") ?? stringMetadata(info, "directory") ?? directory
    const pattern = stringMetadata(info, "pattern")
    return [{ tool, toolInput: { path: absolute(target, directory), ...(pattern ? { pattern } : {}) } }]
  }
  if (["websearch", "codesearch"].includes(info.permission)) {
    const query = stringMetadata(info, "query") ?? info.patterns[0]
    return [{ tool, toolInput: { query: requireValue(query, `${info.permission} query`) } }]
  }
  if (["task", "todowrite"].includes(info.permission)) {
    return [{
      tool,
      toolInput: {
        patterns: [...info.patterns],
        metadata: structuredClone(info.metadata),
      },
    }]
  }
  if (["question", "doom_loop"].includes(info.permission)) {
    return [{ tool, toolInput: { patterns: [...info.patterns], metadata: structuredClone(info.metadata) } }]
  }
  if (["lsp", "skill"].includes(info.permission)) {
    const name = requireValue(info.patterns[0], `${info.permission} name`)
    return [{ tool, toolInput: { name, metadata: structuredClone(info.metadata) } }]
  }
  return [{ tool, toolInput: {} }]
}
