import { describe, expect, test } from "bun:test"
import type { ToolContext } from "@opencode-ai/plugin"
import { createHash } from "node:crypto"
import { mkdtemp, readFile, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import {
  canonicalIntent,
  createAgentHubRunTool,
  defaultRuntime,
  localResultV2Schema,
  readBounded,
  requestSchema,
  trustedInvocationID,
  type AgentHubRunRequest,
  type AgentHubRuntime,
} from "./agent-hub-run"

const DEFAULT_CALL_ID = "call_01JZZZZZZZZZZZZZZZZZZZZZZZ"

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical)
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value as Record<string, unknown>)
      .sort(([left], [right]) => left < right ? -1 : left > right ? 1 : 0)
      .map(([key, nested]) => [key, canonical(nested)]))
  }
  return value
}

function signReceipt<T extends { checksum: string }>(receipt: T): void {
  const { checksum: _checksum, ...decision } = receipt
  receipt.checksum = createHash("sha256")
    .update("agent-hub:agent-hub-run-receipt:v1\0")
    .update(JSON.stringify(canonical(decision)))
    .digest("hex")
}

const validRequest: AgentHubRunRequest = {
  schema: "agent_hub_run_intent_v1",
  environment: "code-auto",
  image_family: "job-code",
  network_class: "build",
  network_resources: ["llm", "go_registry"],
  tool_approval: "auto",
  overlays: ["github:github.com:read"],
  timeout_secs: 3_600,
  prompt: "Implement and test the requested change.",
  workspaces: [{ alias: "source", path: "/work/source", access: "read_write" as const }],
  context_roots: [{ alias: "reference", path: "/work/reference" }],
  primary_workspace: "source",
  workspace_export_root: "/work/source/.agent-hub/runs",
}

function successfulOutput(
  invocationID = trustedInvocationID("session", DEFAULT_CALL_ID),
  runID = "run",
) {
  const reference = {
    path: "/state/run/result.json",
    available: true,
    limit_bytes: 131_072,
    returned_bytes: 2,
    total_bytes: 2,
    truncated: false,
  }
  const document = { reference, document: {}, parse_error: null }
  const output = {
    schema: "agent_hub_run_output_v1",
    invocation_id: invocationID,
    approval_receipt: {
      schema: "agent_hub_run_approval_receipt_v1",
      status: "issued",
      receipt: {
        schema: "agent_hub_run_decision_v1",
        invocation_id: invocationID,
        canonical_digest: "a".repeat(64),
        request_digest: "b".repeat(64),
        decision: "allow",
        authority: "automatic",
        approval_id: invocationID,
        reason: null,
        decided_at: "2026-09-03T12:00:00Z",
        expires_at: "2099-09-03T13:00:00Z",
        checksum: "",
      },
    },
    local_result_v2: {
      schema: "local_result_v2",
      run_id: runID,
      launch_state: "terminal",
      status: "succeeded",
      exit_code: 0,
      error: null,
      permission_outcome: "no_denials",
      process_outcome: "succeeded",
      agent_outcome: "succeeded",
      execution_outcome: "succeeded",
      result_outcome: "succeeded",
      cleanup_outcome: "succeeded",
      agent_final: {
        schema: "agent_result_v1",
        complete: true,
        summary: "done",
        acceptance_results: [],
        blockers: [],
        artifact_references: [],
      },
      retry_class: null,
      retry_eligible: false,
      paths: { workspace_export: "/work/source/.agent-hub/runs/run" },
      result: document,
      artifacts: document,
      repositories: document,
      cleanup: document,
      stdout: reference,
      stderr: reference,
      transcript: reference,
    },
  }
  signReceipt(output.approval_receipt.receipt)
  return output
}

test("receipt checksum matches the Rust golden", () => {
  const output = successfulOutput()
  expect(output.invocation_id).toBe("7697e2a3-3afc-81ca-b119-970017dd1eca")
  expect(output.approval_receipt.receipt.checksum).toBe(
    "0d13310bb0c9dc973ca1c593fa96eb0beb06d172ce0cc392ded5aba70f9a6462",
  )
})

function runtime(overrides: Partial<AgentHubRuntime> = {}): AgentHubRuntime {
  return {
    env: { AGENT_HUB_BIN: "/opt/agent-hub/bin/agent-hub" },
    assertExecutable: async () => {},
    execute: async (_binary, _argv, _stdin, _signal, identity) => ({
      stdout: JSON.stringify(successfulOutput(trustedInvocationID(identity.sessionID, identity.toolCallID))),
      stderr: "",
      exitCode: 0,
    }),
    ...overrides,
  }
}

function context(
  signal: AbortSignal = new AbortController().signal,
  callID: string = DEFAULT_CALL_ID,
): ToolContext {
  return {
    sessionID: "session",
    messageID: "message",
    agent: "lead",
    directory: "/work/source",
    worktree: "/work/source",
    callID,
    abort: signal,
    metadata: () => {},
    ask: async () => {
      throw new Error("context.ask must not be called")
    },
  } as ToolContext & { callID: string }
}

describe("agent_hub_run contract", () => {
  test("matches the generated public intent shape and defaults", () => {
    const input = { ...validRequest, network_resources: undefined, overlays: undefined, context_roots: undefined }
    const parsed = requestSchema.parse(input)
    expect(canonicalIntent(parsed)).toEqual(
      canonicalIntent(
        requestSchema.parse({ ...validRequest, network_resources: [], overlays: [], context_roots: [] }),
      ),
    )
    expect(() => requestSchema.parse({ ...validRequest, workspace_output: {} })).toThrow()
    expect(() => requestSchema.parse({ ...validRequest, image_family: "caller-image" })).toThrow()
    expect(canonicalIntent(requestSchema.parse({ ...validRequest, workspace_export_root: null }))).not.toHaveProperty(
      "workspace_export_root",
    )
  })

  test("enforces Agent Hub alias, root, selector, and export rules", () => {
    const maximumAlias = "a".repeat(63)
    expect(() =>
      requestSchema.parse({
        ...validRequest,
        workspaces: [{ ...validRequest.workspaces[0]!, alias: maximumAlias }],
        primary_workspace: maximumAlias,
        workspace_export_root: undefined,
      }),
    ).not.toThrow()
    expect(() => requestSchema.parse({ ...validRequest, primary_workspace: "a".repeat(64) })).toThrow()
    expect(() => requestSchema.parse({ ...validRequest, primary_workspace: "missing" })).toThrow()
    expect(() => requestSchema.parse({ ...validRequest, workspaces: [{ ...validRequest.workspaces[0]!, alias: "Bad" }] })).toThrow()
    expect(() => requestSchema.parse({ ...validRequest, workspaces: [{ ...validRequest.workspaces[0]!, path: `/${"a".repeat(1024)}` }] })).toThrow()
    expect(() => requestSchema.parse({ ...validRequest, context_roots: [{ alias: "source", path: "/other" }] })).toThrow()
    expect(() => requestSchema.parse({ ...validRequest, overlays: ["same", "same"] })).toThrow()
    expect(() => requestSchema.parse({ ...validRequest, overlays: Array.from({ length: 17 }, (_, index) => `${index}`) })).toThrow()
    expect(() => requestSchema.parse({ ...validRequest, network_resources: ["llm", "llm"] })).toThrow()
    expect(() =>
      requestSchema.parse({
        ...validRequest,
        context_roots: Array.from({ length: 17 }, (_, index) => ({ alias: `root-${index}`, path: `/root/${index}` })),
      }),
    ).toThrow()
    expect(() => requestSchema.parse({ ...validRequest, workspace_export_root: "/work/source/output" })).toThrow()
    expect(() =>
      requestSchema.parse({ ...validRequest, workspace_export_root: "/work/source/.agent-hub/runs" }),
    ).not.toThrow()
  })

  test("uses only the fixed binary, exact argv, and canonical JSON stdin", async () => {
    const calls: Array<{
      binary: string
      argv: string[]
      stdin: string
      trustedIdentity: { sessionID: string; toolCallID: string }
    }> = []
    const instance = createAgentHubRunTool(
      runtime({
        execute: async (binary, argv, stdin, _signal, trustedIdentity) => {
          calls.push({ binary, argv, stdin, trustedIdentity })
          return { stdout: JSON.stringify(successfulOutput()), stderr: "", exitCode: 0 }
        },
      }),
    )
    const result = await instance.execute({ ...validRequest }, context())
    expect(calls).toEqual([
      {
        binary: "/opt/agent-hub/bin/agent-hub",
        argv: ["local", "run", "--request-stdin"],
        stdin: JSON.stringify(canonicalIntent(requestSchema.parse(validRequest))),
        trustedIdentity: { sessionID: "session", toolCallID: "call_01JZZZZZZZZZZZZZZZZZZZZZZZ" },
      },
    ])
    const text = typeof result === "string" ? result : result.output
    expect(JSON.parse(text)).toEqual(successfulOutput())
  })

  test("derives the exact Rust trusted invocation UUIDv8 vector", () => {
    expect(trustedInvocationID("session", "call")).toBe("22356e2a-8bf3-841c-a45a-4aaf1ab6c463")
  })

  test("accepts automatic and human receipts with valid Rust-domain checksums", async () => {
    for (const authority of ["automatic", "human"] as const) {
      const output = successfulOutput()
      output.approval_receipt.receipt.authority = authority
      if (authority === "human") output.approval_receipt.receipt.approval_id = "00000000-0000-0000-0000-000000000007"
      signReceipt(output.approval_receipt.receipt)
      const instance = createAgentHubRunTool(runtime({ execute: async () => ({
        stdout: JSON.stringify(output), stderr: "", exitCode: 0,
      }) }))
      await expect(instance.execute({ ...validRequest }, context())).resolves.toBeTruthy()
    }
  })

  test("accepts a receipt that was valid at call start but expires during the subprocess", async () => {
    const instance = createAgentHubRunTool(runtime({ execute: async () => {
      const output = successfulOutput()
      output.approval_receipt.receipt.decided_at = new Date(Date.now() - 1_000).toISOString()
      output.approval_receipt.receipt.expires_at = new Date(Date.now() + 100).toISOString()
      signReceipt(output.approval_receipt.receipt)
      await Bun.sleep(200)
      return { stdout: JSON.stringify(output), stderr: "", exitCode: 0 }
    } }))
    await expect(instance.execute({ ...validRequest }, context())).resolves.toBeTruthy()
  })

  test("rejects untrusted bindings, noncanonical fields, invalid authority, time, and checksum", async () => {
    const otherInvocation = "00000000-0000-0000-0000-000000000009"
    const mutations: Array<(output: ReturnType<typeof successfulOutput>) => void> = [
      (output) => { output.invocation_id = otherInvocation },
      (output) => {
        output.approval_receipt.receipt.invocation_id = otherInvocation
        output.approval_receipt.receipt.approval_id = otherInvocation
        signReceipt(output.approval_receipt.receipt)
      },
      (output) => { output.approval_receipt.receipt.canonical_digest = "A".repeat(64) },
      (output) => { output.approval_receipt.receipt.checksum = "A".repeat(64) },
      (output) => { output.approval_receipt.receipt.checksum = "0".repeat(64) },
      (output) => {
        output.approval_receipt.receipt.approval_id = otherInvocation
        signReceipt(output.approval_receipt.receipt)
      },
      (output) => {
        output.approval_receipt.receipt.decided_at = new Date(Date.now() + 60_000).toISOString()
        signReceipt(output.approval_receipt.receipt)
      },
      (output) => {
        output.approval_receipt.receipt.expires_at = new Date(Date.now() - 1).toISOString()
        signReceipt(output.approval_receipt.receipt)
      },
    ]
    for (const mutate of mutations) {
      const output = successfulOutput()
      mutate(output)
      const instance = createAgentHubRunTool(runtime({ execute: async () => ({
        stdout: JSON.stringify(output), stderr: "", exitCode: 0,
      }) }))
      await expect(instance.execute({ ...validRequest }, context())).rejects.toThrow()
    }
  })

  test("gives identical calls in one message distinct invocations, approvals, and runs", async () => {
    const calls: string[] = []
    const invocationIDs = [
      trustedInvocationID("session", "call_same_message_1"),
      trustedInvocationID("session", "call_same_message_2"),
    ]
    const instance = createAgentHubRunTool(
      runtime({
        execute: async (_binary, _argv, _stdin, _signal, trustedIdentity) => {
          calls.push(trustedIdentity.toolCallID)
          return {
            stdout: JSON.stringify(successfulOutput(invocationIDs[calls.length - 1], `run-${calls.length}`)),
            stderr: "",
            exitCode: 0,
          }
        },
      }),
    )

    const results = await Promise.all([
      instance.execute({ ...validRequest }, context(undefined, "call_same_message_1")),
      instance.execute({ ...validRequest }, context(undefined, "call_same_message_2")),
    ])

    expect(calls).toEqual(["call_same_message_1", "call_same_message_2"])
    expect(results.map((result) => JSON.parse(typeof result === "string" ? result : result.output).invocation_id)).toEqual(
      invocationIDs,
    )
    expect(
      results.map(
        (result) => JSON.parse(typeof result === "string" ? result : result.output).approval_receipt.receipt.approval_id,
      ),
    ).toEqual(invocationIDs)
    expect(
      results.map((result) => JSON.parse(typeof result === "string" ? result : result.output).local_result_v2.run_id),
    ).toEqual(["run-1", "run-2"])
  })

  test("gives differing calls in one message distinct invocations and runs", async () => {
    const calls: Array<{ toolCallID: string; prompt: string }> = []
    const instance = createAgentHubRunTool(
      runtime({
        execute: async (_binary, _argv, stdin, _signal, trustedIdentity) => {
          calls.push({ toolCallID: trustedIdentity.toolCallID, prompt: JSON.parse(stdin).prompt })
          const invocationID = trustedInvocationID("session", trustedIdentity.toolCallID)
          return { stdout: JSON.stringify(successfulOutput(invocationID, `run-${calls.length}`)), stderr: "", exitCode: 0 }
        },
      }),
    )

    const results = await Promise.all([
      instance.execute({ ...validRequest, prompt: "first" }, context(undefined, "call_different_1")),
      instance.execute({ ...validRequest, prompt: "second" }, context(undefined, "call_different_2")),
    ])

    expect(calls).toEqual([
      { toolCallID: "call_different_1", prompt: "first" },
      { toolCallID: "call_different_2", prompt: "second" },
    ])
    expect(
      results.map((result) => {
        const output = JSON.parse(typeof result === "string" ? result : result.output)
        return [output.invocation_id, output.approval_receipt.receipt.approval_id, output.local_result_v2.run_id]
      }),
    ).toEqual([
      [trustedInvocationID("session", "call_different_1"), trustedInvocationID("session", "call_different_1"), "run-1"],
      [trustedInvocationID("session", "call_different_2"), trustedInvocationID("session", "call_different_2"), "run-2"],
    ])
  })

  test("keeps the trusted identity stable for Agent Hub internal retries", async () => {
    const internalAttempts: string[] = []
    let agentHubEntries = 0
    const instance = createAgentHubRunTool(
      runtime({
        execute: async (_binary, _argv, _stdin, _signal, trustedIdentity) => {
          agentHubEntries++
          internalAttempts.push(trustedIdentity.toolCallID)
          internalAttempts.push(trustedIdentity.toolCallID)
          return {
            stdout: JSON.stringify(successfulOutput(trustedInvocationID("session", trustedIdentity.toolCallID))),
            stderr: "",
            exitCode: 0,
          }
        },
      }),
    )

    await instance.execute({ ...validRequest }, context(undefined, "call_with_internal_retry"))
    expect(agentHubEntries).toBe(1)
    expect(internalAttempts).toEqual(["call_with_internal_retry", "call_with_internal_retry"])
  })

  test("rejects execution without the trusted OpenCode call ID", async () => {
    const instance = createAgentHubRunTool(runtime())
    const untrustedContext = { ...context() }
    delete (untrustedContext as Partial<ToolContext & { callID: string }>).callID
    await expect(instance.execute({ ...validRequest }, untrustedContext)).rejects.toThrow("trusted OpenCode")
  })

  test("requires an absolute configured executable", async () => {
    const instance = createAgentHubRunTool(runtime({ env: { AGENT_HUB_BIN: "agent-hub" } }))
    await expect(instance.execute({ ...validRequest }, context(undefined, "call_invalid_binary"))).rejects.toThrow(
      "fixed absolute",
    )
  })

  test("strictly rejects malformed, failed, incomplete, and unclean output", async () => {
    for (const mutate of [
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value, { extra: true }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2, { execution_outcome: "failed" }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2, { result_outcome: "failed" }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2.agent_final!, { complete: false }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2, { cleanup_outcome: "failed" }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2, { retry_eligible: true }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2, { launch_state: "running" }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2, { exit_code: 1 }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2, { error: "cleanup failed" }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2, { permission_outcome: "denied" }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2, { permission_outcome: "readiness_failed" }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2, { retry_class: "prelaunch" }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2.agent_final, { blockers: ["blocked"] }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2.agent_final, {
        acceptance_results: [{ criterion: "tests", passed: false, evidence: "failed" }],
      }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2.agent_final, { summary: " " }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2.agent_final, {
        acceptance_results: [{ criterion: "tests", passed: true, evidence: " " }],
      }),
      (value: ReturnType<typeof successfulOutput>) => Object.assign(value.local_result_v2.agent_final, {
        artifact_references: [{ path: "", description: "result" }],
      }),
      (value: ReturnType<typeof successfulOutput>) =>
        Object.assign(value.approval_receipt.receipt!, { invocation_id: "00000000-0000-4000-8000-000000000099" }),
    ]) {
      const callID = crypto.randomUUID()
      const output = successfulOutput(trustedInvocationID("session", callID))
      mutate(output)
      const instance = createAgentHubRunTool(
        runtime({ execute: async () => ({ stdout: JSON.stringify(output), stderr: "", exitCode: 0 }) }),
      )
      await expect(instance.execute({ ...validRequest }, context(undefined, callID))).rejects.toThrow()
    }
  })

  test("rejects success-shaped results accompanying a non-zero error or denial", async () => {
    for (const denial of [false, true]) {
      for (const result of [
        successfulOutput().local_result_v2,
        { schema: "local_result_v2", status: "succeeded" },
        { schema: "local_result_v2", execution_outcome: "succeeded", result_outcome: "succeeded", cleanup_outcome: "succeeded" },
      ]) {
        const output = { ...successfulOutput(), error: denial ? null : "host failed", local_result_v2: result }
        if (denial) {
          output.approval_receipt.receipt.decision = "deny"
          signReceipt(output.approval_receipt.receipt)
        }
        const instance = createAgentHubRunTool(
          runtime({ execute: async () => ({ stdout: JSON.stringify(output), stderr: "", exitCode: 1 }) }),
        )
        await expect(instance.execute({ ...validRequest }, context())).rejects.toThrow(/contradict/)
      }
    }
  })

  test("preserves failures after a successful process and sparse prelaunch denials", async () => {
    const output = {
      ...successfulOutput(), error: "invalid agent result",
      local_result_v2: {
        ...successfulOutput().local_result_v2,
        status: "failed", agent_outcome: "malformed", result_outcome: "failed",
      },
    }
    const denied = {
      ...successfulOutput(),
      local_result_v2: { schema: "local_result_v2", run_id: null, status: "failed", cleanup_outcome: "unknown" },
    }
    denied.approval_receipt.receipt.decision = "deny"
    signReceipt(denied.approval_receipt.receipt)
    for (const failure of [output, denied]) {
      const instance = createAgentHubRunTool(
        runtime({ execute: async () => ({ stdout: JSON.stringify(failure), stderr: "", exitCode: 1 }) }),
      )
      const result = await instance.execute({ ...validRequest }, context())
      expect(JSON.parse(typeof result === "string" ? result : result.output)).toEqual(failure)
    }
  })

  test("returns a validated failed terminal aggregate without an envelope error", async () => {
    // host_run.rs returns error: None for run_allowed's Ok(LocalResult), even when main.rs exits nonzero.
    const failure = {
      ...successfulOutput(),
      error: null,
      local_result_v2: {
        ...successfulOutput().local_result_v2,
        status: "failed", execution_outcome: "failed", process_outcome: "failed",
        agent_outcome: "not_produced", agent_final: null, result_outcome: "failed",
        exit_code: 1, error: "worker failed",
      },
    }
    const instance = createAgentHubRunTool(
      runtime({ execute: async () => ({ stdout: JSON.stringify(failure), stderr: "", exitCode: 1 }) }),
    )
    const result = await instance.execute({ ...validRequest }, context())
    const output = JSON.parse(typeof result === "string" ? result : result.output)
    expect(output).toEqual(failure)
    expect(output.local_result_v2.run_id).toBe("run")
    expect(output.local_result_v2.cleanup_outcome).toBe("succeeded")

    for (const aggregate of [
      { ...failure.local_result_v2, cleanup: undefined },
      { ...failure.local_result_v2, launch_state: "running" },
      { schema: "local_result_v2", status: "failed", execution_outcome: "failed" },
    ]) {
      const invalid = createAgentHubRunTool(runtime({ execute: async () => ({
        stdout: JSON.stringify({ ...failure, local_result_v2: aggregate }), stderr: "", exitCode: 1,
      }) }))
      await expect(invalid.execute({ ...validRequest }, context())).rejects.toThrow(/without a structured failure/)
    }
  })

  test("returns a structured failure from a non-zero Agent Hub exit", async () => {
    const failure = {
      schema: "agent_hub_run_output_v1",
      invocation_id: trustedInvocationID("session", "call_failure"),
      approval_receipt: {
        schema: "agent_hub_run_approval_receipt_v1",
        status: "unavailable",
        error: "host setup failed",
      },
      local_result_v2: { schema: "local_result_v2", run_id: null, status: "failed", cleanup_outcome: "unknown" },
      error: "host setup failed",
    }
    const instance = createAgentHubRunTool(
      runtime({ execute: async () => ({ stdout: JSON.stringify(failure), stderr: "host setup failed", exitCode: 1 }) }),
    )
    const result = await instance.execute({ ...validRequest }, context(undefined, "call_failure"))
    expect(JSON.parse(typeof result === "string" ? result : result.output)).toEqual(failure)
  })

  test("parses the exact bounded local result v2 aggregate", () => {
    const reference = {
      path: "/state/run/result.json",
      available: true,
      limit_bytes: 1_048_576,
      returned_bytes: 2,
      total_bytes: 2,
      truncated: false,
    }
    const document = { reference, document: {}, parse_error: null }
    const aggregate = {
      schema: "local_result_v2",
      run_id: "run",
      launch_state: "terminal",
      status: "succeeded",
      error: null,
      agent_final: successfulOutput().local_result_v2.agent_final,
      process_outcome: "succeeded",
      agent_outcome: "succeeded",
      execution_outcome: "succeeded",
      result_outcome: "succeeded",
      permission_outcome: "no_denials",
      cleanup_outcome: "succeeded",
      exit_code: 0,
      retry_class: null,
      retry_eligible: false,
      paths: { workspace_export: "/work/source/.agent-hub/runs/run" },
      result: document,
      artifacts: document,
      repositories: document,
      cleanup: document,
      stdout: reference,
      stderr: reference,
      transcript: reference,
    }
    expect(localResultV2Schema.safeParse(aggregate).success).toBe(true)
    expect(() => localResultV2Schema.parse({ ...aggregate, extra: true })).toThrow()
  })
})

describe("Agent Hub process bounds", () => {
  for (const stream of ["stdout", "stderr"] as const) {
    test(`reaps the owned process before rejecting ${stream} overflow`, async () => {
      const root = await mkdtemp(join(tmpdir(), "agent-hub-reap-"))
      const marker = join(root, "reaped")
      try {
        const running = defaultRuntime.execute(process.execPath, ["-e", `
          import { writeFileSync } from "node:fs";
          process.on("SIGTERM", () => setTimeout(() => {
            writeFileSync(${JSON.stringify(marker)}, String(process.pid));
            process.exit(7);
          }, 100));
          process.${stream}.write("x".repeat(1048577));
          await Bun.sleep(1000);
        `], "", new AbortController().signal, { sessionID: "session", toolCallID: "call" })
        await expect(running).rejects.toThrow("exceeded")
        const pid = Number(await readFile(marker, "utf8"))
        expect(() => process.kill(pid, 0)).toThrow()
      } finally {
        await rm(root, { recursive: true, force: true })
      }
    })
  }

  test("terminates a stream once its byte bound is exceeded", async () => {
    let terminated = false
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(new TextEncoder().encode("too large"))
        controller.close()
      },
    })
    await expect(readBounded(stream, 3, "test", () => (terminated = true))).rejects.toThrow("exceeded")
    expect(terminated).toBe(true)
  })

  test("accepts a stream exactly at its byte bound", async () => {
    const stream = new Blob(["exact"]).stream()
    expect(await readBounded(stream, 5, "test", () => {})).toBe("exact")
  })

  test("cooperatively terminates a launched process when the tool is aborted", async () => {
    const controller = new AbortController()
    const running = defaultRuntime.execute(
      process.execPath,
      [
        "-e",
        'process.on("SIGTERM",()=>{console.log(`${process.env.AGENT_HUB_TRUSTED_SESSION_ID}:${process.env.AGENT_HUB_TRUSTED_TOOL_CALL_ID}`);process.exit(7)});await Bun.sleep(30000)',
      ],
      "",
      controller.signal,
      { sessionID: "session", toolCallID: "call" },
    )
    await Bun.sleep(50)
    controller.abort()
    expect(await running).toEqual({ stdout: "session:call\n", stderr: "", exitCode: 7 })
  })
})
