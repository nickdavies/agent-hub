# claude-notify Deployment & Cutover

## What's built

The server is complete at `~/workspaces/claude_notify/server/`. It builds cleanly, 12 tests pass, zero warnings.

### Project structure
```
src/
├── main.rs         # CLI (clap), serve subcommand, shutdown handler
├── error.rs        # AppError enum (Config, SessionNotFound)
├── mcp.rs          # MCP over Streamable HTTP (rmcp), 3 tools
└── server/
    ├── mod.rs      # Router, AppState, REST handlers
    ├── auth.rs     # Bearer token middleware
    ├── config.rs   # ServerConfig (env), NotifyConfig (runtime mutable)
    ├── hooks.rs    # /hooks/stop, /hooks/notification, /hooks/session-end
    ├── notifier.rs # Notifier trait — implement to add new backends
    ├── presence.rs # Presence state with TTL-on-read
    ├── pushover.rs # Pushover backend
    ├── webhook.rs  # Webhook backend
    ├── sessions.rs # Session registry with TTL eviction
    └── storage.rs  # Storage trait + implementations
```

## 1. Deploy to k8s

### Required env vars

| Var | Description |
|-----|-------------|
| `CLAUDE_NOTIFY_TOKENS` | Comma-separated `label:secret` pairs (e.g. `desktop:abc123,sensor:def456`) |
| `PUSHOVER_TOKEN` | Pushover API token (if using `serve pushover`) |
| `PUSHOVER_USER` | Pushover user key (if using `serve pushover`) |
| `WEBHOOK_URL` | Webhook URL (if using `serve webhook`) |
| `LISTEN_ADDR` | (optional) Default: `127.0.0.1:8080` (no-auth) / `0.0.0.0:8080` (token) |
| `PRESENCE_TTL` | (optional) Seconds before presence falls back to `away`. Default: `120` |
| `SESSION_TTL` | (optional) Seconds before idle sessions are evicted. Default: `7200` |
| `NOTIFICATION_DELAY` | (optional) Seconds to delay permission notifications. Default: `0` (immediate). |

### Build & push image

```sh
cd ~/workspaces/claude_notify/server
docker build -t <registry>/claude-notify:latest .
docker push <registry>/claude-notify:latest
```

The container command should be `serve pushover` or `serve webhook` depending on your backend:

```sh
# Example: Pushover
command: ["serve", "pushover"]

# Example: Webhook
command: ["serve", "webhook"]
```

### k8s resources needed

- Deployment (1 replica, container port 8080)
- Service (ClusterIP or LoadBalancer)
- Ingress or gateway route with TLS (the hooks use HTTPS)
- Health check: `GET /health` (no auth)
- Secret for env vars (CLAUDE_NOTIFY_TOKENS + backend-specific vars)

## 2. Verify the server

Set your server URL and a token:
```sh
export URL="https://<your-server>"
export TOKEN="<one-of-your-tokens>"
```

```sh
# Health (no auth)
curl $URL/health

# Notification hook (should send Pushover — presence defaults to away)
curl -X POST $URL/hooks/notification \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"session_id":"test-1","cwd":"/home/nick/workspaces/myapp","message":"Permission needed"}'

# Check session registered
curl $URL/sessions -H "Authorization: Bearer $TOKEN" | jq

# Set present → suppress notifications
curl -X POST $URL/presence \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"state":"present"}'

# Stop hook (should NOT send Pushover while present)
curl -X POST $URL/hooks/stop \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"session_id":"test-2","cwd":"/home/nick/workspaces/myapp"}'

# Set away → notifications resume
curl -X POST $URL/presence \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"state":"away"}'

# Stop hook (should send Pushover)
curl -X POST $URL/hooks/stop \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"session_id":"test-3","cwd":"/home/nick/workspaces/myapp"}'

# Config endpoints
curl $URL/config -H "Authorization: Bearer $TOKEN" | jq
curl -X PUT $URL/config \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"stop_enabled":false}'

# Unauth → 401
curl -v $URL/sessions
```

## 3. Cutover (after server is verified)

### 3a. Set local env var

Add to your shell profile:
```sh
export CLAUDE_NOTIFY_TOKEN="<your-desktop-token>"
```

### 3b. Update `~/.claude/settings.json`

Replace the current hooks config. Keep dippy, remove PostToolUse (cancel_notify no longer needed), replace command hooks with HTTP:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "~/.claude/dippy/bin/dippy-hook" }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "http",
            "url": "https://<your-server>/hooks/session-end",
            "timeout": 5,
            "headers": { "Authorization": "Bearer $CLAUDE_NOTIFY_TOKEN" },
            "allowedEnvVars": ["CLAUDE_NOTIFY_TOKEN"]
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "http",
            "url": "https://<your-server>/hooks/stop",
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
            "url": "https://<your-server>/hooks/notification",
            "timeout": 5,
            "headers": { "Authorization": "Bearer $CLAUDE_NOTIFY_TOKEN" },
            "allowedEnvVars": ["CLAUDE_NOTIFY_TOKEN"]
          }
        ]
      }
    ]
  },
  "permissions": {
    "allow": [],
    "deny": [],
    "ask": [
      "Bash(sudo *)",
      "Bash(*sudo *)",
      "Bash(rm -rf *)",
      "Bash(rm -r *)",
      "Bash(*rm -rf *)",
      "Bash(*rm -r *)"
    ]
  }
}
```

### 3c. Create `~/.claude/.mcp.json`

```json
{
  "mcpServers": {
    "claude-notify": {
      "type": "http",
      "url": "https://<your-server>/mcp",
      "headers": {
        "Authorization": "Bearer ${CLAUDE_NOTIFY_TOKEN}"
      }
    }
  }
}
```

### 3d. Cleanup old shell hooks

```sh
rm ~/.claude/hooks/notify.sh
rm ~/.claude/hooks/permission_notify.sh
rm ~/.claude/hooks/cancel_notify.sh
rm -rf ~/.claude/hooks/state/
```

The `CLAUDE_NOTIFY=1` env var is no longer needed — remove from shell profile.

## 4. Motion sensor integration

POST presence updates from your sensor:
```sh
curl -X POST https://<your-server>/presence \
  -H "Authorization: Bearer <sensor-token>" \
  -H "Content-Type: application/json" \
  -d '{"state":"present"}'
```

States: `present`, `idle`, `away`. If no update received within `PRESENCE_TTL` seconds (default 120), presence auto-degrades to `away`.
