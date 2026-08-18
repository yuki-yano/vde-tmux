# Releasing

Publishing is driven by Git tags.

## Local API v4 upgrade

API v4 replaces public API 3 with provider capabilities, guarded terminal mutations, exact pane
split, and agent start. Daemon protocol 16, PaneState 9, and private state format 1 do not change;
existing state remains readable and must not be reset. Public API 3 is not retained in parallel.

Before replacement, pass the release gates listed below plus
`scripts/test-agent-api-v4-isolated.sh`, `scripts/test-agent-prompt-isolated.sh`, and
`scripts/test-agent-operation-crash-isolated.sh`. Confirm `vt agent storage status --json` reports
zero `in_flight_operations`. Stage both binaries with
`cargo install --path . --locked --root <temporary-root>` and verify the staged schema reports API
4, protocol 16, PaneState 9, and private state 1.

Close running sidebars and run `vt daemon disable` before copying either executable so hooks cannot
restart a mixed binary generation during replacement. Back up both installed executables, replace
them from the staged root, verify their SHA-256 hashes, then run `vt daemon enable`. Reopen sidebars
and verify the installed schema, daemon health, hook ownership, one guarded copy-mode send on a
scratch server, and a SessionStart-free first Codex prompt. Do not reset PaneState or private Agent
state for this upgrade. On a failed copy or version check, keep the daemon disabled and restore both
executables from the same backup before re-enabling it.

## Historical initial API v3 cutover

The first API v3 cutover is a coordinated restart, not an in-place binary replacement. It changes
the public Agent API from 2 to 3, daemon protocol from 14 to 15, PaneState schema from 8 to 9, and
introduces private state format 1. No runtime compatibility path or PaneState v8 migration exists.
The v8 file is left untouched, while the v9 PaneState and private state start empty.

Before touching the installed binary:

1. Pass `cargo fmt --check`, Clippy with warnings denied, all normal and ignored tests, the three
   release smoke scripts, `scripts/test-agent-prompt-isolated.sh`, and
   `scripts/test-agent-operation-crash-isolated.sh`.
2. Stage `cargo install --path . --locked --root <temporary-root>` and verify the staged `vt`
   reports API 3, protocol 15, PaneState 9, and private state 1 from `vt api schema --json`. Record
   both staged executable hashes; these are the exact cutover candidates.
3. Stop new dispatches and waits. End supported-provider sessions from the old generation after
   confirming that no active execution, unresolved result, or delivery outcome still matters.
4. Close every running sidebar. An old sidebar client cannot reconnect across the protocol change.
5. Record the installed binary paths and hashes. Back up both installed executables outside the
   daemon-managed directories. Back up the state root only after the daemon is disabled below.

Run the cutover in this order, using the currently installed v14 client for the first command:

```sh
INSTALLED_VT="$(command -v vt)"
INSTALLED_VDE_TMUX="$(command -v vde-tmux)"
CUTOVER_BACKUP="$(mktemp -d "${TMPDIR:-/tmp}/vde-tmux-v14-backup.XXXXXX")"
STAGED_ROOT="<temporary-root-used-by-the-passed-staged-install>"
CANDIDATE_VT="$STAGED_ROOT/bin/vt"
CANDIDATE_VDE_TMUX="$STAGED_ROOT/bin/vde-tmux"
STATE_ROOT="${XDG_STATE_HOME:-$HOME/.local/state}/vde-tmux"
CANDIDATE_VT_SHA="$(shasum -a 256 "$CANDIDATE_VT" | awk '{print $1}')"
CANDIDATE_VDE_TMUX_SHA="$(shasum -a 256 "$CANDIDATE_VDE_TMUX" | awk '{print $1}')"

install -m 0755 "$INSTALLED_VT" "$CUTOVER_BACKUP/vt"
install -m 0755 "$INSTALLED_VDE_TMUX" "$CUTOVER_BACKUP/vde-tmux"
shasum -a 256 "$INSTALLED_VT" "$INSTALLED_VDE_TMUX"
shasum -a 256 "$CANDIDATE_VT" "$CANDIDATE_VDE_TMUX"
printf 'rollback backup: %s\n' "$CUTOVER_BACKUP"

tmux list-panes -a -F '#{window_id} #{@vde_sidebar}' \
  | awk '$2 == "1" { print $1 }' \
  | sort -u \
  | while IFS= read -r window_id; do
      vt sidebar close --window "$window_id"
    done

vt daemon disable
if [ -d "$STATE_ROOT" ]; then
  cp -R "$STATE_ROOT" "$CUTOVER_BACKUP/state"
fi

install -m 0755 "$CANDIDATE_VT" "$INSTALLED_VT"
install -m 0755 "$CANDIDATE_VDE_TMUX" "$INSTALLED_VDE_TMUX"
test "$(shasum -a 256 "$INSTALLED_VT" | awk '{print $1}')" = "$CANDIDATE_VT_SHA"
test "$(shasum -a 256 "$INSTALLED_VDE_TMUX" | awk '{print $1}')" = "$CANDIDATE_VDE_TMUX_SHA"

vt daemon enable
vt daemon status
vt api schema --json | jq -e '
  .meta.api_version == 3 and
  .result.contract.versions == {
    public_agent_api: 3,
    daemon_protocol: 15,
    pane_state_schema: 9,
    private_state_format: 1
  }'
vt agent storage status --json
```

`vt daemon disable` removes the owned hooks, records disabled mode, and stops the old daemon before
either executable is replaced. This closes the window where a focus or provider hook could restart
the v14 daemon during installation. If an executable copy or hash comparison fails, leave the
server disabled, restore both executables from `CUTOVER_BACKUP`, verify their hashes, and run the
restored `vt daemon enable`. If the binary was replaced before disabled mode was recorded, the new
`vt daemon disable` revalidates and force-stops the incompatible recorded daemon. Do not restart the
tmux server as a protocol recovery shortcut.

After the version checks pass, reopen sidebars and restart Claude Code and Codex sessions so their
SessionStart hooks are observed by the new generation. Verify one guarded Codex prompt, operation
resume across daemon restart, run completion, response artifact read, and current-run recovery on
the real server. Keep the dotfiles bridge on its raw transport until every API v3 rollout DoD item
is complete; Claude Code durable mutation remains disabled.

For rollback, first stop new dispatches and waits, run `vt daemon disable` with the v15 binary,
restore both recorded v14 executables from the external backup, verify their hashes, and run the
restored `vt daemon enable`. Restore the offline state backup only when rollback requires the exact
pre-cutover state; the untouched PaneState v8 file is otherwise authoritative again. Trigger
rollback on protocol/version mismatch, daemon health failure, duplicate dispatch, failed restart
resume, failed response read, or an unrecoverable private-state startup error. Do not copy
v9/private-state records into the v8 state root.

For the first release that switches pane persistence to the private full-state snapshot, perform
the upgrade only while every agent is Idle and no Done or Blocked state must be retained. Pane
state from the former tmux-option storage is not migrated.

1. Bump `version` in `Cargo.toml` and `Cargo.lock`.
2. Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, `cargo test --locked -- --ignored`, and `cargo publish --dry-run --locked`.
3. Run the isolated local preflight: `scripts/smoke-m6-runtime.sh`, `scripts/preflight-ui-ux.sh`, and `scripts/test-kill-server-isolated.sh`. These use scratch `tmux -L` servers and isolated state directories; they do not touch the real server or normal state.
4. Run the `Runtime smoke` workflow with `workflow_dispatch`. Confirm the runtime smoke passes and the ignored redraw probes either pass on tmux 3.7+ or report an explicit version-based skip.
5. Commit the version bump and release changes.
6. Create a tag that matches the crate version:

   ```sh
   git tag v0.1.2
   git push origin main
   git push origin v0.1.2
   ```

The `Publish` workflow validates that `vX.Y.Z` matches `Cargo.toml` before publishing.

crates.io Trusted Publishing must be configured once for:

- owner: `yuki-yano`
- repository: `vde-tmux`
- workflow: `publish.yml`
- environment: `crates-io`
