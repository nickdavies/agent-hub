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
- opencode with the `permission.ask` plugin hook (the fix is in the
  `feat(permission): add permission.ask plugin hook` commit in the `opencode/`
  subdir of this workspace)

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

Server and token are required by the gateway CLI even when all decisions
resolve locally (fast path never contacts the network). They can be set to
placeholder values if you are not using the server escalation path.

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

### 5. Verify

Run opencode and trigger a tool that requires permission (e.g. a bash
command). You should see `agent-hub-gateway` debug output on stderr and
opencode should allow or deny the action according to your
`~/.config/agent-hub/agent-hub-permissions.json` rules
without showing its own approval prompt.

## How it works

```
opencode permission.ask hook
  └─ plugin calls agent-hub-gateway --opencode (once per pattern)
        └─ gateway loads ~/.config/agent-hub/agent-hub-permissions.json
              Allow  → plugin sets output.status = "allow"  (no prompt shown)
              Deny   → plugin sets output.status = "deny"   (LLM gets reason)
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

- **Blocking**: opencode currently shows its own approval UI before the plugin
  hook fires (`inline_approval = false` in the plan). Once the upstream race
  condition fix lands and is merged, the plugin hook runs first and the UI is
  skipped entirely. Until then, the hook still fires and its decision is
  respected — it just races with the UI.
- **Multiple patterns**: the plugin calls the gateway once per pattern in the
  `patterns` array. The first denied pattern stops evaluation immediately.
