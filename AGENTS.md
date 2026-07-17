# Agent Instructions

## Git

- Write all commit messages in English.
- Do not create commits unless the user explicitly asks for a commit.
- When committing, follow the style of recent commit messages where practical.

## Scripts

These live in `scripts/` and all run against an isolated `tmux -L <scratch>` server, never the real one. See `docs/e2e-smoke.md` for the manual walkthrough.

- `scripts/smoke-m6-runtime.sh`: runtime-contract smoke test. Confirms the current UI/UX contract (session ordering, category resolution, multi-client attention, Blocked notifications, statusline content, two-sidebar interaction state, and the daemon lifecycle) in one isolated run. Run it before a release and after changes to statusline, sidebar, or daemon behavior.
- `scripts/preflight-ui-ux.sh`: multi-client UI/UX preflight against a scratch server. Run it when changing statusline or sidebar rendering.
- `scripts/test-kill-server-isolated.sh`: exercises the session-manager kill-server / tmux-server shutdown path in isolation. Run it when changing the session manager, the kill-server flow, or daemon/tmux shutdown handling.

These scripts are not run in CI; execute them locally.

## Release

- Follow `RELEASING.md` for the release procedure.
- Publish to crates.io only through the tag-triggered GitHub Actions workflow in `.github/workflows/publish.yml`; do not run `cargo publish` locally.
- After committing the version bump and passing the release preflight, create a matching `vX.Y.Z` tag, push the branch and tag, monitor the `Publish` workflow to completion, and verify the published crates.io version.
- Stop before pushing the release tag if any preflight check fails. If the workflow fails after the tag is pushed, report the partial release state and do not claim that the version was published.
