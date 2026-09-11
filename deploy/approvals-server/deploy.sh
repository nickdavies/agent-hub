#!/usr/bin/env bash
set -euo pipefail

mode=
stopped_pid=
if (($# == 1)) && [[ "$1" == "--dry-run" ]]; then
    mode=dry-run
elif (($# == 2)) && [[ "$1" == "--capture-rollback" ]]; then
    mode=capture-rollback
    stopped_pid="$2"
elif (($# == 1)) && [[ "$1" == "--cutover" ]]; then
    mode=cutover
else
    echo "usage: $0 --dry-run | --capture-rollback STOPPED_PID | --cutover" >&2
    exit 2
fi

script_path="${BASH_SOURCE[0]}"
if [[ "$script_path" == */* ]]; then
    script_dir="${script_path%/*}"
else
    script_dir=.
fi
package_dir="$(CDPATH= builtin cd -- "$script_dir" && builtin pwd -P)"
dest_binary="${HOME}/.local/libexec/agent-hub-approvals/agent-hub-server"
config_home="${XDG_CONFIG_HOME:-${HOME}/.config}"
dest_environment="${config_home}/agent-hub/approvals-server.env"
dest_unit="${config_home}/systemd/user/agent-hub-approvals.service"
cache_home="${XDG_CACHE_HOME:-${HOME}/.cache}"
cache_root="${cache_home}/agent-hub-approvals"
state_home="${XDG_STATE_HOME:-${HOME}/.local/state}"
dest_state="${state_home}/agent-hub-approvals/server_data.json"
rollback_root="${cache_root}/rollback"
workspace=
staged_binary=
staged_environment=
staged_unit=
validation_pid=

cleanup() {
    local status=$?
    trap - EXIT
    [[ -z "$staged_binary" ]] || rm -f -- "$staged_binary" || status=1
    [[ -z "$staged_environment" ]] || rm -f -- "$staged_environment" || status=1
    [[ -z "$staged_unit" ]] || rm -f -- "$staged_unit" || status=1
    if [[ -n "$validation_pid" ]]; then
        kill -INT "$validation_pid" 2>/dev/null || true
        wait "$validation_pid" 2>/dev/null || true
    fi
    [[ -z "$workspace" ]] || rm -rf -- "$workspace" || status=1
    exit "$status"
}
trap cleanup EXIT

for command in cargo chmod cmp cp git install kill mkdir mktemp mv python3 rm sha256sum stat tar; do
    command -v "$command" >/dev/null || {
        echo "error: required command not found: $command" >&2
        exit 1
    }
done

if [[ "$mode" == capture-rollback ]]; then
    for command in chmod cp curl kill python3 sleep; do
        command -v "$command" >/dev/null || {
            echo "error: required validation command not found: $command" >&2
            exit 1
        }
    done
    if [[ ! "$stopped_pid" =~ ^[1-9][0-9]*$ ]]; then
        echo "error: STOPPED_PID must be a positive process ID" >&2
        exit 1
    fi
fi

repo_root="$(git -C "$package_dir" rev-parse --show-toplevel)" || {
    echo "error: deployment package is not in a Git worktree" >&2
    exit 1
}

worktree_is_clean() {
    [[ -z "$(git -C "$repo_root" status --porcelain --untracked-files=all)" ]]
}

validate_destination() {
    local label="$1"
    local path="$2"

    if [[ -L "$path" ]] || { [[ -e "$path" ]] && [[ ! -f "$path" ]]; }; then
        echo "error: existing $label must be a regular, non-symlink file: $path" >&2
        return 1
    fi
}

validate_destinations() {
    validate_destination binary "$dest_binary"
    validate_destination environment "$dest_environment"
    validate_destination unit "$dest_unit"
}

validate_directory() {
    local label="$1"
    local path="$2"

    if [[ -L "$path" ]] || { [[ -e "$path" ]] && [[ ! -d "$path" ]]; }; then
        echo "error: existing $label must be a directory, not a symlink: $path" >&2
        return 1
    fi
}

validate_effective_path() {
    local label="$1"
    local path="$2"
    local component
    local -a components

    [[ "$path" == /* ]] || {
        echo "error: $label must be an absolute path: $path" >&2
        return 1
    }
    IFS=/ read -r -a components <<<"${path#/}"
    for component in "${components[@]}"; do
        if [[ "$component" == . || "$component" == .. ]]; then
            echo "error: $label must not contain an explicit '$component' path component: $path" >&2
            return 1
        fi
    done
}

validate_trusted_cache_directory() {
    local label="$1"
    local path="$2"
    local require_invoking_owner="${3:-false}"
    local mode
    local owner

    validate_directory "$label" "$path"
    [[ -d "$path" && ! -L "$path" ]] || {
        echo "error: existing $label must be a directory, not a symlink: $path" >&2
        return 1
    }
    owner="$(stat -c %u -- "$path")"
    mode="$(stat -c %a -- "$path")"
    if [[ "$owner" != 0 && "$owner" != "$EUID" ]]; then
        echo "error: $label must be owned by root or the invoking EUID $EUID, not UID $owner: $path" >&2
        return 1
    fi
    if [[ "$require_invoking_owner" == true && "$owner" != "$EUID" ]]; then
        echo "error: $label must be owned by the invoking EUID $EUID, not UID $owner: $path" >&2
        return 1
    fi
    if (((8#$mode & 8#022) != 0)); then
        echo "error: $label must not be group- or other-writable (mode $mode): $path" >&2
        return 1
    fi
}

validate_cache_path() {
    local label="$1"
    local path="$2"
    local component
    local current=/
    local missing_tail=false
    local -a components

    validate_trusted_cache_directory "$label ancestor" "$current"
    IFS=/ read -r -a components <<<"${path#/}"
    for component in "${components[@]}"; do
        [[ -n "$component" ]] || continue
        current="${current%/}/${component}"
        if [[ -e "$current" || -L "$current" ]]; then
            if [[ "$missing_tail" == true ]]; then
                echo "error: existing $label component has a missing parent: $current" >&2
                return 1
            fi
            validate_trusted_cache_directory "$label ancestor" "$current"
        else
            missing_tail=true
        fi
    done

    if [[ -e "$path" || -L "$path" ]]; then
        validate_trusted_cache_directory "$label" "$path" true
    fi
}

ensure_trusted_cache_directory() {
    local label="$1"
    local path="$2"
    local component
    local current=/
    local -a components

    validate_cache_path "$label" "$path"
    IFS=/ read -r -a components <<<"${path#/}"
    for component in "${components[@]}"; do
        [[ -n "$component" ]] || continue
        current="${current%/}/${component}"
        if [[ ! -e "$current" && ! -L "$current" ]]; then
            mkdir -m 0700 -- "$current"
        fi
    done
    validate_cache_path "$label" "$path"
    validate_trusted_cache_directory "$label" "$path" true
}

parent_directory() {
    local path="$1"
    local parent="${path%/*}"

    printf '%s\n' "${parent:-/}"
}

ensure_directory() {
    local path="$1"
    local mode="$2"
    local current=/
    local component
    local -a components

    [[ "$path" == /* ]] || {
        echo "error: directory path must be absolute: $path" >&2
        return 1
    }
    IFS=/ read -r -a components <<<"${path#/}"
    for component in "${components[@]}"; do
        [[ -n "$component" && "$component" != . ]] || continue
        [[ "$component" != .. ]] || {
            echo "error: directory path must not contain '..': $path" >&2
            return 1
        }
        current="${current%/}/${component}"
        validate_directory directory "$current"
        if [[ ! -d "$current" ]]; then
            mkdir -m "$mode" -- "$current"
            validate_directory directory "$current"
        fi
    done
}

validate_environment() {
    validate_destination environment "$dest_environment"
    if [[ -e "$dest_environment" ]] && [[ "$(stat -c %a -- "$dest_environment")" != 600 ]]; then
        echo "error: existing environment must have mode 0600; run: chmod 600 $dest_environment" >&2
        return 1
    fi
}

sha256() {
    local output
    output="$(sha256sum -- "$1")"
    printf '%s\n' "${output%% *}"
}

validate_effective_path HOME "$HOME"
validate_effective_path XDG_CACHE_HOME "$cache_home"
validate_effective_path XDG_CONFIG_HOME "$config_home"
validate_effective_path XDG_STATE_HOME "$state_home"

if ! worktree_is_clean; then
    echo "error: deployment requires a clean Git worktree (including staged and untracked files)" >&2
    exit 1
fi
captured_head="$(git -C "$repo_root" rev-parse --verify 'HEAD^{commit}')"

if [[ "$mode" == cutover ]] && \
    { [[ ! -f "${rollback_root}/current" ]] || [[ -L "${rollback_root}/current" ]]; }; then
    echo "error: cutover requires a selected rollback bundle; run --capture-rollback first" >&2
    exit 1
fi

for packaged_file in approvals-server.env agent-hub-approvals.service; do
    if [[ ! -f "${package_dir}/${packaged_file}" || -L "${package_dir}/${packaged_file}" ]]; then
        echo "error: packaged file must be a regular, non-symlink file: ${package_dir}/${packaged_file}" >&2
        exit 1
    fi
done
validate_destinations
validate_environment
validate_cache_path cache "$cache_home"
validate_cache_path "deployment cache" "$cache_root"

echo "Deploying captured clean HEAD ${captured_head}"

if [[ "$mode" == dry-run ]]; then
    echo "Would export HEAD ${captured_head} to a private workspace under ${cache_root}"
    echo "Would run: CARGO_TARGET_DIR=<private>/target cargo build --locked --release -p agent-hub-approvals-server"
    echo "Would atomically install <private>/target/release/agent-hub-approvals-server as ${dest_binary} and verify its SHA-256"
    if [[ -e "$dest_environment" ]]; then
        echo "Would preserve the existing mode-0600 environment unchanged: ${dest_environment}"
    else
        echo "Would install the captured approvals-server.env as ${dest_environment} with mode 0600"
    fi
    echo "Would atomically install the captured agent-hub-approvals.service as ${dest_unit} with mode 0644"
    echo "Would require --capture-rollback STOPPED_PID after the old service's SIGINT stop/save"
    echo "Would require --cutover to match that rollback bundle to HEAD and every live file"
    echo "Would not modify service state or invoke systemd"
    exit 0
fi

ensure_trusted_cache_directory cache "$cache_home"
ensure_trusted_cache_directory "deployment cache" "$cache_root"
workspace="$(mktemp -d "${cache_root}/deploy.XXXXXX")"
[[ ! -L "$workspace" && -d "$workspace" && "$(stat -c %a -- "$workspace")" == 700 ]] || {
    echo "error: private deployment workspace must be a mode-0700 directory: $workspace" >&2
    exit 1
}
target_dir="${workspace}/target"
source_binary="${target_dir}/release/agent-hub-approvals-server"
git -C "$repo_root" archive "$captured_head" | tar -x -C "$workspace"
snapshot_package_dir="${workspace}/deploy/approvals-server"

for packaged_file in approvals-server.env agent-hub-approvals.service; do
    if [[ ! -f "${snapshot_package_dir}/${packaged_file}" || -L "${snapshot_package_dir}/${packaged_file}" ]]; then
        echo "error: captured packaged file must be a regular, non-symlink file: ${snapshot_package_dir}/${packaged_file}" >&2
        exit 1
    fi
done

(
    cd "$workspace"
    CARGO_TARGET_DIR="$target_dir" cargo build --locked --release -p agent-hub-approvals-server
)

if [[ ! -f "$source_binary" || -L "$source_binary" ]]; then
    echo "error: build output must be a regular, non-symlink file: $source_binary" >&2
    exit 1
fi

current_head="$(git -C "$repo_root" rev-parse --verify 'HEAD^{commit}')"
if [[ "$current_head" != "$captured_head" ]] || ! worktree_is_clean; then
    echo "error: original HEAD or worktree changed while building; refusing to continue" >&2
    exit 1
fi

if [[ "$mode" == capture-rollback ]]; then
    if kill -0 "$stopped_pid" 2>/dev/null; then
        echo "error: old approvals process $stopped_pid is still running" >&2
        exit 1
    fi
    for live_file in "$dest_binary" "$dest_state" "$dest_environment" "$dest_unit"; do
        if [[ ! -f "$live_file" || -L "$live_file" ]]; then
            echo "error: rollback capture requires a live regular, non-symlink file: $live_file" >&2
            exit 1
        fi
    done
    INSTALLED_ENVIRONMENT="$dest_environment" python3 - <<'PY'
import os
import socket
from pathlib import Path

values = {}
for line in Path(os.environ["INSTALLED_ENVIRONMENT"]).read_text().splitlines():
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
    validation_state="${workspace}/server_data.json"
    legacy_sha256="$(sha256 "$dest_state")"
    cp -- "$dest_state" "$validation_state"
    chmod 0600 "$validation_state"
    port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
    AUTH_MODE=none APPROVAL_MODE=readwrite BASE_URL="http://127.0.0.1:${port}" \
        LISTEN_ADDR="127.0.0.1:${port}" \
        "$source_binary" serve noop local-file --path "$validation_state" \
        >"${workspace}/validation.log" 2>&1 &
    validation_pid=$!
    ready=false
    for _ in {1..100}; do
        if curl --silent --fail "http://127.0.0.1:${port}/health" >/dev/null; then
            ready=true
            break
        fi
        if ! kill -0 "$validation_pid" 2>/dev/null; then
            echo "error: copied legacy state failed to load; see ${workspace}/validation.log" >&2
            exit 1
        fi
        sleep 0.05
    done
    if [[ "$ready" != true ]]; then
        echo "error: copied legacy state did not become healthy" >&2
        exit 1
    fi
    curl --silent --fail "http://127.0.0.1:${port}/approvals/queue" >/dev/null
    curl --silent --fail "http://127.0.0.1:${port}/api/v1/sessions" >/dev/null
    kill -INT "$validation_pid"
    wait "$validation_pid"
    validation_pid=
    if [[ ! -f "$validation_state" || -L "$validation_state" ]]; then
        echo "error: validation server did not save copied state as a regular file" >&2
        exit 1
    fi
    if [[ "$(sha256 "$dest_state")" != "$legacy_sha256" ]]; then
        echo "error: final live state changed during validation" >&2
        exit 1
    fi
    ensure_trusted_cache_directory "rollback cache" "$rollback_root"
    rollback_slot="$(mktemp -d "${rollback_root}/slot.XXXXXX")"
    [[ "$(stat -c %a -- "$rollback_slot")" == 700 ]] || {
        echo "error: rollback slot must have mode 0700" >&2
        exit 1
    }
    cp --preserve=mode -- "$dest_binary" "${rollback_slot}/agent-hub-server"
    cp --preserve=mode -- "$dest_state" "${rollback_slot}/server_data.json"
    cp --preserve=mode -- "$dest_environment" "${rollback_slot}/approvals-server.env"
    cp --preserve=mode -- "$dest_unit" "${rollback_slot}/agent-hub-approvals.service"
    cmp -s -- "$dest_binary" "${rollback_slot}/agent-hub-server"
    cmp -s -- "$dest_state" "${rollback_slot}/server_data.json"
    cmp -s -- "$dest_environment" "${rollback_slot}/approvals-server.env"
    cmp -s -- "$dest_unit" "${rollback_slot}/agent-hub-approvals.service"
    ROLLBACK_SLOT="$rollback_slot" TARGET_COMMIT="$captured_head" STOPPED_PID="$stopped_pid" \
        python3 - <<'PY' >"${rollback_slot}/manifest.json"
import hashlib
import json
import os
from pathlib import Path

root = Path(os.environ["ROLLBACK_SLOT"])
files = []
for path in sorted(root.iterdir()):
    if path.name == "manifest.json":
        continue
    files.append({
        "path": path.name,
        "integrity_digest": hashlib.sha256(path.read_bytes()).hexdigest(),
        "mode": oct(path.stat().st_mode & 0o777)[2:].zfill(4),
    })
print(json.dumps({
    "schema": "agent-hub-approvals-rollback-v1",
    "target_commit": os.environ["TARGET_COMMIT"],
    "stopped_pid": int(os.environ["STOPPED_PID"]),
    "files": files,
}, indent=2, sort_keys=True))
PY
    chmod 0600 "${rollback_slot}/manifest.json"
    rollback_pointer_tmp="$(mktemp "${rollback_root}/.current.XXXXXX")"
    printf '%s\n' "${rollback_slot##*/}" >"$rollback_pointer_tmp"
    chmod 0600 "$rollback_pointer_tmp"
    mv -T -- "$rollback_pointer_tmp" "${rollback_root}/current"
    echo "Confirmed stopped process and validated final SIGINT-saved state"
    echo "Captured rollback slot for ${captured_head}: ${rollback_slot}"
    exit 0
fi

pointer="${rollback_root}/current"
[[ -f "$pointer" && ! -L "$pointer" ]] || {
    echo "error: cutover requires a selected rollback bundle; run --capture-rollback first" >&2
    exit 1
}
slot_name="$(<"$pointer")"
[[ "$slot_name" == slot.* && "$slot_name" != */* ]] || {
    echo "error: invalid rollback slot pointer" >&2
    exit 1
}
rollback_slot="${rollback_root}/${slot_name}"
[[ -d "$rollback_slot" && ! -L "$rollback_slot" && \
    "$(stat -c %u -- "$rollback_slot")" == "$EUID" && \
    "$(stat -c %a -- "$rollback_slot")" == 700 ]] || {
    echo "error: selected rollback bundle is not an owner-private directory" >&2
    exit 1
}
for artifact in manifest.json agent-hub-server server_data.json approvals-server.env \
    agent-hub-approvals.service; do
    [[ -f "${rollback_slot}/${artifact}" && ! -L "${rollback_slot}/${artifact}" ]] || {
        echo "error: rollback bundle artifact must be a regular, non-symlink file: ${rollback_slot}/${artifact}" >&2
        exit 1
    }
done
ROLLBACK_SLOT="$rollback_slot" TARGET_COMMIT="$captured_head" \
    LIVE_BINARY="$dest_binary" LIVE_STATE="$dest_state" \
    LIVE_ENVIRONMENT="$dest_environment" LIVE_UNIT="$dest_unit" python3 - <<'PY'
import hashlib
import json
import os
import socket
from pathlib import Path

root = Path(os.environ["ROLLBACK_SLOT"])
manifest = json.loads((root / "manifest.json").read_text())
if manifest.get("schema") != "agent-hub-approvals-rollback-v1":
    raise SystemExit("error: unsupported rollback manifest")
if manifest.get("target_commit") != os.environ["TARGET_COMMIT"]:
    raise SystemExit("error: rollback bundle is stale for the current deployment commit")
stopped_pid = manifest.get("stopped_pid")
if not isinstance(stopped_pid, int) or stopped_pid <= 0:
    raise SystemExit("error: rollback bundle has no valid stopped process identity")
try:
    os.kill(stopped_pid, 0)
except ProcessLookupError:
    pass
except PermissionError:
    raise SystemExit("error: captured old approvals process identity is running")
else:
    raise SystemExit("error: captured old approvals process identity is running")
live = {
    "agent-hub-server": Path(os.environ["LIVE_BINARY"]),
    "server_data.json": Path(os.environ["LIVE_STATE"]),
    "approvals-server.env": Path(os.environ["LIVE_ENVIRONMENT"]),
    "agent-hub-approvals.service": Path(os.environ["LIVE_UNIT"]),
}
files = {entry["path"]: entry for entry in manifest.get("files", [])}
if set(files) != set(live):
    raise SystemExit("error: rollback manifest file set is invalid")
for name, live_path in live.items():
    captured = root / name
    entry = files[name]
    digest = hashlib.sha256(captured.read_bytes()).hexdigest()
    mode = oct(captured.stat().st_mode & 0o777)[2:].zfill(4)
    if entry.get("integrity_digest") != digest or entry.get("mode") != mode:
        raise SystemExit(f"error: rollback bundle failed integrity validation: {name}")
    if not live_path.is_file() or live_path.is_symlink():
        raise SystemExit(f"error: live rollback source is unavailable: {live_path}")
    if live_path.read_bytes() != captured.read_bytes():
        raise SystemExit(f"error: rollback bundle is stale for live file: {live_path}")
    live_mode = oct(live_path.stat().st_mode & 0o777)[2:].zfill(4)
    if live_mode != mode:
        raise SystemExit(f"error: rollback bundle mode is stale for live file: {live_path}")
values = {}
for line in live["approvals-server.env"].read_text().splitlines():
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
echo "Validated rollback bundle for cutover: ${rollback_slot}"

validate_destinations
validate_environment
environment_existed=false
if [[ -e "$dest_environment" ]]; then
    environment_existed=true
fi

binary_parent="$(parent_directory "$dest_binary")"
environment_parent="$(parent_directory "$dest_environment")"
unit_parent="$(parent_directory "$dest_unit")"
ensure_directory "$binary_parent" 0755
ensure_directory "$environment_parent" 0755
ensure_directory "$unit_parent" 0755

staged_binary="$(mktemp "${binary_parent}/.agent-hub-server.XXXXXX")"
install -m 0755 -- "$source_binary" "$staged_binary"
source_sha256="$(sha256 "$source_binary")"
if [[ "$(sha256 "$staged_binary")" != "$source_sha256" ]] || \
    [[ "$(stat -c %a -- "$staged_binary")" != 755 ]]; then
    echo "error: staged binary failed SHA-256 or mode verification" >&2
    exit 1
fi

if [[ "$environment_existed" == false ]]; then
    staged_environment="$(mktemp "${environment_parent}/.approvals-server.env.XXXXXX")"
    install -m 0600 -- "${snapshot_package_dir}/approvals-server.env" "$staged_environment"
    if ! cmp -s -- "${snapshot_package_dir}/approvals-server.env" "$staged_environment" || \
        [[ "$(stat -c %a -- "$staged_environment")" != 600 ]]; then
        echo "error: staged environment failed byte or mode verification" >&2
        exit 1
    fi
fi

staged_unit="$(mktemp "${unit_parent}/.agent-hub-approvals.service.XXXXXX")"
install -m 0644 -- "${snapshot_package_dir}/agent-hub-approvals.service" "$staged_unit"
if ! cmp -s -- "${snapshot_package_dir}/agent-hub-approvals.service" "$staged_unit" || \
    [[ "$(stat -c %a -- "$staged_unit")" != 644 ]]; then
    echo "error: staged unit failed byte or mode verification" >&2
    exit 1
fi

validate_destinations
validate_environment
for pair in \
    "agent-hub-server:${dest_binary}" \
    "server_data.json:${dest_state}" \
    "approvals-server.env:${dest_environment}" \
    "agent-hub-approvals.service:${dest_unit}"; do
    IFS=: read -r captured_name live_file <<<"$pair"
    if ! cmp -s -- "${rollback_slot}/${captured_name}" "$live_file" || \
        [[ "$(stat -c %a -- "${rollback_slot}/${captured_name}")" != \
        "$(stat -c %a -- "$live_file")" ]]; then
        echo "error: rollback bundle became stale before installation: $live_file" >&2
        exit 1
    fi
done
if [[ "$environment_existed" == true ]]; then
    if [[ ! -e "$dest_environment" ]]; then
        echo "error: existing environment disappeared before commit: $dest_environment" >&2
        exit 1
    fi
else
    if [[ -e "$dest_environment" || -L "$dest_environment" ]]; then
        echo "error: environment appeared before commit: $dest_environment" >&2
        exit 1
    fi
fi

if [[ "$environment_existed" == true ]]; then
    echo "Preserved existing environment: ${dest_environment}"
else
    validate_destination environment "$dest_environment"
    environment_move_failed=false
    if ! mv -T -n -- "$staged_environment" "$dest_environment"; then
        environment_move_failed=true
    fi
    if [[ -e "$staged_environment" ]]; then
        if [[ -e "$dest_environment" || -L "$dest_environment" ]]; then
            echo "error: environment appeared before commit: $dest_environment" >&2
        else
            echo "error: failed to install environment: $dest_environment" >&2
        fi
        exit 1
    fi
    if [[ "$environment_move_failed" == true ]]; then
        echo "error: failed to install environment: $dest_environment" >&2
        exit 1
    fi
    staged_environment=
    echo "Installed packaged environment: ${dest_environment}"
fi
if [[ "$(stat -c %a -- "$dest_environment")" != 600 ]]; then
    echo "error: deployed environment has an unexpected mode" >&2
    exit 1
fi

validate_destination unit "$dest_unit"
mv -T -- "$staged_unit" "$dest_unit"
staged_unit=
if ! cmp -s -- "${snapshot_package_dir}/agent-hub-approvals.service" "$dest_unit" || \
    [[ "$(stat -c %a -- "$dest_unit")" != 644 ]]; then
    echo "error: deployed unit failed byte or mode verification" >&2
    exit 1
fi

validate_destination binary "$dest_binary"
mv -T -- "$staged_binary" "$dest_binary"
staged_binary=
if [[ "$(sha256 "$dest_binary")" != "$source_sha256" ]] || \
    [[ "$(stat -c %a -- "$dest_binary")" != 755 ]]; then
    echo "error: deployed binary failed SHA-256 or mode verification" >&2
    exit 1
fi

echo "Installed binary: ${dest_binary}"
echo "Installed unit: ${dest_unit}"
echo "Service state was not touched; activate the deployment manually."
