# Stable approvals-server bridge

This temporary package deploys the pinned approvals-only Agent Hub server. It
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
cd deploy/approvals-server
./deploy.sh --dry-run
./deploy.sh
```

The script verifies the pinned source binary, copies only the binary,
environment, and unit, and does not invoke systemd or touch state. Deployment
does not cut over the running server automatically.

## Manual cutover

1. Verify the installed unit with `systemd-analyze --user verify
   agent-hub-approvals.service`, then run `systemctl --user daemon-reload`.
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

5. Start `agent-hub-approvals.service`, then check its status and
   `curl --fail http://127.0.0.1:8080/health`.
6. Leave `agent-hub-approvals.service` disabled by default; enable it only if automatic user-session startup is later desired.
