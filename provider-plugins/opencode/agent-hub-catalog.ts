import { createHash } from "node:crypto"
import { spawn, type ChildProcess } from "node:child_process"
import { lstat } from "node:fs/promises"
import { isAbsolute, join, resolve } from "node:path"
import { tool } from "@opencode-ai/plugin"
import { z } from "zod"
import type { AgentHubCatalogV1 } from "./generated/gateway-types"

const CATALOG_MAX_BYTES = 256 * 1024
const INTERNAL_CATALOG_MAX_BYTES = 512 * 1024
const CACHE_WATCH_MAX_ENTRIES = 128 + 128 + 2
const CATALOG_STDERR_MAX_BYTES = 64 * 1024
const CATALOG_TIMEOUT_MS = 15_000
const CATALOG_KILL_GRACE_MS = 250
const DESCRIPTION_MAX_BYTES = 6 * 1024
const DETAIL_MAX_BYTES = 64 * 1024
const FINGERPRINT_DOMAIN = "agent-hub:catalog:v1\0"
const NETWORK_RESOURCES = [
  "llm",
  "github",
  "go_registry",
  "crates_registry",
  "pypi_registry",
  "npm_registry",
  "guarded_web",
] as const
const NETWORK_CLASSES = ["none", "llm-only", "build", "guarded-web"] as const
const TOOL_APPROVALS = ["auto", "dippy"] as const

const utf8Length = (value: string) => Buffer.byteLength(value, "utf8")
const boundedText = (maximum: number, minimum = 0) => z.string().refine(
  (value) => utf8Length(value) >= minimum && utf8Length(value) <= maximum,
  { message: `must contain ${minimum}..${maximum} UTF-8 bytes` },
)
const canonicalText = (maximum: number, minimum = 0) => boundedText(maximum, minimum).refine(
  (value) => value.trim() === value && !Array.from(value).some((character) => /\p{Cc}/u.test(character)),
  { message: "must be canonical text without surrounding whitespace or control characters" },
)
const uint = z.number().int().nonnegative().safe()
const discoveryEntry = z.string().refine((value) => Array.from(value).length >= 1 && Array.from(value).length <= 160)
const discoverySchema = z.object({
  recommended_for: z.array(discoveryEntry).min(1).max(6),
  selection_notes: z.array(discoveryEntry).min(1).max(6),
}).strict()
const networkOptionSchema = z.object({
  network_class: z.enum(NETWORK_CLASSES),
  resources: z.array(z.enum(NETWORK_RESOURCES)).max(7),
}).strict()
const environmentSchema = z.object({
  name: canonicalText(128, 1),
  description: canonicalText(4096),
  discovery: discoverySchema.nullish(),
  image_family: z.literal("job-code"),
  network_options: z.array(networkOptionSchema).min(1).max(128),
  tool_approvals: z.array(z.enum(TOOL_APPROVALS)).min(1).max(2),
  overlays: z.array(canonicalText(128, 1)).max(16),
  max_timeout_secs: z.number().int().min(1).max(86_400),
}).strict()
const imageSchema = z.object({
  family: z.literal("job-code"),
  description: canonicalText(4096),
  discovery: discoverySchema.nullish(),
  tool_facts: z.discriminatedUnion("status", [
    z.object({ status: z.literal("unavailable") }).strict(),
    z.object({
      status: z.literal("available"),
      tools: z.array(z.object({
        name: canonicalText(64, 1).regex(/^[A-Za-z0-9+._-]+$/),
        version: canonicalText(256, 1),
      }).strict()).min(1).max(128),
    }).strict(),
  ]),
}).strict()
const overlaySchema = z.object({
  name: canonicalText(128, 1),
  provider: canonicalText(128, 1),
  description: canonicalText(4096).nullish(),
  discovery: discoverySchema.nullish(),
  scopes: z.array(canonicalText(128, 1)).min(1).max(8),
  prerequisites: z.object({
    network_resources: z.array(z.enum(NETWORK_RESOURCES)).max(2),
    credential: z.enum(["host_command", "host_configuration", "configured_mcp_bridge"]).nullish(),
    repository: z.literal("matching_remote_host").nullish(),
    notes: z.array(canonicalText(256, 1)).max(4).optional(),
  }).strict(),
}).strict()
const guidanceSchema = z.object({
  runtime_readiness_checked: z.literal(false),
  limits: z.object({
    max_prompt_bytes: uint,
    max_workspaces: uint,
    max_context_roots: uint,
    max_overlays: uint,
  }).strict(),
  outputs: z.object({
    structured_result: z.boolean(),
    artifact_manifest: z.boolean(),
    repository_result: z.boolean(),
    cleanup_result: z.boolean(),
    stdout_bytes: uint,
    stderr_bytes: uint,
    transcript_bytes: uint,
    artifact_bytes: uint,
  }).strict(),
  workspace: z.object({
    workspace_required: z.boolean(),
    primary_workspace_required: z.boolean(),
    context_roots_read_only: z.boolean(),
    workspace_export_supported: z.boolean(),
    workspace_export_is_untrusted: z.boolean(),
  }).strict(),
}).strict()

export const catalogSchema = z.object({
  schema: z.literal("agent_hub_catalog_v1"),
  fingerprint: z.string().regex(/^[0-9a-f]{64}$/),
  environments: z.array(environmentSchema).max(64),
  images: z.array(imageSchema).max(64),
  overlays: z.array(overlaySchema).max(64),
  guidance: guidanceSchema,
}).strict()

const CANONICAL_GUIDANCE = {
  runtime_readiness_checked: false,
  limits: { max_prompt_bytes: 128 * 1024, max_workspaces: 16, max_context_roots: 16, max_overlays: 16 },
  outputs: {
    structured_result: true,
    artifact_manifest: true,
    repository_result: true,
    cleanup_result: true,
    stdout_bytes: 16 * 1024 * 1024,
    stderr_bytes: 16 * 1024 * 1024,
    transcript_bytes: 32 * 1024 * 1024,
    artifact_bytes: 4 * 1024 * 1024,
  },
  workspace: {
    workspace_required: true,
    primary_workspace_required: true,
    context_roots_read_only: true,
    workspace_export_supported: true,
    workspace_export_is_untrusted: true,
  },
} as const

function sortedUnique(values: string[], label: string) {
  if (values.some((value, index) => index > 0 && values[index - 1]! >= value)) {
    throw new Error(`catalog ${label} values are not sorted and unique`)
  }
}

function unique(values: string[], label: string) {
  if (new Set(values).size !== values.length) throw new Error(`catalog ${label} values are not unique`)
}

function networkOptionKey(option: z.infer<typeof networkOptionSchema>): string {
  return `${option.network_class}:${option.resources.join(",")}`
}

export function validateCatalog(value: unknown): AgentHubCatalogV1 {
  const catalog = catalogSchema.parse(value)
  sortedUnique(catalog.environments.map(({ name }) => name), "environment")
  sortedUnique(catalog.images.map(({ family }) => family), "image")
  sortedUnique(catalog.overlays.map(({ name }) => name), "overlay")
  const images = new Set(catalog.images.map(({ family }) => family))
  const overlays = new Set(catalog.overlays.map(({ name }) => name))
  const allowedResources: Record<(typeof NETWORK_CLASSES)[number], readonly string[]> = {
    none: [],
    "llm-only": ["llm"],
    build: ["llm", "github", "go_registry", "crates_registry", "pypi_registry", "npm_registry"],
    "guarded-web": ["guarded_web"],
  }
  for (const environment of catalog.environments) {
    if (!images.has(environment.image_family)) throw new Error(`environment '${environment.name}' references an unavailable image family`)
    sortedUnique(environment.network_options.map(networkOptionKey), "network option")
    sortedUnique(environment.tool_approvals, "tool approval")
    sortedUnique(environment.overlays, "environment overlay")
    for (const option of environment.network_options) {
      sortedUnique(option.resources, "network resource")
      if (option.resources.includes("guarded_web") && option.resources.length !== 1) {
        throw new Error("guarded_web must be the sole network resource")
      }
      if (option.resources.some((resource) => !allowedResources[option.network_class].includes(resource))) {
        throw new Error("catalog network option exceeds its class")
      }
    }
    const missing = environment.overlays.find((name) => !overlays.has(name))
    if (missing) throw new Error(`environment '${environment.name}' references unknown overlay '${missing}'`)
    for (const discovery of [environment.discovery]) validateDiscovery(discovery)
  }
  for (const image of catalog.images) validateDiscovery(image.discovery)
  for (const image of catalog.images) {
    if (image.tool_facts.status === "available") {
      sortedUnique(image.tool_facts.tools.map(({ name }) => name), "tool fact")
    }
  }
  for (const overlay of catalog.overlays) {
    sortedUnique(overlay.scopes, "overlay scope")
    sortedUnique(overlay.prerequisites.network_resources, "overlay prerequisite network resource")
    unique(overlay.prerequisites.notes ?? [], "overlay prerequisite note")
    validateDiscovery(overlay.discovery)
  }
  if (JSON.stringify(catalog.guidance) !== JSON.stringify(CANONICAL_GUIDANCE)) {
    throw new Error("catalog guidance is not canonical")
  }
  const projection = {
    schema: catalog.schema,
    fingerprint: "",
    environments: catalog.environments.map((environment) => ({
      name: environment.name,
      description: environment.description,
      ...(environment.discovery == null ? {} : { discovery: environment.discovery }),
      image_family: environment.image_family,
      network_options: environment.network_options,
      tool_approvals: environment.tool_approvals,
      overlays: environment.overlays,
      max_timeout_secs: environment.max_timeout_secs,
    })),
    images: catalog.images.map((image) => ({
      family: image.family,
      description: image.description,
      ...(image.discovery == null ? {} : { discovery: image.discovery }),
      tool_facts: image.tool_facts,
    })),
    overlays: catalog.overlays.map((overlay) => ({
      name: overlay.name,
      provider: overlay.provider,
      ...(overlay.description == null ? {} : { description: overlay.description }),
      ...(overlay.discovery == null ? {} : { discovery: overlay.discovery }),
      scopes: overlay.scopes,
      prerequisites: {
        network_resources: overlay.prerequisites.network_resources,
        ...(overlay.prerequisites.credential == null ? {} : { credential: overlay.prerequisites.credential }),
        ...(overlay.prerequisites.repository == null ? {} : { repository: overlay.prerequisites.repository }),
        ...(overlay.prerequisites.notes?.length ? { notes: overlay.prerequisites.notes } : {}),
      },
    })),
    guidance: catalog.guidance,
  }
  const expected = createHash("sha256").update(FINGERPRINT_DOMAIN).update(JSON.stringify(projection)).digest("hex")
  if (catalog.fingerprint !== expected) throw new Error("catalog fingerprint mismatch")
  if (utf8Length(JSON.stringify(catalog)) > CATALOG_MAX_BYTES) throw new Error("catalog payload exceeds limit")
  return catalog as AgentHubCatalogV1
}

function validateDiscovery(discovery: z.infer<typeof discoverySchema> | null | undefined) {
  if (!discovery) return
  for (const [label, values] of Object.entries(discovery)) {
    unique(values, label)
    for (const value of values) {
      if (value.trim() !== value || Array.from(value).some((character) => /\p{Cc}/u.test(character))) {
        throw new Error(`discovery ${label} entry is outside its canonical bounds`)
      }
    }
  }
}

const watchStampSchema = z.object({
  path: canonicalText(4096, 1).refine(isAbsolute, "must be an absolute path"),
  device: z.string().regex(/^[0-9]+$/),
  inode: z.string().regex(/^[0-9]+$/),
  mode: z.number().int().nonnegative().max(0xffffffff),
  type: z.enum(["file", "directory"]),
  size: z.string().regex(/^[0-9]+$/),
  modified_ns: z.string().regex(/^-?[0-9]+$/),
  changed_ns: z.string().regex(/^-?[0-9]+$/),
}).strict()

const cacheIdentitySchema = z.object({
  schema: z.literal("agent_hub_catalog_cache_identity_v2"),
  backend_metadata_revision: canonicalText(128, 1),
  watch: z.array(watchStampSchema).min(1).max(CACHE_WATCH_MAX_ENTRIES),
}).strict()

const internalEnvelopeSchema = z.object({
  schema: z.literal("agent_hub_catalog_internal_v1"),
  catalog: z.unknown(),
  cache_identity: cacheIdentitySchema,
}).strict()

type FileStamp = Readonly<{
  path: string
  device: string
  inode: string
  mode: number
  type: "file" | "directory" | "symbolic_link"
  size: string
  modified_ns: string
  changed_ns: string
}>

type Freshness = Readonly<{ stamps: readonly FileStamp[] }>
type CatalogProcessResult = Readonly<{ stdout: string; stderr: string }>

export interface CatalogRuntime {
  env: Record<string, string | undefined>
  run(binary: string, argv: string[], signal: AbortSignal, home: string): Promise<CatalogProcessResult>
}

function terminateAndReap(proc: ChildProcess, graceMs: number): () => void {
  let forced: NodeJS.Timeout | undefined
  try {
    if (proc.pid) process.kill(proc.pid, "SIGTERM")
    else proc.kill("SIGTERM")
  } catch {}
  forced = setTimeout(() => {
    try {
      if (proc.pid) process.kill(proc.pid, "SIGKILL")
      else proc.kill("SIGKILL")
    } catch {}
  }, graceMs)
  return () => { if (forced) clearTimeout(forced) }
}

export function runCatalogProcess(
  binary: string,
  argv: string[],
  signal: AbortSignal,
  timeoutMs = CATALOG_TIMEOUT_MS,
  home?: string,
): Promise<CatalogProcessResult> {
  return new Promise((resolvePromise, rejectPromise) => {
    if (signal.aborted) return rejectPromise(new Error("Agent Hub catalog load was cancelled"))
    let proc: ChildProcess
    try {
      proc = spawn(binary, argv, {
        stdio: ["ignore", "pipe", "pipe"],
        ...(home ? { env: { ...process.env, HOME: home } } : {}),
      })
    } catch (error) {
      rejectPromise(new Error(`Agent Hub catalog process could not start: ${String(error)}`))
      return
    }
    let stdoutBytes = 0
    let stderrBytes = 0
    const stdout: Buffer[] = []
    const stderr: Buffer[] = []
    let failure: Error | undefined
    let clearForcedKill = () => {}
    let settled = false
    const fail = (error: Error) => {
      if (failure || settled) return
      failure = error
      clearForcedKill = terminateAndReap(proc, CATALOG_KILL_GRACE_MS)
    }
    const timeout = setTimeout(() => fail(new Error(`Agent Hub catalog process exceeded ${timeoutMs} ms`)), timeoutMs)
    const onAbort = () => fail(new Error("Agent Hub catalog load was cancelled"))
    signal.addEventListener("abort", onAbort, { once: true })
    const capture = (chunks: Buffer[], chunk: Buffer | string, maximum: number, label: string) => {
      const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk)
      const current = label === "stdout" ? stdoutBytes : stderrBytes
      if (current + bytes.byteLength > maximum) {
        fail(new Error(`Agent Hub catalog ${label} exceeded ${maximum} bytes`))
        return
      }
      if (label === "stdout") stdoutBytes += bytes.byteLength
      else stderrBytes += bytes.byteLength
      chunks.push(bytes)
    }
    proc.stdout?.on("data", (chunk) => capture(stdout, chunk, INTERNAL_CATALOG_MAX_BYTES, "stdout"))
    proc.stderr?.on("data", (chunk) => capture(stderr, chunk, CATALOG_STDERR_MAX_BYTES, "stderr"))
    proc.stdout?.on("error", (error) => fail(new Error(`Agent Hub catalog stdout failed: ${error.message}`)))
    proc.stderr?.on("error", (error) => fail(new Error(`Agent Hub catalog stderr failed: ${error.message}`)))
    proc.on("error", (error) => fail(new Error(`Agent Hub catalog process failed: ${error.message}`)))
    proc.on("close", (code, processSignal) => {
      if (settled) return
      settled = true
      clearTimeout(timeout)
      clearForcedKill()
      signal.removeEventListener("abort", onAbort)
      if (failure) return rejectPromise(failure)
      const diagnostic = Buffer.concat(stderr).toString("utf8").trim()
      if (processSignal) return rejectPromise(new Error(`Agent Hub catalog process terminated by ${processSignal}`))
      if (code !== 0) return rejectPromise(new Error(`Agent Hub catalog process exited ${String(code)}${diagnostic ? `: ${diagnostic}` : ""}`))
      try {
        resolvePromise({
          stdout: new TextDecoder("utf-8", { fatal: true }).decode(Buffer.concat(stdout)),
          stderr: new TextDecoder("utf-8", { fatal: true }).decode(Buffer.concat(stderr)),
        })
      } catch (error) {
        rejectPromise(new Error("Agent Hub catalog output was not UTF-8", { cause: error }))
      }
    })
  })
}

export const defaultCatalogRuntime: CatalogRuntime = {
  env: process.env,
  run: (binary, argv, signal, home) => runCatalogProcess(binary, argv, signal, CATALOG_TIMEOUT_MS, home),
}

async function stamp(path: string): Promise<FileStamp> {
  const metadata = await lstat(path, { bigint: true })
  const type = metadata.isFile() ? "file" : metadata.isDirectory() ? "directory" : metadata.isSymbolicLink() ? "symbolic_link" : undefined
  if (!type) throw new Error("catalog freshness input changed type")
  return {
    path,
    device: metadata.dev.toString(),
    inode: metadata.ino.toString(),
    mode: Number(metadata.mode),
    type,
    size: metadata.size.toString(),
    modified_ns: metadata.mtimeNs.toString(),
    changed_ns: metadata.ctimeNs.toString(),
  }
}

async function captureFreshness(identity: Freshness): Promise<Freshness> {
  return { stamps: await Promise.all(identity.stamps.map(({ path }) => stamp(path))) }
}

function sameFreshness(left: Freshness, right: Freshness): boolean {
  return JSON.stringify(left) === JSON.stringify(right)
}

type CacheEntry = Readonly<{ freshness: Freshness; catalog: AgentHubCatalogV1 }>
type Inflight = {
  promise: Promise<AgentHubCatalogV1>
  controller: AbortController
  waiters: number
  settled: boolean
}

export class AgentHubCatalogService {
  readonly #runtime: CatalogRuntime
  readonly #binary: string
  readonly #hostConfig: string
  readonly #home: string
  readonly #key: string
  readonly #configurationError?: Error
  readonly #shutdown = new AbortController()
  readonly #cache = new Map<string, CacheEntry>()
  readonly #inflight = new Map<string, Inflight>()
  readonly #active = new Set<Promise<unknown>>()
  #disposal?: Promise<void>

  constructor(runtime: CatalogRuntime = defaultCatalogRuntime) {
    this.#runtime = runtime
    const binary = runtime.env.AGENT_HUB_BIN
    const home = runtime.env.HOME
    if (!binary || !isAbsolute(binary)) {
      this.#configurationError = new Error("AGENT_HUB_BIN must name the fixed absolute Agent Hub executable")
    } else if (!home || !isAbsolute(home)) {
      this.#configurationError = new Error("HOME must be absolute to derive the trusted Agent Hub host config")
    }
    this.#binary = binary && isAbsolute(binary) ? resolve(binary) : ""
    this.#home = home && isAbsolute(home) ? resolve(home) : ""
    this.#hostConfig = home && isAbsolute(home) ? join(resolve(home), ".config", "agent-hub", "host-run.json") : ""
    this.#key = `${this.#binary}\0${this.#hostConfig}`
  }

  get(signal?: AbortSignal): Promise<AgentHubCatalogV1> {
    if (this.#shutdown.signal.aborted) return Promise.reject(new Error("Agent Hub catalog service is disposed"))
    if (this.#configurationError) return Promise.reject(this.#configurationError)
    if (signal?.aborted) return Promise.reject(new Error("Agent Hub catalog load was cancelled"))
    let inflight = this.#inflight.get(this.#key)
    if (!inflight || inflight.controller.signal.aborted) {
      const controller = new AbortController()
      inflight = { controller, waiters: 0, settled: false, promise: undefined as unknown as Promise<AgentHubCatalogV1> }
      const owned = inflight
      const operation = this.#getFresh(AbortSignal.any([this.#shutdown.signal, controller.signal])).finally(() => {
        owned.settled = true
        if (this.#inflight.get(this.#key) === owned) this.#inflight.delete(this.#key)
        this.#active.delete(operation)
      })
      inflight.promise = operation
      this.#inflight.set(this.#key, inflight)
      this.#active.add(operation)
    }
    return this.#waitFor(inflight, signal)
  }

  #waitFor(inflight: Inflight, signal?: AbortSignal): Promise<AgentHubCatalogV1> {
    inflight.waiters += 1
    return new Promise((resolvePromise, rejectPromise) => {
      let finished = false
      const finish = (callback: () => void) => {
        if (finished) return
        finished = true
        signal?.removeEventListener("abort", onAbort)
        inflight.waiters -= 1
        if (!inflight.settled && inflight.waiters === 0) inflight.controller.abort()
        callback()
      }
      const onAbort = () => finish(() => rejectPromise(new Error("Agent Hub catalog load was cancelled")))
      signal?.addEventListener("abort", onAbort, { once: true })
      if (signal?.aborted) {
        onAbort()
        return
      }
      inflight.promise.then(
        (catalog) => finish(() => resolvePromise(catalog)),
        (error) => finish(() => rejectPromise(error)),
      )
    })
  }

  async #getFresh(signal: AbortSignal): Promise<AgentHubCatalogV1> {
    const cached = this.#cache.get(this.#key)
    if (cached) {
      try {
        if (sameFreshness(cached.freshness, await captureFreshness(cached.freshness))) return cached.catalog
      } catch {}
    }
    this.#cache.delete(this.#key)
    for (let attempt = 0; attempt < 2; attempt++) {
      try {
        const launchBefore = await stamp(this.#binary)
        const result = await this.#runtime.run(this.#binary, ["local", "catalog", "--include-cache-identity"], signal, this.#home)
        if (utf8Length(result.stdout) > INTERNAL_CATALOG_MAX_BYTES) throw new Error(`Agent Hub catalog stdout exceeded ${INTERNAL_CATALOG_MAX_BYTES} bytes`)
        let value: unknown
        try { value = JSON.parse(result.stdout) } catch (error) {
          throw new Error("Agent Hub catalog returned malformed JSON", { cause: error })
        }
        const envelope = internalEnvelopeSchema.parse(value)
        const paths = envelope.cache_identity.watch.map(({ path }) => path)
        sortedUnique(paths, "cache watch path")
        const catalog = validateCatalog(envelope.catalog)
        const reported: Freshness = {
          stamps: envelope.cache_identity.watch.some(({ path }) => path === this.#binary)
            ? envelope.cache_identity.watch
            : [...envelope.cache_identity.watch, launchBefore],
        }
        const after = await captureFreshness(reported)
        if (!sameFreshness(reported, after)) {
          if (attempt === 0) continue
          throw new Error("Agent Hub catalog inputs changed during consecutive loads")
        }
        this.#cache.set(this.#key, { freshness: reported, catalog })
        return catalog
      } catch (error) {
        this.#cache.delete(this.#key)
        throw error
      }
    }
    throw new Error("Agent Hub catalog inputs changed during load")
  }

  dispose(): Promise<void> {
    this.#disposal ??= (async () => {
      this.#shutdown.abort()
      await Promise.allSettled([...this.#active])
      this.#cache.clear()
    })()
    return this.#disposal
  }
}

const operationSchema = z.enum(["overview", "detail"])
const sectionSchema = z.enum(["environments", "images", "overlays", "guidance"])
const selectorSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("environment"), name: canonicalText(128, 1) }).strict(),
  z.object({ kind: z.literal("image"), family: z.literal("job-code") }).strict(),
  z.object({ kind: z.literal("overlay"), name: canonicalText(128, 1) }).strict(),
])

export const agentHubCatalogArgs = {
  operation: operationSchema.default("overview"),
  sections: z.array(sectionSchema).max(4).default([]).refine((values) => new Set(values).size === values.length, "sections must be unique"),
  selectors: z.array(selectorSchema).max(16).default([]).refine(
    (values) => new Set(values.map((value) => JSON.stringify(value))).size === values.length,
    "selectors must be unique",
  ),
}

export const catalogRequestSchema = z.object(agentHubCatalogArgs).strict().superRefine((request, context) => {
  if (request.operation === "overview") {
    if (request.sections.length || request.selectors.length) context.addIssue({ code: "custom", message: "overview does not accept sections or selectors" })
    return
  }
  if (!request.sections.length) context.addIssue({ code: "custom", message: "detail requires at least one section" })
  const selected = new Set(request.selectors.map(({ kind }) => `${kind}s`))
  for (const section of request.sections) {
    if (section !== "guidance" && !selected.has(section)) {
      context.addIssue({ code: "custom", message: `section '${section}' requires at least one matching selector` })
    }
  }
  for (const section of selected) {
    if (!request.sections.includes(section as z.infer<typeof sectionSchema>)) {
      context.addIssue({ code: "custom", message: `selector for unrequested section '${section}'` })
    }
  }
})

export type CatalogRequest = z.infer<typeof catalogRequestSchema>

export function projectCatalog(catalog: AgentHubCatalogV1, input: CatalogRequest): string {
  const environments = new Map(catalog.environments.map((value) => [value.name, value]))
  const images = new Map(catalog.images.map((value) => [value.family, value]))
  const overlays = new Map(catalog.overlays.map((value) => [value.name, value]))
  const output: Record<string, unknown> = { schema: "agent_hub_catalog_detail_v1", fingerprint: catalog.fingerprint }
  if (input.operation === "overview") {
    output.overview = {
      counts: { environments: catalog.environments.length, images: catalog.images.length, overlays: catalog.overlays.length },
      environments: catalog.environments.map(({ name, description, discovery }) => ({
        name,
        summary: clipped(description, 120),
        ...(discovery ? { recommended_for: discovery.recommended_for.slice(0, 2).map((value) => clipped(value, 80)) } : {}),
      })),
      images: catalog.images.map(({ family, description }) => ({ family, summary: clipped(description, 120) })),
      overlays: catalog.overlays.map(({ name, provider, description }) => ({
        name, provider, ...(description == null ? {} : { summary: clipped(description, 120) }),
      })),
    }
  }
  for (const section of input.sections) {
    if (section === "guidance") output.guidance = catalog.guidance
    if (section === "environments") output.environments = input.selectors.filter((item) => item.kind === "environment").map((selector) => {
      const value = environments.get(selector.name)
      if (!value) throw new Error(`Unknown Agent Hub environment '${selector.name}'`)
      return value
    })
    if (section === "images") output.images = input.selectors.filter((item) => item.kind === "image").map((selector) => {
      const value = images.get(selector.family)
      if (!value) throw new Error(`Unknown Agent Hub image '${selector.family}'`)
      return value
    })
    if (section === "overlays") output.overlays = input.selectors.filter((item) => item.kind === "overlay").map((selector) => {
      const value = overlays.get(selector.name)
      if (!value) throw new Error(`Unknown Agent Hub overlay '${selector.name}'`)
      return value
    })
  }
  const result = JSON.stringify(output)
  if (utf8Length(result) > DETAIL_MAX_BYTES) throw new Error(`Agent Hub catalog detail exceeded ${DETAIL_MAX_BYTES} bytes; request fewer selectors`)
  return result
}

function quoted(value: string): string {
  return JSON.stringify(value)
    .replace(/</g, "\\u003c")
    .replace(/>/g, "\\u003e")
    .replace(/&/g, "\\u0026")
    .replace(/`/g, "\\u0060")
}

function clipped(value: string, characters: number): string {
  const points = Array.from(value)
  return points.length <= characters ? value : `${points.slice(0, characters - 1).join("")}…`
}

function environmentRow(catalog: AgentHubCatalogV1, environment: AgentHubCatalogV1["environments"][number]): string {
  const image = catalog.images.find(({ family }) => family === environment.image_family)!
  const tuples = environment.network_options.map(({ network_class, resources }) => `${network_class}+[${resources.join(",")}]`)
  const shownTuples = tuples.slice(0, 3).join(" | ")
  const tupleSummary = tuples.length <= 3 ? shownTuples : `${tuples.length} exact tuples; examples: ${shownTuples}`
  const recommendation = environment.discovery?.recommended_for[0]
  const note = environment.discovery?.selection_notes[0]
  const tools = image.tool_facts.status === "available" ? `${image.tool_facts.tools.length} published` : "not published"
  return `- ${quoted(environment.name)} — ${quoted(clipped(environment.description, 150))}${recommendation ? `; recommended: ${quoted(clipped(recommendation, 120))}` : ""}${note ? `; note: ${quoted(clipped(note, 120))}` : ""}; image=${environment.image_family} (${tools}); approvals=${environment.tool_approvals.join("/")}; network=${tupleSummary}; overlays=${environment.overlays.length}; timeout≤${environment.max_timeout_secs}s.`
}

export function renderRunDescription(catalog: AgentHubCatalogV1): string {
  const pointer = "Use agent_hub_catalog operation=overview to list identifiers, then operation=detail with explicit sections/selectors. Network classes and resources are not freely composable and are not authorization or readiness grants: select one exact network_options tuple from the chosen environment detail (empty resources means no connectivity; guarded_web is exclusive). Credential overlays are separate, imply neither connectivity nor launch approval, and may still be unavailable at launch."
  const rows = catalog.environments.map((environment) => environmentRow(catalog, environment))
  const prose = [
    "Run one capability-scoped sandbox through Agent Hub's trusted host policy and lifecycle implementation.",
    "Available environments:",
    ...rows,
    pointer,
  ].join("\n")
  if (utf8Length(prose) <= DESCRIPTION_MAX_BYTES) return prose
  const prefix = `Run one capability-scoped sandbox through Agent Hub's trusted host policy and lifecycle implementation. Selectable environments (${catalog.environments.length}): `
  let names = ""
  let shown = 0
  for (const environment of catalog.environments) {
    const candidate = `${names ? ", " : ""}${quoted(environment.name)}`
    if (utf8Length(prefix + names + candidate + `; ${pointer}`) > DESCRIPTION_MAX_BYTES) break
    names += candidate
    shown += 1
  }
  const remaining = catalog.environments.length - shown
  const fallback = `${prefix}${names || "none"}${remaining ? `; ${remaining} more available via catalog overview` : ""}. ${pointer}`
  if (utf8Length(fallback) > DESCRIPTION_MAX_BYTES) throw new Error("Agent Hub catalog compact description exceeded its bound")
  return fallback
}

export function unavailableRunDescription(_error: unknown): string {
  return "Agent Hub catalog unavailable; agent_hub_run capability choices cannot be advertised safely. Verify the installed Agent Hub catalog configuration and retry. No cached or fallback capability menu is used."
}

export function createAgentHubCatalogTool(service: AgentHubCatalogService) {
  return tool({
    description: "Read bounded typed Agent Hub capability metadata. Start with operation=overview to discover identifiers, then use operation=detail with explicit sections/selectors. This does not launch a sandbox.",
    args: agentHubCatalogArgs,
    async execute(input, context) {
      const request = catalogRequestSchema.parse(input)
      let catalog: AgentHubCatalogV1
      try {
        catalog = await service.get(context.abort)
      } catch {
        throw new Error("Agent Hub catalog metadata is unavailable because the installed catalog failed freshness or validation checks; repair it and retry")
      }
      return projectCatalog(catalog, request)
    },
  })
}
