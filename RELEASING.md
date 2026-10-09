# Releasing

Publishing to crates.io is driven by Git tags.
A local binary upgrade without publishing is described in [Local binary upgrade](#local-binary-upgrade).

## Release

1. Bump `version` in `Cargo.toml` and `Cargo.lock`.
2. Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, `cargo test --locked --test tmux_redraw_probe tmux_expands_dynamic_pane_elapsed_with_the_former_boundaries -- --ignored --exact`, and `cargo publish --dry-run --locked`.
3. With Bash 5 or newer selected on `PATH` (`/bin/bash` 3.2 on macOS is unsupported for the runtime SLA clock), run the isolated local preflight once each: `scripts/smoke-m6-runtime.sh --extended`, `scripts/preflight-ui-ux.sh`, and `scripts/test-kill-server-isolated.sh`. The extended run includes the normal runtime smoke, so do not also run it without `--extended`. These use scratch `tmux -L` servers and isolated state directories; they do not touch the real server or normal state.
4. Run the `Runtime smoke` workflow with `workflow_dispatch`. Confirm the extended runtime smoke and the ignored elapsed-format integration pass on the pinned tmux version.
5. Commit the version bump and release changes.
6. Create a tag that matches the crate version:

   ```sh
   git tag v0.1.2
   git push origin main
   git push origin v0.1.2
   ```

The `Publish` workflow validates that `vX.Y.Z` matches `Cargo.toml` before publishing.

When updating the pinned tmux dependency or relying on a new tmux redraw characteristic, dispatch
the separate `Tmux redraw probe` workflow. Its redraw-only probe is intentionally not part of the
normal release gate because it measures tmux itself rather than `vt` behavior.

crates.io Trusted Publishing must be configured once for:

- owner: `yuki-yano`
- repository: `vde-tmux`
- workflow: `publish.yml`
- environment: `crates-io`

## Local binary upgrade

A local install replaces the installed `vt` and `vde-tmux` executables without a crate version
bump or a release tag. The current generation is Agent API 7, daemon protocol 32, PaneState schema
11, private state format 1, and Question sidecar schema 1. CLI, daemon, and sidebars must run the
same generation. Keep the existing pane state, Runs, and question-notice sidecar.

1. Pass formatting, Clippy, tests, and the checks applicable to the change under `AGENTS.md`.
   Reuse successful results for unchanged code. A local binary replacement does not by itself
   require the extended runtime smoke, UI/UX preflight, kill-server test, or a long question load
   test; the full release preflight above remains required before publishing.
   For question-notice changes, select the checks by the affected path:
   - Run `python3 scripts/test-question-notice-isolated.py` after changes to the question
     hook/API/Q path.
   - Add `--extended` when changing ordinary-prompt auto-ack scheduling, capture admission, status
     delivery performance, or their resource limits, or when investigating an unresolved concern
     in those paths. Do not add it solely for reply-ID parsing, reply-worker changes,
     documentation, or installation. Its gate is a condition-wise p95 increase of at most 50ms,
     zero normal capture failures, at most 20MiB RSS growth, and retained notices on probe drop.
   - Run `python3 scripts/test-question-notice-isolated.py --reply-load-only` when changing
     reply-worker admission, queueing, execution budgets, retries, journal waits, or shared
     mutation/IO scheduling. It runs valid ID-matched reply workers with 58 panes, two clients,
     and three concurrent producers (two ABBA cycles, 200 status samples per condition). Its gate
     is acknowledgement during active load, bounded RSS growth, zero normal capture failures,
     status p95 within a 2s/1.5x budget, and no terminal reply/journal failure. Every submitted
     reply must settle before manual Q: accepted frames and acknowledged notice orders equal the
     submitted cycles, including combined prefix advancement. Per-phase counters and retention
     reasons are saved as evidence. This check does not replace an applicable `--extended` run.
   - Do not run both load tests for every question change. Reuse accepted evidence for unchanged
     product code, and distinguish a re-evaluation of saved data from a new run.
2. Stage both binaries with `cargo install --path . --locked --root <temporary-root>`.
   Confirm `vt api schema --json` reports API 7, protocol 32, PaneState 11, and private state 1.
   Validate the staged binaries on a scratch server before replacing the installed generation:
   `VDE_VT_BIN=<temporary-root>/bin/vt python3 scripts/test-codex-observation-isolated.py`.
3. Confirm the installed `vt agent storage status --json` reports zero `in_flight_operations`.
   Record sidebar windows, widths, active panes, client focus, installed paths, and executable hashes.
   Enumerate all live tmux incarnations and daemons using the executables being replaced. Include
   each affected server in the disabled-daemon cutover and state conversion; do not leave another
   live server on the old binary generation. Inactive historical incarnations are left untouched.
4. Close running sidebars using the installed client, then run its `vt daemon disable` so hooks
   cannot restart the old daemon during replacement. Back up both executables and the stopped
   server's state directory outside daemon-managed storage.
   For a schema 10 installation, perform the one-time offline conversion below before enabling
   the new daemon. The new binary rejects an unconverted schema 10 snapshot; it never treats
   that existing state as an empty installation.
5. Replace both executables from the staged root, verify hashes and schema, then run
   `vt daemon enable`. Require `Serving / Healthy / Ready`, no transition error, and healthy
   Agent storage. Restore the recorded sidebar widths and focus using the new client; set
   `TMUX_PANE` to each window's recorded content pane when reopening its sidebar.
6. Verify each sidebar is alive and uses the installed executable, the private-state generation
   and retained Runs are preserved, and live Codex panes expose healthy `question_notice` summaries.
   Existing stock PostToolUse hooks must include `request_user_input_async`; Embedded mode is
   required. See [Codex question notices](README.md#codex-question-notices).
7. `in_flight_operations` counts only Prepared/DispatchStarted Operations, so an existing
   `delivery_unknown` Operation does not block the replacement. Inspect such Operations and their
   provider queues before using
   [manual abandonment](AGENT_API.md#delayed-prompt-confirmation-and-manual-fence-release);
   do not abandon or resend automatically during installation. Unlinked Runs stored by daemon
   protocols before 29 have no prompt digest and cannot be matched retrospectively. Rolling back
   to a binary before protocol 29 restores the dispatch fence of operator-abandoned Operations.

Never reset state or restart the tmux server as a protocol recovery shortcut. If replacement
fails, leave the daemon disabled until both executables and their hashes are coherent, for
example by restoring both from the same backup.

## One-time PaneState schema 10 → 11 conversion

This is an installation operation while the daemon is disabled, not a runtime compatibility
path. First back up the stopped incarnation state directory outside daemon-managed storage.
Keep its server identity, every existing record field, Runs/Operations, and Question sidecar.
Change only the outer and record `schema_version` from 10 to 11, adding these fields per record:

```json
{
  "claude_background": { "tasks": [], "faults": [], "paused": false, "paused_at": null },
  "claude_crons": { "entries": [], "observed_at": null }
}
```

Write the result as a new `pane-state-v11.json` in the same private incarnation directory
(mode 0700), using a private temporary file (0600), fsync, atomic rename, and directory fsync.
Reject unexpected source versions, pre-existing destination files, symlinks, or insecure owners
and permissions. Keep `pane-state-v10.json` unchanged until the new daemon has successfully loaded v11. Then
move v10 into the existing backup outside daemon-managed storage and fsync both directories.
Leaving v10 beside v11 would block a later intentional reset and invite stale-state reconversion.
If v11 is intentionally removed to reset state, archive the old v10 instead of converting it again.
Validate the converted records with the staged binary on an isolated server before performing
the real conversion: change only `server_identity` in the validation copy to the scratch server
identity so its strict identity check passes. All record fields still undergo validation. The real
conversion must retain the original server identity unchanged. No notifications or
reservations that predate this feature are inferred. Enable the new daemon only after both
installed executables match the staged hashes; verify retained pane identities, epochs, run and
completion sequences, unread state, storage generation, Runs, and Question notice orders. For each live affected incarnation, either preserve and convert its
state or obtain explicit approval to discard it; never silently reset a second server.

The Claude hooks must include synchronous `PostToolUse`, `UserPromptSubmit`, `Stop`, and
`SessionEnd`; preserve unrelated hooks and settings. Confirm `badge.glyphs.awaiting_result`
resolves to the intended glyph. Verify one ordinary Claude wait with a resident command,
result-wait → response → Done, and one cron addition/deletion after installing.
