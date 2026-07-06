# Sidebar Toggle Semantics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace sidebar chat pinning with persisted manual open/close toggles.

**Architecture:** Keep the existing sidebar state file and daemon save path. Remove pin behavior from input, row metadata, and rendering; make chat expansion depend only on saved open/close state instead of selection.

**Tech Stack:** Rust, ratatui, serde JSON state, existing cargo tests.

---

## Definition of Done

### Functional Completion

- [ ] Chat row click toggles that chat open/closed and selects the chat.
- [ ] Detail/meta row click toggles the parent chat closed/open and selects the parent chat.
- [ ] `space` toggles the focused row open/closed; focused chat rows no longer pin.
- [ ] Moving focus or jumping panes does not auto-expand chat rows.
- [ ] `h`, `l`, `left`, and `right` no longer map to sidebar expand/collapse.
- [ ] `J/K` reorder chat rows when a chat row is selected, not only repo rows.
- [ ] Chat row order is saved and restored through the existing sidebar state path.
- [ ] Pin state, pin rendering, and pinned meta-row behavior are removed.
- [ ] Closed running chat rows keep right-edge elapsed time.
- [ ] Closed completed chat rows show white `ago` time on the right edge.
- [ ] Open running state detail rows show full multi-unit duration down to seconds.
- [ ] Closed unfocused chat rows render as one terminal line within the sidebar width.

### Test Completion

- [ ] Add failing tests for input key mapping changes.
- [ ] Add failing tests for chat/detail click toggle semantics.
- [ ] Add failing tests for focus not auto-expanding chat rows.
- [ ] Add failing tests for `J/K` chat row reordering.
- [ ] Add failing tests for closed done `ago` labels and full running detail duration.
- [ ] Add or update render-width tests for one-line closed rows.
- [ ] Run focused sidebar/runtime tests and full cargo test.

### Operational Reflection

- [ ] Existing state save path remains unchanged.
- [ ] No compatibility fallback for removed `pinned` state is added.
- [ ] No unrelated statusline/config/doc changes are modified.
- [ ] No commit is created unless explicitly requested.

## File Map

- `src/sidebar/state.rs`: remove pin state/actions and keep persisted collapsed state as the source of truth.
- `src/sidebar/state.rs`: add chat-level manual ordering for `J/K`.
- `src/sidebar/input.rs`: remove `h/l/left/right` mappings.
- `src/sidebar/tree.rs`: remove pin metadata, stop selected chat auto-expansion, add completed-age metadata and full duration formatting for detail rows.
- `src/sidebar/render.rs`: remove pin glyph, render closed done-age right labels in white, keep line width bounded.
- `src/sidebar/tui.rs`: keep row click dispatch, with detail/meta rows toggling parent chat through daemon.
- `src/daemon/runtime.rs`: change `space` and `toggle:chat::*` from pinning to expansion toggles.

## Tasks

### Task 1: Red Tests

- [ ] Write tests that describe the new input and runtime behavior before production edits.
- [ ] Run focused tests and confirm they fail for the expected old pin/auto-expand behavior.

### Task 2: State and Runtime

- [ ] Remove pin state and change daemon key handling so chat toggles mutate `collapsed`.
- [ ] Add chat row `J/K` handling alongside existing repo manual ordering.
- [ ] Rebuild rows after changes and keep dirty-state save behavior unchanged.
- [ ] Run focused runtime/state tests.

### Task 3: Tree and Rendering

- [ ] Remove selected-chat auto-expansion and pin meta rows.
- [ ] Add completed age metadata and full detail duration formatting.
- [ ] Update right-label styles and width tests.
- [ ] Run focused tree/render tests.

### Task 4: Verification

- [ ] Run `rtk cargo fmt --check`.
- [ ] Run `rtk cargo test`.
- [ ] Review `git diff --stat` and confirm only intended files changed.
