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

describe("attended socket approval transport", () => {
  test("passes only one complete immutable transport to the gateway", () => {
    const transport = approvalTransport({
      AGENT_HUB_APPROVAL_MODE: "attended_socket",
      AGENT_HUB_ATTENDED_APPROVAL_SOCKET: "/agent-hub/approval.sock",
      AGENT_HUB_ATTENDED_APPROVAL_BINDING: binding,
      AGENT_HUB_ATTENDED_RUN_DEADLINE: "2099-01-02T03:04:05Z",
    })
    const invocation = gatewayApprovalInvocation(transport, {
      PATH: "/bin",
      AGENT_HUB_APPROVAL_MODE: "attended_socket",
      AGENT_HUB_ATTENDED_APPROVAL_SOCKET: "/agent-hub/replaced.sock",
      AGENT_HUB_ATTENDED_APPROVAL_BINDING: "replaced",
      AGENT_HUB_ATTENDED_RUN_DEADLINE: "replaced",
      AGENT_HUB_SERVER: "secret",
      AGENT_HUB_TOKEN: "secret",
    }, 7)
    expect(Object.isFrozen(transport)).toBe(true)
    expect(invocation.args).toContain("/agent-hub/approval.sock")
    expect(invocation.args).toContain("7")
    expect(invocation.env).toEqual({ PATH: "/bin" })
  })

  test("rejects implicit, incomplete, mixed, and malformed socket transports", () => {
    expect(() => approvalTransport({ AGENT_HUB_ATTENDED_APPROVAL_SOCKET: "/agent-hub/approval.sock" })).toThrow()
    expect(() => approvalTransport({ AGENT_HUB_APPROVAL_MODE: "attended_socket" })).toThrow()
    expect(() => approvalTransport({
      AGENT_HUB_APPROVAL_MODE: "attended_socket",
      AGENT_HUB_ATTENDED_APPROVAL_SOCKET: "relative.sock",
      AGENT_HUB_ATTENDED_APPROVAL_BINDING: binding,
      AGENT_HUB_ATTENDED_RUN_DEADLINE: "2099-01-02T03:04:05Z",
    })).toThrow()
    expect(() => approvalTransport({
      AGENT_HUB_APPROVAL_MODE: "attended_socket",
      AGENT_HUB_ATTENDED_APPROVAL_SOCKET: "/agent-hub/approval.sock",
      AGENT_HUB_ATTENDED_APPROVAL_BINDING: binding,
      AGENT_HUB_ATTENDED_RUN_DEADLINE: "2099-01-02T03:04:05Z",
      AGENT_HUB_TOKEN: "must-not-enter-sandbox",
    })).toThrow()
  })
})

describe("host approval transport", () => {
  test("preserves the existing HTTP URL and token arguments", () => {
    const transport = approvalTransport({ AGENT_HUB_SERVER: "https://hub.example", AGENT_HUB_TOKEN: "token" })
    const invocation = gatewayApprovalInvocation(transport, {})
    expect(invocation.args).toEqual(["approval", "--opencode", "--server", "https://hub.example", "--token", "token"])
  })
})
