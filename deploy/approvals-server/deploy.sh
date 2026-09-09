#!/usr/bin/env bash
set -euo pipefail

mode=deploy
legacy_state=
if (($# == 1)) && [[ "$1" == "--dry-run" ]]; then
    mode=dry-run
elif (($# == 2)) && [[ "$1" == "--validate-legacy-state" ]]; then
    mode=validate-legacy-state
    legacy_state="$2"
elif (($# != 0)); then
    echo "usage: $0 [--dry-run | --validate-legacy-state PATH]" >&2
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

for command in cargo cmp git install mkdir mktemp mv rm sha256sum stat tar; do
    command -v "$command" >/dev/null || {
        echo "error: required command not found: $command" >&2
        exit 1
    }
done

if [[ "$mode" == validate-legacy-state ]]; then
    for command in chmod cp curl kill python3 sleep; do
        command -v "$command" >/dev/null || {
            echo "error: required validation command not found: $command" >&2
            exit 1
        }
    done
    if [[ ! -f "$legacy_state" || -L "$legacy_state" ]]; then
        echo "error: legacy state must be a regular, non-symlink file: $legacy_state" >&2
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

if ! worktree_is_clean; then
    echo "error: deployment requires a clean Git worktree (including staged and untracked files)" >&2
    exit 1
fi
captured_head="$(git -C "$repo_root" rev-parse --verify 'HEAD^{commit}')"

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
    echo "Would not touch service state or invoke systemd"
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

if [[ "$mode" == validate-legacy-state ]]; then
    validation_state="${workspace}/server_data.json"
    legacy_sha256="$(sha256 "$legacy_state")"
    cp -- "$legacy_state" "$validation_state"
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
    if [[ "$(sha256 "$legacy_state")" != "$legacy_sha256" ]]; then
        echo "error: source legacy state changed during validation" >&2
        exit 1
    fi
    echo "Validated copied legacy state load, mobile queue, API, SIGINT stop, and save"
    echo "Source legacy state was not modified: ${legacy_state}"
    exit 0
fi

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
