/**
 * generate-types.ts — Convert JSON Schema files into TypeScript type definitions.
 *
 * Usage:
 *   bun run generate-types.ts
 *
 * Reads all *.schema.json files from ./generated/ and produces a single
 * ./generated/gateway-types.ts with all the TypeScript interfaces.
 *
 * This script is part of the Rust → JSON Schema → TypeScript pipeline:
 *   1. `cargo run -p protocol --bin generate_schemas` (Rust → .schema.json)
 *   2. `bun run generate-types.ts` (JSON Schema → TypeScript)
 */

import { compileFromFile } from "json-schema-to-typescript"
import { readdir, writeFile } from "fs/promises"
import path from "path"

const SCHEMA_DIR = path.join(import.meta.dir, "generated")
const OUTPUT_FILE = path.join(SCHEMA_DIR, "gateway-types.ts")

// Known OpenCodeTool values — the Rust enum has a custom JsonSchema impl that
// just emits "type": "string" because of the Unknown(String) catch-all variant.
// We define the known values here as a string literal union for better TS DX.
const KNOWN_OPENCODE_TOOLS = [
  "bash",
  "edit",
  "glob",
  "grep",
  "multiedit",
  "read",
  "task",
  "todowrite",
  "webfetch",
  "write",
] as const

/**
 * Deduplicate type/interface declarations from multiple schema compilations.
 *
 * Each schema file includes its own `$defs` which get compiled to top-level
 * type aliases (e.g. SessionId, Provider). When multiple schemas share the same
 * $def, we get duplicate declarations. This function keeps only the first
 * occurrence of each `export type X =` or `export interface X {` declaration.
 */
function deduplicateDeclarations(tsBlocks: string[]): string {
  const seen = new Set<string>()
  const result: string[] = []

  for (const block of tsBlocks) {
    const lines = block.split("\n")
    const filteredLines: string[] = []
    let skipping = false
    let skipDepth = 0

    for (let i = 0; i < lines.length; i++) {
      const line = lines[i]

      // Match `export type Foo = ...` or `export interface Foo {`
      const typeMatch = line.match(/^export (?:type|interface) (\w+)[\s={]/)
      if (typeMatch) {
        const name = typeMatch[1]
        if (seen.has(name)) {
          // Skip this entire declaration — could be single-line or multi-line
          if (line.includes("{") && !line.includes("}")) {
            // Multi-line interface — skip until closing brace
            skipping = true
            skipDepth = 1
          }
          // Single-line type alias — just skip this line
          continue
        }
        seen.add(name)
      }

      if (skipping) {
        for (const ch of line) {
          if (ch === "{") skipDepth++
          if (ch === "}") skipDepth--
        }
        if (skipDepth <= 0) {
          skipping = false
          skipDepth = 0
        }
        continue
      }

      // Also skip JSDoc comments that precede a duplicate declaration
      if (line.startsWith("/**") && i + 1 < lines.length) {
        // Look ahead to find the end of the JSDoc comment and check what follows
        let j = i
        while (j < lines.length && !lines[j].includes("*/")) j++
        if (j + 1 < lines.length) {
          const nextDecl = lines[j + 1].match(/^export (?:type|interface) (\w+)[\s={]/)
          if (nextDecl && seen.has(nextDecl[1])) {
            // Skip the JSDoc comment too — advance i past it
            i = j // the for loop will increment to j+1, which is the duplicate decl line
            continue
          }
        }
      }

      filteredLines.push(line)
    }

    result.push(filteredLines.join("\n").trim())
  }

  return result.filter(Boolean).join("\n\n")
}

async function main() {
  const files = (await readdir(SCHEMA_DIR))
    .filter((f) => f.endsWith(".schema.json"))
    .sort()

  const header = [
    "// AUTO-GENERATED — do not edit by hand.",
    "// Source: protocol crate JSON schemas → json-schema-to-typescript",
    "//",
    "// Regenerate with:",
    "//   cargo run -p protocol --bin generate_schemas -- provider-plugins/opencode/generated",
    "//   bun run generate-types.ts",
    "",
  ].join("\n")

  const tsBlocks: string[] = []
  for (const file of files) {
    const filePath = path.join(SCHEMA_DIR, file)
    const ts = await compileFromFile(filePath, {
      bannerComment: "",
      additionalProperties: false,
      style: {
        semi: false,
      },
    })
    tsBlocks.push(ts.trim())
  }

  const deduplicated = deduplicateDeclarations(tsBlocks)

  // Add the known tool values as a string literal union type + array constant.
  // OpenCodeTool from the schema is just `string`; this provides autocomplete.
  const knownTools = [
    "",
    "/**",
    " * Known OpenCodeTool values. The gateway accepts any string (for unknown/MCP",
    " * tools), but these are the recognized tool names.",
    " */",
    `export const OPENCODE_TOOL_NAMES = [${KNOWN_OPENCODE_TOOLS.map((t) => `"${t}"`).join(", ")}] as const`,
    "",
    "export type KnownOpenCodeTool = (typeof OPENCODE_TOOL_NAMES)[number]",
    "",
  ].join("\n")

  await writeFile(OUTPUT_FILE, header + "\n" + deduplicated + "\n" + knownTools)
  console.log(`Wrote ${OUTPUT_FILE}`)
}

main().catch((err) => {
  console.error(err)
  process.exit(1)
})
