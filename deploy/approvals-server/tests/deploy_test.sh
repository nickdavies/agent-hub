#!/usr/bin/env bash
set -euo pipefail

tests_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
package_dir="$(dirname -- "$tests_dir")"
repo_root="$(git -C "$package_dir" rev-parse --show-toplevel)"
test_parent="$repo_root/target"
mkdir -p -- "$test_parent"
test_root="$(mktemp -d "$test_parent/approvals-server-deploy-tests.XXXXXX")"
real_git="$(command -v git)"
real_install="$(command -v install)"
real_mktemp="$(command -v mktemp)"
real_mv="$(command -v mv)"
real_stat="$(command -v stat)"

passed=0
failed=0

cleanup() {
    rm -rf -- "$test_root"
}
trap cleanup EXIT

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

assert_eq() {
    local expected="$1"
    local actual="$2"
    local message="$3"
    [[ "$actual" == "$expected" ]] || fail "$message (expected '$expected', got '$actual')"
}

assert_file_contains() {
    local path="$1"
    local pattern="$2"
    local message="$3"
    grep -Eq -- "$pattern" "$path" || fail "$message ($path did not match /$pattern/)"
}

assert_output_contains() {
    local pattern="$1"
    local message="$2"
    grep -Eqi -- "$pattern" <<<"$deploy_output" || fail "$message (output: $deploy_output)"
}

write_fakes() {
    local fake_bin="$1"

    mkdir -p -- "$fake_bin"
    cat >"$fake_bin/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s|%s\n' "$PWD" "$*" >>"$FIXTURE_ROOT/cargo.log"
[[ "$*" == "build --locked --release -p agent-hub-server" ]] || {
    printf 'unexpected cargo arguments: %s\n' "$*" >&2
    exit 91
}
[[ "${FAKE_CARGO_FAIL:-0}" != 1 ]] || exit 92
if [[ -n "${FAKE_CARGO_MUTATE_PATH:-}" ]]; then
    printf 'mutated-during-build\n' >"$FIXTURE_REPO/$FAKE_CARGO_MUTATE_PATH"
fi
printf '%s|%s|%s\n' "$PWD" "$CARGO_TARGET_DIR" "$(<server-version.txt)" \
    >>"$FIXTURE_ROOT/cargo-source.log"
printf '%s\n' "$(stat -c %a "$PWD")" >"$FIXTURE_ROOT/workspace-mode.log"
[[ -f Cargo.toml ]] || {
    printf 'snapshot is missing Cargo.toml\n' >&2
    exit 94
}
[[ ! -e .git ]] || {
    printf 'snapshot unexpectedly contains .git\n' >&2
    exit 95
}
target_dir="${CARGO_TARGET_DIR:-$PWD/target}"
mkdir -p -- "$target_dir/release"
printf 'binary:%s\n' "$(<server-version.txt)" >"$target_dir/release/agent-hub-server"
chmod 0755 "$target_dir/release/agent-hub-server"
if [[ -n "${FAKE_POST_CARGO_INJECT_PATH:-}" ]]; then
    : >"$FIXTURE_ROOT/post-cargo-hook-ready"
fi
EOF

    cat >"$fake_bin/install" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$FIXTURE_ROOT/install.log"
for arg in "$@"; do
    if [[ -n "${FAKE_INSTALL_FAIL_BASENAME:-}" && "$arg" == */"$FAKE_INSTALL_FAIL_BASENAME" ]]; then
        exit 93
    fi
    if [[ "$arg" == */agent-hub-approvals.service && ! -e "$FIXTURE_ROOT/install-hook-ran" ]]; then
        : >"$FIXTURE_ROOT/install-hook-ran"
        if [[ -n "${FAKE_INSTALL_EDIT_ENV_PATH:-}" ]]; then
            printf '%s\n' "${FAKE_INSTALL_EDIT_ENV_CONTENT:-edited-during-deployment}" \
                >"$FAKE_INSTALL_EDIT_ENV_PATH"
        fi
        if [[ -n "${FAKE_INSTALL_SWAP_ENV_PATH:-}" ]]; then
            rm -f -- "$FAKE_INSTALL_SWAP_ENV_PATH"
            case "${FAKE_INSTALL_SWAP_ENV_KIND:-symlink}" in
                symlink) ln -s -- "$FAKE_INSTALL_SWAP_ENV_TARGET" "$FAKE_INSTALL_SWAP_ENV_PATH" ;;
                directory)
                    mkdir -- "$FAKE_INSTALL_SWAP_ENV_PATH"
                    printf 'do-not-touch\n' >"$FAKE_INSTALL_SWAP_ENV_PATH/sentinel"
                    ;;
                *) exit 96 ;;
            esac
        fi
    fi
done
exec "$REAL_INSTALL" "$@"
EOF

    cat >"$fake_bin/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ -e "$FIXTURE_ROOT/post-cargo-hook-ready" && ! -e "$FIXTURE_ROOT/post-cargo-hook-ran" ]]; then
    rm -- "$FIXTURE_ROOT/post-cargo-hook-ready"
    : >"$FIXTURE_ROOT/post-cargo-hook-ran"
    mkdir -p -- "$(dirname -- "$FAKE_POST_CARGO_INJECT_PATH")"
    printf 'binary:injected-shared-artifact\n' >"$FAKE_POST_CARGO_INJECT_PATH"
    chmod 0755 "$FAKE_POST_CARGO_INJECT_PATH"
fi
exec "$REAL_GIT" "$@"
EOF

    cat >"$fake_bin/mktemp" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$FIXTURE_ROOT/mktemp.log"
exec "$REAL_MKTEMP" "$@"
EOF

cat >"$fake_bin/mv" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$FIXTURE_ROOT/mv.log"
if [[ -n "${FAKE_MV_CREATE_ENV_PATH:-}" && "$*" == *'.approvals-server.env.'* && \
    ! -e "$FIXTURE_ROOT/mv-env-hook-ran" ]]; then
    : >"$FIXTURE_ROOT/mv-env-hook-ran"
    printf '%s\n' "${FAKE_MV_CREATE_ENV_CONTENT:-OPERATOR_SETTING=appeared-during-deployment}" \
        >"$FAKE_MV_CREATE_ENV_PATH"
    chmod 0600 "$FAKE_MV_CREATE_ENV_PATH"
fi
if [[ -n "${FAKE_MV_ENV_STATUS:-}" && "$*" == *'.approvals-server.env.'* ]]; then
    exit "$FAKE_MV_ENV_STATUS"
fi
exec "$REAL_MV" "$@"
EOF

    cat >"$fake_bin/stat" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
format=
previous=
for arg in "$@"; do
    if [[ "$previous" == -c || "$previous" == --format ]]; then
        format="$arg"
    fi
    previous="$arg"
done
path="${!#}"
printf '%s|%s\n' "$format" "$path" >>"$FIXTURE_ROOT/stat.log"
if [[ -n "${FAKE_STAT_OWNER_PATH:-}" && "$path" == "$FAKE_STAT_OWNER_PATH" && "$format" == %u ]]; then
    printf '%s\n' "$FAKE_STAT_OWNER_UID"
    exit 0
fi
exec "$REAL_STAT" "$@"
EOF

    cat >"$fake_bin/systemctl" <<'EOF'
#!/usr/bin/env bash
printf 'systemctl %s\n' "$*" >>"$FIXTURE_ROOT/systemd.log"
exit 97
EOF

    cat >"$fake_bin/systemd-analyze" <<'EOF'
#!/usr/bin/env bash
printf 'systemd-analyze %s\n' "$*" >>"$FIXTURE_ROOT/systemd.log"
exit 97
EOF

    chmod 0755 "$fake_bin/cargo" "$fake_bin/git" "$fake_bin/install" "$fake_bin/mktemp" \
        "$fake_bin/mv" "$fake_bin/stat" "$fake_bin/systemctl" "$fake_bin/systemd-analyze"
}

make_fixture() {
    local name="$1"
    fixture="$test_root/$name"
    fixture_repo="$fixture/repo"
    fixture_home="$fixture/home"
    fixture_cache="$fixture/cache"
    fixture_config="$fixture/config"
    fixture_state="$fixture/state"
    fake_bin="$fixture/fake-bin"

    mkdir -p -- "$fixture_repo/deploy/approvals-server" "$fixture_repo/caller" \
        "$fixture_home/.bin" "$fixture_config" "$fixture_state"
    cp -- "$package_dir/deploy.sh" "$package_dir/approvals-server.env" \
        "$package_dir/agent-hub-approvals.service" "$package_dir/README.md" \
        "$fixture_repo/deploy/approvals-server/"
    printf '[workspace]\nmembers = []\n' >"$fixture_repo/Cargo.toml"
    printf '/target\n' >"$fixture_repo/.gitignore"
    printf 'version-1\n' >"$fixture_repo/server-version.txt"
    printf 'legacy-prebuilt\n' >"$fixture_home/.bin/agent-hub-server"
    chmod 0755 "$fixture_home/.bin/agent-hub-server"

    git -C "$fixture_repo" init -q
    git -C "$fixture_repo" config user.name 'Deployment Test'
    git -C "$fixture_repo" config user.email 'deployment-test@example.invalid'
    git -C "$fixture_repo" add .
    git -C "$fixture_repo" commit -qm 'fixture version 1'
    write_fakes "$fake_bin"
}

run_deploy() {
    local caller_dir="$1"
    shift
    set +e
    deploy_output="$({
        cd "$caller_dir"
        PATH="$fake_bin:$PATH" \
            HOME="${DEPLOY_HOME_OVERRIDE-$fixture_home}" \
            XDG_CONFIG_HOME="${DEPLOY_CONFIG_HOME_OVERRIDE-$fixture_config}" \
            XDG_CACHE_HOME="${DEPLOY_CACHE_HOME_OVERRIDE-$fixture_cache}" \
            XDG_STATE_HOME="$fixture_state" \
            FIXTURE_ROOT="$fixture" \
            FIXTURE_REPO="$fixture_repo" \
            REAL_GIT="$real_git" \
            REAL_INSTALL="$real_install" \
            REAL_MKTEMP="$real_mktemp" \
            REAL_MV="$real_mv" \
            REAL_STAT="$real_stat" \
            "$fixture_repo/deploy/approvals-server/deploy.sh" "$@"
    } 2>&1)"
    deploy_status=$?
    set -e
}

assert_no_systemd_invocation() {
    [[ ! -e "$fixture/systemd.log" ]] || fail "deployment invoked systemd: $(<"$fixture/systemd.log")"
}

assert_rejected_before_snapshot_build_or_install() {
    [[ ! -e "$fixture/cargo.log" ]] || fail "validation must fail before build"
    [[ ! -e "$fixture/install.log" ]] || fail "validation must fail before install"
    [[ ! -e "$fixture/mktemp.log" ]] || fail "validation must fail before snapshot staging"
}

set_path_override() {
    local variable="$1"
    local value="$2"

    case "$variable" in
        HOME) export DEPLOY_HOME_OVERRIDE="$value" ;;
        XDG_CONFIG_HOME) export DEPLOY_CONFIG_HOME_OVERRIDE="$value" ;;
        XDG_CACHE_HOME) export DEPLOY_CACHE_HOME_OVERRIDE="$value" ;;
        *) fail "unknown path override: $variable" ;;
    esac
}

test_rejects_invalid_effective_paths() {
    local variable="$1"
    local slug="$2"
    local mode
    local kind
    local bad_path

    for kind in relative dot dotdot; do
        for mode in actual dry-run; do
            make_fixture "invalid-${slug}-${kind}-${mode}"
            case "$kind" in
                relative) bad_path="relative-${slug}" ;;
                dot) bad_path="$fixture/path-base/./${slug}" ;;
                dotdot) bad_path="$fixture/path-base/child/../${slug}" ;;
            esac
            set_path_override "$variable" "$bad_path"
            if [[ "$mode" == dry-run ]]; then
                run_deploy "$fixture_repo/caller" --dry-run
            else
                run_deploy "$fixture_repo/caller"
            fi

            [[ "$deploy_status" -ne 0 ]] || \
                fail "$variable with a $kind path must hard-fail in $mode mode"
            assert_output_contains 'absolute|path component|\.\.|/\./' \
                "$variable $kind path failure should explain the invalid path"
            assert_rejected_before_snapshot_build_or_install
            unset DEPLOY_HOME_OVERRIDE DEPLOY_CONFIG_HOME_OVERRIDE DEPLOY_CACHE_HOME_OVERRIDE
        done
    done
}

seed_managed_artifacts() {
    mkdir -p -- "$fixture_home/.local/libexec/agent-hub-approvals" \
        "$fixture_config/agent-hub" "$fixture_config/systemd/user"
    printf 'installed-binary\n' >"$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server"
    chmod 0755 "$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server"
    printf 'OPERATOR_SETTING=installed\n' >"$fixture_config/agent-hub/approvals-server.env"
    chmod 0600 "$fixture_config/agent-hub/approvals-server.env"
    printf 'installed-unit\n' >"$fixture_config/systemd/user/agent-hub-approvals.service"
    chmod 0644 "$fixture_config/systemd/user/agent-hub-approvals.service"
}

assert_binary_and_environment_unchanged() {
    assert_eq 'installed-binary' \
        "$(<"$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server")" \
        "installed binary must remain unchanged"
    assert_eq 'OPERATOR_SETTING=installed' \
        "$(<"$fixture_config/agent-hub/approvals-server.env")" \
        "installed environment must remain unchanged"
}

assert_environment_and_unit_unchanged() {
    assert_eq 'OPERATOR_SETTING=installed' \
        "$(<"$fixture_config/agent-hub/approvals-server.env")" \
        "installed environment must remain unchanged"
    assert_eq 'installed-unit' \
        "$(<"$fixture_config/systemd/user/agent-hub-approvals.service")" \
        "installed unit must remain unchanged"
}

test_builds_captured_head_snapshot_from_any_cwd() {
    make_fixture any-cwd
    run_deploy "$fixture_repo/caller"

    assert_eq 0 "$deploy_status" "deployment from an unrelated caller cwd should succeed"
    [[ -f "$fixture/cargo.log" ]] || fail "deployment must invoke cargo"
    cargo_cwd="$(cut -d '|' -f 1 "$fixture/cargo.log")"
    cargo_args="$(cut -d '|' -f 2- "$fixture/cargo.log")"
    assert_eq 'build --locked --release -p agent-hub-server' "$cargo_args" \
        "cargo must receive the exact build arguments"
    [[ "$cargo_cwd" != "$fixture_repo" ]] || fail "cargo must not build from the mutable original checkout"
    [[ "$cargo_cwd" == "$fixture_cache/"* ]] || \
        fail "captured HEAD snapshot must be created under the private cache outside the repository (got $cargo_cwd)"
    assert_eq 700 "$(<"$fixture/workspace-mode.log")" \
        "captured HEAD snapshot workspace must have mode 0700"
    [[ ! -e "$cargo_cwd" ]] || fail "captured HEAD snapshot must be cleaned after deployment"
    cargo_target_dir="$(cut -d '|' -f 2 "$fixture/cargo-source.log")"
    [[ "$cargo_target_dir" == "$cargo_cwd/"* ]] || \
        fail "snapshot build must use a private target directory inside its snapshot workspace (got $cargo_target_dir)"
    [[ ! -e "$cargo_target_dir" ]] || fail "private build target must be cleaned with the snapshot"
    [[ -f "$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server" ]] || \
        fail "deployment must install an agent-hub-server binary"
    assert_eq 'binary:version-1' \
        "$(<"$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server")" \
        "deployment must install the binary produced by that build"
}

test_source_mutation_during_build_uses_snapshot_and_aborts_install() {
    make_fixture mutate-source
    seed_managed_artifacts
    export FAKE_CARGO_MUTATE_PATH=server-version.txt
    run_deploy "$fixture_repo/caller"
    unset FAKE_CARGO_MUTATE_PATH

    [[ "$deploy_status" -ne 0 ]] || fail "deployment must reject a tracked source mutation during build"
    assert_output_contains 'changed|dirty|head|clean|worktree' \
        "post-build source mutation failure should explain the revalidation"
    assert_eq 'mutated-during-build' "$(<"$fixture_repo/server-version.txt")" \
        "fake cargo must mutate the original tracked source"
    cargo_cwd="$(cut -d '|' -f 1 "$fixture/cargo-source.log")"
    cargo_version="$(cut -d '|' -f 3 "$fixture/cargo-source.log")"
    [[ "$cargo_cwd" == "$fixture_cache/"* ]] || \
        fail "cargo must run from the captured HEAD snapshot (got $cargo_cwd)"
    assert_eq 'version-1' "$cargo_version" \
        "cargo must observe captured HEAD content, not the concurrently mutated original source"
    assert_environment_and_unit_unchanged
    assert_eq 'installed-binary' \
        "$(<"$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server")" \
        "rejected source mutation must not replace the installed binary"
}

test_deployment_file_mutation_during_build_aborts_install() {
    make_fixture mutate-deployment-file
    seed_managed_artifacts
    export FAKE_CARGO_MUTATE_PATH=deploy/approvals-server/agent-hub-approvals.service
    run_deploy "$fixture_repo"
    unset FAKE_CARGO_MUTATE_PATH

    [[ "$deploy_status" -ne 0 ]] || fail "deployment must reject a tracked deployment-file mutation during build"
    assert_output_contains 'changed|dirty|head|clean|worktree' \
        "post-build deployment-file mutation failure should explain the revalidation"
    assert_eq 'mutated-during-build' \
        "$(<"$fixture_repo/deploy/approvals-server/agent-hub-approvals.service")" \
        "fake cargo must mutate the original tracked deployment file"
    assert_binary_and_environment_unchanged
    assert_eq 'installed-unit' \
        "$(<"$fixture_config/systemd/user/agent-hub-approvals.service")" \
        "rejected deployment-file mutation must not replace the installed unit"
}

test_ignores_inherited_target_and_shared_artifact_injected_after_cargo() {
    make_fixture private-target
    shared_target="$fixture/caller-shared-target"
    export CARGO_TARGET_DIR="$shared_target"
    export FAKE_POST_CARGO_INJECT_PATH="$shared_target/release/agent-hub-server"
    run_deploy "$fixture_repo/caller"
    unset CARGO_TARGET_DIR
    unset FAKE_POST_CARGO_INJECT_PATH

    assert_eq 0 "$deploy_status" "deployment with an inherited shared CARGO_TARGET_DIR should succeed"
    [[ -e "$fixture/post-cargo-hook-ran" ]] || fail "shared artifact injection hook must run after cargo returns"
    assert_eq 'binary:injected-shared-artifact' "$(<"$shared_target/release/agent-hub-server")" \
        "hook must replace the caller-selected shared artifact"
    private_target="$(cut -d '|' -f 2 "$fixture/cargo-source.log")"
    [[ "$private_target" == "$fixture_cache/"* ]] || \
        fail "cargo must ignore inherited CARGO_TARGET_DIR and use the private cache workspace (got $private_target)"
    [[ "$private_target" != "$shared_target" ]] || fail "cargo must not build into the inherited shared target"
    assert_eq 'binary:version-1' \
        "$(<"$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server")" \
        "deployment must install only the captured snapshot's private output"
}

test_rejects_dirty_tracked_worktree() {
    make_fixture dirty-tracked
    printf 'dirty\n' >>"$fixture_repo/server-version.txt"
    run_deploy "$fixture_repo/caller"

    [[ "$deploy_status" -ne 0 ]] || fail "deployment should reject a dirty tracked file"
    assert_output_contains 'dirty|clean|worktree' "dirty-worktree failure should explain the cause"
    [[ ! -e "$fixture/cargo.log" ]] || fail "dirty worktree must be rejected before cargo runs"
    [[ ! -e "$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server" ]] || \
        fail "dirty worktree must be rejected before installation"
}

test_rejects_staged_worktree() {
    make_fixture dirty-staged
    printf 'version-staged\n' >"$fixture_repo/server-version.txt"
    git -C "$fixture_repo" add server-version.txt
    run_deploy "$fixture_repo"

    [[ "$deploy_status" -ne 0 ]] || fail "deployment should reject staged changes"
    assert_output_contains 'dirty|clean|worktree' "staged-worktree failure should explain the cause"
    [[ ! -e "$fixture/cargo.log" ]] || fail "staged changes must be rejected before cargo runs"
    [[ ! -e "$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server" ]] || \
        fail "staged changes must be rejected before installation"
}

test_rejects_untracked_worktree() {
    make_fixture dirty-untracked
    printf 'untracked\n' >"$fixture_repo/untracked.txt"
    run_deploy "$fixture_repo"

    [[ "$deploy_status" -ne 0 ]] || fail "deployment should reject untracked files"
    assert_output_contains 'dirty|clean|worktree' "untracked-worktree failure should explain the cause"
    [[ ! -e "$fixture/cargo.log" ]] || fail "untracked files must be rejected before cargo runs"
    [[ ! -e "$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server" ]] || \
        fail "untracked files must be rejected before installation"
}

test_rejects_binary_destination_directory() {
    make_fixture binary-directory
    seed_managed_artifacts
    destination="$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server"
    rm -- "$destination"
    mkdir -- "$destination"
    printf 'do-not-touch\n' >"$destination/sentinel"
    run_deploy "$fixture_repo"

    [[ "$deploy_status" -ne 0 ]] || fail "binary destination directory must hard-fail"
    [[ -d "$destination" && ! -L "$destination" ]] || fail "binary destination directory must not be replaced"
    assert_eq 'sentinel' "$(ls -A -- "$destination")" \
        "mv must not treat the binary destination as a target directory"
    assert_eq 'do-not-touch' "$(<"$destination/sentinel")" \
        "binary destination directory contents must remain unchanged"
    assert_environment_and_unit_unchanged
}

test_rejects_binary_destination_symlink_to_file() {
    make_fixture binary-symlink-file
    seed_managed_artifacts
    destination="$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server"
    target="$fixture/binary-symlink-target"
    printf 'do-not-touch\n' >"$target"
    rm -- "$destination"
    ln -s -- "$target" "$destination"
    run_deploy "$fixture_repo"

    [[ "$deploy_status" -ne 0 ]] || fail "binary destination symlink to a file must hard-fail"
    [[ -L "$destination" ]] || fail "binary destination symlink must not be replaced"
    assert_eq "$target" "$(readlink -- "$destination")" "binary destination symlink target must remain unchanged"
    assert_eq 'do-not-touch' "$(<"$target")" "binary symlink target file must not be overwritten"
    assert_environment_and_unit_unchanged
}

test_rejects_binary_destination_symlink_to_directory() {
    make_fixture binary-symlink-directory
    seed_managed_artifacts
    destination="$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server"
    target="$fixture/binary-symlink-target-directory"
    mkdir -- "$target"
    printf 'do-not-touch\n' >"$target/sentinel"
    rm -- "$destination"
    ln -s -- "$target" "$destination"
    run_deploy "$fixture_repo"

    [[ "$deploy_status" -ne 0 ]] || fail "binary destination symlink to a directory must hard-fail"
    [[ -L "$destination" ]] || fail "binary destination symlink must not be replaced"
    assert_eq "$target" "$(readlink -- "$destination")" "binary destination symlink target must remain unchanged"
    assert_eq 'sentinel' "$(ls -A -- "$target")" \
        "mv must not follow the binary destination symlink as a target directory"
    assert_eq 'do-not-touch' "$(<"$target/sentinel")" "binary symlink target directory must remain unchanged"
    assert_environment_and_unit_unchanged
}

test_rejects_unit_destination_directory() {
    make_fixture unit-directory
    seed_managed_artifacts
    destination="$fixture_config/systemd/user/agent-hub-approvals.service"
    rm -- "$destination"
    mkdir -- "$destination"
    printf 'do-not-touch\n' >"$destination/sentinel"
    run_deploy "$fixture_repo"

    [[ "$deploy_status" -ne 0 ]] || fail "unit destination directory must hard-fail"
    [[ -d "$destination" && ! -L "$destination" ]] || fail "unit destination directory must not be replaced"
    assert_eq 'sentinel' "$(ls -A -- "$destination")" \
        "unit staging must not write inside the destination directory"
    assert_eq 'do-not-touch' "$(<"$destination/sentinel")" \
        "unit destination directory contents must remain unchanged"
    assert_binary_and_environment_unchanged
}

test_rejects_unit_destination_symlink_to_file() {
    make_fixture unit-symlink-file
    seed_managed_artifacts
    destination="$fixture_config/systemd/user/agent-hub-approvals.service"
    target="$fixture/unit-symlink-target"
    printf 'do-not-touch\n' >"$target"
    rm -- "$destination"
    ln -s -- "$target" "$destination"
    run_deploy "$fixture_repo"

    [[ "$deploy_status" -ne 0 ]] || fail "unit destination symlink to a file must hard-fail"
    [[ -L "$destination" ]] || fail "unit destination symlink must not be replaced"
    assert_eq "$target" "$(readlink -- "$destination")" "unit destination symlink target must remain unchanged"
    assert_eq 'do-not-touch' "$(<"$target")" "unit symlink target file must not be overwritten"
    assert_binary_and_environment_unchanged
}

test_rejects_unit_destination_symlink_to_directory() {
    make_fixture unit-symlink-directory
    seed_managed_artifacts
    destination="$fixture_config/systemd/user/agent-hub-approvals.service"
    target="$fixture/unit-symlink-target-directory"
    mkdir -- "$target"
    printf 'do-not-touch\n' >"$target/sentinel"
    rm -- "$destination"
    ln -s -- "$target" "$destination"
    run_deploy "$fixture_repo"

    [[ "$deploy_status" -ne 0 ]] || fail "unit destination symlink to a directory must hard-fail"
    [[ -L "$destination" ]] || fail "unit destination symlink must not be replaced"
    assert_eq "$target" "$(readlink -- "$destination")" "unit destination symlink target must remain unchanged"
    assert_eq 'sentinel' "$(ls -A -- "$target")" \
        "unit staging must not follow the destination symlink as a target directory"
    assert_eq 'do-not-touch' "$(<"$target/sentinel")" "unit symlink target directory must remain unchanged"
    assert_binary_and_environment_unchanged
}

test_rejects_environment_symlink_race_before_mode_enforcement() {
    make_fixture environment-symlink-race
    seed_managed_artifacts
    destination="$fixture_config/agent-hub/approvals-server.env"
    target="$fixture/environment-race-target"
    cp -- "$destination" "$target"
    chmod 0644 "$target"
    export FAKE_INSTALL_SWAP_ENV_PATH="$destination"
    export FAKE_INSTALL_SWAP_ENV_KIND=symlink
    export FAKE_INSTALL_SWAP_ENV_TARGET="$target"
    run_deploy "$fixture_repo"

    [[ "$deploy_status" -ne 0 ]] || fail "environment symlink race must hard-fail"
    [[ -L "$destination" ]] || fail "environment source must become the injected symlink"
    assert_eq "$target" "$(readlink -- "$destination")" \
        "environment symlink race must leave the injected target unchanged"
    assert_eq 644 "$(stat -c %a "$target")" \
        "environment symlink race must not chmod the symlink target"
    assert_eq 'installed-binary' \
        "$(<"$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server")" \
        "environment staging failure must preserve the installed binary"
    if compgen -G "$fixture_home/.local/libexec/agent-hub-approvals/.agent-hub-server.*" >/dev/null || \
        compgen -G "$fixture_config/agent-hub/.approvals-server.env.*" >/dev/null; then
        fail "environment staging failure must clean staged artifacts"
    fi
}

test_rejects_environment_directory_race() {
    make_fixture environment-directory-race
    seed_managed_artifacts
    destination="$fixture_config/agent-hub/approvals-server.env"
    export FAKE_INSTALL_SWAP_ENV_PATH="$destination"
    export FAKE_INSTALL_SWAP_ENV_KIND=directory
    run_deploy "$fixture_repo"

    [[ "$deploy_status" -ne 0 ]] || fail "environment directory race must hard-fail"
    [[ -d "$destination" && ! -L "$destination" ]] || \
        fail "environment destination must become the injected directory"
    assert_eq 'do-not-touch' "$(<"$destination/sentinel")" \
        "environment directory race must leave injected contents unchanged"
    assert_eq 'installed-binary' \
        "$(<"$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server")" \
        "environment race must preserve the installed binary"
    assert_eq 'installed-unit' "$(<"$fixture_config/systemd/user/agent-hub-approvals.service")" \
        "environment race must preserve the installed unit"
}

test_rejects_existing_environment_with_nonrestrictive_mode_before_build() {
    make_fixture environment-mode
    seed_managed_artifacts
    destination="$fixture_config/agent-hub/approvals-server.env"
    chmod 0644 "$destination"
    binary_before="$(sha256sum "$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server")"
    unit_before="$(sha256sum "$fixture_config/systemd/user/agent-hub-approvals.service")"
    env_before="$(sha256sum "$destination")"
    inode_before="$(stat -c %i "$destination")"
    run_deploy "$fixture_repo"

    [[ "$deploy_status" -ne 0 ]] || fail "existing environment mode other than 0600 must hard-fail"
    assert_output_contains 'mode 0600|chmod 600' \
        "environment mode failure must include clear remediation"
    [[ ! -e "$fixture/cargo.log" ]] || fail "invalid environment mode must fail before cargo runs"
    [[ ! -e "$fixture/install.log" ]] || fail "invalid environment mode must fail before staging or installation"
    assert_eq "$binary_before" "$(sha256sum "$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server")" \
        "invalid environment mode must preserve the binary"
    assert_eq "$unit_before" "$(sha256sum "$fixture_config/systemd/user/agent-hub-approvals.service")" \
        "invalid environment mode must preserve the unit"
    assert_eq "$env_before" "$(sha256sum "$destination")" \
        "invalid environment mode must preserve environment bytes"
    assert_eq "$inode_before" "$(stat -c %i "$destination")" \
        "invalid environment mode must preserve the environment inode"
    assert_eq 644 "$(stat -c %a "$destination")" \
        "failed deployment must not repair the environment mode implicitly"
}

test_concurrent_environment_edit_survives_without_inode_replacement() {
    make_fixture concurrent-environment-edit
    seed_managed_artifacts
    destination="$fixture_config/agent-hub/approvals-server.env"
    inode_before="$(stat -c %i "$destination")"
    export FAKE_INSTALL_EDIT_ENV_PATH="$destination"
    export FAKE_INSTALL_EDIT_ENV_CONTENT='OPERATOR_SETTING=edited-during-deployment'
    run_deploy "$fixture_repo"

    assert_eq 0 "$deploy_status" "deployment must allow a concurrent regular-file environment edit"
    assert_eq 'OPERATOR_SETTING=edited-during-deployment' "$(<"$destination")" \
        "concurrent environment edit must survive deployment"
    assert_eq "$inode_before" "$(stat -c %i "$destination")" \
        "existing environment inode must remain untouched"
    if [[ -e "$fixture/mv.log" ]] && grep -Eq 'approvals-server\.env' "$fixture/mv.log"; then
        fail "existing environment must not be replaced by a staged rename"
    fi
}

test_rejects_environment_appearing_during_first_install() {
    local mv_status

    for mv_status in 0 98; do
        make_fixture "absent-environment-race-${mv_status}"
        seed_managed_artifacts
        destination="$fixture_config/agent-hub/approvals-server.env"
        rm -- "$destination"
        export FAKE_MV_CREATE_ENV_PATH="$destination"
        export FAKE_MV_CREATE_ENV_CONTENT='OPERATOR_SETTING=appeared-during-deployment'
        export FAKE_MV_ENV_STATUS="$mv_status"
        run_deploy "$fixture_repo"
        unset FAKE_MV_CREATE_ENV_PATH
        unset FAKE_MV_CREATE_ENV_CONTENT
        unset FAKE_MV_ENV_STATUS

        [[ "$deploy_status" -ne 0 ]] || fail "environment appearing during first install must hard-fail"
        [[ -e "$fixture/mv-env-hook-ran" ]] || fail "absent-environment race hook must run"
        assert_output_contains 'environment appeared before commit' \
            "absent-environment race failure should explain the conflict"
        assert_eq 'OPERATOR_SETTING=appeared-during-deployment' "$(<"$destination")" \
            "deployment must not overwrite an environment that appears during first install"
        assert_eq 600 "$(stat -c %a "$destination")" \
            "deployment must not alter the appearing environment mode"
        assert_eq 'installed-binary' \
            "$(<"$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server")" \
            "absent-environment race must preserve the installed binary"
        assert_eq 'installed-unit' "$(<"$fixture_config/systemd/user/agent-hub-approvals.service")" \
            "absent-environment race must preserve the installed unit"
        if compgen -G "$fixture_config/agent-hub/.approvals-server.env.*" >/dev/null; then
            fail "absent-environment race must clean the staged environment"
        fi
    done
}

test_reports_environment_move_failure_without_a_destination_conflict() {
    make_fixture environment-move-failure
    seed_managed_artifacts
    destination="$fixture_config/agent-hub/approvals-server.env"
    rm -- "$destination"
    export FAKE_MV_ENV_STATUS=98
    run_deploy "$fixture_repo"
    unset FAKE_MV_ENV_STATUS

    [[ "$deploy_status" -ne 0 ]] || fail "environment move failure must hard-fail"
    assert_output_contains 'failed to install environment' \
        "environment move failure should explain the failed operation"
    if grep -Eqi 'environment appeared before commit' <<<"$deploy_output"; then
        fail "environment move failure without a destination must not report a conflict"
    fi
    [[ ! -e "$destination" ]] || fail "failed environment move must not create the destination"
    assert_eq 'installed-binary' \
        "$(<"$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server")" \
        "environment move failure must preserve the installed binary"
    assert_eq 'installed-unit' "$(<"$fixture_config/systemd/user/agent-hub-approvals.service")" \
        "environment move failure must preserve the installed unit"
    if compgen -G "$fixture_config/agent-hub/.approvals-server.env.*" >/dev/null; then
        fail "environment move failure must clean the staged environment"
    fi
}

test_existing_parent_directory_modes_are_preserved() {
    make_fixture parent-modes
    seed_managed_artifacts
    mkdir -p -- "$fixture_cache/agent-hub-approvals"
    binary_parent="$fixture_home/.local/libexec/agent-hub-approvals"
    environment_parent="$fixture_config/agent-hub"
    unit_parent="$fixture_config/systemd/user"
    cache_parent="$fixture_cache"
    deployment_cache="$fixture_cache/agent-hub-approvals"
    chmod 0711 "$binary_parent"
    chmod 0750 "$environment_parent"
    chmod 0700 "$unit_parent"
    chmod 0751 "$cache_parent"
    chmod 0710 "$deployment_cache"
    run_deploy "$fixture_repo"

    assert_eq 0 "$deploy_status" "deployment into existing parent directories should succeed"
    assert_eq 711 "$(stat -c %a "$binary_parent")" "binary parent mode must remain unchanged"
    assert_eq 750 "$(stat -c %a "$environment_parent")" "environment parent mode must remain unchanged"
    assert_eq 700 "$(stat -c %a "$unit_parent")" "unit parent mode must remain unchanged"
    assert_eq 751 "$(stat -c %a "$cache_parent")" "cache parent mode must remain unchanged"
    assert_eq 710 "$(stat -c %a "$deployment_cache")" "deployment cache mode must remain unchanged"
}

test_config_staging_failure_preserves_installed_binary() {
    make_fixture config-staging-failure
    seed_managed_artifacts
    rm -- "$fixture_config/agent-hub/approvals-server.env"
    export FAKE_INSTALL_FAIL_BASENAME=approvals-server.env
    run_deploy "$fixture_repo"
    unset FAKE_INSTALL_FAIL_BASENAME

    [[ "$deploy_status" -ne 0 ]] || fail "injected config staging failure must fail deployment"
    assert_eq 'installed-binary' \
        "$(<"$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server")" \
        "config staging failure must occur before binary commit"
    [[ ! -e "$fixture_config/agent-hub/approvals-server.env" ]] || \
        fail "failed config staging must not expose a partial destination"
    assert_eq 'installed-unit' "$(<"$fixture_config/systemd/user/agent-hub-approvals.service")" \
        "config staging failure must preserve the installed unit"
}

test_unit_staging_failure_preserves_installed_binary() {
    make_fixture unit-staging-failure
    seed_managed_artifacts
    export FAKE_INSTALL_FAIL_BASENAME=agent-hub-approvals.service
    run_deploy "$fixture_repo"
    unset FAKE_INSTALL_FAIL_BASENAME

    [[ "$deploy_status" -ne 0 ]] || fail "injected unit staging failure must fail deployment"
    assert_binary_and_environment_unchanged
    assert_eq 'installed-unit' "$(<"$fixture_config/systemd/user/agent-hub-approvals.service")" \
        "failed unit staging must preserve the installed unit"
}

test_first_deploy_installs_packaged_files_with_restrictive_modes() {
    make_fixture first-deploy
    run_deploy "$fixture_repo"

    assert_eq 0 "$deploy_status" "first deployment should succeed"
    cmp -s "$fixture_repo/deploy/approvals-server/approvals-server.env" \
        "$fixture_config/agent-hub/approvals-server.env" || fail "first deploy must install packaged environment"
    cmp -s "$fixture_repo/deploy/approvals-server/agent-hub-approvals.service" \
        "$fixture_config/systemd/user/agent-hub-approvals.service" || fail "first deploy must install packaged unit"
    assert_eq 600 "$(stat -c %a "$fixture_config/agent-hub/approvals-server.env")" \
        "environment mode must be restrictive"
    assert_file_contains "$fixture/mv.log" \
        '(^|[[:space:]])(-[^[:space:]]*T|--no-target-directory)([[:space:]]|$).*approvals-server\.env' \
        "first environment install must use an atomic no-target-directory rename"
    assert_eq 644 "$(stat -c %a "$fixture_config/systemd/user/agent-hub-approvals.service")" \
        "unit mode must be readable by systemd"
    assert_eq 755 "$(stat -c %a "$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server")" \
        "binary mode must be executable"
}

test_redeploy_preserves_operator_environment_and_state() {
    make_fixture redeploy
    run_deploy "$fixture_repo"
    assert_eq 0 "$deploy_status" "initial deployment should succeed"

    printf 'OPERATOR_SETTING=preserve-me\n' >"$fixture_config/agent-hub/approvals-server.env"
    environment_inode_before="$(stat -c %i "$fixture_config/agent-hub/approvals-server.env")"
    environment_bytes_before="$(sha256sum "$fixture_config/agent-hub/approvals-server.env")"
    mkdir -p "$fixture_state/agent-hub-approvals"
    printf 'state\0bytes\n' >"$fixture_state/agent-hub-approvals/server_data.json"
    state_before="$(sha256sum "$fixture_state/agent-hub-approvals/server_data.json")"
    printf 'version-2\n' >"$fixture_repo/server-version.txt"
    printf '\nEnvironment=DEPLOY_TEST_VERSION=2\n' >>"$fixture_repo/deploy/approvals-server/agent-hub-approvals.service"
    git -C "$fixture_repo" add server-version.txt deploy/approvals-server/agent-hub-approvals.service
    git -C "$fixture_repo" commit -qm 'fixture version 2'

    run_deploy "$fixture_repo/caller"
    assert_eq 0 "$deploy_status" "redeployment from the newer clean commit should succeed"
    assert_eq 'OPERATOR_SETTING=preserve-me' "$(<"$fixture_config/agent-hub/approvals-server.env")" \
        "redeploy must preserve operator-modified environment"
    assert_eq "$environment_bytes_before" "$(sha256sum "$fixture_config/agent-hub/approvals-server.env")" \
        "redeploy must leave existing environment bytes untouched"
    assert_eq "$environment_inode_before" "$(stat -c %i "$fixture_config/agent-hub/approvals-server.env")" \
        "redeploy must leave the existing environment inode untouched"
    assert_eq "$state_before" "$(sha256sum "$fixture_state/agent-hub-approvals/server_data.json")" \
        "redeploy must preserve every state byte"
    assert_eq 'binary:version-2' \
        "$(<"$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server")" \
        "redeploy must install the newly built binary"
    cmp -s "$fixture_repo/deploy/approvals-server/agent-hub-approvals.service" \
        "$fixture_config/systemd/user/agent-hub-approvals.service" || fail "redeploy must update the managed unit"
}

test_binary_update_is_atomic() {
    make_fixture atomic
    run_deploy "$fixture_repo"
    assert_eq 0 "$deploy_status" "initial deployment should succeed"
    destination="$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server"
    inode_before="$(stat -c %i "$destination")"

    printf 'version-2\n' >"$fixture_repo/server-version.txt"
    printf 'binary:version-2\n' >"$fixture_home/.bin/agent-hub-server"
    git -C "$fixture_repo" add server-version.txt
    git -C "$fixture_repo" commit -qm 'fixture version 2'
    run_deploy "$fixture_repo"

    assert_eq 0 "$deploy_status" "binary update should succeed"
    assert_eq 'binary:version-2' "$(<"$destination")" "atomic update must expose the complete new binary"
    [[ "$(stat -c %i "$destination")" != "$inode_before" ]] || \
        fail "binary must be replaced by rename rather than overwritten in place"
    assert_file_contains "$fixture/mv.log" \
        '(^|[[:space:]])(-[^[:space:]]*T|--no-target-directory)([[:space:]]|$)' \
        "final binary rename must use no-target-directory semantics"
}

test_deploy_never_invokes_systemd_and_manual_path_is_explicit() {
    make_fixture no-systemd
    run_deploy "$fixture_repo"

    assert_eq 0 "$deploy_status" "deployment should succeed without systemd"
    assert_no_systemd_invocation
    assert_file_contains "$fixture_repo/deploy/approvals-server/README.md" \
        'systemctl --user daemon-reload' "README must document manual daemon reload"
    assert_file_contains "$fixture_repo/deploy/approvals-server/README.md" \
        'Start `agent-hub-approvals.service`' "README must keep service start manual"
}

test_dry_run_builds_nothing_and_mutates_no_install_paths() {
    make_fixture dry-run
    run_deploy "$fixture_repo/caller" --dry-run

    assert_eq 0 "$deploy_status" "dry run should succeed"
    [[ ! -e "$fixture/cargo.log" ]] || fail "dry run must not invoke cargo"
    [[ ! -e "$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server" ]] || \
        fail "dry run must not install a binary"
    [[ ! -e "$fixture_config/agent-hub/approvals-server.env" ]] || fail "dry run must not install environment"
    [[ ! -e "$fixture_config/systemd/user/agent-hub-approvals.service" ]] || fail "dry run must not install unit"
    [[ ! -e "$fixture_repo/target" ]] || fail "dry run must not create snapshot or build directories"
    [[ ! -e "$fixture_cache" ]] || fail "dry run must not create the private cache workspace"
    assert_output_contains 'cargo build --locked --release -p agent-hub-server' \
        "dry run must report the intended exact build"
    assert_output_contains 'agent-hub-server' "dry run must report intended binary installation"
    assert_output_contains 'approvals-server.env' "dry run must report intended environment installation"
    assert_output_contains 'agent-hub-approvals.service' "dry run must report intended unit installation"
    assert_no_systemd_invocation
}

test_rejects_invalid_home_paths_before_dry_run_or_deploy() {
    test_rejects_invalid_effective_paths HOME home
}

test_rejects_invalid_config_home_paths_before_dry_run_or_deploy() {
    test_rejects_invalid_effective_paths XDG_CONFIG_HOME config-home
}

test_rejects_invalid_cache_home_paths_before_dry_run_or_deploy() {
    test_rejects_invalid_effective_paths XDG_CACHE_HOME cache-home
}

test_rejects_symlink_cache_authority_directories() {
    local location
    local mode

    for location in cache-home cache-root; do
        for mode in actual dry-run; do
            make_fixture "symlink-${location}-${mode}"
            mkdir -p -- "$fixture/cache-target"
            if [[ "$location" == cache-home ]]; then
                ln -s -- "$fixture/cache-target" "$fixture_cache"
            else
                mkdir -p -- "$fixture_cache"
                ln -s -- "$fixture/cache-target" "$fixture_cache/agent-hub-approvals"
            fi
            if [[ "$mode" == dry-run ]]; then
                run_deploy "$fixture_repo" --dry-run
            else
                run_deploy "$fixture_repo"
            fi

            [[ "$deploy_status" -ne 0 ]] || fail "$location symlink must hard-fail in $mode mode"
            assert_output_contains 'cache.*directory|symlink' \
                "$location symlink failure should explain the invalid cache directory"
            assert_rejected_before_snapshot_build_or_install
        done
    done
}

test_rejects_nondirectory_cache_authority_paths() {
    local location
    local mode

    for location in cache-home cache-root; do
        for mode in actual dry-run; do
            make_fixture "file-${location}-${mode}"
            if [[ "$location" == cache-home ]]; then
                printf 'not-a-directory\n' >"$fixture_cache"
            else
                mkdir -p -- "$fixture_cache"
                printf 'not-a-directory\n' >"$fixture_cache/agent-hub-approvals"
            fi
            if [[ "$mode" == dry-run ]]; then
                run_deploy "$fixture_repo" --dry-run
            else
                run_deploy "$fixture_repo"
            fi

            [[ "$deploy_status" -ne 0 ]] || fail "$location regular file must hard-fail in $mode mode"
            assert_output_contains 'cache.*directory' \
                "$location regular-file failure should explain the invalid cache directory"
            assert_rejected_before_snapshot_build_or_install
        done
    done
}

test_rejects_group_or_other_writable_cache_authority_directories() {
    local location
    local mode
    local authority_path

    for location in cache-home cache-root; do
        for mode in actual dry-run; do
            make_fixture "writable-${location}-${mode}"
            mkdir -p -- "$fixture_cache/agent-hub-approvals"
            authority_path="$fixture_cache"
            [[ "$location" == cache-home ]] || authority_path="$fixture_cache/agent-hub-approvals"
            chmod 0777 "$authority_path"
            if [[ "$mode" == dry-run ]]; then
                run_deploy "$fixture_repo" --dry-run
            else
                run_deploy "$fixture_repo"
            fi

            [[ "$deploy_status" -ne 0 ]] || fail "mode-0777 $location must hard-fail in $mode mode"
            assert_output_contains 'writable|mode|permission' \
                "$location mode failure should explain the unsafe permissions"
            assert_eq 777 "$(stat -c %a -- "$authority_path")" \
                "deployment must not repair unsafe $location permissions"
            assert_rejected_before_snapshot_build_or_install
        done
    done
}

test_rejects_foreign_owned_cache_authority_directories() {
    local location
    local mode

    for location in cache-home cache-root; do
        for mode in actual dry-run; do
            make_fixture "foreign-owner-${location}-${mode}"
            mkdir -p -- "$fixture_cache/agent-hub-approvals"
            export FAKE_STAT_OWNER_PATH="$fixture_cache"
            [[ "$location" == cache-home ]] || \
                FAKE_STAT_OWNER_PATH="$fixture_cache/agent-hub-approvals"
            export FAKE_STAT_OWNER_PATH
            export FAKE_STAT_OWNER_UID="$((EUID + 1))"
            if [[ "$mode" == dry-run ]]; then
                run_deploy "$fixture_repo" --dry-run
            else
                run_deploy "$fixture_repo"
            fi

            [[ "$deploy_status" -ne 0 ]] || fail "foreign-owned $location must hard-fail in $mode mode"
            assert_output_contains 'owner|owned|uid' \
                "$location ownership failure should explain the foreign owner"
            assert_rejected_before_snapshot_build_or_install
            unset FAKE_STAT_OWNER_PATH FAKE_STAT_OWNER_UID
        done
    done
}

test_rejects_missing_cache_beneath_writable_existing_ancestor() {
    local mode
    local ancestor
    local missing_cache

    for mode in actual dry-run; do
        make_fixture "missing-cache-writable-ancestor-${mode}"
        ancestor="$fixture/untrusted-cache-parent"
        missing_cache="$ancestor/missing/cache"
        mkdir -- "$ancestor"
        chmod 0777 "$ancestor"
        export DEPLOY_CACHE_HOME_OVERRIDE="$missing_cache"
        if [[ "$mode" == dry-run ]]; then
            run_deploy "$fixture_repo" --dry-run
        else
            run_deploy "$fixture_repo"
        fi

        [[ "$deploy_status" -ne 0 ]] || \
            fail "missing cache beneath a writable ancestor must hard-fail in $mode mode"
        assert_output_contains 'ancestor|writable|mode|permission' \
            "unsafe cache ancestor failure should explain the trust violation"
        [[ ! -e "$ancestor/missing" ]] || fail "$mode must not create directories beneath an unsafe ancestor"
        assert_rejected_before_snapshot_build_or_install
        unset DEPLOY_CACHE_HOME_OVERRIDE
    done
}

test_rejects_missing_cache_beneath_foreign_owned_existing_ancestor() {
    local mode
    local ancestor
    local missing_cache

    for mode in actual dry-run; do
        make_fixture "missing-cache-foreign-ancestor-${mode}"
        ancestor="$fixture/foreign-cache-parent"
        missing_cache="$ancestor/missing/cache"
        mkdir -- "$ancestor"
        chmod 0700 "$ancestor"
        export DEPLOY_CACHE_HOME_OVERRIDE="$missing_cache"
        export FAKE_STAT_OWNER_PATH="$ancestor"
        export FAKE_STAT_OWNER_UID="$((EUID + 1))"
        if [[ "$mode" == dry-run ]]; then
            run_deploy "$fixture_repo" --dry-run
        else
            run_deploy "$fixture_repo"
        fi

        [[ "$deploy_status" -ne 0 ]] || \
            fail "missing cache beneath a foreign-owned ancestor must hard-fail in $mode mode"
        assert_output_contains 'ancestor|owner|owned|uid' \
            "foreign cache ancestor failure should explain the trust violation"
        [[ ! -e "$ancestor/missing" ]] || fail "$mode must not create directories beneath a foreign-owned ancestor"
        assert_rejected_before_snapshot_build_or_install
        unset DEPLOY_CACHE_HOME_OVERRIDE FAKE_STAT_OWNER_PATH FAKE_STAT_OWNER_UID
    done
}

test_rejects_existing_cache_paths_with_symlink_intermediate_component() {
    local mode
    local real_parent
    local symlink_parent
    local cache_home
    local cache_root

    for mode in actual dry-run; do
        make_fixture "existing-cache-symlink-intermediate-${mode}"
        real_parent="$fixture/cache-real-parent"
        symlink_parent="$fixture/cache-symlink-parent"
        cache_home="$symlink_parent/cache-home"
        cache_root="$cache_home/agent-hub-approvals"
        mkdir -p -- "$real_parent/cache-home/agent-hub-approvals"
        chmod 0700 "$real_parent/cache-home" "$real_parent/cache-home/agent-hub-approvals"
        ln -s -- "$real_parent" "$symlink_parent"
        [[ -d "$cache_home" && ! -L "$cache_home" ]] || fail "cache home fixture must appear to be a safe final directory"
        [[ -d "$cache_root" && ! -L "$cache_root" ]] || fail "cache root fixture must appear to be a safe final directory"
        export DEPLOY_CACHE_HOME_OVERRIDE="$cache_home"
        if [[ "$mode" == dry-run ]]; then
            run_deploy "$fixture_repo" --dry-run
        else
            run_deploy "$fixture_repo"
        fi

        [[ "$deploy_status" -ne 0 ]] || \
            fail "existing cache paths beneath a symlink component must hard-fail in $mode mode"
        assert_output_contains 'cache|ancestor|component|symlink' \
            "intermediate cache symlink failure should explain the trust violation"
        assert_rejected_before_snapshot_build_or_install
        unset DEPLOY_CACHE_HOME_OVERRIDE
    done
}

test_rejects_existing_cache_beneath_writable_intermediate_directory() {
    local mode
    local unsafe_ancestor
    local cache_home

    for mode in actual dry-run; do
        make_fixture "existing-cache-writable-intermediate-${mode}"
        unsafe_ancestor="$fixture/writable-cache-ancestor"
        cache_home="$unsafe_ancestor/safe-cache-home"
        mkdir -p -- "$cache_home/agent-hub-approvals"
        chmod 0770 "$unsafe_ancestor"
        chmod 0700 "$cache_home" "$cache_home/agent-hub-approvals"
        export DEPLOY_CACHE_HOME_OVERRIDE="$cache_home"
        if [[ "$mode" == dry-run ]]; then
            run_deploy "$fixture_repo" --dry-run
        else
            run_deploy "$fixture_repo"
        fi

        [[ "$deploy_status" -ne 0 ]] || \
            fail "safe final cache beneath a writable intermediate directory must hard-fail in $mode mode"
        assert_output_contains 'ancestor|writable|mode|permission' \
            "writable intermediate cache failure should explain the trust violation"
        assert_eq 770 "$(stat -c %a -- "$unsafe_ancestor")" \
            "deployment must not repair unsafe intermediate permissions"
        assert_rejected_before_snapshot_build_or_install
        unset DEPLOY_CACHE_HOME_OVERRIDE
    done
}

test_rejects_existing_cache_beneath_foreign_owned_intermediate_directory() {
    local mode
    local foreign_ancestor
    local cache_home

    for mode in actual dry-run; do
        make_fixture "existing-cache-foreign-intermediate-${mode}"
        foreign_ancestor="$fixture/foreign-cache-ancestor"
        cache_home="$foreign_ancestor/safe-cache-home"
        mkdir -p -- "$cache_home/agent-hub-approvals"
        chmod 0700 "$foreign_ancestor" "$cache_home" "$cache_home/agent-hub-approvals"
        export DEPLOY_CACHE_HOME_OVERRIDE="$cache_home"
        export FAKE_STAT_OWNER_PATH="$foreign_ancestor"
        export FAKE_STAT_OWNER_UID="$((EUID + 1))"
        if [[ "$mode" == dry-run ]]; then
            run_deploy "$fixture_repo" --dry-run
        else
            run_deploy "$fixture_repo"
        fi

        [[ "$deploy_status" -ne 0 ]] || \
            fail "safe final cache beneath a foreign-owned intermediate directory must hard-fail in $mode mode"
        assert_output_contains 'ancestor|owner|owned|uid' \
            "foreign-owned intermediate cache failure should explain the trust violation"
        assert_rejected_before_snapshot_build_or_install
        unset DEPLOY_CACHE_HOME_OVERRIDE FAKE_STAT_OWNER_PATH FAKE_STAT_OWNER_UID
    done
}

test_accepts_trusted_system_and_user_cache_ancestors() {
    local mode
    local system_ancestor
    local user_ancestor
    local cache_home

    for mode in actual dry-run; do
        make_fixture "trusted-system-user-ancestors-${mode}"
        system_ancestor="$fixture/system-cache-ancestor"
        user_ancestor="$system_ancestor/user-cache-ancestor"
        cache_home="$user_ancestor/cache-home"
        mkdir -p -- "$cache_home/agent-hub-approvals"
        chmod 0755 "$system_ancestor"
        chmod 0700 "$user_ancestor" "$cache_home" "$cache_home/agent-hub-approvals"
        export DEPLOY_CACHE_HOME_OVERRIDE="$cache_home"
        export FAKE_STAT_OWNER_PATH="$system_ancestor"
        export FAKE_STAT_OWNER_UID=0
        if [[ "$mode" == dry-run ]]; then
            run_deploy "$fixture_repo" --dry-run
        else
            run_deploy "$fixture_repo"
        fi

        assert_eq 0 "$deploy_status" \
            "$mode must accept root-owned non-writable system ancestors and EUID-owned non-writable user ancestors"
        assert_file_contains "$fixture/stat.log" "^%u\\|${system_ancestor//./\\.}$" \
            "cache validation must inspect the system ancestor owner"
        assert_file_contains "$fixture/stat.log" "^%a\\|${system_ancestor//./\\.}$" \
            "cache validation must inspect the system ancestor mode"
        assert_file_contains "$fixture/stat.log" "^%u\\|${user_ancestor//./\\.}$" \
            "cache validation must inspect the user ancestor owner"
        assert_file_contains "$fixture/stat.log" "^%a\\|${user_ancestor//./\\.}$" \
            "cache validation must inspect the user ancestor mode"
        unset DEPLOY_CACHE_HOME_OVERRIDE FAKE_STAT_OWNER_PATH FAKE_STAT_OWNER_UID
    done
}

test_missing_cache_under_trusted_ancestor_is_validated_without_dry_run_creation() {
    make_fixture missing-cache-trusted-ancestor
    trusted_ancestor="$fixture/trusted-cache-parent"
    missing_cache="$trusted_ancestor/missing/cache"
    mkdir -- "$trusted_ancestor"
    chmod 0700 "$trusted_ancestor"
    export DEPLOY_CACHE_HOME_OVERRIDE="$missing_cache"

    run_deploy "$fixture_repo" --dry-run
    assert_eq 0 "$deploy_status" "dry run should accept feasible cache creation beneath a trusted ancestor"
    [[ ! -e "$trusted_ancestor/missing" ]] || fail "dry run must not create feasible missing cache directories"

    run_deploy "$fixture_repo"
    assert_eq 0 "$deploy_status" "actual deploy should create cache beneath a trusted ancestor"
    for created_directory in "$trusted_ancestor/missing" "$missing_cache" \
        "$missing_cache/agent-hub-approvals"; do
        [[ -d "$created_directory" && ! -L "$created_directory" ]] || \
            fail "deployment must create a real cache directory: $created_directory"
        assert_eq "$EUID" "$(stat -c %u -- "$created_directory")" \
            "created cache directory must be owned by the invoking EUID"
        created_mode="$(stat -c %a -- "$created_directory")"
        (((8#$created_mode & 8#022) == 0)) || \
            fail "created cache directory must not be group/other writable: $created_directory (mode $created_mode)"
    done
    unset DEPLOY_CACHE_HOME_OVERRIDE
}

test_bootstraps_without_external_dirname() {
    local deploy_script="$package_dir/deploy.sh"

    if grep -Eq '(^|[^[:alnum:]_])dirname([^[:alnum:]_]|$)' "$deploy_script"; then
        fail "deployment script must derive paths without external dirname"
    fi
    assert_file_contains "$deploy_script" '\$\{script_path%/\*\}' \
        "deployment script must derive its directory with Bash parameter expansion"
    assert_file_contains "$deploy_script" 'builtin cd' \
        "deployment script must use builtin cd during bootstrap"
    assert_file_contains "$deploy_script" 'builtin pwd' \
        "deployment script must use builtin pwd during bootstrap"
}

test_reports_missing_git_prerequisite_before_using_git() {
    local bootstrap_bin

    make_fixture missing-git-bootstrap
    bootstrap_bin="$fixture/bootstrap-bin"
    mkdir -- "$bootstrap_bin"
    ln -s -- "$(command -v bash)" "$bootstrap_bin/bash"
    for command in cargo cmp install mkdir mktemp mv rm sha256sum stat tar; do
        ln -s -- "$(type -P true)" "$bootstrap_bin/$command"
    done

    set +e
    deploy_output="$(PATH="$bootstrap_bin" \
        HOME="$fixture_home" \
        XDG_CONFIG_HOME="$fixture_config" \
        XDG_CACHE_HOME="$fixture_cache" \
        "$fixture_repo/deploy/approvals-server/deploy.sh" --dry-run 2>&1)"
    deploy_status=$?
    set -e

    [[ "$deploy_status" -ne 0 ]] || fail "deployment must fail when git is unavailable"
    assert_output_contains '^error: required command not found: git$' \
        "missing git must use the explicit prerequisite error"
    if grep -Eqi 'git: command not found|not in a Git worktree' <<<"$deploy_output"; then
        fail "deployment must check the git prerequisite before trying to use git (output: $deploy_output)"
    fi
    assert_rejected_before_snapshot_build_or_install
}

test_ci_runs_deployment_syntax_harness_and_unit_verification() {
    local workflow="$repo_root/.github/workflows/ci.yml"

    assert_file_contains "$workflow" \
        'bash[[:space:]]+-n[[:space:]].*deploy/approvals-server/deploy\.sh' \
        "CI must syntax-check the deployment script"
    assert_file_contains "$workflow" \
        'bash[[:space:]]+-n[[:space:]].*deploy/approvals-server/tests/deploy_test\.sh' \
        "CI must syntax-check the deployment harness"
    assert_file_contains "$workflow" \
        'deploy/approvals-server/tests/deploy_test\.sh' \
        "CI must execute the deployment harness"
    assert_file_contains "$workflow" \
        'systemd-analyze[[:space:]]+verify[[:space:]].*agent-hub-approvals\.service' \
        "ubuntu CI must verify the packaged systemd unit"
}

test_service_shutdown_allows_long_poll_and_state_save() {
    local unit="$package_dir/agent-hub-approvals.service"
    assert_file_contains "$unit" '^KillSignal=SIGINT$' "service must stop the server with SIGINT"

    timeout_value="$(grep -E '^TimeoutStopSec=' "$unit" | cut -d= -f2)"
    case "$timeout_value" in
        *min) timeout_seconds=$((10#${timeout_value%min} * 60)) ;;
        *m) timeout_seconds=$((10#${timeout_value%m} * 60)) ;;
        *s) timeout_seconds=$((10#${timeout_value%s})) ;;
        *) timeout_seconds=$((10#$timeout_value)) ;;
    esac
    ((timeout_seconds >= 90)) || fail "TimeoutStopSec must be at least 90s (got $timeout_value)"
}

test_repeated_deploy_is_idempotent() {
    make_fixture idempotent
    run_deploy "$fixture_repo"
    assert_eq 0 "$deploy_status" "initial deployment should succeed"
    snapshot_before="$(
        sha256sum "$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server" \
            "$fixture_config/agent-hub/approvals-server.env" \
            "$fixture_config/systemd/user/agent-hub-approvals.service"
        stat -c '%a %n' "$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server" \
            "$fixture_config/agent-hub/approvals-server.env" \
            "$fixture_config/systemd/user/agent-hub-approvals.service"
    )"

    run_deploy "$fixture_repo/caller"
    assert_eq 0 "$deploy_status" "repeated deployment should succeed"
    snapshot_after="$(
        sha256sum "$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server" \
            "$fixture_config/agent-hub/approvals-server.env" \
            "$fixture_config/systemd/user/agent-hub-approvals.service"
        stat -c '%a %n' "$fixture_home/.local/libexec/agent-hub-approvals/agent-hub-server" \
            "$fixture_config/agent-hub/approvals-server.env" \
            "$fixture_config/systemd/user/agent-hub-approvals.service"
    )"
    assert_eq "$snapshot_before" "$snapshot_after" "repeated deploy must preserve installed bytes and modes"
    assert_no_systemd_invocation
}

run_test() {
    local test_name="$1"
    local test_function="$2"
    printf 'TEST %s\n' "$test_name"
    if ("$test_function"); then
        printf 'PASS %s\n' "$test_name"
        passed=$((passed + 1))
    else
        printf 'FAIL %s\n' "$test_name"
        failed=$((failed + 1))
    fi
}

run_test 'builds a captured HEAD snapshot from any cwd' test_builds_captured_head_snapshot_from_any_cwd
run_test 'uses only private snapshot output despite shared artifact injection' test_ignores_inherited_target_and_shared_artifact_injected_after_cargo
run_test 'rejects source mutation after building the captured snapshot' test_source_mutation_during_build_uses_snapshot_and_aborts_install
run_test 'rejects deployment-file mutation during build' test_deployment_file_mutation_during_build_aborts_install
run_test 'rejects a dirty tracked worktree' test_rejects_dirty_tracked_worktree
run_test 'rejects staged changes' test_rejects_staged_worktree
run_test 'rejects untracked files' test_rejects_untracked_worktree
run_test 'rejects a binary destination directory' test_rejects_binary_destination_directory
run_test 'rejects a binary destination symlink to a file' test_rejects_binary_destination_symlink_to_file
run_test 'rejects a binary destination symlink to a directory' test_rejects_binary_destination_symlink_to_directory
run_test 'rejects a unit destination directory' test_rejects_unit_destination_directory
run_test 'rejects a unit destination symlink to a file' test_rejects_unit_destination_symlink_to_file
run_test 'rejects a unit destination symlink to a directory' test_rejects_unit_destination_symlink_to_directory
run_test 'rejects an environment symlink race before mode enforcement' test_rejects_environment_symlink_race_before_mode_enforcement
run_test 'rejects an environment directory race' test_rejects_environment_directory_race
run_test 'rejects a non-0600 existing environment before build' test_rejects_existing_environment_with_nonrestrictive_mode_before_build
run_test 'preserves a concurrent regular-file environment edit' test_concurrent_environment_edit_survives_without_inode_replacement
run_test 'rejects an environment appearing during first install' test_rejects_environment_appearing_during_first_install
run_test 'reports an environment move failure without a conflict' test_reports_environment_move_failure_without_a_destination_conflict
run_test 'preserves existing parent directory modes' test_existing_parent_directory_modes_are_preserved
run_test 'preserves the binary when config staging fails' test_config_staging_failure_preserves_installed_binary
run_test 'preserves the binary when unit staging fails' test_unit_staging_failure_preserves_installed_binary
run_test 'installs packaged files with restrictive modes' test_first_deploy_installs_packaged_files_with_restrictive_modes
run_test 'preserves operator environment and state on redeploy' test_redeploy_preserves_operator_environment_and_state
run_test 'replaces the binary atomically' test_binary_update_is_atomic
run_test 'leaves systemd operation manual' test_deploy_never_invokes_systemd_and_manual_path_is_explicit
run_test 'dry run has no side effects' test_dry_run_builds_nothing_and_mutates_no_install_paths
run_test 'rejects invalid HOME paths before dry run or deploy' test_rejects_invalid_home_paths_before_dry_run_or_deploy
run_test 'rejects invalid XDG_CONFIG_HOME paths before dry run or deploy' test_rejects_invalid_config_home_paths_before_dry_run_or_deploy
run_test 'rejects invalid XDG_CACHE_HOME paths before dry run or deploy' test_rejects_invalid_cache_home_paths_before_dry_run_or_deploy
run_test 'rejects symlink cache authority directories' test_rejects_symlink_cache_authority_directories
run_test 'rejects nondirectory cache authority paths' test_rejects_nondirectory_cache_authority_paths
run_test 'rejects group/other-writable cache authority directories' test_rejects_group_or_other_writable_cache_authority_directories
run_test 'rejects foreign-owned cache authority directories' test_rejects_foreign_owned_cache_authority_directories
run_test 'rejects missing cache beneath a writable ancestor' test_rejects_missing_cache_beneath_writable_existing_ancestor
run_test 'rejects missing cache beneath a foreign-owned ancestor' test_rejects_missing_cache_beneath_foreign_owned_existing_ancestor
run_test 'rejects existing cache paths beneath a symlink component' test_rejects_existing_cache_paths_with_symlink_intermediate_component
run_test 'rejects existing cache beneath a writable intermediate directory' test_rejects_existing_cache_beneath_writable_intermediate_directory
run_test 'rejects existing cache beneath a foreign-owned intermediate directory' test_rejects_existing_cache_beneath_foreign_owned_intermediate_directory
run_test 'accepts trusted system and user cache ancestors' test_accepts_trusted_system_and_user_cache_ancestors
run_test 'validates missing cache beneath a trusted ancestor without dry-run creation' test_missing_cache_under_trusted_ancestor_is_validated_without_dry_run_creation
run_test 'bootstraps without external dirname' test_bootstraps_without_external_dirname
run_test 'reports a missing git prerequisite before using git' test_reports_missing_git_prerequisite_before_using_git
run_test 'allows enough graceful shutdown time' test_service_shutdown_allows_long_poll_and_state_save
run_test 'is idempotent when repeated' test_repeated_deploy_is_idempotent
run_test 'enforces deployment checks in CI' test_ci_runs_deployment_syntax_harness_and_unit_verification

printf '\nRESULT: %d passed, %d failed, %d total\n' "$passed" "$failed" "$((passed + failed))"
((failed == 0))
