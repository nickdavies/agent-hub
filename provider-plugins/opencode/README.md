# opencode-plugin-agent-hub

opencode plugin that connects the `permission.ask` hook to `agent-hub-gateway`.

Every tool-use permission check opencode would normally show to the user is
instead evaluated by the gateway's local rule engine first. Fast allow/deny
decisions return immediately with no network call. Anything that needs human
review escalates to the agent-hub server.

## Prerequisites

- `agent-hub-gateway` built and on your `PATH` (see `agent-hub/` in this
  workspace)
- An `agent-hub-server` instance running (only needed for `ask`-escalated
  decisions; local allow/deny work without it)
- opencode 1.18.18, whose `permission.asked` event and permission reply API are
  the runtime permission integration used by this plugin

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
opencode permission.asked event
  └─ plugin calls agent-hub-gateway --opencode (once per pattern)
        └─ gateway loads ~/.config/agent-hub/agent-hub-permissions.json
              Allow  → plugin replies "once" through the OpenCode API
              Deny   → plugin replies "reject" through the OpenCode API
              Delegate(dippy) → Dippy allow/deny → same as above
                                Dippy ask        → gateway escalates to server
              Ask    → gateway long-polls server → allow/deny → same as above
```

The `permission` field from opencode (`bash`, `edit`, `read`) is normalised to
the canonical gateway tool name (`Bash`, `Write`, `Read`) before rule
evaluation. The `patterns` array from opencode (commands or file paths) is
mapped to the correct `tool_input` field (`command` for shell, `path` for file
tools) so that pattern-matching rules in
`~/.config/agent-hub/agent-hub-permissions.json` work as expected.

## Permission → tool name mapping

| opencode `permission` | gateway canonical name | `tool_input` field |
|---|---|---|
| `bash` | `Bash` | `command` |
| `edit` | `Write` | `path` |
| `read` | `Read` | `path` |
| anything else | passed through unchanged | `pattern` (no rule match) |

Unknown permissions fall through to the `default` action in
`~/.config/agent-hub/agent-hub-permissions.json`.

## Caveats

- **Reply race**: the plugin and an attached OpenCode UI can both reply to the
  same request. Supervised headless runs do not attach an OpenCode approval UI.
- **Multiple patterns**: the plugin calls the gateway once per pattern in the
  `patterns` array. The first denied pattern stops evaluation immediately.
