# Detect Compatibility Notes

## Source Reviewed

- Read-only reference: `~/repos/github.com/yuki-yano/vde-tmux-sidebar`
- Main file checked: `crates/sidebar-core/src/detect.rs`
- Related files checked by search: `crates/sidebar-cli/src/daemon.rs`, `crates/sidebar-cli/src/hook.rs`, `crates/sidebar-cli/src/client.rs`

The old implementation spreads detection behavior across pure detect helpers,
hook mapping, daemon polling, and display rollup tests.
The new implementation keeps hook contracts in `src/hook/*` and implements only
daemon-side display inference here.

## Adopted Cases

- Codex permission screen is detected from adjacent question and choice lines.
- A standalone or distant `yes` string does not trigger permission detection.
- `capture-pane` inference updates the daemon snapshot clone only.
- `@vde_status=running` panes with stale activity are demoted for display.
- Hook-provided `wait_reason=permission_prompt` continues to roll up as permission through `pane_rollup_level`.
- Panes with no agent or sidebar marker are excluded from screen polling.

## Narrowed Cases

- Old Codex screen prompt variants are represented by `permission_prompt` only.
  The new code does not preserve old `codex_screen_prompt` vs hook-origin reason
  distinctions because the current display contract only needs permission rollup.
- Claude screen polling is treated as a future extension unless it can use the
  same adjacent question/choice detector without broad false positives.
- Old progress/subagent decoding remains in hook writer tests and is not part of
  daemon screen detection.

## Not Adopted

- Old `@pane_*` option names are not supported; the new contract is `@vde_*`.
- Old protocol-specific wait reason names are not emitted.
- Old client-side direct rendering detection paths are not kept because sidebar
  rendering now consumes daemon snapshots.
- Manual compatibility fallbacks for missing daemon are not added; sidebar
  commands ensure the daemon is started.

## New Tests

- `detect::tests::does_not_detect_yes_when_permission_question_is_not_adjacent`
- `detect::tests::detects_codex_permission_prompt_with_adjacent_choice`
- `daemon::workers::tests::tmux_worker_applies_capture_pane_detection`
- `daemon::workers::tests::stale_running_is_demoted_in_snapshot_only`
