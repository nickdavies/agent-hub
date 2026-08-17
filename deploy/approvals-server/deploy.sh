#!/usr/bin/env bash
set -euo pipefail

if (($# > 1)) || (($# == 1)) && [[ "$1" != "--dry-run" ]]; then
    echo "usage: $0 [--dry-run]" >&2
    exit 2
fi

readonly pinned_sha256="9a8adfb930d288b48d96abeddf52ac000ed8f1e8f361cf494c1d6559003a913c"
source_binary="${HOME}/.bin/agent-hub-server"
dest_binary="${HOME}/.local/libexec/agent-hub-approvals/agent-hub-server"
config_home="${XDG_CONFIG_HOME:-${HOME}/.config}"
package_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
dry_run=()
[[ "${1:-}" == "--dry-run" ]] && dry_run=(--dry-run)

if [[ ! -f "$source_binary" || -L "$source_binary" ]]; then
    echo "error: source binary must be a regular, non-symlink file: $source_binary" >&2
    exit 1
fi

actual_sha256="$(sha256sum -- "$source_binary" | cut -d ' ' -f 1)"
if [[ "$actual_sha256" != "$pinned_sha256" ]]; then
    echo "error: unexpected SHA-256 for $source_binary" >&2
    exit 1
fi

rsync "${dry_run[@]}" --archive --checksum --copy-links --itemize-changes --mkpath \
    --chmod=F755 -- "$source_binary" "$dest_binary"

if ((${#dry_run[@]} == 0)); then
    [[ "$(sha256sum -- "$dest_binary" | cut -d ' ' -f 1)" == "$pinned_sha256" ]] || {
        echo "error: deployed binary failed SHA-256 verification" >&2
        exit 1
    }
fi

rsync "${dry_run[@]}" --archive --checksum --copy-links --itemize-changes --mkpath --chmod=F600 -- \
    "${package_dir}/approvals-server.env" "${config_home}/agent-hub/approvals-server.env"
rsync "${dry_run[@]}" --archive --checksum --copy-links --itemize-changes --mkpath --chmod=F644 -- \
    "${package_dir}/agent-hub-approvals.service" \
    "${config_home}/systemd/user/agent-hub-approvals.service"
