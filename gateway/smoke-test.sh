#!/usr/bin/env bash
# Simulated smoke tests for agent-hub-gateway.
# Exercises local rule evaluation only — no server calls.
# The smoke config uses only allow/deny rules so nothing escalates to the server.

set -euo pipefail

BIN="$(dirname "$0")/../target/debug/agent-hub-gateway"
CONFIG="$(dirname "$0")/config/smoke-test.json"
WORKSPACE="/home/user/project"

PASS=0
FAIL=0

# Dummy values — required by CLI but not used when rules resolve locally.
SERVER="http://localhost:9999"
TOKEN="smoke-test-token"

run() {
    local label="$1"
    local provider_flag="$2"
    local payload="$3"
    local expect_field="$4"   # jq path to check, e.g. '.hookSpecificOutput.permissionDecision'
    local expect_value="$5"   # expected string value

    local stdout stderr exit_code
    stdout=$(echo "$payload" | timeout 5 "$BIN" approval "$provider_flag" --server "$SERVER" --token "$TOKEN" --config "$CONFIG" 2>/tmp/smoke-stderr) || exit_code=$?
    exit_code=${exit_code:-0}
    stderr=$(cat /tmp/smoke-stderr)

    local actual
    actual=$(echo "$stdout" | jq -r "$expect_field" 2>/dev/null || echo "PARSE_ERROR")

    if [[ "$actual" == "$expect_value" ]]; then
        echo "  PASS  $label"
        PASS=$((PASS + 1))
    else
        echo "  FAIL  $label"
        echo "        expected $expect_field = \"$expect_value\""
        echo "        got      \"$actual\""
        echo "        stdout:  $stdout"
        echo "        stderr:  $stderr"
        FAIL=$((FAIL + 1))
    fi
}

echo ""
echo "=== Claude Code provider ==="

# Read inside workspace → allow (PreToolUse)
run "Read inside workspace → allow" \
    "--claude" \
    '{"session_id":"abc123","tool_name":"Read","tool_input":{"path":"'"$WORKSPACE"'/src/main.rs"},"cwd":"'"$WORKSPACE"'","hook_event_name":"PreToolUse"}' \
    '.hookSpecificOutput.permissionDecision' "allow"

# Read outside workspace → still allow (@file_read rule has no in_workspace constraint)
run "Read outside workspace → allow" \
    "--claude" \
    '{"session_id":"abc123","tool_name":"Read","tool_input":{"path":"/etc/passwd"},"cwd":"'"$WORKSPACE"'","hook_event_name":"PreToolUse"}' \
    '.hookSpecificOutput.permissionDecision' "allow"

# Write inside workspace → allow
run "Write inside workspace → allow" \
    "--claude" \
    '{"session_id":"abc123","tool_name":"Write","tool_input":{"path":"'"$WORKSPACE"'/src/main.rs"},"cwd":"'"$WORKSPACE"'","hook_event_name":"PreToolUse"}' \
    '.hookSpecificOutput.permissionDecision' "allow"

# Write outside workspace → deny
run "Write outside workspace → deny" \
    "--claude" \
    '{"session_id":"abc123","tool_name":"Write","tool_input":{"path":"/tmp/evil.sh"},"cwd":"'"$WORKSPACE"'","hook_event_name":"PreToolUse"}' \
    '.hookSpecificOutput.permissionDecision' "deny"

# Bash command → allow
run "Bash → allow" \
    "--claude" \
    '{"session_id":"abc123","tool_name":"Bash","tool_input":{"command":"cargo test"},"cwd":"'"$WORKSPACE"'","hook_event_name":"PreToolUse"}' \
    '.hookSpecificOutput.permissionDecision' "allow"

# Read .ssh file → deny
run "Read .ssh → deny" \
    "--claude" \
    '{"session_id":"abc123","tool_name":"Read","tool_input":{"path":"/home/user/.ssh/id_rsa"},"cwd":"'"$WORKSPACE"'","hook_event_name":"PreToolUse"}' \
    '.hookSpecificOutput.permissionDecision' "deny"

# Bash in .ssh dir → deny (glob **/.ssh/** matches against the command string)
run "Bash in .ssh → deny" \
    "--claude" \
    '{"session_id":"abc123","tool_name":"Bash","tool_input":{"command":"cat /home/user/.ssh/id_rsa"},"cwd":"'"$WORKSPACE"'","hook_event_name":"PreToolUse"}' \
    '.hookSpecificOutput.permissionDecision' "deny"

# WebFetch → deny
run "WebFetch → deny" \
    "--claude" \
    '{"session_id":"abc123","tool_name":"WebFetch","tool_input":{"url":"https://example.com"},"cwd":"'"$WORKSPACE"'","hook_event_name":"PreToolUse"}' \
    '.hookSpecificOutput.permissionDecision' "deny"

# PermissionRequest event: Read → allow
run "PermissionRequest Read → allow" \
    "--claude" \
    '{"session_id":"abc123","tool_name":"Read","tool_input":{"path":"'"$WORKSPACE"'/src/lib.rs"},"cwd":"'"$WORKSPACE"'","hook_event_name":"PermissionRequest"}' \
    '.hookSpecificOutput.decision.behavior' "allow"

# PermissionRequest event: Write outside → deny
run "PermissionRequest Write outside → deny" \
    "--claude" \
    '{"session_id":"abc123","tool_name":"Write","tool_input":{"path":"/tmp/evil.sh"},"cwd":"'"$WORKSPACE"'","hook_event_name":"PermissionRequest"}' \
    '.hookSpecificOutput.decision.behavior' "deny"

echo ""
echo "=== Cursor provider ==="

# Cursor uses conversation_id instead of session_id
run "Cursor Read → allow" \
    "--cursor" \
    '{"conversation_id":"cur123","tool_name":"Read","tool_input":{"path":"'"$WORKSPACE"'/foo.rs"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"]}' \
    '.permission' "allow"

run "Cursor Write inside workspace → allow" \
    "--cursor" \
    '{"conversation_id":"cur123","tool_name":"Write","tool_input":{"path":"'"$WORKSPACE"'/foo.rs"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"]}' \
    '.permission' "allow"

run "Cursor Write outside workspace → deny" \
    "--cursor" \
    '{"conversation_id":"cur123","tool_name":"Write","tool_input":{"path":"/tmp/evil.sh"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"]}' \
    '.permission' "deny"

run "Cursor Shell → allow" \
    "--cursor" \
    '{"conversation_id":"cur123","tool_name":"Shell","tool_input":{"command":"ls"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"]}' \
    '.permission' "allow"

echo ""
echo "=== Opencode provider ==="

run "Opencode Read → allow" \
    "--opencode" \
    '{"session_id":"oc123","tool_name":"Read","tool_input":{"path":"'"$WORKSPACE"'/foo.rs"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"],"hook_event_name":"tool.execute.before"}' \
    '.allowed' "true"

run "Opencode Write inside workspace → allow" \
    "--opencode" \
    '{"session_id":"oc123","tool_name":"Write","tool_input":{"path":"'"$WORKSPACE"'/foo.rs"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"],"hook_event_name":"tool.execute.before"}' \
    '.allowed' "true"

run "Opencode Write outside workspace → deny" \
    "--opencode" \
    '{"session_id":"oc123","tool_name":"Write","tool_input":{"path":"/tmp/evil.sh"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"],"hook_event_name":"tool.execute.before"}' \
    '.allowed' "false"

run "Opencode Bash → allow" \
    "--opencode" \
    '{"session_id":"oc123","tool_name":"Bash","tool_input":{"command":"cargo build"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"],"hook_event_name":"tool.execute.before"}' \
    '.allowed' "true"

echo ""
echo "=== Opencode permission.ask hook (permission name normalisation) ==="

# "bash" permission → normalised to "Bash" → matches @shell allow rule
run "permission.ask bash → allow" \
    "--opencode" \
    '{"session_id":"oc123","tool_name":"bash","tool_input":{"command":"cargo test"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"],"hook_event_name":"permission.ask"}' \
    '.allowed' "true"

# "edit" permission → normalised to "Write" → path inside workspace → allow
run "permission.ask edit inside workspace → allow" \
    "--opencode" \
    '{"session_id":"oc123","tool_name":"edit","tool_input":{"path":"'"$WORKSPACE"'/src/main.rs"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"],"hook_event_name":"permission.ask"}' \
    '.allowed' "true"

# "edit" permission → normalised to "Write" → path outside workspace → deny
run "permission.ask edit outside workspace → deny" \
    "--opencode" \
    '{"session_id":"oc123","tool_name":"edit","tool_input":{"path":"/tmp/evil.sh"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"],"hook_event_name":"permission.ask"}' \
    '.allowed' "false"

# "read" permission → normalised to "Read" → allow
run "permission.ask read → allow" \
    "--opencode" \
    '{"session_id":"oc123","tool_name":"read","tool_input":{"path":"'"$WORKSPACE"'/README.md"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"],"hook_event_name":"permission.ask"}' \
    '.allowed' "true"

# .ssh deny rule fires even through permission.ask path
run "permission.ask read .ssh → deny" \
    "--opencode" \
    '{"session_id":"oc123","tool_name":"read","tool_input":{"path":"/home/user/.ssh/id_rsa"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"],"hook_event_name":"permission.ask"}' \
    '.allowed' "false"

# "webfetch" permission → normalised to "WebFetch" → matches deny rule
run "permission.ask webfetch → deny" \
    "--opencode" \
    '{"session_id":"oc123","tool_name":"webfetch","tool_input":{"url":"https://example.com"},"cwd":"'"$WORKSPACE"'","workspace_roots":["'"$WORKSPACE"'"],"hook_event_name":"permission.ask"}' \
    '.allowed' "false"

echo ""
if [[ $FAIL -eq 0 ]]; then
    echo "All $PASS tests passed."
else
    echo "$PASS passed, $FAIL FAILED."
    exit 1
fi
