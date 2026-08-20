import { describe, expect, test } from "bun:test"
import {
  DEFAULT_TITLE_RE,
  makeGlobToolInput,
  makePayload,
  makeReadToolInput,
} from "./agent-hub"

describe("makePayload", () => {
  test("uses directory for cwd and matching worktree for workspace_roots", () => {
    const payload = makePayload(
      "ses_abc",
      "Bash",
      { command: "ls" },
      "/home/user/project",
      "/home/user/project",
      "Fix auth bug",
    )

    expect(payload.cwd).toBe("/home/user/project")
    expect(payload.workspace_roots).toEqual(["/home/user/project"])
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
    expect(payload.workspace_roots).toEqual(["/home/user/project"])
    expect(payload.session_title).toBeNull()
  })

  test("uses directory as workspace root for the root sentinel", () => {
    const payload = makePayload(
      "ses_abc",
      "Read",
      { path: "/usr/bin/env" },
      "/home/user/project",
      "/",
    )

    expect(payload.cwd).toBe("/home/user/project")
    expect(payload.workspace_roots).toEqual(["/home/user/project"])
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

describe("DEFAULT_TITLE_RE", () => {
  test("matches default titles but not generated titles", () => {
    expect(DEFAULT_TITLE_RE.test("New session - 2026-04-01T22:18:18")).toBe(true)
    expect(DEFAULT_TITLE_RE.test("Child session - 2026-04-01T10:00:00")).toBe(true)
    expect(DEFAULT_TITLE_RE.test("Debugging production 500 errors")).toBe(false)
  })
})
