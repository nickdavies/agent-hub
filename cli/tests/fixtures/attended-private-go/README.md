# Attended V3 private-Go proof fixtures

The ignored tests provide two proof levels:

* `git_view::tests::real_docker_private_go_two_linked_worktree_fixture` is fully
  local and deterministic. It uses `private_go_http_origin.py`, a synthetic
  host-scoped token, an isolated Docker network, and a local bare Git module.
  No production credential is read.
* `attended::tests::real_docker_attended_v3_private_go_multi_worktree_host_proof`
  runs the complete attended V3 propose/authorize/run/cleanup path. It is
  fail-closed unless real credential execution and cleanup are separately
  acknowledged.

The local Docker fixture requires a Linux Docker daemon and a **local immutable
image reference** supplied as `AGENT_HUB_PRIVATE_GO_TEST_IMAGE`. The image must
contain `sh`, `git`, `python3`, and the pinned local Go toolchain reported by
`AGENT_HUB_PRIVATE_GO_TEST_GO_VERSION` (for example, `go1.23.12`). Run it with:

```sh
AGENT_HUB_PRIVATE_GO_DOCKER_INTEGRATION=1 \
AGENT_HUB_PRIVATE_GO_ALLOW_CLEANUP=1 \
AGENT_HUB_PRIVATE_GO_TEST_IMAGE='repository@sha256:...' \
AGENT_HUB_PRIVATE_GO_TEST_GO_VERSION='go1.23.12' \
cargo test -p agent-hub real_docker_private_go_two_linked_worktree_fixture \
  -- --ignored --exact
```

The real host proof prerequisites are listed in the repository test's gate
errors. It requires all of the following:

* Linux, `git`, Rust/Cargo, a working Docker CLI/daemon, and the attended
  immutable job-code image already available to Docker.
* A canonical authoritative config root whose selected environment allows
  exactly the `github` network resource and the
  `github:github.snooguts.net:read_write` overlay.
* That overlay must bind `https://github.snooguts.net/api/v3`, credential
  provider `github:github.snooguts.net:read_write`, target
  `/agent-hub/github/tokens/github.snooguts.net`, and repository HTTPS remotes.
  The provider command must be exactly `gh auth token --hostname
  github.snooguts.net`; `gh auth status --hostname github.snooguts.net` must
  already succeed non-interactively.
* Two existing, canonical **linked** worktrees under config-approved roots.
  `primary` needs `workspace_rw` and `git_metadata`; `sibling` needs
  `workspace_ro` and `git_metadata`. Both must have an HTTPS origin on
  `github.snooguts.net`. The primary Go module must already require the private
  module named below; the sibling should contain the sentinel expected by the
  reviewed prompt.
* An existing private state-parent directory owned by the invoking user.
* Any model credential required by the selected automatic OpenCode runner.

Run only after reviewing the worktrees and cleanup scope:

```sh
AGENT_HUB_ATTENDED_PRIVATE_GO_INTEGRATION=1 \
AGENT_HUB_ATTENDED_PRIVATE_GO_REAL_CREDENTIALS=1 \
AGENT_HUB_ATTENDED_PRIVATE_GO_ALLOW_CLEANUP=1 \
AGENT_HUB_ATTENDED_PRIVATE_GO_CONFIG_ROOT=/canonical/config \
AGENT_HUB_ATTENDED_PRIVATE_GO_PRIMARY_WORKTREE=/canonical/worktree-a \
AGENT_HUB_ATTENDED_PRIVATE_GO_SIBLING_WORKTREE=/canonical/worktree-b \
AGENT_HUB_ATTENDED_PRIVATE_GO_STATE_PARENT=/canonical/private-state-parent \
AGENT_HUB_ATTENDED_PRIVATE_GO_ENVIRONMENT=code-auto \
AGENT_HUB_ATTENDED_PRIVATE_GO_IMAGE_FAMILY=job-code \
AGENT_HUB_ATTENDED_PRIVATE_GO_NETWORK_CLASS=build \
AGENT_HUB_ATTENDED_PRIVATE_GO_MODULE=github.snooguts.net/owner/private-module \
cargo test -p agent-hub \
  attended::tests::real_docker_attended_v3_private_go_multi_worktree_host_proof \
  -- --ignored --exact
```

Test worktrees are caller-owned and are never removed, reset, committed,
checked out, or pruned by the harness. The primary worktree is expected to
retain the reviewed formatting/module artifacts; sibling HEAD and status must
remain byte-for-byte equivalent to their starting observations.
