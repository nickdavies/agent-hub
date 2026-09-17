# opencode-plugin-agent-hub

opencode plugin that connects `tool.execute.before` to `agent-hub-gateway` and
provides the host-only `agent_hub_run` attended-sandbox launch tool.

Every capability-bearing OpenCode 1.18.18 execution is awaited in
`tool.execute.before` and evaluated by the gateway before the tool can run.
Fast allow/deny decisions return immediately with no network call. Anything
that needs human review escalates to the agent-hub server.

## Prerequisites

- `agent-hub-gateway` built and on your `PATH` (see `agent-hub/` in this
  workspace)
- An `agent-hub-server` instance running (only needed for `ask`-escalated
  decisions; local allow/deny work without it)
- opencode 1.18.18, whose config and `tool.execute.before` plugin APIs are the
  runtime permission integration used by this plugin

The host plugin installation is built from `@opencode-ai/plugin` and
`@opencode-ai/sdk` 1.18.26. That SDK identity is independent of the supervised
OpenCode 1.18.18 sandbox runtime and must not be substituted for it.

## Installation

### 1. Build the gateway binary

```sh
cargo build --release -p agent-hub-gateway \
  --manifest-path agent-hub/Cargo.toml
# binary lands at agent-hub/target/release/agent-hub-gateway
# copy or symlink it onto PATH, e.g.:
ln -sf "$PWD/agent-hub/target/release/agent-hub-gateway" ~/.local/bin/
```

### 2. Configure environment variables

Add to the shell profile that launches opencode (e.g. `~/.zshrc`):

```sh
export AGENT_HUB_SERVER="https://hub.example.com"   # your server URL
# Optional for HTTP or HTTPS — omit when the server does not require auth
# export AGENT_HUB_TOKEN="your-bearer-token"
# Optional — defaults to ~/.config/agent-hub/agent-hub-permissions.json
# export AGENT_HUB_CONFIG="/path/to/agent-hub-permissions.json"
# Optional — defaults to "agent-hub-gateway" on PATH
# export AGENT_HUB_GATEWAY="/absolute/path/to/agent-hub-gateway"
# Required to execute agent_hub_run in host mode
# export AGENT_HUB_BIN="/absolute/path/to/agent-hub"
```

For a sandboxed attended run, the trusted launcher sets the complete transport:

```sh
export AGENT_HUB_APPROVAL_MODE="attended_socket"
export AGENT_HUB_ATTENDED_APPROVAL_SOCKET="/agent-hub/approval/approval.sock"
export AGENT_HUB_ATTENDED_APPROVAL_BINDING='{"run_epoch":"...","authorization_digest":"...","environment_bundle_digest":"...","image_digest":"sha256:...","network_digest":"...","approval_bundle_digest":"..."}'
export AGENT_HUB_ATTENDED_RUN_DEADLINE="2099-01-02T03:04:05Z"
export AGENT_HUB_PLUGIN_RUN_NONCE="0123456789abcdef0123456789abcdef"
export AGENT_HUB_CONFIG="/agent-hub/agent-hub-permissions.json"
```

Sandbox mode is explicit and fail closed. All five transport values are required, and
`AGENT_HUB_SERVER`, `AGENT_HUB_TOKEN`, `AGENT_HUB_APPROVALS_SERVER`, and
`AGENT_HUB_APPROVALS_TOKEN` are forbidden. Without `AGENT_HUB_APPROVAL_MODE`,
the plugin retains the existing host HTTP behavior and rejects stray attended
transport values.

The plugin captures this absolute path once at initialization, removes
all approval transport values from the subprocess environment, and passes the
captured binding as CLI arguments. Missing gateway binaries, malformed OpenCode
permissions, unavailable sockets, invalid decisions, and relay loss deny the
tool request. HTTP status and question subprocesses are disabled in attended
socket mode; no host-network route or proxy exception is required.
The host launch tool is not registered in attended socket mode.

### Host relay protocol v1

The socket is a one-request-per-connection Unix stream using the shared
`AttendedGatewayRequestV1` and `AttendedGatewayDecisionV1` protocol types. Each
frame is a four-byte unsigned big-endian length followed by canonical compact
UTF-8 JSON, with a maximum length of 1 MiB. Object keys are sorted recursively;
arrays retain order. The relay must close after its single decision frame.
Extra bytes or another frame are rejected as a replay. The request binds the
run digests, single-use request ID, exact normalized tool
input digest, and deadline. Every binding is echoed by the decision.

The launcher must create and own the socket parent, mount that parent read-only,
and preserve UID compatibility with the container runtime. The client requires
a canonical non-writable parent owned by its effective UID and a socket with
exact type, owner, and mode `0600`; it rechecks device/inode after connecting.
It also requires the relay's `SO_PEERCRED` UID to equal the runtime UID.
`SO_PEERCRED` authenticates kernel credentials at connection time, but does not
attest the relay executable or host identity, and PID values may be translated
by namespaces. The immutable host-created mount and run binding remain the
authority boundary.

In host mode, the server and optional token arguments are supplied to the
gateway even when decisions resolve locally. HTTP and HTTPS endpoints may use
localhost, wildcard/listener addresses, LAN names or IPs, or remote names.
Socket mode supplies neither.

### 3. Create a rule config

The gateway looks for `~/.config/agent-hub/agent-hub-permissions.json` by default. A minimal
config that matches the plan:

```json
{
  "version": 1,
  "default": "ask",
  "rules": [
    { "tools": ["@file_read"], "action": "allow" },
    { "tools": ["@shell"], "action": "delegate", "command": "dippy --claude" }
  ]
}
```

Use `in_cwds` to scope any rule to one or more working-directory roots. The
event working directory must equal or be a descendant of a configured root;
similarly prefixed sibling directories do not match. Matching uses lexical path
semantics: paths need not exist, and symlinks are not resolved through the
filesystem.

See `agent-hub/gateway/config/tools.json` for a checked-in example and
`agent-hub/gateway/config/smoke-test.json` for a fully worked example.

### 4. Register the plugin with opencode

Add the plugin to your project or global opencode config. The config file is
`opencode.json` in your project root, or
`~/.config/opencode/opencode.json` for global use.

**Project config** (`opencode.json`):
```json
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": [
    "file:///absolute/path/to/provider-plugins/opencode/index.ts"
  ]
}
```

**Global config** (`~/.config/opencode/opencode.json`):
```json
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": [
    "file:///home/you/code/agent-hub/provider-plugins/opencode/index.ts"
  ]
}
```

The path must be absolute and point to `provider-plugins/opencode/index.ts`
in this repository. This entrypoint exports exactly one default `Plugin`
function returning the gateway hooks and, in host mode, the launch tool.
Bundle from `index.ts`, not the helper modules; bundled single-file artifacts
must retain that sole default function export. Install only this plugin, not
an additional config-repository copy of the launch tool. Quit and restart
opencode after changing the plugin installation.

### Host launch authorization

`agent_hub_run` accepts structured `agent_hub_run_intent_v1` input. The ordinary
gateway execution hook authorizes that exact input before the host executable
can run. For example, a rule in the gateway permission config can require review:

```json
{ "tools": ["agent_hub_run"], "action": "ask" }
```

The tool does not call `context.ask`, does not use a synthetic
`agent_hub_run(local)` permission, and never falls back to a native prompt.
The trusted host independently resolves the intent and requires a bound
`agent_hub_run_resolved` authorization; the ordinary tool rule cannot replace
that decision. Host policy, credentials, executable, and authority are not
model-controlled tool arguments. The tool runs the configured absolute
`AGENT_HUB_BIN` with exactly `local run --request-stdin`, canonical JSON stdin,
and trusted OpenCode session and tool-call identities.

Cancellation and output overflow signal the owned host process with SIGTERM
and await its exit, allowing the host's cancellation/cleanup lifecycle to run.
Plugin disposal also cancels and awaits its active host launches. SIGINT and
SIGTERM await all registered plugin instances before exiting the parent process;
shutdown rejects further launches. A host that does not cooperate keeps shutdown
pending: there is no timeout that abandons the cleanup owner or reports success.
Overflow output is drained without retaining bytes beyond the bound. A tool
error is not evidence of successful sandbox cleanup; there is no forced-kill
fallback or additional host cleanup command. Success requires a consistent
terminal, approved, complete, successful, cleaned result.

### Capability catalog

Catalog discovery is an opt-in host feature enabled by setting absolute
`AGENT_HUB_BIN` while using the HTTP approval transport. The plugin registers
`agent_hub_catalog` only in HTTP mode and invokes the fixed executable as
exactly `local catalog`; it never supplies a model-controlled or plugin-selected
host-config argument. The CLI derives the trusted
`$HOME/.config/agent-hub/host-run.json` configuration itself.

The `tool.definition` hook loads the public `AgentHubCatalogV1` projection and
replaces the `agent_hub_run` description with a bounded environment menu. This
read-only menu lookup is not a tool execution and does not route through the
gateway. It includes public purpose, image/tool facts, exact approval and
network choices, timeout limits, and a pointer to typed details. Network access
and overlay credential prerequisites are described separately. If discovery
fails, the description explicitly says the catalog is unavailable rather than
retaining generic launch guidance.

Agents request typed detail through `agent_hub_catalog` using the required,
bounded `sections` array and matching explicit `selectors`. Unknown fields,
sections, selectors, names, malformed contracts, invalid references/order, and
fingerprint mismatches fail closed. Unlike menu lookup, this explicit tool call
uses the normal permission-ownership and gateway route, and one matching allow
is consumed even when the catalog data is already cached.

Successful catalogs are cached only while internal `lstat` snapshots of the
configured binary, trusted host config, authoritative config root, pinned files,
and relevant config directories remain identical. Concurrent loads are
singleflight. Missing, malformed, oversized, or mutated inputs invalidate the
cache before reload; failures never fall back to stale data. Inputs are captured
before and after the bounded subprocess, with one retry for a race. Timeout,
cancellation, or disposal sends SIGTERM, escalates to SIGKILL after a short
grace period, and waits for process reaping. Filesystem watch inventory is
private and is never included in tool output or descriptions.

Approval-bundle producers must record
`sources.agent_hub.plugin_entrypoint = "provider-plugins/opencode/index.ts"`.
Regenerate bundles and their source/artifact digest pins together when adopting
this package; manifests naming the helper module are rejected.

### 5. Verify

Run opencode and trigger a tool that requires permission (e.g. a bash
command). You should see `agent-hub-gateway` debug output on stderr and
opencode should allow or deny the action according to your
`~/.config/agent-hub/agent-hub-permissions.json` rules
without showing its own approval prompt.

## How it works

```
opencode tool.execute.before hook
  └─ plugin calls agent-hub-gateway --opencode
        └─ gateway loads ~/.config/agent-hub/agent-hub-permissions.json
              Allow  → plugin permits tool execution
              Deny   → plugin rejects tool execution
              Delegate(dippy) → Dippy allow/deny → same as above
                                Dippy ask        → gateway escalates to server
              Ask    → gateway long-polls server → allow/deny → same as above
```

The hook contract uses the execution tool name, `callID`, and the tool's actual
camelCase argument object. The complete raw argument object is retained for
review, delegation, transcript reconciliation, and attended request digests.
A separate canonical match input resolves paths and maps the fields consumed by
the gateway rule engine. `ApplyPatch` produces one canonical authorization for
every source and move destination while retaining the complete `patchText` in
each review request.

## Execution-hook contract

| OpenCode execution | gateway canonical name | canonical match field |
|---|---|---|
| `bash` | `Bash` | `command` |
| `read` | `Read` | resolved `path` from `filePath` |
| `write`, `edit`, `apply_patch` | write category | every resolved file `path` |
| `glob`, `list`, `grep` | matching file category | resolved search root |
| `webfetch`, `websearch`, code search | matching network/search category | URL or query |
| `task`, `skill`, `lsp`, `question`, todo, doom loop | matching built-in category | built-in canonical fields |
| MCP, plugin, custom, and future names | typed `Unknown(name)` | no known-tool match |

Unknown executions retain their exact typed name and raw arguments. Valid
path-bearing fields are resolved for evidence, but are not reclassified as a
known tool. The immutable default ask/deny policy therefore decides them; the
plugin never pre-denies an unknown name before policy evaluation. If policy
allows an unknown call whose path-bearing field is malformed, conservative
post-policy validation still blocks the execution before side effects.

`StructuredOutput` is the sole explicit exclusion. OpenCode injects it to copy
model-validated data into the transcript; it has no filesystem, network,
process, or subagent capability. The plugin records its call ID and raw digest
as `transcript_only`, but does not send it to policy. This is an OpenCode
1.18.18 capability interception guarantee, not a claim that every possible
tool in every runtime is universally intercepted.

Before/after records bind `sessionID`, `callID`, typed tool name, raw argument
digest, canonical match digests, and resolved paths. Finalization rejects
missing, duplicate, pending, denied-as-success, or mismatched records. Thus a
`no_denials` outcome is valid only when every routed execution has matching
post-execution evidence and every exclusion is an exact StructuredOutput call.

## Caveats

- **Permission ownership**: immutable supervised config starts with the exact
  `{ "*": "deny" }` fail-closed fallback at the top level and for every
  configured agent. The plugin retains those native fallbacks and transfers one
   session at a time to `"allow"` only after `session.update` returns valid rules
   ending in the requested wildcard allow rule (updates append rules). The awaited execution hook routes tools only for
  sessions with that proof. Any leaked native prompt is explicitly denied.
- **Multiple files**: the plugin calls the gateway once per canonical file. The
  first denied file stops evaluation immediately.
