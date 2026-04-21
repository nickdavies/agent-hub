#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ENV_FILE="$ROOT_DIR/.env"
COMPOSE_FILE="$ROOT_DIR/docker-compose.dev.yml"

if [ ! -f "$ENV_FILE" ]; then
  echo "Missing $ENV_FILE (copy from env.example and fill required values)."
  exit 1
fi

if [ ! -f "$COMPOSE_FILE" ]; then
  echo "Missing $COMPOSE_FILE"
  exit 1
fi

# shellcheck disable=SC1090
set -a
source "$ENV_FILE"
set +a

BOT_USERNAME="${VIKUNJA_BOT_USERNAME:-}"
BOT_EMAIL="${VIKUNJA_BOT_EMAIL:-}"
BOT_PASSWORD="${VIKUNJA_BOT_PASSWORD:-}"
PROJECT_TITLE="${VIKUNJA_PROJECT_TITLE:-Agent Hub Local Dev}"

if [ -z "$BOT_USERNAME" ] || [ -z "$BOT_EMAIL" ] || [ -z "$BOT_PASSWORD" ]; then
  echo "VIKUNJA_BOT_USERNAME, VIKUNJA_BOT_EMAIL, and VIKUNJA_BOT_PASSWORD must be set in .env"
  exit 1
fi

echo "Starting compose stack (vikunja + server)..."
docker compose -f "$COMPOSE_FILE" up -d vikunja server

echo "Waiting for Vikunja API readiness..."
for _ in $(seq 1 60); do
  if curl -fsS "http://127.0.0.1:3456/api/v1/info" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

if ! curl -fsS "http://127.0.0.1:3456/api/v1/info" >/dev/null 2>&1; then
  echo "Vikunja API did not become ready"
  exit 1
fi

echo "Ensuring local bot user exists..."
register_payload=$(jq -n \
  --arg username "$BOT_USERNAME" \
  --arg email "$BOT_EMAIL" \
  --arg password "$BOT_PASSWORD" \
  '{username:$username,email:$email,password:$password}')

curl -sS -o /dev/null -w "%{http_code}" \
  -X POST "http://127.0.0.1:3456/api/v1/register" \
  -H "Content-Type: application/json" \
  -d "$register_payload" >/tmp/vikunja-register-code.txt

register_code="$(cat /tmp/vikunja-register-code.txt)"
if [ "$register_code" != "200" ] && [ "$register_code" != "412" ]; then
  echo "Unexpected register response code: $register_code"
  exit 1
fi

echo "Logging in bot user..."
login_payload=$(jq -n --arg username "$BOT_USERNAME" --arg password "$BOT_PASSWORD" '{username:$username,password:$password}')
token="$(curl -fsS \
  -X POST "http://127.0.0.1:3456/api/v1/login" \
  -H "Content-Type: application/json" \
  -d "$login_payload" | jq -r '.token')"

if [ -z "$token" ] || [ "$token" = "null" ]; then
  echo "Failed to obtain Vikunja token"
  exit 1
fi

echo "Ensuring project exists..."
projects_json="$(curl -fsS \
  -H "Authorization: Bearer $token" \
  "http://127.0.0.1:3456/api/v1/projects")"
project_id="$(printf '%s' "$projects_json" | jq -r --arg title "$PROJECT_TITLE" '.[] | select(.title==$title) | .id' | head -n1)"

if [ -z "$project_id" ]; then
  create_payload="$(jq -n --arg title "$PROJECT_TITLE" '{title:$title}')"
  project_id="$(curl -fsS \
    -X PUT "http://127.0.0.1:3456/api/v1/projects" \
    -H "Authorization: Bearer $token" \
    -H "Content-Type: application/json" \
    -d "$create_payload" | jq -r '.id')"
fi

if [ -z "$project_id" ] || [ "$project_id" = "null" ]; then
  echo "Failed to ensure Vikunja project"
  exit 1
fi

echo
echo "Bootstrap complete. Add/update these values in .env:"
echo "VIKUNJA_TOKEN=$token"
echo "VIKUNJA_PROJECT_ID=$project_id"
echo
echo "Then restart server: docker compose -f docker-compose.dev.yml up -d server"
