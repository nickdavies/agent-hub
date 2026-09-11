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

dest_binary="${HOME}/.local/libexec/agent-hub-approvals/agent-hub-server"
dest_state="${state_home}/agent-hub-approvals/server_data.json"
dest_environment="${config_home}/agent-hub/approvals-server.env"
dest_unit="${config_home}/systemd/user/agent-hub-approvals.service"

ROLLBACK_SLOT="$slot" LIVE_ENVIRONMENT="$dest_environment" python3 - <<'PY'
import hashlib
import json
import os
import socket
from pathlib import Path

root = Path(os.environ["ROLLBACK_SLOT"])
manifest = json.loads((root / "manifest.json").read_text())
if manifest.get("schema") != "agent-hub-approvals-rollback-v1":
    raise SystemExit("error: unsupported rollback manifest")
stopped_pid = manifest.get("stopped_pid")
if not isinstance(stopped_pid, int) or stopped_pid <= 0:
    raise SystemExit("error: rollback manifest has no valid stopped process identity")
try:
    os.kill(stopped_pid, 0)
except ProcessLookupError:
    pass
except PermissionError:
    raise SystemExit("error: expected stopped approvals process identity is running")
else:
    raise SystemExit("error: expected stopped approvals process identity is running")
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

environment = Path(os.environ["LIVE_ENVIRONMENT"])
if not environment.is_file() or environment.is_symlink():
    raise SystemExit(f"error: installed environment is unavailable: {environment}")
values = {}
for line in environment.read_text().splitlines():
    line = line.strip()
    if not line or line.startswith("#"):
        continue
    if "=" not in line:
        raise SystemExit("error: installed environment contains a malformed line")
    key, value = line.split("=", 1)
    values[key] = value
listen_addr = values.get("LISTEN_ADDR")
if not listen_addr or listen_addr.count(":") != 1:
    raise SystemExit("error: installed environment must contain an IPv4 LISTEN_ADDR")
host, port = listen_addr.rsplit(":", 1)
probe = socket.socket()
try:
    probe.bind((host, int(port)))
except OSError as error:
    raise SystemExit(f"error: approvals listener is not stopped at {listen_addr}: {error}")
finally:
    probe.close()
PY

if command -v systemctl >/dev/null; then
    unit_name=agent-hub-approvals.service
    if load_state="$(systemctl --user show "$unit_name" --property=LoadState --value 2>/dev/null)" && \
        [[ "$load_state" != not-found ]]; then
        active_state="$(systemctl --user show "$unit_name" --property=ActiveState --value)"
        sub_state="$(systemctl --user show "$unit_name" --property=SubState --value)"
        main_pid="$(systemctl --user show "$unit_name" --property=MainPID --value)"
        if [[ "$active_state" != inactive || "$sub_state" != dead || "$main_pid" != 0 ]]; then
            echo "error: approvals unit is not proven stopped (ActiveState=$active_state SubState=$sub_state MainPID=$main_pid)" >&2
            exit 1
        fi
    fi
fi

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
