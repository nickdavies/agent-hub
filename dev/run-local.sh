#!/usr/bin/env bash
set -euo pipefail

# Local orchestration API and admin UI harness.
#
# Usage:
#   ./dev/run-local.sh

SERVER_PORT=8080
TOKEN="test-token-$(head -c 12 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9')"
BASE_URL="http://localhost:${SERVER_PORT}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

cleanup() {
    echo ""
    echo "Shutting down..."
    kill "$SERVER_PID" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

echo "Building agent-hub-server..."
cargo build --manifest-path "$ROOT_DIR/Cargo.toml" -p agent-hub-server

echo "Starting agent-hub-server on :${SERVER_PORT}..."
export AGENT_HUB_TOKENS="test:${TOKEN}"
export LISTEN_ADDR="127.0.0.1:${SERVER_PORT}"
export BASE_URL
export BASIC_AUTH_USER=admin
export BASIC_AUTH_PASSWORD=admin
export AGENT_HUB_APPROVALS_SERVER="${AGENT_HUB_APPROVALS_SERVER:-}"
export AGENT_HUB_APPROVALS_TOKEN="${AGENT_HUB_APPROVALS_TOKEN:-}"
export RUST_LOG=info

"$ROOT_DIR/target/debug/agent-hub-server" serve &
SERVER_PID=$!
sleep 1

echo ""
echo "============================================================"
echo " agent-hub orchestration server running"
echo "============================================================"
echo ""
echo "  Admin UI:  ${BASE_URL}/auth/login"
echo "  Health:    ${BASE_URL}/health"
echo "  API:       ${BASE_URL}/api/v1/orchestration"
echo "  Token:     ${TOKEN}"
echo "  Login:     admin / admin"
echo ""
echo "  Approval and question APIs are served by agent-hub-approvals-server."
echo "  Set AGENT_HUB_APPROVALS_SERVER to its endpoint when workflows need it."
echo ""
echo "  Ctrl+C to stop."
echo "============================================================"
echo ""

wait "$SERVER_PID"
