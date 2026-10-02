# Codex 0.159.3 question rendering fixtures

These unchanged `.snap` files come from `openai/codex` tag `rust-v0.159.3`,
`codex-rs/tui/src/bottom_pane/async_questions/snapshots/`, under the included
Apache-2.0 license and NOTICE. The original filename prefix was removed.
Source: https://github.com/openai/codex/tree/rust-v0.159.3/codex-rs/tui/src/bottom_pane/async_questions

The corresponding `state_tests.rs` supplies the full titles and named options.
Tests replay the snapshot text as a bottom-pane region with the larger viewport geometry.
Width-boundary snapshots include both full and clipped regions. A clipped title
or missing named choice is retaining ambiguity, never clearance evidence.

Freeform placeholder/draft cases also follow `render_inline_input` (defined in
`chat_composer/inline_input.rs`, called from `async_questions/render.rs`):
there is no composer `›` prefix in that input. The synthetic hook fixture is a
contract harness, not evidence of running the actual Codex CLI.

The corresponding rendering, layout, input, state tests, inline composer, hook
payload, and question protocol types are unchanged in tag `rust-v0.160.0`.
Both exact profiles replay these fixtures; unknown versions stay conservative.
