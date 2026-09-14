import { expect, test } from "bun:test"
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"
import { pathToFileURL } from "node:url"

async function waitForFile(file: string): Promise<string> {
  const deadline = Date.now() + 5000
  while (true) {
    try {
      const content = await readFile(file, "utf8")
      if (content) return content
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error
    }
    if (Date.now() >= deadline) throw new Error(`Timed out waiting for fixture marker ${file}`)
    await Bun.sleep(10)
  }
}

for (const shutdown of ["dispose", "SIGINT", "SIGTERM"] as const) {
  test(`${shutdown} cancels active host launches and waits for every instance's cleanup`, async () => {
    const root = await mkdtemp(join(tmpdir(), "agent-hub-shutdown-"))
    const binary = join(root, "host")
    const gateway = join(root, "gateway")
    await mkdir(join(root, "opencode", "log"), { recursive: true })
    await writeFile(gateway, `#!${process.execPath}\nawait new Response(Bun.stdin).text(); console.log('{"allowed":true}');`)
    await chmod(gateway, 0o700)
    await writeFile(binary, `#!${process.execPath}
      import { existsSync, writeFileSync } from "node:fs";
      const prefix = ${JSON.stringify(root)} + "/" + process.env.AGENT_HUB_TRUSTED_TOOL_CALL_ID;
      process.once("SIGTERM", async () => {
        writeFileSync(prefix + ".cancelled", "cancelled");
        while (!existsSync(prefix + ".release")) await Bun.sleep(10);
        writeFileSync(prefix + ".cleanup", "cleanup complete");
        console.log(JSON.stringify({
          schema: "agent_hub_run_output_v1", invocation_id: "00000000-0000-4000-8000-000000000001",
          approval_receipt: { schema: "agent_hub_run_approval_receipt_v1", status: "unavailable", error: "cancelled" },
          local_result_v2: { schema: "local_result_v2", status: "cancelled", cleanup_outcome: "succeeded" },
          error: "cancelled"
        }));
        process.exit(1);
      });
      writeFileSync(prefix + ".ready", String(process.pid));
      await Bun.sleep(60000);
    `)
    await chmod(binary, 0o700)
    const pluginURL = pathToFileURL(resolve("agent-hub.ts")).href
    const script = `
      import { mock } from "bun:test";
      import { existsSync, writeFileSync } from "node:fs";
      mock.module("@opencode-ai/sdk/v2", () => ({ OpencodeClient: class {
        session = { update: async () => ({ data: { permission: [{ permission: "*", pattern: "*", action: "allow" }] } }) };
      } }));
      const { createPluginInstance } = await import(${JSON.stringify(pluginURL)});
      const instances = [];
      for (let index = 0; index < 2; index++) {
        const sessionID = "session-" + index;
        const callID = "call-" + index;
        const hooks = await createPluginInstance({
          directory: ${JSON.stringify(root)}, worktree: ${JSON.stringify(root)},
          client: { _client: {}, session: { get: async () => ({ data: {} }) } }
        });
        instances.push(hooks);
        await hooks.config({});
        await hooks["chat.message"]({ sessionID }, {});
        const args = {
          schema: "agent_hub_run_intent_v1", environment: "code-auto", image_family: "job-code",
          network_class: "none", network_resources: [], tool_approval: "auto", overlays: [], timeout_secs: 60,
          prompt: "test", workspaces: [{ alias: "source", path: "/work/source", access: "read_write" }],
          context_roots: [], primary_workspace: "source"
        };
        await hooks["tool.execute.before"]({ sessionID, callID, tool: "agent_hub_run" }, { args });
        void hooks.tool.agent_hub_run.execute(args, {
          sessionID, callID, abort: new AbortController().signal,
          metadata: () => {}, ask: async () => { throw new Error("native prompt"); }
        }).then(
          () => writeFileSync(${JSON.stringify(root)} + "/" + callID + ".settled", "settled"),
          error => writeFileSync(${JSON.stringify(root)} + "/" + callID + ".settled", String(error))
        );
      }
      if (${JSON.stringify(shutdown)} === "dispose") {
        while (!existsSync(${JSON.stringify(join(root, "dispose"))})) await Bun.sleep(10);
        await Promise.all(instances.map(hooks => hooks.dispose()));
        process.exit(0);
      }
      await Bun.sleep(60000);
    `
    const parent = Bun.spawn([process.execPath, "--eval", script], {
      cwd: process.cwd(),
      env: { ...process.env, AGENT_HUB_BIN: binary, AGENT_HUB_GATEWAY: gateway, XDG_DATA_HOME: root },
      stdout: "pipe", stderr: "pipe",
    })
    const stderr = new Response(parent.stderr).text()
    let exited = false
    void parent.exited.then(() => { exited = true })
    try {
      for (let index = 0; index < 2; index++) await waitForFile(join(root, `call-${index}.ready`))
      if (shutdown === "dispose") await writeFile(join(root, "dispose"), "dispose")
      else parent.kill(shutdown)
      for (let index = 0; index < 2; index++) {
        await waitForFile(join(root, `call-${index}.cancelled`))
        expect(await Bun.file(join(root, `call-${index}.cleanup`)).exists()).toBe(false)
      }
      expect(exited).toBe(false)
      await writeFile(join(root, "call-0.release"), "release")
      expect(await waitForFile(join(root, "call-0.settled"))).toBe("settled")
      expect(exited).toBe(false)
      await writeFile(join(root, "call-1.release"), "release")
      expect(await parent.exited).toBe(shutdown === "dispose" ? 0 : shutdown === "SIGINT" ? 130 : 143)
      for (let index = 0; index < 2; index++) {
        expect(await readFile(join(root, `call-${index}.cleanup`), "utf8")).toBe("cleanup complete")
        expect(await readFile(join(root, `call-${index}.settled`), "utf8")).toBe("settled")
      }
      expect(await stderr).toBe("")
    } finally {
      // Release and cancel only fixture-owned children, including when the parent exits prematurely.
      for (let index = 0; index < 2; index++) {
        await writeFile(join(root, `call-${index}.release`), "release")
        if (await Bun.file(join(root, `call-${index}.settled`)).exists()) {
          const outcome = await readFile(join(root, `call-${index}.settled`), "utf8")
          if (outcome !== "settled") console.error(outcome)
        }
        if (await Bun.file(join(root, `call-${index}.ready`)).exists()) {
          const pid = Number(await readFile(join(root, `call-${index}.ready`), "utf8"))
          if (!await Bun.file(join(root, `call-${index}.cancelled`)).exists()) {
            try { process.kill(pid, "SIGTERM") } catch (error) {
              if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error
            }
          }
          await waitForFile(join(root, `call-${index}.cleanup`))
        }
      }
      if (!exited) parent.kill("SIGTERM")
      await parent.exited
      if (await stderr) console.error(await stderr)
      await rm(root, { recursive: true, force: true })
    }
  }, 15000)
}
