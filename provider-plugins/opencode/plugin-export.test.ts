import { afterAll, describe, expect, test } from "bun:test"
import { mkdirSync, rmSync, writeFileSync } from "node:fs"
import { pathToFileURL } from "node:url"
import path from "node:path"

const fixtureRoot = path.resolve("../../target", `opencode-plugin-export-${crypto.randomUUID()}`)

afterAll(() => rmSync(fixtureRoot, { recursive: true, force: true }))

describe("bundled plugin entrypoint", () => {
  test("exports the exact OpenCode Plugin function shape", async () => {
    mkdirSync(fixtureRoot, { recursive: true })
    const build = await Bun.build({
      entrypoints: [path.resolve("agent-hub.ts")],
      format: "esm",
      minify: true,
      target: "node",
      write: false,
    })
    expect(build.success).toBe(true)
    expect(build.outputs).toHaveLength(1)
    const artifact = path.join(fixtureRoot, "agent-hub-opencode-plugin.js")
    writeFileSync(artifact, await build.outputs[0].arrayBuffer())

    const inspection = Bun.spawnSync([
      "node",
      "--eval",
      `import(${JSON.stringify(pathToFileURL(artifact).href)}).then((module) => console.log(JSON.stringify({defaultType: typeof module.default, keys: Object.keys(module).sort()})))`,
    ], {
      env: {
        ...process.env,
        AGENT_HUB_APPROVAL_MODE: "attended_socket",
        AGENT_HUB_ATTENDED_APPROVAL_SOCKET: "/agent-hub/approval/approval.sock",
        AGENT_HUB_ATTENDED_APPROVAL_BINDING: JSON.stringify({
          run_epoch: "018f0f21-76c0-7b37-a446-31f82a5e7601",
          authorization_digest: "a".repeat(64),
          environment_bundle_digest: "b".repeat(64),
          image_digest: `sha256:${"c".repeat(64)}`,
          network_digest: "d".repeat(64),
          approval_bundle_digest: "e".repeat(64),
        }),
        AGENT_HUB_ATTENDED_RUN_DEADLINE: "2099-01-02T03:04:05Z",
        AGENT_HUB_PLUGIN_RUN_NONCE: "f".repeat(32),
      },
    })
    expect(inspection.exitCode).toBe(0)
    expect(JSON.parse(inspection.stdout.toString())).toEqual({
      defaultType: "function",
      keys: ["default"],
    })
  })
})
