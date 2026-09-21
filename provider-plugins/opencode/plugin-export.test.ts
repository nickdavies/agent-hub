import { afterAll, describe, expect, test } from "bun:test"
import { mkdirSync, rmSync, writeFileSync } from "node:fs"
import { pathToFileURL } from "node:url"
import path from "node:path"

const fixtureRoot = path.resolve("../../target", `opencode-plugin-export-${crypto.randomUUID()}`)
const forbiddenHTTPTransportKeys = [
  "AGENT_HUB_SERVER",
  "AGENT_HUB_TOKEN",
  "AGENT_HUB_APPROVALS_SERVER",
  "AGENT_HUB_APPROVALS_TOKEN",
] as const

afterAll(() => rmSync(fixtureRoot, { recursive: true, force: true }))

describe("bundled plugin entrypoint", () => {
  test("exports the OpenCode Plugin function as default", async () => {
    mkdirSync(fixtureRoot, { recursive: true })
    const build = await Bun.build({
      entrypoints: [path.resolve("index.ts")],
      format: "esm",
      minify: true,
      target: "node",
    })
    expect(build.success).toBe(true)
    expect(build.outputs).toHaveLength(1)
    const artifact = path.join(fixtureRoot, "agent-hub-opencode-plugin.js")
    writeFileSync(artifact, new Uint8Array(await build.outputs[0].arrayBuffer()))

    const globalTransportBefore = Object.fromEntries(
      forbiddenHTTPTransportKeys.map((key) => [key, process.env[key]]),
    )
    const inheritedParentEnv: NodeJS.ProcessEnv = {
      ...process.env,
      AGENT_HUB_SERVER: "https://synthetic-parent.invalid",
      AGENT_HUB_TOKEN: "synthetic-parent-token",
      AGENT_HUB_APPROVALS_SERVER: "https://synthetic-approvals.invalid",
      AGENT_HUB_APPROVALS_TOKEN: "synthetic-parent-approvals-token",
    }
    const childEnv = { ...inheritedParentEnv }
    for (const key of forbiddenHTTPTransportKeys) delete childEnv[key]
    Object.assign(childEnv, {
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
    })
    const inspection = Bun.spawnSync([
      "node",
      "--eval",
      `import(${JSON.stringify(pathToFileURL(artifact).href)}).then(async (module) => {
        const hooks = await module.default({ directory: "/work", worktree: "/work", client: { _client: {} } });
        const forbiddenHTTPTransportKeys = ${JSON.stringify(forbiddenHTTPTransportKeys)};
        console.log(JSON.stringify({defaultType: typeof module.default, keys: Object.keys(module).sort(), tools: Object.keys(hooks.tool ?? {}), forbiddenHTTPTransportKeys: forbiddenHTTPTransportKeys.filter((key) => process.env[key] !== undefined)}));
        await hooks.dispose();
      })`,
    ], {
      env: childEnv,
    })
    const stdout = inspection.stdout.toString().slice(0, 4096)
    const stderr = inspection.stderr.toString().slice(0, 4096)
    expect(
      inspection.exitCode,
      `Node plugin inspection failed (stdout=${JSON.stringify(stdout)}, stderr=${JSON.stringify(stderr)})`,
    ).toBe(0)
    expect(JSON.parse(inspection.stdout.toString())).toEqual({
      defaultType: "function",
      keys: ["default"],
      tools: [],
      forbiddenHTTPTransportKeys: [],
    })
    expect(forbiddenHTTPTransportKeys.map((key) => inheritedParentEnv[key])).toEqual([
      "https://synthetic-parent.invalid",
      "synthetic-parent-token",
      "https://synthetic-approvals.invalid",
      "synthetic-parent-approvals-token",
    ])
    expect(Object.fromEntries(forbiddenHTTPTransportKeys.map((key) => [key, process.env[key]]))).toEqual(
      globalTransportBefore,
    )
  })
})
