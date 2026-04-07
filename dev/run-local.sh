#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Local test harness for agent-hub with remote approvals
#
# Uses basic auth by default. Set GOOGLE_CLIENT_ID + GOOGLE_CLIENT_SECRET
# to also enable Google OIDC.
#
# Usage:
#   ./dev/run-local.sh
#   GOOGLE_CLIENT_ID=xxx GOOGLE_CLIENT_SECRET=yyy ./dev/run-local.sh
# ---------------------------------------------------------------------------

WEBHOOK_PORT=9999
SERVER_PORT=8080
TOKEN="test-token-$(head -c 12 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9')"
BASE_URL="http://localhost:${SERVER_PORT}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

cleanup() {
    echo ""
    echo "Shutting down..."
    kill "$WEBHOOK_PID" 2>/dev/null || true
    kill "$SERVER_PID" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

# --- Build ---
echo "Building agent-hub..."
cargo build --manifest-path "$ROOT_DIR/Cargo.toml" 2>&1 | tail -3

# --- Start webhook logger ---
echo ""
echo "Starting webhook logger on :${WEBHOOK_PORT}..."
python3 "$SCRIPT_DIR/webhook-logger.py" "$WEBHOOK_PORT" &
WEBHOOK_PID=$!
sleep 0.3

# --- Start server ---
echo "Starting agent-hub-server on :${SERVER_PORT}..."
export AGENT_HUB_TOKENS="test:${TOKEN}"
export LISTEN_ADDR="127.0.0.1:${SERVER_PORT}"
export APPROVAL_MODE=readwrite
export BASE_URL
export BASIC_AUTH_USER=admin
export BASIC_AUTH_PASSWORD=admin
export DEFAULT_APPROVAL_MODE=remote
export RUST_LOG=info

# Optional: Google OIDC (if env vars are set)
if [ -n "${GOOGLE_CLIENT_ID:-}" ] && [ -n "${GOOGLE_CLIENT_SECRET:-}" ]; then
    export GOOGLE_CLIENT_ID
    export GOOGLE_CLIENT_SECRET
    export OAUTH_ALLOWED_EMAILS="${ALLOWED_EMAIL:-}"
    echo "  Google OIDC: enabled"
else
    echo "  Google OIDC: disabled (set GOOGLE_CLIENT_ID + GOOGLE_CLIENT_SECRET to enable)"
fi

"$ROOT_DIR/target/debug/agent-hub-server" serve webhook --url "http://127.0.0.1:${WEBHOOK_PORT}" &
SERVER_PID=$!
sleep 1

# --- Print cheat sheet ---
echo ""
echo "============================================================"
echo " agent-hub test harness running"
echo "============================================================"
echo ""
echo "  Web UI:    ${BASE_URL}/auth/login"
echo "  Health:    ${BASE_URL}/health"
echo "  Token:     ${TOKEN}"
echo "  Login:     admin / admin"
echo ""
echo "--- Simulate a hook binary (register an approval) ----------"
echo ""
cat <<CURL
  # 1. Register an approval
  curl -s -X POST ${BASE_URL}/api/v1/hooks/approval \\
    -H "Authorization: Bearer ${TOKEN}" \\
    -H "Content-Type: application/json" \\
    -d '{
      "request_id": "test-req-1",
      "session_id": "test-session-1",
      "cwd": "/home/you/my-project",
      "tool_name": "Bash",
      "tool_input": {"command": "rm -rf node_modules"},
      "context": "Cleaning up before fresh install"
    }' | python3 -m json.tool

  # 2. Long-poll for decision (will block up to 55s, returns 202 if timeout)
  #    Copy the "id" from step 1 into APPROVAL_ID
  APPROVAL_ID=<id-from-step-1>
  curl -s ${BASE_URL}/api/v1/approvals/\${APPROVAL_ID}/wait \\
    -H "Authorization: Bearer ${TOKEN}" | python3 -m json.tool

  # 3. Resolve via API (or use the web UI instead)
  curl -s -X POST ${BASE_URL}/api/v1/approvals/\${APPROVAL_ID}/resolve \\
    -H "Authorization: Bearer ${TOKEN}" \\
    -H "Content-Type: application/json" \\
    -d '{"decision": "approve", "message": "looks good"}' | python3 -m json.tool

  # 4. Check session approval mode
  curl -s ${BASE_URL}/api/v1/sessions/test-session-1/approval-mode \\
    -H "Authorization: Bearer ${TOKEN}" | python3 -m json.tool

CURL
echo ""
echo "--- Simulate status reports ---------------------------------"
echo ""
cat <<CURL2
  # Report a session as active (e.g. agent working)
  curl -s -X POST ${BASE_URL}/api/v1/hooks/status \\
    -H "Authorization: Bearer ${TOKEN}" \\
    -H "Content-Type: application/json" \\
    -d '{
      "session_id": "test-session-1",
      "cwd": "/home/you/my-project",
      "status": "active",
      "editor_type": "opencode"
    }' | python3 -m json.tool

  # Report idle (agent finished working, waiting for user)
  curl -s -X POST ${BASE_URL}/api/v1/hooks/status \\
    -H "Authorization: Bearer ${TOKEN}" \\
    -H "Content-Type: application/json" \\
    -d '{
      "session_id": "test-session-1",
      "cwd": "/home/you/my-project",
      "status": "idle",
      "editor_type": "opencode"
    }' | python3 -m json.tool

  # Report ended (session closed)
  curl -s -X POST ${BASE_URL}/api/v1/hooks/status \\
    -H "Authorization: Bearer ${TOKEN}" \\
    -H "Content-Type: application/json" \\
    -d '{
      "session_id": "test-session-1",
      "cwd": "/home/you/my-project",
      "status": "ended",
      "editor_type": "opencode"
    }' | python3 -m json.tool

CURL2
echo ""
echo "--- Automated session simulator ------------------------------"
echo ""
echo "  For a full lifecycle test (active → idle → ended), use:"
echo ""
echo "    ./dev/simulate-session.sh --token ${TOKEN}"
echo "    ./dev/simulate-session.sh --token ${TOKEN} --fast"
echo "    ./dev/simulate-session.sh --token ${TOKEN} --with-approval"
echo ""
echo "--- What to test -------------------------------------------"
echo ""
echo "  1. Open ${BASE_URL}/auth/login in browser, sign in with admin/admin"
echo "  2. Run the curl commands above to register an approval"
echo "  3. Check the webhook logger output (this terminal) for push notification"
echo "  4. Open ${BASE_URL}/approvals in browser to see the pending approval"
echo "  5. Click 'Review' to see detail, approve/deny from the web UI"
echo "  6. Check that the /wait curl unblocks with the decision"
echo "  7. Toggle session approval mode in the dashboard"
echo "  8. Run the status report curls above and watch the dashboard update"
echo "  9. Run ./dev/simulate-session.sh for a full automated lifecycle test"
echo ""
echo "  Ctrl+C to stop everything."
echo "============================================================"
echo ""

# Wait for either process to exit
wait -n "$WEBHOOK_PID" "$SERVER_PID" 2>/dev/null || true
