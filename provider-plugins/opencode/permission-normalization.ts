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
  edit: "Write",
  glob: "Glob",
  grep: "Grep",
  read: "Read",
  webfetch: "WebFetch",
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

export function normalizePermission(
  info: PermissionInfo,
  directory: string,
  worktree: string,
): NormalizedPermission[] {
  const tool = TOOL_NAMES[info.permission] ?? info.permission
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
  if (info.permission === "webfetch") {
    return [{ tool, toolInput: { url: requireValue(stringMetadata(info, "url"), "webfetch URL") } }]
  }
  if (info.permission === "grep") {
    const pattern = requireValue(info.patterns[0], "grep pattern")
    const target = stringMetadata(info, "path") ?? stringMetadata(info, "directory")
    return [{ tool, toolInput: { pattern, ...(target ? { path: absolute(target, directory) } : {}) } }]
  }
  if (info.permission === "glob") {
    const target = stringMetadata(info, "path") ?? stringMetadata(info, "directory") ?? directory
    const pattern = stringMetadata(info, "pattern")
    return [{ tool, toolInput: { path: absolute(target, directory), ...(pattern ? { pattern } : {}) } }]
  }
  return [{ tool, toolInput: {} }]
}
