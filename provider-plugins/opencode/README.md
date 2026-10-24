# opencode-plugin-agent-hub

Generic OpenCode tool execution gating through the normal HTTP Agent Hub gateway.
The plugin and SDK peer/dev dependencies are pinned to **1.18.26**. This integration
does not install or change the OpenCode CLI runtime.

## Setup

Build `agent-hub-gateway` from the repository root with
`cargo build -p agent-hub-gateway`, and make it available on `PATH` or set
`AGENT_HUB_GATEWAY` to its absolute path. Add this entry to `opencode.json`:

```json
{
  "plugin": ["file:///absolute/path/to/provider-plugins/opencode/index.ts"]
}
```

The package entrypoint exports one default plugin function, not a legacy
`{ id, server }` object. Import helper functions from `agent-hub.ts` only in tests;
do not register that implementation module as an OpenCode plugin entrypoint.

Environment inherited when the plugin initializes:

| Variable | Meaning |
| --- | --- |
| `AGENT_HUB_GATEWAY` | Gateway executable; defaults to `agent-hub-gateway` on `PATH` |
| `AGENT_HUB_SERVER` | HTTP server URL; defaults to `http://localhost:8080` |
| `AGENT_HUB_TOKEN` | Optional bearer token; `--token` is omitted when unset or empty |
| `AGENT_HUB_CONFIG` | Optional explicit approval config passed with `--config` |

The plugin always passes a server URL as required by the host CLI. Local
allow/deny rules need no server connection; approvals requiring operator review,
question proxying, and status reports require the HTTP server. The gateway uses
its normal default config path when `AGENT_HUB_CONFIG` is absent. Transport and
policy are not inferred from other environment variables.

## Execution and permission ownership

1. The config hook sets global and configured-agent native permissions to deny.
2. On `chat.message`, the plugin asks the host's in-process v2 session API to append
   a wildcard allow rule. It checks the returned rule structure and final wildcard
   rule before considering that session owned. Failed updates revoke ownership.
3. `tool.execute.before` rejects unowned sessions and sends the **actual raw tool
   arguments** to `gateway approval --opencode`. Separate `tool_match_input` holds
   canonical policy fields. Every tool, including unknown/MCP tools, is gated.
4. A leaked `permission.ask` always denies, independent of logging success.
5. Per-call records reject reused call IDs, unauthorized/duplicate completions,
   and before/after argument mismatches. These are local consistency checks, not
   an attestation that another plugin cannot change arguments after this hook.

The v2 SDK wraps `input.client._client`: requests use the host's in-process
transport, not a new HTTP client pointed at a foreground server. The published
plugin types still expose v1 client/config types; narrow boundary casts bridge
those types to the installed v2 host contract.

### Canonical policy input

| Runtime tool | Gateway tool | Canonical fields |
| --- | --- | --- |
| `read`, `write`, `edit` | `Read`, `Write`, `Write` | Absolute `path`; raw edit strings preserved |
| `apply_patch` | `Write` | One `path` decision per recognized unique source **and** destination |
| `multiedit` | `MultiEdit` | One absolute `path`, complete `edits` batch |
| `glob` | `Glob` | `target_directory`; selectors remain in raw arguments |
| `grep` | `Grep` | `path`, `pattern` |
| `list` | `List` | `path` |
| `external_directory` | `ExternalDirectory` | `path` |
| `bash`, `webfetch` | `Bash`, `WebFetch` | `command`, `url`, respectively |
| `websearch`, `codesearch`, `code_search` | `WebSearch`, `CodeSearch` | `query` |
| `skill`, `lsp` | `Skill`, `Lsp` | `name`, `metadata` |
| `question`, `doom_loop` | `Question`, `DoomLoop` | `patterns`, `metadata` |
| `task`, `todowrite` | `Task`, `TodoWrite` | Complete arguments |
| Unknown/MCP | Exact runtime name | Complete arguments, unchanged |

This historical extraction checkpoint preserves file/edit/patch path resolution
against the worktree; search roots and LSP paths resolve against the instance
directory. Path handling is lexical, not symlink resolution. Malformed native
arguments fail closed before gateway approval.

**Known historical security limits — do not deploy this checkpoint.** The source
normalizer recognizes patch headers only with one literal space after the colon
and does not trim the path. Other native-accepted header whitespace can skip or
misidentify targets. Relative file/edit/patch paths also use the worktree instead
of the native execution directory when these differ. These two original-source
bugs are deliberately retained for the byte-identical legacy extraction
checkpoint; two standalone security follow-ups must land before main is pushed
or deployed. Patch authorization currently covers only the recognized paths and
stops at the first denial, not necessarily every path the native parser accepts.

## Failure handling and lifecycle

Gateway stdout and stderr are separately bounded to 1 MiB. Approval/question
subprocesses have a 24-hour-plus-10-second deadline (covering the gateway's
operator wait); status reports have a 15-second deadline. A failed spawn, pipe,
deadline, signal, nonzero exit, invalid JSON, or malformed response never approves
a tool. Error messages are bounded to 4096 UTF-8 bytes. HTTP response limits and
redirect rejection are enforced by the generic gateway's HTTP client.

Question events retain answer/reject/timeout semantics. Local answers cancel the
pending child and suppress duplicate replies. SDK errors trigger an explicit
pipeline-error reply, then rejection if that reply fails. Terminal SDK failures
surface to the event caller. Status updates are debounced; failed registration
retries on later activity. Non-default session titles are cached per instance.

Each plugin instance owns its state, subprocesses, timers, and process listeners.
Abort kills that session's children and revokes ownership. Disposal removes only
that instance's listeners and cancels its work. Process exit attempts bounded
synchronous `ended` status reports for registered sessions.

## Development

```sh
# Required once before the test suite, from the repository root:
cargo build -p agent-hub-gateway
# Then from provider-plugins/opencode:
bun install --ignore-scripts
bun run generate
bun run typecheck
bun test
```

`generate` runs the existing protocol JSON-schema generator and then converts the
selected generic gateway/status/question schemas into `generated/gateway-types.ts`.
Do not add unrelated service protocols to this generated boundary. Tests use an
in-process SDK transport and temporary gateway fixtures. Offline provider contract
tests execute the actual `target/debug/agent-hub-gateway` approval command with a
temporary explicit local allow/deny policy and required dummy server URL
`http://127.0.0.1:9`, without a token. Status/question fixture commands stay inert;
these contracts do not make HTTP requests or run backend tools. A missing gateway
binary fails the suite with the build prerequisite instead of skipping tests.
Tests never launch OpenCode, contact paid services, or read auth stores or operator policy. The Node
ESM regression bundles the public SDK entrypoints without source aliases.
