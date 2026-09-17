import { createHash, randomUUID } from "node:crypto"
import { chmod, lstat, mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { describe, expect, test } from "bun:test"
import {
  AgentHubCatalogService,
  catalogRequestSchema,
  projectCatalog,
  renderRunDescription,
  runCatalogProcess,
  unavailableRunDescription,
  validateCatalog,
  type CatalogRuntime,
} from "./agent-hub-catalog"

const guidance = {
  limits: { max_prompt_bytes: 131072, max_workspaces: 16, max_context_roots: 16, max_overlays: 16 },
  outputs: {
    structured_result: true,
    artifact_manifest: true,
    repository_result: true,
    cleanup_result: true,
    stdout_bytes: 16777216,
    stderr_bytes: 16777216,
    transcript_bytes: 33554432,
    artifact_bytes: 4194304,
  },
  workspace: {
    workspace_required: true,
    primary_workspace_required: true,
    context_roots_read_only: true,
    workspace_export_supported: true,
    workspace_export_is_untrusted: true,
  },
}

function catalog(description = "Focused implementation work") {
  const value = {
    schema: "agent_hub_catalog_v1",
    fingerprint: "",
    environments: [{
      name: "code-auto",
      description,
      discovery: { recommended_for: ["Code changes"], selection_notes: ["Use least privilege"] },
      image_family: "job-code",
      network_options: [
        { network_class: "build", resources: ["github", "llm", "npm_registry"] },
        { network_class: "none", resources: [] },
      ],
      tool_approvals: ["auto", "dippy"],
      overlays: ["github:read"],
      max_timeout_secs: 3600,
    }],
    images: [{ family: "job-code", description: "Pinned job image", tool_facts: { status: "unavailable" } }],
    overlays: [{
      name: "github:read",
      provider: "github",
      description: "Repository credential",
      scopes: ["repositories:read"],
      prerequisites: {
        network_resources: ["github"],
        credential: "host_configuration",
        repository: "matching_remote_host",
        notes: ["Requires a matching remote host"],
      },
    }],
    guidance,
  }
  value.fingerprint = createHash("sha256")
    .update("agent-hub:catalog:v1\0")
    .update(JSON.stringify(value))
    .digest("hex")
  return value
}

function realisticCatalog() {
  const value: any = catalog()
  const names = ["code-auto", "code-supervised", "infra-supervised", "research-auto", "research-web-auto"]
  value.environments = names.map((name, index) => ({
    ...value.environments[0],
    name,
    description: `Purpose-specific environment ${index + 1}`,
    discovery: { recommended_for: [`Recommended workflow ${index + 1}`], selection_notes: [`Selection note ${index + 1}`] },
  })).sort((left: any, right: any) => left.name.localeCompare(right.name))
  value.fingerprint = ""
  value.fingerprint = createHash("sha256").update("agent-hub:catalog:v1\0").update(JSON.stringify(value)).digest("hex")
  return value
}

type Fixture = Awaited<ReturnType<typeof fixture>>

async function fixture() {
  const root = await mkdtemp(join(tmpdir(), "agent-hub-catalog-"))
  const home = join(root, "home")
  const hostDirectory = join(home, ".config", "agent-hub")
  const configRoot = join(root, "config")
  const environmentDirectory = join(configRoot, "environments", "code-auto")
  const binary = join(root, "agent-hub")
  const gateway = join(root, "gateway")
  const policy = join(root, "policy.json")
  const system = join(configRoot, "system.toml")
  const environment = join(environmentDirectory, "environment.toml")
  const hostConfig = join(hostDirectory, "host-run.json")
  await mkdir(hostDirectory, { recursive: true })
  await mkdir(environmentDirectory, { recursive: true })
  for (const path of [binary, gateway]) {
    await writeFile(path, "fixture executable")
    await chmod(path, 0o700)
  }
  await writeFile(policy, "{}")
  await writeFile(system, "[system]\n")
  await writeFile(environment, "[environment]\n")
  const pin = (path: string) => ({ path, sha256: "a".repeat(64), source_identity: "fixture", device: 1, inode: 1 })
  await writeFile(hostConfig, JSON.stringify({
    schema: "agent_hub_run_host_v1",
    config_root: configRoot,
    config_root_identity: { canonical_path: configRoot, device: 1, inode: 1 },
    config_artifacts: [pin(system), pin(environment)],
    self_binary: pin(binary),
    gateway: pin(gateway),
    policy: pin(policy),
    image_digests: {},
  }))
  return { root, home, hostConfig, configRoot, environmentDirectory, binary, gateway, policy, system, environment }
}

function runtime(files: Fixture, run: CatalogRuntime["run"]): CatalogRuntime {
  return { env: { AGENT_HUB_BIN: files.binary, HOME: files.home }, run }
}

async function envelope(files: Fixture, value = catalog(), watchedBinary = files.binary, additionalPaths: string[] = []) {
  const paths = [watchedBinary, files.configRoot, files.environmentDirectory, files.environment, files.hostConfig, files.system, ...additionalPaths].sort()
  const watch = await Promise.all(paths.map(async (path) => {
    const metadata = await lstat(path, { bigint: true })
    return {
      path,
      device: metadata.dev.toString(),
      inode: metadata.ino.toString(),
      mode: Number(metadata.mode),
      type: metadata.isDirectory() ? "directory" : "file",
      size: metadata.size.toString(),
      modified_ns: metadata.mtimeNs.toString(),
      changed_ns: metadata.ctimeNs.toString(),
    }
  }))
  return JSON.stringify({ schema: "agent_hub_catalog_internal_v1", catalog: value, cache_identity: { schema: "agent_hub_catalog_cache_identity_v1", watch } })
}

describe("catalog validation and projection", () => {
  test("accepts canonical data and rejects unknown fields, ordering, references, resources, guidance, and fingerprints", () => {
    expect(validateCatalog(catalog()).schema).toBe("agent_hub_catalog_v1")
    const withTools: any = catalog()
    withTools.images[0].tool_facts = { status: "available", tools: [{ name: "git", version: "2.47.0" }, { name: "node", version: "22.18.0" }] }
    withTools.fingerprint = ""
    withTools.fingerprint = createHash("sha256").update("agent-hub:catalog:v1\0").update(JSON.stringify(withTools)).digest("hex")
    expect(validateCatalog(withTools).images[0]?.tool_facts.status).toBe("available")
    withTools.images[0].tool_facts.tools[0].command = "/private/git"
    expect(() => validateCatalog(withTools)).toThrow()
    delete withTools.images[0].tool_facts.tools[0].command
    withTools.images[0].tool_facts.tools[0].name = "private/tool"
    expect(() => validateCatalog(withTools)).toThrow()
    for (const mutate of [
      (value: any) => { value.private_path = "/secret" },
      (value: any) => { value.environments[0].unknown = true },
      (value: any) => { value.environments[0].network_options.reverse() },
      (value: any) => { value.environments[0].overlays = ["missing"] },
      (value: any) => { value.environments[0].network_options[0].resources.push("guarded_web") },
      (value: any) => { value.guidance.limits.max_workspaces = 15 },
      (value: any) => { value.fingerprint = "0".repeat(64) },
    ]) {
      const invalid: any = structuredClone(catalog())
      mutate(invalid)
      expect(() => validateCatalog(invalid)).toThrow()
    }
  })

  test("requires bounded explicit sections/selectors and returns only selected typed data", () => {
    expect(() => catalogRequestSchema.parse({ operation: "detail", sections: ["environments"], selectors: [] })).toThrow()
    expect(() => catalogRequestSchema.parse({ sections: ["guidance"], selectors: [], extra: true })).toThrow()
    const overview = JSON.parse(projectCatalog(validateCatalog(catalog()), catalogRequestSchema.parse({})))
    expect(overview.overview.environments).toEqual([{ name: "code-auto", summary: "Focused implementation work", recommended_for: ["Code changes"] }])
    expect(overview.overview.overlays[0].name).toBe("github:read")
    const request = catalogRequestSchema.parse({
      operation: "detail",
      sections: ["environments", "overlays", "guidance"],
      selectors: [{ kind: "environment", name: "code-auto" }, { kind: "overlay", name: "github:read" }],
    })
    const detail = JSON.parse(projectCatalog(validateCatalog(catalog()), request))
    expect(detail).toEqual({
      schema: "agent_hub_catalog_detail_v1",
      fingerprint: catalog().fingerprint,
      environments: [catalog().environments[0]],
      overlays: [catalog().overlays[0]],
      guidance,
    })
    expect(() => projectCatalog(validateCatalog(catalog()), catalogRequestSchema.parse({
      operation: "detail", sections: ["overlays"], selectors: [{ kind: "overlay", name: "unknown" }],
    }))).toThrow("Unknown Agent Hub overlay")
  })

  test("renders bounded escaped menu prose with exact capability facts and a truthful oversized fallback", () => {
    const menu = renderRunDescription(validateCatalog(catalog("Use <tag> & `command` safely")))
    expect(menu).toContain("\\u003ctag\\u003e")
    expect(menu).toContain("(not published)")
    expect(menu).toContain("approvals=auto/dippy")
    expect(menu).toContain("network=build+[github,llm,npm_registry] | none+[]")
    expect(menu).toContain("not freely composable")
    expect(menu).not.toContain("llm-only+[npm_registry]")
    expect(menu).toContain("Credential overlays are separate")
    expect(Buffer.byteLength(menu)).toBeLessThanOrEqual(6 * 1024)

    const many = catalog()
    many.environments = Array.from({ length: 64 }, (_, index) => ({
      ...many.environments[0]!, name: `code-${index.toString().padStart(2, "0")}`, description: "x".repeat(200), overlays: [],
    }))
    many.overlays = []
    many.fingerprint = createHash("sha256").update("agent-hub:catalog:v1\0").update(JSON.stringify({ ...many, fingerprint: "" })).digest("hex")
    const fallback = renderRunDescription(validateCatalog(many))
    expect(fallback).toContain("Selectable environments (64)")
    expect(fallback).toContain("code-00")
    expect(fallback).toContain("operation=overview")
    expect(Buffer.byteLength(unavailableRunDescription("x".repeat(20_000)))).toBeLessThanOrEqual(6 * 1024)
    expect(unavailableRunDescription("/private/token secret")).not.toContain("/private/token")
  })

  test("keeps a realistic five-environment menu useful and compact", () => {
    const names = ["code-auto", "code-supervised", "infra-supervised", "research-auto", "research-web-auto"]
    const menu = renderRunDescription(validateCatalog(realisticCatalog()))
    for (const name of names) expect(menu).toContain(name)
    expect(menu).toContain("recommended")
    expect(menu).toContain("exact network_options tuple")
    expect(Buffer.byteLength(menu)).toBeLessThan(4096)
  })

  test("keeps a maximum-cardinality valid overview bounded and usable", () => {
    const many: any = catalog()
    many.environments = Array.from({ length: 64 }, (_, index) => ({
      ...many.environments[0],
      name: `environment-${index.toString().padStart(2, "0")}`,
      description: `${index}:` + "d".repeat(900),
      discovery: { recommended_for: ["r".repeat(160)], selection_notes: ["n".repeat(160)] },
      overlays: [],
    }))
    many.overlays = Array.from({ length: 64 }, (_, index) => ({
      ...catalog().overlays[0],
      name: `overlay-${index.toString().padStart(2, "0")}`,
      description: "o".repeat(900),
    }))
    many.fingerprint = ""
    many.fingerprint = createHash("sha256").update("agent-hub:catalog:v1\0").update(JSON.stringify(many)).digest("hex")
    const valid = validateCatalog(many)
    const overview = projectCatalog(valid, catalogRequestSchema.parse({}))
    expect(Buffer.byteLength(overview)).toBeLessThanOrEqual(64 * 1024)
    expect(overview).toContain("environment-63")
    expect(overview).toContain("overlay-63")
    const selectors = Array.from({ length: 16 }, (_, index) => ({
      kind: "environment" as const, name: `environment-${index.toString().padStart(2, "0")}`,
    }))
    const detail = projectCatalog(valid, catalogRequestSchema.parse({
      operation: "detail", sections: ["environments"], selectors,
    }))
    expect(Buffer.byteLength(detail)).toBeLessThanOrEqual(64 * 1024)
    expect(detail).toContain("environment-15")
    expect(() => catalogRequestSchema.parse({
      operation: "detail", sections: ["environments"], selectors: [...selectors, { kind: "environment", name: "environment-16" }],
    })).toThrow()
  })
})

describe("catalog freshness cache", () => {
  test("accepts the 258-entry maximum backend watch inventory and rejects an excess entry", async () => {
    const files = await fixture()
    const additionalPaths = Array.from({ length: 253 }, (_, index) => join(files.root, `watch-${index.toString().padStart(3, "0")}`))
    await Promise.all(additionalPaths.map((path) => writeFile(path, "watch fixture")))
    let includeExcess = false
    const service = new AgentHubCatalogService(runtime(files, async () => ({
      stdout: await envelope(files, catalog(), files.binary, additionalPaths.slice(0, includeExcess ? 253 : 252)),
      stderr: "",
    })))
    try {
      expect((await service.get()).schema).toBe("agent_hub_catalog_v1")
      await writeFile(files.system, "[system]\n# invalidate\n")
      includeExcess = true
      await expect(service.get()).rejects.toThrow()
    } finally {
      await service.dispose()
      await rm(files.root, { recursive: true, force: true })
    }
  })

  test("singleflights concurrent loads and caches only a successful unchanged snapshot with fixed argv", async () => {
    const files = await fixture()
    let calls = 0
    const invocations: Array<{ binary: string; argv: string[] }> = []
    let release!: () => void
    const blocked = new Promise<void>((resolve) => { release = resolve })
    const service = new AgentHubCatalogService(runtime(files, async (binary, argv) => {
      calls += 1
      invocations.push({ binary, argv })
      await blocked
      return { stdout: await envelope(files), stderr: "" }
    }))
    try {
      const first = service.get()
      const second = service.get()
      release()
      expect((await Promise.all([first, second]))[0]).toBe((await service.get()))
      expect(calls).toBe(1)
      expect(invocations).toEqual([{ binary: files.binary, argv: ["local", "catalog", "--include-cache-identity"] }])
    } finally {
      await service.dispose()
      await rm(files.root, { recursive: true, force: true })
    }
  })

  test.each(["file", "directory", "binary"] as const)("invalidates on %s mutation", async (kind) => {
    const files = await fixture()
    let calls = 0
    const service = new AgentHubCatalogService(runtime(files, async () => {
      calls += 1
      return { stdout: await envelope(files, catalog(`catalog ${calls}`)), stderr: "" }
    }))
    try {
      await service.get()
      if (kind === "file") await writeFile(files.environment, `[environment]\n# ${randomUUID()}\n`)
      if (kind === "directory") await writeFile(join(files.environmentDirectory, `added-${randomUUID()}.toml`), "added")
      if (kind === "binary") await writeFile(files.binary, `changed-${randomUUID()}`)
      expect((await service.get()).environments[0]?.description).toBe("catalog 2")
      expect(calls).toBe(2)
    } finally {
      await service.dispose()
      await rm(files.root, { recursive: true, force: true })
    }
  })

  test.each(["missing", "malformed", "oversize"] as const)("does not serve stale data when the backend rejects a %s host config", async (kind) => {
    const files = await fixture()
    let calls = 0
    const service = new AgentHubCatalogService(runtime(files, async () => {
      calls += 1
      if (calls > 1) throw new Error("backend rejected host config")
      return { stdout: await envelope(files), stderr: "" }
    }))
    try {
      await service.get()
      if (kind === "missing") await rm(files.hostConfig)
      if (kind === "malformed") await writeFile(files.hostConfig, "{")
      if (kind === "oversize") await writeFile(files.hostConfig, "x".repeat(256 * 1024 + 1))
      await expect(service.get()).rejects.toThrow()
      expect(calls).toBe(2)
    } finally {
      await service.dispose()
      await rm(files.root, { recursive: true, force: true })
    }
  })

  test("invalidates successful data before a malformed reload and retries after an input race", async () => {
    const files = await fixture()
    let calls = 0
    let malformed = false
    let race = false
    const service = new AgentHubCatalogService(runtime(files, async () => {
      calls += 1
      if (malformed) return { stdout: "not json", stderr: "" }
      const stdout = await envelope(files, catalog(`catalog ${calls}`))
      if (race && calls === 3) await writeFile(files.environment, "[environment]\n# raced\n")
      return { stdout, stderr: "" }
    }))
    try {
      await service.get()
      await writeFile(files.system, "[system]\n# invalidate\n")
      malformed = true
      await expect(service.get()).rejects.toThrow("malformed JSON")
      malformed = false
      race = true
      expect((await service.get()).environments[0]?.description).toBe("catalog 4")
      expect(calls).toBe(4)
    } finally {
      await service.dispose()
      await rm(files.root, { recursive: true, force: true })
    }
  })

  test("keeps private watch inventory out of catalog projections", async () => {
    const files = await fixture()
    const service = new AgentHubCatalogService(runtime(files, async () => ({ stdout: await envelope(files), stderr: "" })))
    try {
      const publicValue = JSON.stringify(await service.get()) + renderRunDescription(await service.get())
      for (const privateValue of [files.root, files.hostConfig, files.binary, files.configRoot]) expect(publicValue).not.toContain(privateValue)
    } finally {
      await service.dispose()
      await rm(files.root, { recursive: true, force: true })
    }
  })

  test("invalidates before serving cache when the configured binary symlink is retargeted", async () => {
    const files = await fixture()
    const second = join(files.root, "agent-hub-second")
    const alias = join(files.root, "agent-hub-link")
    await writeFile(second, "second fixture executable")
    await chmod(second, 0o700)
    await symlink(files.binary, alias)
    let calls = 0
    let watched = files.binary
    const service = new AgentHubCatalogService({
      env: { AGENT_HUB_BIN: alias, HOME: files.home },
      run: async () => ({ stdout: await envelope(files, catalog(`catalog ${++calls}`), watched), stderr: "" }),
    })
    try {
      expect((await service.get()).environments[0]?.description).toBe("catalog 1")
      await rm(alias)
      await symlink(second, alias)
      watched = second
      expect((await service.get()).environments[0]?.description).toBe("catalog 2")
      expect(calls).toBe(2)
    } finally {
      await service.dispose()
      await rm(files.root, { recursive: true, force: true })
    }
  })

  test("freezes trusted HOME for the cache key and catalog child", async () => {
    const files = await fixture()
    const mutableEnv = { AGENT_HUB_BIN: files.binary, HOME: files.home }
    const homes: string[] = []
    const service = new AgentHubCatalogService({
      env: mutableEnv,
      run: async (_binary, _argv, _signal, home) => {
        homes.push(home)
        await Bun.sleep(10)
        return { stdout: await envelope(files), stderr: "" }
      },
    })
    try {
      const loading = service.get()
      mutableEnv.HOME = join(files.root, "other-home")
      await loading
      await service.get()
      expect(homes).toEqual([files.home])
    } finally {
      await service.dispose()
      await rm(files.root, { recursive: true, force: true })
    }
  })

  test("catalog waiters own cancellation without aborting other waiters", async () => {
    const files = await fixture()
    let childSignal: AbortSignal | undefined
    let release!: () => void
    const blocked = new Promise<void>((resolve) => { release = resolve })
    const service = new AgentHubCatalogService(runtime(files, async (_binary, _argv, signal) => {
      childSignal = signal
      await blocked
      if (signal.aborted) throw new Error("child cancelled")
      return { stdout: await envelope(files), stderr: "" }
    }))
    try {
      const firstController = new AbortController()
      const secondController = new AbortController()
      const first = service.get(firstController.signal)
      const second = service.get(secondController.signal)
      while (!childSignal) await Bun.sleep(1)
      firstController.abort()
      await expect(first).rejects.toThrow("cancelled")
      expect(childSignal?.aborted).toBe(false)
      release()
      expect((await second).environments[0]?.name).toBe("code-auto")
      const already = new AbortController()
      already.abort()
      await expect(service.get(already.signal)).rejects.toThrow("cancelled")
    } finally {
      release()
      await service.dispose()
      await rm(files.root, { recursive: true, force: true })
    }
  })

  test("cancels the child when the last active waiter leaves and disposal awaits rejection", async () => {
    const files = await fixture()
    let childAborted = false
    const service = new AgentHubCatalogService(runtime(files, async (_binary, _argv, signal) => {
      await new Promise<void>((_resolve, reject) => {
        const abort = () => {
          childAborted = true
          reject(new Error("fixture child reaped"))
        }
        if (signal.aborted) abort()
        else signal.addEventListener("abort", abort, { once: true })
      })
      throw new Error("unreachable")
    }))
    const one = new AbortController()
    const two = new AbortController()
    try {
      const first = service.get(one.signal)
      const second = service.get(two.signal)
      one.abort()
      expect(childAborted).toBe(false)
      two.abort()
      await Promise.allSettled([first, second])
      await service.dispose()
      expect(childAborted).toBe(true)
    } finally {
      await service.dispose()
      await rm(files.root, { recursive: true, force: true })
    }
  })

  test("rejects malformed and oversized internal payloads without retaining data", async () => {
    const files = await fixture()
    let stdout = "{}"
    const service = new AgentHubCatalogService(runtime(files, async () => ({ stdout, stderr: "" })))
    try {
      await expect(service.get()).rejects.toThrow()
      stdout = "x".repeat(512 * 1024 + 1)
      await expect(service.get()).rejects.toThrow("exceeded")
    } finally {
      await service.dispose()
      await rm(files.root, { recursive: true, force: true })
    }
  })

  test("measures realistic cold load and warm cached descriptions", async () => {
    const files = await fixture()
    let processCount = 0
    const service = new AgentHubCatalogService(runtime(files, async () => {
      processCount += 1
      return { stdout: await envelope(files, realisticCatalog()), stderr: "" }
    }))
    try {
      const coldStart = performance.now()
      const loaded = await service.get()
      const menu = renderRunDescription(loaded)
      const coldMs = performance.now() - coldStart
      const warmStart = performance.now()
      for (let index = 0; index < 100; index++) renderRunDescription(await service.get())
      const warmAverageMs = (performance.now() - warmStart) / 100
      const measurement = {
        fixture: "five-environment plugin fixture with mocked host subprocess",
        cold_ms: Number(coldMs.toFixed(3)),
        warm_cached_description_average_ms: Number(warmAverageMs.toFixed(3)),
        iterations: 100,
        process_count: processCount,
        public_payload_bytes: Buffer.byteLength(JSON.stringify(loaded)),
        prose_bytes: Buffer.byteLength(menu),
        token_estimate_labelled_chars_div_4: Math.ceil(Array.from(menu).length / 4),
      }
      console.log("agent-hub-catalog-measurement", JSON.stringify(measurement))
      expect(processCount).toBe(1)
      expect(measurement.prose_bytes).toBeLessThan(4096)
    } finally {
      await service.dispose()
      await rm(files.root, { recursive: true, force: true })
    }
  })
})

describe("catalog process lifecycle", () => {
  for (const mode of ["timeout", "cancel"] as const) {
    test(`${mode} sends TERM, escalates to KILL, and reaps before rejecting`, async () => {
      const root = await mkdtemp(join(tmpdir(), "agent-hub-catalog-process-"))
      const marker = join(root, "pid")
      const controller = new AbortController()
      try {
        const running = runCatalogProcess("/usr/local/bin/node", ["-e", `
          import { writeFileSync } from "node:fs";
          writeFileSync(${JSON.stringify(marker)}, String(process.pid));
          process.on("SIGTERM", () => {});
          setTimeout(() => {}, 30000);
        `], controller.signal, mode === "timeout" ? 100 : 30_000)
        const outcome = running.then(() => undefined, (error: unknown) => error)
        if (mode === "cancel") {
          await Bun.sleep(100)
          controller.abort()
        }
        const error = await outcome
        expect(String(error)).toContain(mode === "timeout" ? "exceeded" : "cancelled")
        const pid = Number(await readFile(marker, "utf8"))
        expect(() => process.kill(pid, 0)).toThrow()
      } finally {
        await rm(root, { recursive: true, force: true })
      }
    })
  }
})
