# Releasing

Publishing is driven by Git tags.

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
