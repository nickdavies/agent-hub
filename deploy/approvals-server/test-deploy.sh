#!/usr/bin/env bash
set -euo pipefail

for command in git mktemp python3 tar timeout; do
    command -v "$command" >/dev/null || {
        echo "error: required harness command not found: $command" >&2
        exit 1
    }
done
real_home="$HOME"
real_cargo_home="${CARGO_HOME:-${HOME}/.cargo}"
real_rustup_home="${RUSTUP_HOME:-${HOME}/.rustup}"
real_cargo="$(command -v cargo)"

repo_root="$(git -C "${BASH_SOURCE[0]%/*}" rev-parse --show-toplevel)"
mkdir -p -- "${repo_root}/target"
test_root="${repo_root}/target/deploy-approvals-server-test"
sandbox="$(mktemp -d "${test_root}.XXXXXX")"
trap 'rm -rf -- "$sandbox"' EXIT

source_repo="${sandbox}/source"
home="${sandbox}/home"
config_home="${sandbox}/config"
cache_home="${sandbox}/cache"
bin_dir="${sandbox}/bin"
mkdir -m 0700 -- "$source_repo" "$home" "$config_home" "$cache_home" "$bin_dir"
git -C "$repo_root" ls-files --cached --others --exclude-standard -z | \
    tar --null -C "$repo_root" -T - -cf - | tar -x -C "$source_repo"
git -C "$source_repo" init -q
git -C "$source_repo" add .
git -C "$source_repo" -c user.name=deploy-test -c user.email=deploy-test.invalid commit -qm snapshot

fake_server="${sandbox}/fake-server"
cat >"$fake_server" <<'PYTHON'
#!/usr/bin/env python3
import json
import os
import signal
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

state_path = sys.argv[sys.argv.index("--path") + 1]
with open(state_path, encoding="utf-8") as state_file:
    state = json.load(state_file)
host, port = os.environ["LISTEN_ADDR"].rsplit(":", 1)

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path not in ("/health", "/approvals/queue", "/api/v1/sessions"):
            self.send_error(404)
            return
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, _format, *_args):
        pass

server = HTTPServer((host, int(port)), Handler)

def stop(_signal, _frame):
    raise SystemExit(0)

signal.signal(signal.SIGINT, stop)
try:
    server.serve_forever()
finally:
    server.server_close()
    with open(state_path, "w", encoding="utf-8") as state_file:
        json.dump(state, state_file)
PYTHON
chmod 0755 "$fake_server"

cat >"${bin_dir}/cargo" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ "$1" == build ]]
[[ "$PWD" == "${XDG_CACHE_HOME}/agent-hub-approvals/"deploy.* ]]
[[ "$(sha256sum deploy/approvals-server/agent-hub-approvals.service | cut -d' ' -f1)" == "$EXPECTED_UNIT_SHA256" ]]
printf '%s\n' "$PWD" >>"$BUILD_LOG"
mkdir -p -- "${CARGO_TARGET_DIR}/release"
install -m 0755 -- "$HARNESS_FAKE_SERVER" "${CARGO_TARGET_DIR}/release/agent-hub-approvals-server"
SH
chmod 0755 "${bin_dir}/cargo"

cat >"${bin_dir}/mv" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$MV_LOG"
exec /usr/bin/mv "$@"
SH
chmod 0755 "${bin_dir}/mv"

deploy="${source_repo}/deploy/approvals-server/deploy.sh"
build_log="${sandbox}/build.log"
mv_log="${sandbox}/mv.log"
unit_sha256="$(sha256sum "${source_repo}/deploy/approvals-server/agent-hub-approvals.service" | cut -d' ' -f1)"
export HOME="$home"
export XDG_CONFIG_HOME="$config_home"
export XDG_CACHE_HOME="$cache_home"
export PATH="${bin_dir}:${PATH}"
export HARNESS_FAKE_SERVER="$fake_server"
export EXPECTED_UNIT_SHA256="$unit_sha256"
export BUILD_LOG="$build_log"
export MV_LOG="$mv_log"

release_output="${sandbox}/release-output"
release_binaries="${sandbox}/release-binaries"
mkdir -- "$release_binaries"
install -m 0755 -- "$fake_server" "${release_binaries}/agent-hub-approvals-server"
install -m 0755 -- "$fake_server" "${release_binaries}/agent-hub-gateway"
"${source_repo}/deploy/approvals-server/package-release.sh" \
    "$release_binaries" "$release_output"
(
    cd "$release_output"
    sha256sum --check SHA256SUMS
)
grep -q '"integrity_digest"' "${release_output}/approvals-server-release-manifest.json"
grep -q '"integrity_digest"' "${release_output}/gateway-release-manifest.json"

touch "${source_repo}/untracked"
if "$deploy" --dry-run >"${sandbox}/dirty.out" 2>"${sandbox}/dirty.err"; then
    echo "error: dirty source unexpectedly passed deployment validation" >&2
    exit 1
fi
grep -q "clean Git worktree" "${sandbox}/dirty.err"
rm -- "${source_repo}/untracked"

dry_run="$($deploy --dry-run)"
grep -q "Would export HEAD" <<<"$dry_run"
grep -q "Would not modify service state or invoke systemd" <<<"$dry_run"
[[ ! -e "${home}/.local" ]]
[[ ! -e "${config_home}/agent-hub" ]]

dest_binary="${home}/.local/libexec/agent-hub-approvals/agent-hub-server"
mkdir -p -- "${dest_binary%/*}"
ln -s -- "$fake_server" "$dest_binary"
if "$deploy" --capture-rollback 999999 >"${sandbox}/symlink.out" 2>"${sandbox}/symlink.err"; then
    echo "error: symlink destination unexpectedly passed validation" >&2
    exit 1
fi
grep -q "existing binary must be a regular, non-symlink file" "${sandbox}/symlink.err"
rm -- "$dest_binary"

dest_environment="${config_home}/agent-hub/approvals-server.env"
dest_unit="${config_home}/systemd/user/agent-hub-approvals.service"
dest_state="${home}/.local/state/agent-hub-approvals/server_data.json"
mkdir -p -- "${dest_environment%/*}" "${dest_unit%/*}" "${dest_state%/*}"
install -m 0755 -- "$fake_server" "$dest_binary"
install -m 0600 -- "${source_repo}/deploy/approvals-server/approvals-server.env" "$dest_environment"
install -m 0644 -- "${source_repo}/deploy/approvals-server/agent-hub-approvals.service" "$dest_unit"
listen_port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
INSTALLED_ENVIRONMENT="$dest_environment" LISTEN_PORT="$listen_port" python3 - <<'PY'
import os
from pathlib import Path

path = Path(os.environ["INSTALLED_ENVIRONMENT"])
lines = path.read_text().splitlines()
path.write_text("\n".join(
    f"LISTEN_ADDR=127.0.0.1:{os.environ['LISTEN_PORT']}" if line.startswith("LISTEN_ADDR=") else line
    for line in lines
) + "\n")
PY
cp -- "$dest_environment" "${sandbox}/expected-installed.env"
printf '%s\n' '{"sessions":{},"notify_config":null,"presence":null,"pending_approvals":[]}' >"$dest_state"
chmod 0600 "$dest_state"
if "$deploy" --cutover >"${sandbox}/missing.out" 2>"${sandbox}/missing.err"; then
    echo "error: cutover without rollback capture unexpectedly succeeded" >&2
    exit 1
fi
grep -q "selected rollback bundle" "${sandbox}/missing.err"

LISTEN_ADDR="127.0.0.1:${listen_port}" "$dest_binary" serve noop local-file --path "$dest_state" \
    >"${sandbox}/old-server.log" 2>&1 &
old_pid=$!
for _ in {1..100}; do
    if LISTEN_PORT="$listen_port" python3 -c 'import os, socket; s=socket.create_connection(("127.0.0.1", int(os.environ["LISTEN_PORT"])), .1); s.close()' 2>/dev/null; then
        break
    fi
    sleep 0.05
done
kill -INT "$old_pid"
wait "$old_pid"
set +e
timeout --signal=TERM --kill-after=2s 20s \
    "$deploy" --capture-rollback "$old_pid" >"${sandbox}/capture.out"
capture_status=$?
set -e
if ((capture_status != 0)); then
    echo "error: rollback capture failed or exceeded its deadline (status $capture_status)" >&2
    exit 1
fi
grep -q "Confirmed stopped process" "${sandbox}/capture.out"

rollback_root="${cache_home}/agent-hub-approvals/rollback"
rollback_slot="${rollback_root}/$(<"${rollback_root}/current")"
final_state_sha256="$(sha256sum "$dest_state" | cut -d' ' -f1)"
printf '%s\n' stale >"$dest_state"
if "$deploy" --cutover >"${sandbox}/stale.out" 2>"${sandbox}/stale.err"; then
    echo "error: cutover with a stale rollback capture unexpectedly succeeded" >&2
    exit 1
fi
grep -q "rollback bundle is stale" "${sandbox}/stale.err"
cp -- "${rollback_slot}/server_data.json" "$dest_state"
chmod 0600 "$dest_state"

"$deploy" --cutover >"${sandbox}/deploy.out"
[[ "$(sha256sum "$dest_binary" | cut -d' ' -f1)" == "$(sha256sum "$fake_server" | cut -d' ' -f1)" ]]
cmp -s -- "${sandbox}/expected-installed.env" "$dest_environment"
cmp -s -- "${source_repo}/deploy/approvals-server/agent-hub-approvals.service" "$dest_unit"
[[ "$(stat -c %a -- "$dest_binary")" == 755 ]]
[[ "$(stat -c %a -- "$dest_environment")" == 600 ]]
[[ "$(stat -c %a -- "$dest_unit")" == 644 ]]
grep -q '/\.agent-hub-server\.' "$mv_log"
grep -q '/\.agent-hub-approvals.service\.' "$mv_log"
while IFS= read -r build_dir; do
    [[ "$build_dir" == "${cache_home}/agent-hub-approvals/"deploy.* ]]
    [[ "$build_dir" != "$source_repo" ]]
done <"$build_log"
[[ "$(wc -l <"$build_log")" == 3 ]]
[[ "$(stat -c %a -- "$rollback_slot")" == 700 ]]
grep -q '"integrity_digest"' "${rollback_slot}/manifest.json"

printf '%s\n' replaced >"$dest_state"
"${source_repo}/deploy/approvals-server/rollback.sh" >"${sandbox}/rollback.out"
[[ "$(sha256sum "$dest_state" | cut -d' ' -f1)" == "$final_state_sha256" ]]

HOME="$real_home" CARGO_HOME="$real_cargo_home" RUSTUP_HOME="$real_rustup_home" \
    PATH="${PATH#*:}" "$real_cargo" test --locked \
        --manifest-path "${repo_root}/Cargo.toml" \
        -p agent-hub-approvals-server --test crash_durability
HOME="$real_home" CARGO_HOME="$real_cargo_home" RUSTUP_HOME="$real_rustup_home" \
    PATH="${PATH#*:}" "$real_cargo" test --locked \
        --manifest-path "${repo_root}/Cargo.toml" \
        -p agent-hub-gateway --test real_service_compat -- --ignored

printf '%s\n' "deploy.sh harness passed"
