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

Review a dry run. After the old process has completed its SIGINT save, capture
the stopped live installation and perform an explicit cutover:

```sh
deploy/approvals-server/deploy.sh --dry-run
kill -INT "$old_pid"
wait "$old_pid"
deploy/approvals-server/deploy.sh --capture-rollback "$old_pid"
deploy/approvals-server/deploy.sh --cutover
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
output. Deployment requires the live approvals binary, state, environment, and
unit to be regular files. Rollback capture is a mandatory separate phase. It
verifies the reported old PID has exited, proves the installed listen address is
unbound, validates a private copy of the final SIGINT-saved state with the target
binary, and only then captures the exact live bytes and modes in a new owner-only
rollback slot. The manifest binds the slot to the target source commit.
`--cutover` hard-fails if the slot is missing, belongs to another commit, fails
integrity validation, or no longer exactly matches any live file or mode.

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

An existing environment must be a regular non-symlink file with exact mode
`0600`; otherwise deployment stops with a `chmod 600 <path>` remediation. Valid
installed environments are left untouched, including the accepted standalone
`AUTH_MODE=none` and `LISTEN_ADDR=0.0.0.0:8080` configuration. The managed unit is
atomically updated from the same captured snapshot with mode
`0644`; service state and its bytes are never touched. A dry run validates the
repository, packaged files, and existing destinations and reports the build,
install, and configuration behavior without creating snapshot, build, or
installation directories.

The script does not invoke systemd or modify the live state. Stop/start remains an
explicit operator action.

During `--capture-rollback`, the script exports and builds the same captured clean
HEAD required by cutover, copies the final live state into its private deployment
workspace, and starts the standalone approvals server against only that copy. It
proves `/health`, the mobile
`/approvals/queue`, and `/api/v1/sessions`, sends the configured `SIGINT`, waits
for a successful stop/save, and verifies that the source state's SHA-256 did not
change. The capture phase installs nothing, does not invoke systemd, and removes
the copied state with the private workspace.

## Manual cutover

1. Quiesce writes and identify the exact old server PID. Disable its supervisor if
   it has one.
2. Send that PID `SIGINT`, wait until it exits successfully, and verify the final
   state save completed. Do not copy or capture state before this wait returns.
3. Run `--capture-rollback "$old_pid"`. This confirms the PID and listener are
   stopped, validates the final state copy, and captures binary, state,
   environment, and unit together.
4. Run `--cutover`. It revalidates that the stopped live installation still
   exactly matches the selected rollback bundle before installing anything.
5. Verify the installed unit, then reload the user manager:

   ```sh
   systemd-analyze --user verify agent-hub-approvals.service
   systemctl --user daemon-reload
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

The rollback command validates the slot manifest and all `integrity_digest` values,
confirms the captured stopped PID remains absent, parses the installed environment and
proves its listener address is unbound, and requires an inactive/dead unit with no main
PID when the user manager can identify the unit. Only then does it stage all four files
and atomically replace each approvals binary, state, environment, and unit file. It does
not change systemd state.

Each file replacement is atomic, but the four replacements are not one filesystem
transaction. If rollback exits after replacing only some files, keep the service stopped,
correct the reported destination or filesystem problem, and rerun the same rollback
command. The selected immutable slot remains the source for every file, so a successful
rerun replaces and verifies the complete binary, state, environment, and unit set. Do not
start or reload the service between a partial failure and the successful rerun.

For a coordinated rollback, perform the orchestration v4 drain and v3 preflight
before replacing the v4-capable API binary, then stop and restore the approvals
service and its state from this bundle. See `docs/local-orchestration-dev.md` for
the complete order.

## Release inputs

The release artifacts produced here contain only the approvals server and gateway.
The reviewed Dippy policy/configuration, OpenCode/plugin installation, and
`agent-hub-configs` revision are externally pinned deployment inputs. Their own tag
workflows are not prerequisites for publishing an approvals/gateway tag; record
their immutable pins in the release evidence instead.
