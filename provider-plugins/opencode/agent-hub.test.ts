/**
 * Tests for the agent-hub opencode plugin's session title logic.
 *
 * Run: bun test agent-hub.test.ts
 *
 * These tests validate:
 * 1. makePayload includes session_title when provided
 * 2. makePayload omits session_title when undefined
 * 3. fetchTitle returns cached value on second call
 * 4. fetchTitle filters out default opencode titles
 * 5. fetchTitle passes directory to session.get
 * 6. fetchTitle returns undefined when SDK returns no data
 */

import { describe, test, expect, mock } from "bun:test"
import type { OpenCodeHookInput, OpenCodeTool } from "./generated/gateway-types"

// ---------------------------------------------------------------------------
// Replicate the key logic from agent-hub.ts (module-private functions)
// so we can unit test without modifying exports.
// ---------------------------------------------------------------------------

const DEFAULT_TITLE_RE = /^(New session - |Child session - )\d{4}-\d{2}-\d{2}T/

function makePayload(
  sessionId: string,
  tool: OpenCodeTool,
  toolInput: Record<string, unknown>,
  cwd: string,
  title?: string,
): OpenCodeHookInput {
  return {
    session_id: sessionId,
    tool_name: tool,
    tool_input: toolInput,
    cwd,
    workspace_roots: [cwd],
    hook_event_name: "permission.ask",
    session_title: title ?? null,
  }
}

// Simplified fetchTitle for testing — takes a mock get function that
// mirrors the v1 SDK calling convention: get({ path: { id }, query: { directory } })
async function fetchTitle(
  get: (params: { path: { id: string }; query: { directory: string } }) => Promise<{ data?: { title?: string } }>,
  sid: string,
  dir: string,
  cache: Map<string, string>,
): Promise<string | undefined> {
  const cached = cache.get(sid)
  if (cached) return cached

  try {
    const res = await get({ path: { id: sid }, query: { directory: dir } })
    const title = res.data?.title
    if (title && !DEFAULT_TITLE_RE.test(title)) {
      cache.set(sid, title)
      return title
    }
    return undefined
  } catch {
    return undefined
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("makePayload", () => {
  test("includes session_title when title is provided", () => {
    const p = makePayload("ses_abc", "Bash", { command: "ls" }, "/home/user", "Fix auth bug")
    expect(p.session_title).toBe("Fix auth bug")
    expect(p.session_id).toBe("ses_abc")
    expect(p.tool_name).toBe("Bash")
  })

  test("sets session_title to null when title is undefined", () => {
    const p = makePayload("ses_abc", "Read", { path: "/tmp" }, "/home/user")
    expect(p.session_title).toBeNull()
  })

  test("sets session_title to empty string when title is empty string", () => {
    const p = makePayload("ses_abc", "Read", { path: "/tmp" }, "/home/user", "")
    // ?? only maps undefined/null to null; empty string passes through.
    // In practice fetchTitle never returns "" — it returns undefined or a real title.
    expect(p.session_title).toBe("")
  })
})

describe("DEFAULT_TITLE_RE", () => {
  test("matches default 'New session' titles", () => {
    expect(DEFAULT_TITLE_RE.test("New session - 2026-04-01T22:18:18")).toBe(true)
  })

  test("matches default 'Child session' titles", () => {
    expect(DEFAULT_TITLE_RE.test("Child session - 2026-04-01T10:00:00")).toBe(true)
  })

  test("does not match LLM-generated titles", () => {
    expect(DEFAULT_TITLE_RE.test("Debugging production 500 errors")).toBe(false)
    expect(DEFAULT_TITLE_RE.test("Fix auth bug in login flow")).toBe(false)
  })
})

describe("fetchTitle", () => {
  test("returns title from SDK when available and non-default", async () => {
    const cache = new Map<string, string>()
    const get = mock(async () => ({ data: { title: "Debugging production errors" } }))

    const title = await fetchTitle(get, "ses_abc", "/home/user/project", cache)
    expect(title).toBe("Debugging production errors")
    expect(get).toHaveBeenCalledTimes(1)
    expect(get).toHaveBeenCalledWith({ path: { id: "ses_abc" }, query: { directory: "/home/user/project" } })
  })

  test("caches title after first successful fetch", async () => {
    const cache = new Map<string, string>()
    const get = mock(async () => ({ data: { title: "Fix auth bug" } }))

    await fetchTitle(get, "ses_abc", "/home/user", cache)
    const title2 = await fetchTitle(get, "ses_abc", "/home/user", cache)

    expect(title2).toBe("Fix auth bug")
    expect(get).toHaveBeenCalledTimes(1) // cached — no second SDK call
  })

  test("returns undefined when data is missing (directory not passed)", async () => {
    const cache = new Map<string, string>()
    const get = mock(async () => ({ data: undefined }))

    const title = await fetchTitle(get, "ses_abc", "/home/user", cache)
    expect(title).toBeUndefined()
    expect(cache.size).toBe(0)
  })

  test("returns undefined for default 'New session' titles", async () => {
    const cache = new Map<string, string>()
    const get = mock(async () => ({ data: { title: "New session - 2026-04-01T22:18:18" } }))

    const title = await fetchTitle(get, "ses_abc", "/home/user", cache)
    expect(title).toBeUndefined()
    expect(cache.size).toBe(0) // not cached
  })

  test("does not cache undefined — retries on next call", async () => {
    const cache = new Map<string, string>()
    let callCount = 0
    const get = mock(async () => {
      callCount++
      if (callCount === 1) return { data: { title: "New session - 2026-04-01T00:00:00" } }
      return { data: { title: "Debugging 500 errors" } }
    })

    const t1 = await fetchTitle(get, "ses_abc", "/home/user", cache)
    expect(t1).toBeUndefined()

    const t2 = await fetchTitle(get, "ses_abc", "/home/user", cache)
    expect(t2).toBe("Debugging 500 errors")
    expect(get).toHaveBeenCalledTimes(2)
  })

  test("returns undefined on SDK error", async () => {
    const cache = new Map<string, string>()
    const get = mock(async () => { throw new Error("network error") })

    const title = await fetchTitle(get, "ses_abc", "/home/user", cache)
    expect(title).toBeUndefined()
  })

  test("passes directory parameter to SDK call", async () => {
    const cache = new Map<string, string>()
    const get = mock(async () => ({ data: { title: "Some title" } }))

    await fetchTitle(get, "ses_xyz", "/my/workspace", cache)
    expect(get).toHaveBeenCalledWith({ path: { id: "ses_xyz" }, query: { directory: "/my/workspace" } })
  })
})
