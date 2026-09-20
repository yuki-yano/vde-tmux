# Agent Instructions

## Git

- Write all commit messages in English.
- Do not create commits unless the user explicitly asks for a commit.
- When committing, follow the style of recent commit messages where practical.

## Scripts

These live in `scripts/` and all run against an isolated `tmux -L <scratch>` server, never the real one. See `docs/e2e-smoke.md` for the manual walkthrough.

- `scripts/smoke-m6-runtime.sh`: normal runtime-contract smoke test. Confirms session ordering, category resolution and switching, multi-client attention, Blocked notifications, statusline content, two-sidebar interaction state, and the daemon lifecycle in one isolated run. Run it after changes to daemon behavior, read/state semantics, category switching, or other runtime contracts.
- `scripts/smoke-m6-runtime.sh --extended`: adds the 58-pane status-delivery check and category warm-switch distribution/SLA to the normal runtime smoke. Run it after changes to the category performance path or high-volume status delivery.
- `scripts/preflight-ui-ux.sh`: multi-client UI/UX preflight against a scratch server. Run it for statusline/sidebar rendering or other visual-only changes.
- `scripts/test-kill-server-isolated.sh`: exercises the session-manager kill-server / tmux-server shutdown path in isolation. Run it when changing the session manager, the kill-server flow, or daemon/tmux shutdown handling.

The manually dispatched `Runtime smoke` GitHub Actions workflow runs the extended runtime smoke. Before a release, run `scripts/smoke-m6-runtime.sh --extended`, the UI/UX preflight, and the kill-server test once each; do not also repeat the normal runtime smoke. The UI/UX preflight and kill-server test remain local-only.

## Release

- Follow `RELEASING.md` for the release procedure.
- Publish to crates.io only through the tag-triggered GitHub Actions workflow in `.github/workflows/publish.yml`; do not run `cargo publish` locally.
- After committing the version bump and passing the release preflight, create a matching `vX.Y.Z` tag, push the branch and tag, monitor the `Publish` workflow to completion, and verify the published crates.io version.
- Stop before pushing the release tag if any preflight check fails. If the workflow fails after the tag is pushed, report the partial release state and do not claim that the version was published.
