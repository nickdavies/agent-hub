import { expect, test } from "bun:test"
import { mkdtempSync, rmSync } from "fs"
import { spawnSync } from "child_process"
import os from "os"
import path from "path"
import * as entry from "./index"
import manifest from "./package.json"

test("package exposes exactly one default plugin function", () => {
  expect(Object.keys(entry)).toEqual(["default"])
  expect(typeof entry.default).toBe("function")
  expect(manifest.main).toBe("index.ts")
  expect(manifest["oc-plugin"]).toEqual(["default"])
  expect(manifest.peerDependencies["@opencode-ai/plugin"]).toBe("1.18.26")
  expect(manifest.peerDependencies["@opencode-ai/sdk"]).toBe("1.18.26")
})

test("bundled Node ESM entry loads without OpenCode or a foreground server", async () => {
  const directory = mkdtempSync(path.join(os.tmpdir(), "agent-hub-entry-test-"))
  try {
    const result = await Bun.build({ entrypoints: [path.join(import.meta.dir, "index.ts")], outdir: directory, target: "node", format: "esm", naming: "plugin.mjs" })
    expect(result.success).toBe(true)
    const loaded = spawnSync("node", ["--input-type=module", "-e", `
      import assert from 'node:assert/strict';
      const before = process.listenerCount('exit');
      const entry = await import(${JSON.stringify(path.join(directory, "plugin.mjs"))});
      assert.deepEqual(Object.keys(entry), ['default']);
      assert.equal(typeof entry.default, 'function');
      assert.equal(process.listenerCount('exit'), before);
    `], { encoding: "utf8", timeout: 5000 })
    expect(loaded.stderr).toBe("")
    expect(loaded.status).toBe(0)
  } finally { rmSync(directory, { recursive: true, force: true }) }
})
