# Stable approvals-server bridge

This temporary package deploys the approvals-only Agent Hub server. It
is deliberately isolated from the reactive branch and has no reactive-agent,
workflow, task, artifact, workspace, or `CONFIG_ROOT` configuration.

## Paths

- Binary: `~/.local/libexec/agent-hub-approvals/agent-hub-server`
- Environment: `${XDG_CONFIG_HOME:-$HOME/.config}/agent-hub/approvals-server.env`
- User unit: `${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/agent-hub-approvals.service`
- State: `${XDG_STATE_HOME:-$HOME/.local/state}/agent-hub-approvals/server_data.json`

## Deploy

Review a dry run, then deploy:

```sh
deploy/approvals-server/deploy.sh --dry-run
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
atomically installs the resulting binary and verifies the installed bytes against
the build output.

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

The script does not invoke systemd. Restart is intentionally separate so an
operator controls activation of the newly installed binary and unit.

## Manual cutover

1. Verify the installed unit, then reload the user manager:

   ```sh
   systemd-analyze --user verify agent-hub-approvals.service
   systemctl --user daemon-reload
   ```
2. Identify the exact old server PID. The current process is interactive and
   unmanaged; if that changes, disable its supervisor before continuing.
3. Send that PID `SIGINT`, wait until it exits, and verify port 8080 has no
   listener before copying state or starting the unit.
4. Derive the state location, create it with mode `0700`, and copy final state:

   ```sh
   state_home="${XDG_STATE_HOME:-$HOME/.local/state}"
   install -d -m 0700 "$state_home/agent-hub-approvals"
   rsync --archive --chmod=F600 /path/to/final/server_data.json \
     "$state_home/agent-hub-approvals/server_data.json"
   ```

5. Start `agent-hub-approvals.service` for the first activation or apply a
   redeployment with an explicit restart, then check its status and health:

   ```sh
   systemctl --user restart agent-hub-approvals.service
   systemctl --user status agent-hub-approvals.service
   curl --fail http://127.0.0.1:8080/health
   ```
6. Leave `agent-hub-approvals.service` disabled by default; enable it only if automatic user-session startup is later desired.
