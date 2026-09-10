# Stable approvals-server bridge

This package builds and deploys the approvals-only Agent Hub server. It
is deliberately isolated from the reactive branch and has no reactive-agent,
workflow, task, artifact, workspace, or `CONFIG_ROOT` configuration.

## Paths

- Binary: `~/.local/libexec/agent-hub-approvals/agent-hub-server`
- Environment: `${XDG_CONFIG_HOME:-$HOME/.config}/agent-hub/approvals-server.env`
- User unit: `${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/agent-hub-approvals.service`
- State: `${XDG_STATE_HOME:-$HOME/.local/state}/agent-hub-approvals/server_data.json`
- Rollback slots: `${XDG_CACHE_HOME:-$HOME/.cache}/agent-hub-approvals/rollback`

## Deploy

Review a dry run, then deploy:

```sh
deploy/approvals-server/deploy.sh --dry-run
deploy/approvals-server/deploy.sh --validate-legacy-state /path/to/current/server_data.json
deploy/approvals-server/deploy.sh
```

The script locates its containing repository, so it works from any current
directory. It requires a clean checkout, including no staged or untracked files,
then captures the full HEAD commit. For a deployment it exports that commit to a
private workspace under
`${XDG_CACHE_HOME:-$HOME/.cache}/agent-hub-approvals`, builds with a private
`CARGO_TARGET_DIR` inside that workspace, and removes the workspace afterward.
An inherited `CARGO_TARGET_DIR` is deliberately ignored so the installed binary
can only come from the captured source and private build. Before installation,
the script confirms that the original HEAD and clean worktree are unchanged. It
atomically installs the resulting approvals-only binary under the established
`agent-hub-server` filename and verifies the installed bytes against the build
output. The package never supplies a separately built orchestration API server as
an approvals rollback artifact. Deployment requires the live approvals
binary, state, environment, and unit to be regular files. It captures their exact
bytes and modes in a new owner-only rollback slot, writes a checksum manifest, and
atomically selects that slot before replacing the approvals-only binary and unit.

Effective `HOME`, `XDG_CACHE_HOME`, and `XDG_CONFIG_HOME` paths must be absolute
and contain no explicit `.` or `..` components. Every existing component from
`/` through the cache and deployment cache paths must be a real, non-symlink
directory owned by root or the invoking user and not writable by group or
others. Existing final cache directories must be owned by the invoking user.
For a missing cache tail, every component through its nearest existing ancestor
must meet the ancestor requirements. A dry run checks this without creating
directories; deployment creates missing cache directories privately, validates
the complete chain again, and leaves permissions on existing directories
unchanged.

On the first deployment, the packaged environment is installed with mode
`0600`. An existing environment must already be a regular non-symlink file with
exact mode `0600`; otherwise deployment stops with a `chmod 600 <path>`
remediation. Valid existing environments are left untouched. The managed unit
is atomically updated from the same captured snapshot with mode
`0644`; service state and its bytes are never touched. A dry run validates the
repository, packaged files, and existing destinations and reports the build,
install, and configuration behavior without creating snapshot, build, or
installation directories.

The script does not invoke systemd or modify the live state. Stop/start remains an
explicit operator action. An existing `AUTH_MODE=none` environment, including the
accepted standalone `LISTEN_ADDR=0.0.0.0:8080` configuration, is preserved.

Before cutover, `--validate-legacy-state` exports and builds the same captured
clean HEAD used by deployment, copies the specified regular non-symlink legacy
state into its private deployment workspace, and starts the standalone approvals
server against only that copy. It proves `/health`, the mobile
`/approvals/queue`, and `/api/v1/sessions`, sends the configured `SIGINT`, waits
for a successful stop/save, and verifies that the source state's SHA-256 did not
change. The validation mode installs nothing, does not invoke systemd, and removes
the copied state with the private workspace. It records the validated source
checksum in the private deployment cache. Deployment hard-fails unless that
checksum still matches the exact live state, so validation must precede stopping
the old process and cutover.

## Manual cutover

1. Quiesce writes and run `--validate-legacy-state` against the live state. The
   validator reads and tests only a private copy and verifies the source bytes did
   not change.
2. Verify the installed unit, then reload the user manager:

   ```sh
   systemd-analyze --user verify agent-hub-approvals.service
   systemctl --user daemon-reload
   ```
3. Identify the exact old server PID. The current process is interactive and
   unmanaged; if that changes, disable its supervisor before continuing.
4. Send that PID `SIGINT`, wait until it exits, and verify port 8080 has no
   listener before copying state or starting the unit.
5. Derive the state location, create it with mode `0700`, and copy final state:

   ```sh
   state_home="${XDG_STATE_HOME:-$HOME/.local/state}"
   install -d -m 0700 "$state_home/agent-hub-approvals"
   rsync --archive --chmod=F600 /path/to/final/server_data.json \
     "$state_home/agent-hub-approvals/server_data.json"
   ```

6. Start `agent-hub-approvals.service` for the first activation or apply a
   redeployment with an explicit restart, then check its status and health:

   ```sh
   systemctl --user restart agent-hub-approvals.service
   systemctl --user status agent-hub-approvals.service
   curl --fail http://127.0.0.1:8080/health
   ```
7. Leave `agent-hub-approvals.service` disabled by default; enable it only if automatic user-session startup is later desired.

## Rollback

Stop the approvals service, then restore the selected owner-private rollback slot:

```sh
deploy/approvals-server/rollback.sh
systemctl --user daemon-reload
systemctl --user start agent-hub-approvals.service
```

The rollback command validates the slot manifest and all integrity digests before
staging all four files and atomically replacing each approvals binary, state,
environment, and unit file. It does not invoke systemd. The manifest reader accepts the former
`signed_by_digest` spelling, while newly written manifests use
`integrity_digest`.

This rollback is independent of orchestration API protocol/database rollback.
See `docs/local-orchestration-dev.md` for the API server's v4-to-v3 and older
preflights.
