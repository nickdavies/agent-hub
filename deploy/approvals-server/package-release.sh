#!/usr/bin/env bash
set -euo pipefail

if (($# != 2)); then
    echo "usage: $0 TARGET_DIR OUTPUT_DIR" >&2
    exit 2
fi

target_dir="$1"
output_dir="$2"
script_dir="$(CDPATH= builtin cd -- "${BASH_SOURCE[0]%/*}" && builtin pwd -P)"
repo_root="$(git -C "$script_dir" rev-parse --show-toplevel)"
commit="$(git -C "$repo_root" rev-parse --verify 'HEAD^{commit}')"
if [[ -n "$(git -C "$repo_root" status --porcelain --untracked-files=all)" ]]; then
    echo "error: release packaging requires a clean Git worktree" >&2
    exit 1
fi

for command in git gzip install mkdir mktemp mv python3 rm sha256sum tar; do
    command -v "$command" >/dev/null || {
        echo "error: required command not found: $command" >&2
        exit 1
    }
done

for binary in agent-hub-approvals-server agent-hub-gateway; do
    if [[ ! -f "${target_dir}/${binary}" || -L "${target_dir}/${binary}" ]]; then
        echo "error: release binary must be a regular, non-symlink file: ${target_dir}/${binary}" >&2
        exit 1
    fi
done

mkdir -p -- "$output_dir"
workspace="$(mktemp -d "${output_dir}/.package.XXXXXX")"
trap 'rm -rf -- "$workspace"' EXIT

mkdir -- "${workspace}/approvals-server" "${workspace}/gateway"
install -m 0755 -- "${target_dir}/agent-hub-approvals-server" \
    "${workspace}/approvals-server/agent-hub-approvals-server"
install -m 0600 -- "${script_dir}/approvals-server.env" \
    "${workspace}/approvals-server/approvals-server.env"
install -m 0644 -- "${script_dir}/agent-hub-approvals.service" \
    "${workspace}/approvals-server/agent-hub-approvals.service"
install -m 0755 -- "${target_dir}/agent-hub-gateway" \
    "${workspace}/gateway/agent-hub-gateway"

for package in approvals-server gateway; do
    manifest="${workspace}/${package}/release-manifest.json"
    PACKAGE="$package" COMMIT="$commit" PACKAGE_ROOT="${workspace}/${package}" \
        python3 - <<'PY' >"$manifest"
import hashlib
import json
import os
from pathlib import Path

root = Path(os.environ["PACKAGE_ROOT"])
artifacts = []
for path in sorted(root.iterdir()):
    if path.name == "release-manifest.json":
        continue
    artifacts.append({
        "path": path.name,
        "integrity_digest": hashlib.sha256(path.read_bytes()).hexdigest(),
    })
print(json.dumps({
    "schema": "agent-hub-release-manifest-v1",
    "package": os.environ["PACKAGE"],
    "commit": os.environ["COMMIT"],
    "artifacts": artifacts,
}, indent=2, sort_keys=True))
PY
    tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
        -C "$workspace" -cf - "$package" | gzip -n >"${workspace}/${package}.tar.gz"
done

for package in approvals-server gateway; do
    mv -T -- "${workspace}/${package}/release-manifest.json" \
        "${workspace}/${package}-release-manifest.json"
done

(
    cd "$workspace"
    sha256sum -- approvals-server.tar.gz gateway.tar.gz \
        approvals-server-release-manifest.json gateway-release-manifest.json >SHA256SUMS
)

for artifact in approvals-server.tar.gz gateway.tar.gz SHA256SUMS \
    approvals-server-release-manifest.json gateway-release-manifest.json; do
    mv -T -- "${workspace}/${artifact}" "${output_dir}/${artifact}"
done

echo "Packaged approvals-server and gateway release artifacts for ${commit}"
