# Releasing

Publishing is driven by Git tags.

1. Bump `version` in `Cargo.toml` and `Cargo.lock`.
2. Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, and `cargo publish --dry-run --locked`.
3. Run the isolated runtime smoke test `scripts/smoke-m6-runtime.sh` (it uses a scratch `tmux -L` server and does not touch the real one). See `AGENTS.md` for the other scripts.
4. Commit the version bump and release changes.
5. Create a tag that matches the crate version:

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
