#!/usr/bin/env bash
set -euo pipefail

if (($# != 0)); then
    echo "usage: $0" >&2
    exit 2
fi

for command in cp mktemp mv python3 rm stat; do
    command -v "$command" >/dev/null || {
        echo "error: required rollback command not found: $command" >&2
        exit 1
    }
done

config_home="${XDG_CONFIG_HOME:-${HOME}/.config}"
cache_home="${XDG_CACHE_HOME:-${HOME}/.cache}"
state_home="${XDG_STATE_HOME:-${HOME}/.local/state}"
rollback_root="${cache_home}/agent-hub-approvals/rollback"
pointer="${rollback_root}/current"

[[ -f "$pointer" && ! -L "$pointer" ]] || {
    echo "error: no rollback slot is selected" >&2
    exit 1
}
slot_name="$(<"$pointer")"
[[ "$slot_name" == slot.* && "$slot_name" != */* ]] || {
    echo "error: invalid rollback slot pointer" >&2
    exit 1
}
slot="${rollback_root}/${slot_name}"
[[ -d "$slot" && ! -L "$slot" && "$(stat -c %u -- "$slot")" == "$EUID" && \
    "$(stat -c %a -- "$slot")" == 700 ]] || {
    echo "error: rollback slot is not an owner-private directory" >&2
    exit 1
}
for artifact in manifest.json agent-hub-server server_data.json approvals-server.env \
    agent-hub-approvals.service; do
    [[ -f "${slot}/${artifact}" && ! -L "${slot}/${artifact}" ]] || {
        echo "error: rollback artifact must be a regular, non-symlink file: ${slot}/${artifact}" >&2
        exit 1
    }
done

ROLLBACK_SLOT="$slot" python3 - <<'PY'
import hashlib
import json
import os
from pathlib import Path

root = Path(os.environ["ROLLBACK_SLOT"])
manifest = json.loads((root / "manifest.json").read_text())
if manifest.get("schema") != "agent-hub-approvals-rollback-v1":
    raise SystemExit("error: unsupported rollback manifest")
expected = {
    "agent-hub-server",
    "server_data.json",
    "approvals-server.env",
    "agent-hub-approvals.service",
}
files = {entry["path"]: entry for entry in manifest.get("files", [])}
if set(files) != expected:
    raise SystemExit("error: rollback manifest file set is invalid")
for name in expected:
    path = root / name
    entry = files[name]
    digest = entry.get("integrity_digest")
    mode = oct(path.stat().st_mode & 0o777)[2:].zfill(4)
    if entry.get("mode") != mode or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
        raise SystemExit(f"error: rollback artifact validation failed: {name}")
PY

dest_binary="${HOME}/.local/libexec/agent-hub-approvals/agent-hub-server"
dest_state="${state_home}/agent-hub-approvals/server_data.json"
dest_environment="${config_home}/agent-hub/approvals-server.env"
dest_unit="${config_home}/systemd/user/agent-hub-approvals.service"

declare -a staged_files destinations
for pair in \
    "agent-hub-server:${dest_binary}" \
    "server_data.json:${dest_state}" \
    "approvals-server.env:${dest_environment}" \
    "agent-hub-approvals.service:${dest_unit}"; do
    IFS=: read -r source destination <<<"$pair"
    parent="${destination%/*}"
    [[ -d "$parent" && ! -L "$parent" ]] || {
        echo "error: rollback destination parent is unavailable: $parent" >&2
        exit 1
    }
    staged="$(mktemp "${parent}/.${source}.rollback.XXXXXX")"
    cp --preserve=mode -- "${slot}/${source}" "$staged"
    staged_files+=("$staged")
    destinations+=("$destination")
done

cleanup() {
    local staged
    for staged in "${staged_files[@]}"; do
        [[ ! -e "$staged" ]] || rm -f -- "$staged"
    done
}
trap cleanup EXIT
for index in "${!staged_files[@]}"; do
    mv -T -- "${staged_files[$index]}" "${destinations[$index]}"
done
trap - EXIT

echo "Restored approvals binary, state, environment, and unit from ${slot}"
echo "Service state was not changed; reload and start the unit manually."
