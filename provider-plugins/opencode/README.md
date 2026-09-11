# opencode-plugin-agent-hub

opencode plugin that connects `tool.execute.before` to `agent-hub-gateway`.

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
export AGENT_HUB_TOKEN="your-bearer-token"          # server auth token
# Optional — defaults to ~/.config/agent-hub/agent-hub-permissions.json
# export AGENT_HUB_CONFIG="/path/to/agent-hub-permissions.json"
# Optional — defaults to "agent-hub-gateway" on PATH
# export AGENT_HUB_GATEWAY="/absolute/path/to/agent-hub-gateway"
```

For a sandboxed attended run, the trusted launcher sets the complete transport:

```sh
export AGENT_HUB_APPROVAL_MODE="attended_socket"
export AGENT_HUB_ATTENDED_APPROVAL_SOCKET="/agent-hub/approval/approval.sock"
export AGENT_HUB_ATTENDED_APPROVAL_BINDING='{"run_epoch":"...","authorization_digest":"...","environment_bundle_digest":"...","image_digest":"sha256:...","network_digest":"...","approval_bundle_digest":"..."}'
export AGENT_HUB_ATTENDED_RUN_DEADLINE="2099-01-02T03:04:05Z"
export AGENT_HUB_CONFIG="/agent-hub/agent-hub-permissions.json"
```

Sandbox mode is explicit and fail closed. All four values are required, and
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

In host mode, server and token arguments are still supplied to the gateway even
when decisions resolve locally. Socket mode supplies neither.

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
`.opencode/config.json` in your project root, or
`~/.config/opencode/config.json` for global use.

**Project config** (`.opencode/config.json`):
```json
{
  "plugin": [
    "file:///absolute/path/to/opencode-plugin"
  ]
}
```

**Global config** (`~/.config/opencode/config.json`):
```json
{
  "plugin": [
    "file:///home/you/workspaces/ai_environment/opencode-plugin"
  ]
}
```

The path must be absolute and point to the `opencode-plugin/` directory in
this workspace (the one that contains `package.json` and `agent-hub.ts`).
Bundled single-file artifacts must preserve the default `{ id, server }`
export required by OpenCode's v1 plugin module loader.

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
  `{ "*": "ask" }` fail-closed fallback. Once the one exact bundled plugin is
  initialized, its config hook replaces the top-level permission tree and every
  configured agent-specific tree with `"allow"`. The awaited execution hook is
  then the sole policy owner; any tool call observed before that transformation
  fails closed.
- **Multiple files**: the plugin calls the gateway once per canonical file. The
  first denied file stops evaluation immediately.
