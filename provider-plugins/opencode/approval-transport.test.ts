import { describe, expect, test } from "bun:test"
import { approvalTransport, gatewayApprovalInvocation } from "./approval-transport"

const binding = JSON.stringify({
  run_epoch: "018f0f21-76c0-7b37-a446-31f82a5e7601",
  authorization_digest: "a".repeat(64),
  environment_bundle_digest: "b".repeat(64),
  image_digest: `sha256:${"c".repeat(64)}`,
  network_digest: "d".repeat(64),
  approval_bundle_digest: "e".repeat(64),
})

const readiness = {
  AGENT_HUB_PLUGIN_RUN_NONCE: "f".repeat(32),
}

describe("attended socket approval transport", () => {
  test("passes only one complete immutable transport to the gateway", () => {
    const transport = approvalTransport({
      AGENT_HUB_APPROVAL_MODE: "attended_socket",
      AGENT_HUB_ATTENDED_APPROVAL_SOCKET: "/agent-hub/approval/approval.sock",
      AGENT_HUB_ATTENDED_APPROVAL_BINDING: binding,
      AGENT_HUB_ATTENDED_RUN_DEADLINE: "2099-01-02T03:04:05Z",
      ...readiness,
    })
    const invocation = gatewayApprovalInvocation(transport, {
      PATH: "/bin",
      AGENT_HUB_APPROVAL_MODE: "attended_socket",
      AGENT_HUB_ATTENDED_APPROVAL_SOCKET: "/agent-hub/replaced.sock",
      AGENT_HUB_ATTENDED_APPROVAL_BINDING: "replaced",
      AGENT_HUB_ATTENDED_RUN_DEADLINE: "replaced",
      ...readiness,
      AGENT_HUB_SERVER: "secret",
      AGENT_HUB_TOKEN: "secret",
    })
    expect(Object.isFrozen(transport)).toBe(true)
    expect(invocation.args).toContain("/agent-hub/approval/approval.sock")
    expect(invocation.args).not.toContain("--approval-sequence")
    expect(invocation.env).toEqual({ PATH: "/bin" })
  })

  test("rejects implicit, incomplete, mixed, and malformed socket transports", () => {
    expect(() => approvalTransport({ AGENT_HUB_ATTENDED_APPROVAL_SOCKET: "/agent-hub/approval/approval.sock" })).toThrow()
    expect(() => approvalTransport({ AGENT_HUB_APPROVAL_MODE: "attended_socket" })).toThrow()
    expect(() => approvalTransport({
      AGENT_HUB_APPROVAL_MODE: "attended_socket",
      AGENT_HUB_ATTENDED_APPROVAL_SOCKET: "relative.sock",
      AGENT_HUB_ATTENDED_APPROVAL_BINDING: binding,
      AGENT_HUB_ATTENDED_RUN_DEADLINE: "2099-01-02T03:04:05Z",
      ...readiness,
    })).toThrow()
    expect(() => approvalTransport({
      AGENT_HUB_APPROVAL_MODE: "attended_socket",
      AGENT_HUB_ATTENDED_APPROVAL_SOCKET: "/agent-hub/approval/approval.sock",
      AGENT_HUB_ATTENDED_APPROVAL_BINDING: binding,
      AGENT_HUB_ATTENDED_RUN_DEADLINE: "2099-01-02T03:04:05Z",
      ...readiness,
      AGENT_HUB_TOKEN: "must-not-enter-sandbox",
    })).toThrow()
  })
})

describe("host approval transport", () => {
  test("defaults no-auth transport to HTTP without a token", () => {
    const invocation = gatewayApprovalInvocation(approvalTransport({}), {})
    expect(invocation.args).toEqual([
      "approval", "--opencode", "--server", "http://127.0.0.1:8080",
    ])
  })

  test("preserves the existing HTTP URL and token arguments", () => {
    const transport = approvalTransport({ AGENT_HUB_SERVER: "https://hub.example", AGENT_HUB_TOKEN: "token" })
    const invocation = gatewayApprovalInvocation(transport, {})
    expect(invocation.args).toEqual(["approval", "--opencode", "--server", "https://hub.example", "--token", "token"])
  })

  test("preserves nonloopback HTTP without a token", () => {
    const transport = approvalTransport({ AGENT_HUB_SERVER: "http://approvals.lan:8080" })
    const invocation = gatewayApprovalInvocation(transport, {})
    expect(invocation.args).toEqual(["approval", "--opencode", "--server", "http://approvals.lan:8080"])
  })
})
