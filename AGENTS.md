# Agent Instructions

## Git

- Write all commit messages in English.
- Do not create commits unless the user explicitly asks for a commit.
- When committing, follow the style of recent commit messages where practical.

## Release

- Follow `RELEASING.md` for the release procedure.
- Publish to crates.io only through the tag-triggered GitHub Actions workflow in `.github/workflows/publish.yml`; do not run `cargo publish` locally.
- After committing the version bump and passing the release preflight, create a matching `vX.Y.Z` tag, push the branch and tag, monitor the `Publish` workflow to completion, and verify the published crates.io version.
- Stop before pushing the release tag if any preflight check fails. If the workflow fails after the tag is pushed, report the partial release state and do not claim that the version was published.
