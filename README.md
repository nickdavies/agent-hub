# claude-notify

> **Experimental** — This project is under active development. APIs, configuration, and behavior may change without notice.

A notification server for [Claude Code](https://docs.anthropic.com/en/docs/claude-code). It receives webhook events from Claude Code sessions (permission prompts, task completions), gates them against your physical presence, and forwards notifications via a pluggable backend (Pushover, webhook, or bring your own).

## Why

Claude Code can run autonomously for long stretches, then block on a permission prompt or finish a task while you're away from your desk. The built-in notification hook runs a local shell script — fine for simple cases, but it can't know whether you're actually sitting at your computer.

claude-notify solves this by running as a server (designed for k8s, works anywhere) that:

- **Gates notifications on presence** — a motion sensor (or any HTTP client) posts your presence state; notifications are suppressed while you're present
- **Tracks sessions** — auto-registers Claude Code sessions on first hook contact, with per-session notification control
- **Exposes MCP tools** — Claude itself can query and configure its own notification preferences mid-session
- **Supports multiple clients** — any number of Claude Code instances across machines can report to the same server
- **Pluggable notification backends** — ships with Pushover and webhook support, easy to add more

## Architecture

```
                                              ┌──Pushover API──▶  Phone
Claude Code  ──HTTP hooks──▶  claude-notify  ─┤
     │                             ▲          └──Webhook POST──▶  Your service
     ├──MCP (HTTP)──▶  /mcp       │
     │                             │
Motion sensor  ────────────────────┘  POST /presence
```

Single binary (`claude-notify serve <backend>`), single process. REST API and MCP protocol are served together.

## Setup

### Requirements

- Rust 1.80+ (or Docker)
- A notification backend (see below)

### Notification backends

#### Pushover

Uses the [Pushover](https://pushover.net/) API to send push notifications to your phone.

| Variable | Description |
|----------|-------------|
| `PUSHOVER_TOKEN` | Pushover API token |
| `PUSHOVER_USER` | Pushover user key |

```sh
claude-notify serve pushover
# or explicitly:
claude-notify serve pushover --token "..." --user "..."
```

#### Webhook

POSTs a JSON payload to any URL. Useful for integrating with Slack, Discord, Home Assistant, or any custom service.

| Variable | Description |
|----------|-------------|
| `WEBHOOK_URL` | URL to POST notifications to |

```sh
claude-notify serve webhook --url "https://example.com/notify"
```

The payload is:

```json
{
  "title": "Claude Code",
  "message": "[my-project] Claude finished"
}
```

### Common environment variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `CLAUDE_NOTIFY_TOKENS` | yes | — | Auth tokens (see [Authentication](#authentication)) |
| `LISTEN_ADDR` | no | `0.0.0.0:8080` | Bind address |
| `PRESENCE_TTL` | no | `120` | Seconds before presence degrades to `away` |
| `SESSION_TTL` | no | `7200` | Seconds before idle sessions are evicted |
| `NOTIFICATION_DELAY` | no | `0` | Seconds to wait before sending permission notifications (0 = immediate) |

### Run locally

```sh
export PUSHOVER_TOKEN="..."
export PUSHOVER_USER="..."
export CLAUDE_NOTIFY_TOKENS="desktop:some-secret-token"

cargo run -- serve pushover
```

Or with webhook:

```sh
export WEBHOOK_URL="https://example.com/notify"
export CLAUDE_NOTIFY_TOKENS="desktop:some-secret-token"

cargo run -- serve webhook
```

### State persistence

By default, state is not persisted across restarts. To persist sessions, config, and presence:

```sh
claude-notify serve pushover local-file --path state.json
```

### Run with Docker

```sh
docker build -t claude-notify .
docker run -p 8080:8080 \
  -e PUSHOVER_TOKEN="..." \
  -e PUSHOVER_USER="..." \
  -e CLAUDE_NOTIFY_TOKENS="desktop:some-secret-token" \
  claude-notify serve pushover
```

### Configure Claude Code

Add HTTP hooks to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "http",
            "url": "https://your-server/hooks/stop",
            "timeout": 5,
            "headers": { "Authorization": "Bearer $CLAUDE_NOTIFY_TOKEN" },
            "allowedEnvVars": ["CLAUDE_NOTIFY_TOKEN"]
          }
        ]
      }
    ],
    "Notification": [
      {
        "matcher": "permission_prompt",
        "hooks": [
          {
            "type": "http",
            "url": "https://your-server/hooks/notification",
            "timeout": 5,
            "headers": { "Authorization": "Bearer $CLAUDE_NOTIFY_TOKEN" },
            "allowedEnvVars": ["CLAUDE_NOTIFY_TOKEN"]
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "http",
            "url": "https://your-server/hooks/session-end",
            "timeout": 5,
            "headers": { "Authorization": "Bearer $CLAUDE_NOTIFY_TOKEN" },
            "allowedEnvVars": ["CLAUDE_NOTIFY_TOKEN"]
          }
        ]
      }
    ]
  }
}
```

Set `CLAUDE_NOTIFY_TOKEN` in your shell profile to match one of the tokens in `CLAUDE_NOTIFY_TOKENS`.

### Enable MCP tools (optional)

Create `~/.claude/.mcp.json`:

```json
{
  "mcpServers": {
    "claude-notify": {
      "type": "http",
      "url": "https://your-server/mcp",
      "headers": {
        "Authorization": "Bearer ${CLAUDE_NOTIFY_TOKEN}"
      }
    }
  }
}
```

This gives Claude access to three tools:

| Tool | Description |
|------|-------------|
| `configure_session` | Toggle stop/permission notifications for a session |
| `list_sessions` | List all active sessions and their config |
| `get_status` | Show presence state, global config, session count |

## API

### Hook endpoints

| Method | Path | Description |
|--------|------|-------------|
| POST | `/hooks/stop` | Agent stopped. Notifies if away, then deregisters session. |
| POST | `/hooks/notification` | Permission prompt. Notifies if away. |
| POST | `/hooks/session-end` | Deregisters session. |

Hook payloads should include `session_id` and `cwd`. Sessions are auto-registered on first contact.

### Management endpoints

| Method | Path | Description |
|--------|------|-------------|
| POST | `/presence` | Update presence state (`present`, `idle`, `away`) |
| GET | `/sessions` | List active sessions |
| PUT | `/sessions/{id}` | Update per-session notification config |
| GET | `/config` | Read global config + presence state |
| PUT | `/config` | Toggle global notification types |
| GET | `/health` | Liveness/readiness (no auth) |
| POST | `/mcp` | MCP Streamable HTTP transport |

All endpoints except `/health` require a valid Bearer token.

## Authentication

There is no token issuing mechanism — you generate tokens yourself and configure both the server and clients with them. Any sufficiently random string works. Use a different token per client so you can revoke or identify them independently.

Generate tokens with `openssl`:

```sh
openssl rand -hex 32
```

Then set `CLAUDE_NOTIFY_TOKENS` as a comma-separated list of `label:token` pairs:

```sh
export CLAUDE_NOTIFY_TOKENS="desktop:$(openssl rand -hex 32),sensor:$(openssl rand -hex 32)"
```

The label is an arbitrary name for logging — it identifies which client made a request. If you omit the label (just a bare token), the token itself is used as the identifier in logs.

Give each client its corresponding token:

- **Claude Code** — set `CLAUDE_NOTIFY_TOKEN` in your shell profile to the `desktop` token
- **Motion sensor** — configure it with the `sensor` token
- **Other machines** — add more `label:token` entries as needed

All endpoints except `/health` require a valid `Authorization: Bearer <token>` header. Invalid or missing tokens return 401.

## Presence

Presence has three states: `present`, `idle`, `away`. The server starts in `away`.

Notifications are only sent when presence is **not** `present`. If no presence update is received within `PRESENCE_TTL` seconds (default 120), the state automatically degrades to `away`.

Post presence from a motion sensor, cron job, or anything that can make an HTTP request:

```sh
curl -X POST https://your-server/presence \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"state": "present"}'
```

## Notification gating

A notification fires only when all three conditions are true:

1. Presence is not `present`
2. The notification type is enabled globally (`GET/PUT /config`)
3. The notification type is enabled for that session (`PUT /sessions/{id}`)

Both global and per-session config default to all notifications enabled.

## Notification delay

Permission notifications can be delayed to avoid notifying you about prompts you answer quickly. Set `NOTIFICATION_DELAY=30` to wait 30 seconds before sending. If the session ends, stops, or a new permission prompt arrives for the same session during the delay, the pending notification is cancelled.

This is useful when you don't have presence detection set up yet — without it, every permission prompt sends immediately even when you're at your desk.

The delay is configurable at runtime via `PUT /config`:

```sh
# Enable 30 second delay
curl -X PUT https://your-server/config \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"notification_delay_secs": 30}'

# Disable delay (send immediately)
curl -X PUT https://your-server/config \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"notification_delay_secs": 0}'
```

Stop notifications are always sent immediately — the delay only applies to permission prompts.

## Contributing a notification channel

Adding a new notification backend takes three steps:

### 1. Implement the `Notifier` trait

Create a new file in `src/server/` (e.g., `discord.rs`):

```rust
use super::notifier::{Notifier, NotifyError};

pub struct DiscordClient {
    // your fields
}

impl Notifier for DiscordClient {
    fn name(&self) -> &'static str {
        "discord"
    }

    async fn send(&self, title: &str, message: &str) -> Result<(), NotifyError> {
        // Send the notification. Return NotifyError on failure.
        todo!()
    }
}
```

The trait contract is intentionally minimal — `name()` for logging and `send()` for delivery. See `pushover.rs` and `webhook.rs` for working examples.

### 2. Register the module and CLI subcommand

In `src/server/mod.rs`, add `pub mod discord;`.

In `src/main.rs`, add a variant to `NotifierArgs`:

```rust
Discord {
    #[arg(long, env = "DISCORD_WEBHOOK_URL")]
    url: String,

    #[command(subcommand)]
    storage: Option<StorageArgs>,
},
```

And handle it in the `main()` match:

```rust
NotifierArgs::Discord { url, storage } => {
    let notifier = DiscordClient::new(url);
    serve_with_storage(notifier, storage).await
}
```

### 3. Update docs

Add your backend to the "Notification backends" section in this README.

That's it — the rest of the system (presence gating, session tracking, delay logic, MCP) works automatically with any `Notifier` implementation.
