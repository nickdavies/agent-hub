import path from "path"

export type ApprovalEnvironment = Record<string, string | undefined>

type AttendedBinding = Readonly<{
  run_epoch: string
  authorization_digest: string
  environment_bundle_digest: string
  image_digest: string
  network_digest: string
  approval_bundle_digest: string
}>

export type ApprovalTransport = Readonly<
  | {
    kind: "attended_socket"
    socket: string
    binding: string
    deadline: string
    nonce: string
  }
  | { kind: "http"; server: string; token?: string }
>

const ATTENDED_KEYS = [
  "AGENT_HUB_ATTENDED_APPROVAL_SOCKET",
  "AGENT_HUB_ATTENDED_APPROVAL_BINDING",
  "AGENT_HUB_ATTENDED_RUN_DEADLINE",
  "AGENT_HUB_PLUGIN_RUN_NONCE",
] as const

const SECRET_KEYS = [
  "AGENT_HUB_SERVER",
  "AGENT_HUB_TOKEN",
  "AGENT_HUB_APPROVALS_SERVER",
  "AGENT_HUB_APPROVALS_TOKEN",
] as const

function required(env: ApprovalEnvironment, name: string): string {
  const value = env[name]
  if (!value) throw new Error(`${name} is required in attended_socket mode`)
  return value
}

function parseBinding(raw: string): string {
  let value: unknown
  try {
    value = JSON.parse(raw)
  } catch {
    throw new Error("AGENT_HUB_ATTENDED_APPROVAL_BINDING must be valid JSON")
  }
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("AGENT_HUB_ATTENDED_APPROVAL_BINDING must be an object")
  }
  const binding = value as Record<string, unknown>
  const expected = [
    "approval_bundle_digest",
    "authorization_digest",
    "environment_bundle_digest",
    "image_digest",
    "network_digest",
    "run_epoch",
  ]
  if (Object.keys(binding).sort().join("\0") !== expected.join("\0")) {
    throw new Error("AGENT_HUB_ATTENDED_APPROVAL_BINDING has missing or unknown fields")
  }
  const hex = /^[0-9a-f]{64}$/
  for (const key of ["approval_bundle_digest", "authorization_digest", "environment_bundle_digest", "network_digest"]) {
    if (typeof binding[key] !== "string" || !hex.test(binding[key])) {
      throw new Error(`AGENT_HUB_ATTENDED_APPROVAL_BINDING.${key} is not lowercase SHA-256`)
    }
  }
  if (typeof binding.image_digest !== "string" || !/^sha256:[0-9a-f]{64}$/.test(binding.image_digest)) {
    throw new Error("AGENT_HUB_ATTENDED_APPROVAL_BINDING.image_digest is invalid")
  }
  if (typeof binding.run_epoch !== "string" || !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(binding.run_epoch)) {
    throw new Error("AGENT_HUB_ATTENDED_APPROVAL_BINDING.run_epoch is not a UUID")
  }
  return JSON.stringify(Object.fromEntries(expected.map((key) => [key, binding[key]])) as AttendedBinding)
}

/** Capture this once at plugin initialization so a run cannot redirect its
 * approval authority by mutating process.env. */
export function approvalTransport(env: ApprovalEnvironment): ApprovalTransport {
  const mode = env.AGENT_HUB_APPROVAL_MODE
  if (mode === "attended_socket") {
    for (const name of SECRET_KEYS) {
      if (Object.prototype.hasOwnProperty.call(env, name)) {
        throw new Error(`${name} is forbidden in attended_socket mode`)
      }
    }
    const socket = required(env, "AGENT_HUB_ATTENDED_APPROVAL_SOCKET")
    if (!path.isAbsolute(socket)) {
      throw new Error("AGENT_HUB_ATTENDED_APPROVAL_SOCKET must be absolute")
    }
    const binding = parseBinding(required(env, "AGENT_HUB_ATTENDED_APPROVAL_BINDING"))
    const deadline = required(env, "AGENT_HUB_ATTENDED_RUN_DEADLINE")
    if (!Number.isFinite(Date.parse(deadline)) || Date.parse(deadline) <= Date.now()) {
      throw new Error("AGENT_HUB_ATTENDED_RUN_DEADLINE must be a future RFC 3339 timestamp")
    }
    const nonce = required(env, "AGENT_HUB_PLUGIN_RUN_NONCE")
    if (!/^[0-9a-f]{32}$/.test(nonce)) {
      throw new Error("AGENT_HUB_PLUGIN_RUN_NONCE must be 32 lowercase hexadecimal characters")
    }
    return Object.freeze({
      kind: "attended_socket" as const,
      socket,
      binding,
      deadline,
      nonce,
    })
  }
  if (mode !== undefined) throw new Error(`unsupported AGENT_HUB_APPROVAL_MODE '${mode}'`)
  for (const name of ATTENDED_KEYS) {
    if (Object.prototype.hasOwnProperty.call(env, name)) {
      throw new Error(`${name} requires AGENT_HUB_APPROVAL_MODE=attended_socket`)
    }
  }
  return Object.freeze({
    kind: "http" as const,
    server: env.AGENT_HUB_SERVER ?? "http://127.0.0.1:8080",
    ...(env.AGENT_HUB_TOKEN ? { token: env.AGENT_HUB_TOKEN } : {}),
  })
}

export function gatewayApprovalInvocation(
  transport: ApprovalTransport,
  inheritedEnv: ApprovalEnvironment,
  execution?: Readonly<{ callID: string; requestID: string; rawDigest: string; canonicalDigest: string }>,
): { args: string[]; env: ApprovalEnvironment } {
  if (transport.kind === "http") {
    return {
      args: [
        "approval", "--opencode", "--server", transport.server,
        ...(transport.token ? ["--token", transport.token] : []),
      ],
      env: inheritedEnv,
    }
  }
  const env = { ...inheritedEnv }
  delete env.AGENT_HUB_APPROVAL_MODE
  for (const name of [...ATTENDED_KEYS, ...SECRET_KEYS]) delete env[name]
  return {
    args: [
      "approval", "--opencode",
      "--approval-mode", "attended_socket",
      "--approval-socket", transport.socket,
      "--approval-binding", transport.binding,
      "--approval-deadline", transport.deadline,
      ...(execution ? [
        "--call-id", execution.callID,
        "--request-id", execution.requestID,
        "--raw-args-digest", execution.rawDigest,
        "--canonical-args-digest", execution.canonicalDigest,
      ] : []),
    ],
    env,
  }
}

export function gatewaySupervisionInvocation(
  transport: Extract<ApprovalTransport, { kind: "attended_socket" }>,
  inheritedEnv: ApprovalEnvironment,
): { args: string[]; env: ApprovalEnvironment } {
  const env = { ...inheritedEnv }
  delete env.AGENT_HUB_APPROVAL_MODE
  for (const name of [...ATTENDED_KEYS, ...SECRET_KEYS]) delete env[name]
  return {
    args: [
      "supervision",
      "--approval-socket", transport.socket,
      "--approval-binding", transport.binding,
      "--approval-deadline", transport.deadline,
    ],
    env,
  }
}
