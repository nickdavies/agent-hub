import { createHash } from "crypto"
import { describe, expect, test } from "bun:test"
import fixture from "../../protocol/tests/fixtures/attended_gateway_v1.json"

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical)
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).sort(([left], [right]) => left < right ? -1 : left > right ? 1 : 0).map(([key, nested]) => [key, canonical(nested)]))
  }
  return value
}

function canonicalJson(value: unknown): string {
  return JSON.stringify(canonical(value))
}

function framePrefix(value: string): string {
  const prefix = Buffer.alloc(4)
  prefix.writeUInt32BE(Buffer.byteLength(value))
  return prefix.toString("hex")
}

describe("attended gateway cross-language golden fixture", () => {
  test("matches canonical JSON, domain digest, and big-endian frame lengths", () => {
    expect(canonicalJson(fixture.tool_input)).toBe(fixture.tool_input_canonical)
    expect(createHash("sha256")
      .update("agent-hub:attended-tool-input:v1\0")
      .update(fixture.tool_input_canonical)
      .digest("hex")).toBe(fixture.tool_input_digest)
    expect(canonicalJson(fixture.request)).toBe(fixture.request_canonical)
    expect(framePrefix(fixture.request_canonical)).toBe(fixture.request_frame_prefix_hex)
    expect(canonicalJson(fixture.decision)).toBe(fixture.decision_canonical)
    expect(framePrefix(fixture.decision_canonical)).toBe(fixture.decision_frame_prefix_hex)
  })
})
