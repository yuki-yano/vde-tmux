# vde-tmux

**English** | [日本語](./README.ja.md)

A tmux state & UI manager for AI coding agents.
vde-tmux tracks every Claude Code / Codex / opencode pane on your tmux server and surfaces their status through the tmux status line and a dedicated sidebar, together with category-based session management and fzf-powered session/project switching.

![vde-tmux sidebar](https://github.com/user-attachments/assets/e912448f-b657-49d9-b175-39a0cbad04f2)

## Why

Running several AI coding agents in parallel across tmux sessions quickly becomes a monitoring problem: one agent is waiting for your permission, another finished minutes ago, a third is still working, and none of that is visible unless you visit each pane.
vde-tmux watches all agent panes and answers one question at a glance: **which pane needs me right now?**

## Features

- **Agent status tracking** — every agent pane is classified into four states: `▲` Blocked (waiting for your input), `●` Working, `✓` Done (finished, unread), `○` Idle.
- **Status line segments** — a session list with per-session agent badges, state summary counts (`▲2 ●1`), and an attention indicator for every blocked pane except the exact pane focused by an eligible regular client (`▲ session · perm 2m07s`). A visible but unfocused split still needs attention.
- **Agent sidebar** — a TUI pane that lists all agent panes with their state, latest prompt, and elapsed time. Jump to a pane, preview its scrollback, or watch a live tail of its output.
- **Session categories** — group sessions into categories (e.g. `work` / `private`) with path or session-name rules, then cycle between categories and between the sessions inside one.
- **Session & project switching** — an fzf-based session manager and a ghq-based project selector, both usable as tmux popups.
- **Notifications** — optionally run any command (e.g. `terminal-notifier`) the moment an agent becomes blocked.

## Requirements

- **tmux** 3.2+ (popups are used for the session manager, project selector, and sidebar preview)
- **Rust toolchain** (edition 2024; Rust 1.85 or later) to build from source
- **fzf** — required by the session manager and project selector
- **ghq** — required by the project selector
- **git** — used for repository/branch badges in the sidebar
- **less** — used by the sidebar preview pager

## Installation

```bash
cargo install --git https://github.com/yuki-yano/vde-tmux vde-tmux
```

This installs two binaries that provide the identical CLI:

- `vt` — short alias for everyday use
- `vde-tmux` — full name

The examples below use `vt`.

## Getting started

### 1. Add the status line segments

In `~/.tmux.conf`:

```tmux
run-shell -b 'vt daemon ensure'
set -g status-left-length 10000
set -g status-left '#{@vde_status_category}#[fg=#8f8ba8] │ #[default]#{@vde_status_sessions}#[fg=#8f8ba8] │ #[default]#{@vde_status_windows}'
set -g status-right '#{@vde_status_attention} #{@vde_status_summary}'
set -g pane-border-status bottom
set -g pane-border-format '#{?#{@vde_status_pane},#{@vde_status_pane},#{pane_index} #{pane_current_command}}'
setw -g window-status-format ''
setw -g window-status-current-format ''
set -g window-status-separator ''
bind-key -n MouseDown1Status run-shell "vt statusline-click --client-name #{q:client_name} --session-id #{q:session_id} #{q:mouse_status_range}"
bind-key -n M-h run-shell "vt session-cycle prev --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n M-l run-shell "vt session-cycle next --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n User0 run-shell "vt category prev --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n User1 run-shell "vt category next --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n M-1 run-shell "vt statusline-sessions --client-name #{q:client_name} --session-id #{q:session_id} switch 1"
bind-key -n M-! run-shell "vt category use private --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n M-C run-shell "vt session new -c ~/ --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n M-e run-shell "vt sidebar focus-toggle --window #{q:window_id}"
```

Every interactive category/session binding must capture both the invoking client and source session. This keeps navigation pinned to the correct client when multiple clients display the same pane; omitting the scope fails closed instead of moving an arbitrary client. The `q:` format modifier shell-escapes captured values. Bind sidebar operations to a stable window ID rather than relying on an implicit current window.

- `@vde_status_category` — the current category (and the other categories, depending on config)
- `@vde_status_sessions` — sessions in the current category, each prefixed with an agent state badge. Set `statusline.session_badge.mode: counts` to show counts such as `▲ 2 ● 1 ○ 5`
- `@vde_status_windows` — windows in the current session, formatted by `statusline.windows`
- `@vde_status_pane` — each pane border label, formatted by `statusline.panes`
- `@vde_status_summary` — state counts across all agents, budgeted and published per session, e.g. `▲2 ●1`
- `@vde_status_attention` — every blocked agent except the exact pane focused by an eligible regular client, e.g. `▲ session · perm 2m07s`. A blocked pane in a visible but unfocused split remains in this segment

Use `statusline.sessions.badge_style: chip` to render session badges as a connected chip before each session segment. Category and window badges use `statusline.category.agent_badge` / `statusline.windows.agent_badge` for state selection, `statusline.category.badge_style` / `statusline.windows.badge_style` for placement, and the `{badge}` placeholder for inline styles. The chip color and cap glyphs are configured under `statusline.session_badge.chip`.

Set `status-left-length` to a large value to remove the artificial left segment cap; the terminal width remains the real display limit. `@vde_status_windows` replaces tmux's native window list, so the native `window-status-*` formats should be empty when using it.

The ordered session list is always published in full so every visible index and next/previous action has a stable target; sessions are never collapsed into `+N`. The renderer still uses an 80-cell target for the other status content, preserving blocked attention and the current category and window before lower-priority peers. A complete session list may make the total status projection wider than 80 cells, in which case tmux clips it to the actual terminal width.

The daemon renders these values and pushes them into tmux options. tmux expands only format variables while drawing the status line, so periodic drawing does not start a process. The pane fallback is used only before the daemon has initialized `@vde_status_pane`; the daemon also renders non-agent panes after initialization.

Summary, category, session, window, and attention values are session-scoped. tmux expands them against the session each client is attached to, so one crowded session can omit its summary without clearing another session's summary.

`run-shell -b 'vt daemon ensure'` is the cold-start path for a newly created tmux server, including the first start after an OS restart. It is idempotent when the config is reloaded. Use the lifecycle commands below for manual control.

Changes to vde-tmux settings take effect only after `vt daemon reload`; reloading tmux configuration alone changes the format references but does not rebuild the daemon's runtime state or rendered values. Config-dependent commands compare the strictly loaded disk config with the daemon's active config hash and fail before mutation when the config is invalid or differs, with an explicit `vt daemon reload` instruction.

### 2. Hook up your agents

Agents are detected even without hooks (from the pane's running command), but hooks give you accurate state transitions, prompts, and timing.

**Claude Code** — add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "vt hook claude SessionStart" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "vt hook claude UserPromptSubmit" }] }],
    "PreToolUse": [{ "hooks": [{ "type": "command", "command": "vt hook claude PreToolUse" }] }],
    "PostToolUse": [{ "hooks": [{ "type": "command", "command": "vt hook claude PostToolUse" }] }],
    "Notification": [{ "hooks": [{ "type": "command", "command": "vt hook claude Notification" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "vt hook claude Stop" }] }]
  }
}
```

The `PostToolUse` hook lets vde-tmux read Claude Code task tool events (`TaskCreate` / `TaskUpdate`, plus `TodoWrite` snapshots when emitted) for the collapsed `done/total` counter and expanded task rows.

**Codex** — add to `~/.codex/hooks.json` (or a project-local `.codex/hooks.json`), then review and trust the hooks from Codex with `/hooks`:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|resume|clear",
        "hooks": [
          { "type": "command", "command": "vt hook codex SessionStart" }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          { "type": "command", "command": "vt hook codex UserPromptSubmit" }
        ]
      }
    ],
    "PermissionRequest": [
      {
        "hooks": [
          { "type": "command", "command": "vt hook codex PermissionRequest" }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "^update_plan$",
        "hooks": [
          { "type": "command", "command": "vt hook codex PostToolUse" }
        ]
      },
      {
        "matcher": "^Bash$",
        "hooks": [
          { "type": "command", "command": "vt hook codex PostToolUse" }
        ]
      }
    ],
    "SubagentStart": [
      {
        "hooks": [
          { "type": "command", "command": "vt hook codex SubagentStart" }
        ]
      }
    ],
    "SubagentStop": [
      {
        "hooks": [
          { "type": "command", "command": "vt hook codex SubagentStop" }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          { "type": "command", "command": "vt hook codex Stop" }
        ]
      }
    ]
  }
}
```

Remove the legacy `notify = ["vt", "hook", "codex"]` setting. The legacy `agent-turn-complete` route is unsupported and returns `UnsupportedLegacyNotify`; the `Stop` hook above is the only supported Codex completion route.
The `PostToolUse` hooks let vde-tmux read `update_plan` snapshots for the collapsed `done/total` counter and expanded task rows, and detect recognized `vw exec` Bash commands as a temporary `vw exec <worktree_name>` row under the prompt.
`SubagentStart` and `SubagentStop` add expanded `Agent - ... #id` rows.
For Codex subagents, vde-tmux resolves the session metadata when possible and prefers the Codex nickname, such as `Agent - Fermat #019f`; otherwise it shows the hook `agent_type`.
When a pane is inside a linked git worktree, expanded details start with `+ <worktree_name>`; this active worktree row is separate from `vw exec` activity, and the old `session · pane_id` place row is not shown.
Detail colors can be overridden under `sidebar.colors` with `task_done`, `task_working`, `task_pending`, `task_label`, `subagent_label`, `subagent_id`, `worktree`, and `worktree_activity`.

**Other agents** — anything can report its state through the generic low-level command:

```bash
vt hook emit --agent myagent --session-id run-42 \
  --status running --prompt "fix the build" --prompt-source user
```

`--agent` and a stable, non-empty `--session-id` are required on every generic report. Reuse that session ID for one agent session and choose a new one when the agent session changes.

- `--status` accepts `running`, `waiting`, `idle`, or `error`.
- `--prompt TEXT` requires `--prompt-source SOURCE`; use `--clear-prompt` instead of setting and clearing a prompt in the same report.
- `--status waiting` requires `--wait-reason permission_prompt` or `--wait-reason 'other:TEXT'`.
- `--started-at` is valid only with `--status running`; `--completed-at` and `--attention` are valid only with `--status idle`.
- `--tasks DONE/TOTAL` replaces task progress, for example `--tasks 2/5`; `--clear-tasks` clears it.
- `--subagents 'ID:TYPE|ID:TYPE'` replaces the subagent set, for example `--subagents 'worker-1:reviewer|worker-2:tester'`; `--clear-subagents` clears it.

Set and clear forms for the same field are mutually exclusive. A report with no lifecycle or field change is ignored.

### 3. Open the sidebar

```bash
vt sidebar toggle
```

Convenient tmux bindings:

```tmux
bind-key e run-shell "vt sidebar focus-toggle"   # open → focus → close
bind-key b run-shell "vt sidebar focus"          # return to the sidebar after jumping to a pane
bind-key -n M-C run-shell "vt session new -c ~/"  # create a managed session at home
bind-key s run-shell "vt session-manager --popup"
bind-key g run-shell "vt project selector --popup"
```

## Agent states

| Badge | State | Meaning |
| --- | --- | --- |
| `▲` | Blocked | The agent is waiting for you (permission prompt, question) |
| `●` | Working | The agent is running |
| `✓` | Done | The agent finished and you have not looked at it yet |
| `○` | Idle | Nothing to do, or already acknowledged |

Glyphs and colors are configurable via `badge.glyphs` and `badge.colors`.

### Canonical pane state and acknowledgment

The daemon is the only writer of pane lifecycle state. It stores the agent lifecycle, session identity, run/completion/acknowledgment sequences, prompt, tasks, subagents, and worktree activity together in the pane-scoped `@vde_pane_state` JSON option. The status line and sidebar consume the same resolved daemon snapshot; display options such as `@vde_status_pane` are output only and are never read back as state.

A `Done` badge means that the latest completed run has not been acknowledged. Acknowledgment is persistent and pane-global: when one eligible regular tmux client views the required scope, the daemon advances the acknowledged sequence for every client and sidebar. Leaving that pane or window cannot turn `Idle` back into `Done`, and only a later run completion can create a new `Done`.

`Blocked` attention is separate from `Done` acknowledgment. The attention segment excludes only the exact pane focused by any eligible regular client; a blocked pane that is merely visible in another split remains visible in attention. Every transition to `Blocked` also invokes the configured external notification command, regardless of focus or visibility.

`daemon.done_clear_on` selects the required scope:

- `pane` acknowledges only the pane that becomes active.
- `window` acknowledges all agent panes that were present in the window when it was viewed.

Short focus changes that complete before the next poll are guaranteed when the owned view hooks are healthy, an eligible regular client witnesses the view, the hook receives an acceptance response within 500 ms, the daemon remains alive until application, and persistence succeeds. Before initial hook installation, during a hook collision or `Degraded` hook health, after a daemon crash, or after a response/deadline failure, recovery is best effort. The periodic reconciliation can acknowledge a pane that is still visible, but it cannot recreate a view that already ended.

## Sidebar

The sidebar is a TUI pane inside the current tmux window.
Each running sidebar keeps its own selection, expansion, scrolling, return target, transient connection state, and toast state. View mode and filter changes are saved only as defaults for the next sidebar opened; they never update another sidebar that is already running.
When opened from a non-agent pane, the sidebar initially selects the first focusable resolved agent in the same stable tmux session ID. An exact pane-ID/PID agent match still takes priority, and no row is selected when that session has no agent.
It lists agent panes grouped flat, by repository, or by category, and shows each agent's state, latest prompt, and elapsed time (`42s`, `2m07s`, `12m`, `1h30m`, ...). Second precision is preserved below ten minutes.
Closed agent rows use a two-line digest at standard widths: the first line keeps the state, elapsed time, task progress (`☑ done/total`), and running subagent count (`↳ n`) scannable, while the second line keeps the prompt readable and shows a shortened blocked reason such as `↩ permission` when relevant.
Expanding a row reveals the full prompt and the pane's location, and the sidebar auto-follows window layout changes.
The TUI keeps its last valid snapshot during a daemon restart, reports the degraded or reconnecting state explicitly, and resumes without discarding local interaction state.
Success, warning/progress, and failure notices use distinct existing semantic colors, retain textual markers, and are truncated to the sidebar width. The narrow rail is three cells wide so double-digit counts remain explicit as `9+` while retaining the state glyph.

### Key bindings

| Key | Action |
| --- | --- |
| `j` / `k`, `↓` / `↑` | Move down / up |
| `Enter` | Jump to the selected agent pane |
| `Space` | Expand / collapse the selected row |
| `v`, `1` / `2` / `3` | Cycle / set view mode (Flat / ByRepo / ByCategory) |
| `Tab` | Cycle filter (all → needs action → working → done → idle; empty filters are skipped) |
| `n` / `N` | Focus next / previous row that needs attention |
| `d` | Mark the selected completed run as acknowledged (`Mark complete`) |
| `J` / `K` | Reorder pinned rows |
| `p` | Preview the pane's scrollback in a floating pane (`less`) |
| `e` | Toggle the live-output mode |
| `q` / `Esc` | Close the sidebar TUI |

### Sidebar commands

```bash
vt sidebar open [--width 40|20%]   # open in the current window
vt sidebar toggle [--all]          # toggle (optionally in all windows)
vt sidebar focus-toggle            # open if hidden, focus if unfocused, close if focused
vt sidebar rail                    # collapse to a minimal rail
vt sidebar close
```

## Sessions, categories, and projects

Sessions are grouped into **categories** (e.g. `work`, `private`) using rules on the project path or the session name:

```yaml
categories:
  default_category: misc
  rules:
    - category: work
      path_patterns:
        - github.com/acme/*
```

- `vt category next` / `vt category prev` / `vt category use <name>` — switch category and remember the last session used in each category; next/previous cycle every effective category that currently has a session, including categories hidden by `mode: current` or compact rendering, and return an error when fewer than two are available. Clicks remain limited to rendered category ranges. Multi-client bindings must pass `--client-name #{q:client_name} --session-id #{q:session_id}` so the command can pin the invoking client and source session
- `vt session-cycle next` / `vt session-cycle prev` — cycle through every session in the currently displayed category; no extra argument is needed when one regular client is attached, while multi-client bindings must capture `--client-name` and `--session-id`
- `vt session new [-c <path>]` — create a managed tmux session and initialize its project path/category metadata
- `vt session set-category <session> <category>` — manually override a session's category
- `vt session-manager --popup` — fzf UI to switch or kill sessions, windows, and panes
- `vt project switch <path>` / `vt project selector --popup` — create or switch to a session for a ghq-managed project

The status line orders sessions by their case-sensitive Unicode session name, with the stable tmux session ID as the tie-breaker. Numeric indexes are positions in the latest published snapshot at command execution, not persistent identities, so renaming or adding a session can move an index. Category and session cycling, numeric switching, and mouse ranges resolve the stable IDs stored in the current session-scoped status option. Interactive cycle/switch commands resolve and pin the invoking client and source session together. `TMUX_PANE` identifies the client when it is unique; a stale `run-shell` pane is accepted only when there is exactly one regular client. Multi-client bindings capture `--client-name #{q:client_name} --session-id #{q:session_id}`, and the source ID must match that client. Ambiguous clients, partial ranges, duplicate IDs, a missing current ID, or a missing requested index fail closed without switching a client.

Configured categories with no sessions are omitted. A session that matches no rule remains available through the effective uncategorized/default category.

## Configuration

The config file lives at `~/.config/vde/tmux/config.yml` (respects `$XDG_CONFIG_HOME`).
Everything has sensible defaults; a missing config file is fine.

A JSON Schema ships at `schemas/config.schema.json` and is also printed by `vt config schema`.
For YAML language servers, put this at the top of your config:

```yaml
# yaml-language-server: $schema=/path/to/vde-tmux/schemas/config.schema.json
```

A typical starting point:

```yaml
categories:
  default_category: misc
  rules:
    - category: work
      path_patterns:
        - github.com/acme/*

daemon:
  done_clear_on: window # window | pane

statusline:
  session_badge:
    mode: rollup       # rollup | counts
    chip:
      bg: "#303047"
      cap_left: ""
      cap_right: ""
  sessions:
    badge_style: inline   # inline | plain | outer | chip
  category:
    format: "{badge}{category} "
    badge_style: chip   # inline | plain | outer | chip
    agent_badge:
      enabled: true
      mode: rollup        # rollup | counts
  windows:
    badge_style: inline   # inline | plain | outer | chip
    current:
      format: " {badge}{index}:{window} "
    other:
      format: " {badge}{index}:{window} "
    agent_badge:
      enabled: false
      mode: rollup        # rollup | counts
  summary:
    enabled: true

sidebar:
  width: "20%"            # columns or percentage
  min_width: 40
  live:
    enabled: true
    lines: 3

badge:
  glyphs:
    blocked: "▲"
    working: "●"
    done: "✓"
    idle: "○"

notify:
  enabled: true
  # Runs on every transition to blocked, regardless of pane focus or visibility.
  # Receives VDE_PANE_ID / VDE_AGENT / VDE_BADGE_STATE as environment variables.
  command: 'terminal-notifier -title vde-tmux -message "$VDE_AGENT $VDE_BADGE_STATE"'
```

`${ENV_VAR}` expansion is available inside `path_patterns` and session-name `patterns`.

`daemon.done_clear_on` controls when a `Done` badge is acknowledged. `window` clears it when the containing window is viewed; `pane` keeps it until that pane itself is focused.

### Pane-state daemon and owned hooks

During startup the daemon installs and verifies five state-management hooks at tmux hook index `70`, hydrates canonical pane state, reconciles current views, initializes all display options, and then enters `Serving`. The phase and hook health are independent. `Serving + Degraded` still accepts pane events and serves queries, subscriptions, polling reconciliation, and status output, but the short-focus guarantee above is suspended until all owned hooks verify as healthy again.

Define custom handlers for the same tmux hook with an explicit index other than `70`, for example `set-hook -g client-session-changed[0] '...'`. An unindexed `set-hook` replaces the hook array and removes the daemon-owned `[70]` handler when the tmux configuration is reloaded.

Daemon lifecycle commands have the following effects:

| Command | Effect |
| --- | --- |
| `vt daemon ensure` | Idempotently reaches `Serving` when enabled; succeeds without changing state when disabled |
| `vt daemon start` | Explicitly reaches `Serving`; returns an error while disabled |
| `vt daemon stop` | Stops temporarily but leaves owned hooks and automatic startup enabled, so a later event can restart it |
| `vt daemon stop --force` | Kills only after revalidating the recorded process, tmux incarnation, and socket identity |
| `vt daemon disable` | Sets the server-wide disabled marker first, removes owned hooks, then stops; events cannot restart it |
| `vt daemon enable` | Installs and verifies owned hooks, reaches `Serving`, then clears the disabled marker; any failure restores the disabled marker, removes owned hooks, and stops the just-started daemon, while an incomplete rollback remains diagnosable as degraded |
| `vt daemon reload` | Strictly validates configuration, stops, and starts; invalid configuration leaves the current daemon unchanged, while a failed new start leaves it stopped without rollback |
| `vt daemon restart` | Exact alias for `reload`, including strict validation and no rollback after a failed new start |
| `vt daemon status` | Reads lifecycle and runtime health without starting or mutating the daemon |
| `vt daemon doctor` | Performs read-only checks of configuration, hooks, projection, notifications, status push, and runtime paths |
| `vt daemon logs [daemon\|notification\|status-push\|pane-state-hook] --lines N` | Reads a bounded private log tail; `N` must be between 1 and 500 |

`vt daemon stop` and daemon crashes deliberately leave the owned hooks installed so they can deliver startup events to the next daemon. Use `vt daemon disable` when automatic restart must remain suppressed. Only the explicit command below removes owned index `70` entries without changing the desired daemon mode; it refuses to remove foreign commands.

```bash
vt pane-state hooks uninstall
```

Use daemon-mediated maintenance commands for pane state. Do not unset `@vde_pane_state` directly.

```bash
vt pane-state reset --target %42       # replace current or quarantined state with a reset tombstone
vt pane-state cleanup-legacy --all --dry-run  # report the fixed legacy-key impact without mutation
vt pane-state cleanup-legacy --all            # remove only the fixed legacy 19 state keys
```

Cleanup is never run automatically at daemon startup. It preserves category, project-path, sidebar-marker, canonical `@vde_pane_state`, and display `@vde_status_*` options. Both dry-run and mutation report before, removed, remaining, per-scope, and bounded failure counts; a nonzero remaining/failure result is a partial cleanup, not success.

### Migration from legacy pane state

This migration has no compatibility period. Rehearse it first on a scratch tmux server with a dedicated socket and temporary configuration; do not point the rehearsal at a regular tmux server. A regular server cutover is a separate operational decision because cleanup is not automatically reversible.

Before a regular-server cutover, record the exact tmux socket, the daemon that will be stopped, the agent sessions that will be restarted, and the fixed legacy-key cleanup impact, then obtain explicit approval for that target.

Use this order for each approved target server:

1. While the old binary is still installed, run its `vt daemon stop` and verify that its process has exited. The new binary contains no v1 detector, migration helper, fallback, or v1 stop path.
2. Replace every `#(vt statusline-*)` and pane-border command with the `#{@vde_status_*}` format references shown above, and add `run-shell -b 'vt daemon ensure'`. The new daemon's startup preflight rejects a legacy pull-based command on any active status surface.
3. Replace the binary, reload the tmux configuration, and verify protocol v3 plus owned-hook post-verification.
4. Remove Codex's legacy `notify` setting, keep the `Stop` hook as the only completion route, and restart all agent sessions so they emit events through the new adapters.
5. Send one controlled agent event and verify that the pane option and daemon cache have the same full state version and that the daemon phase is `Serving`.
6. Attach two regular clients to different sessions and verify the session-specific status context, badge, counts, pane borders, and sidebar against the same canonical state.
7. Only after steps 1–6 succeed, run `vt pane-state cleanup-legacy --all`.
8. Recheck every display surface and confirm that no legacy state option is required. Use `vt pane-state reset --target <pane-id>` explicitly for a state that must be discarded.

If the controlled event or two-client verification fails, do not run cleanup; stop and report a partial cutover. If any cleanup removal fails, also stop and treat the server as partially cut over. There is no automatic rollback after cleanup; returning to the old binary requires restoring its hooks and restarting agent sessions.

### Recommended status line colors

A truecolor preset that keeps state glyphs readable on the bar and uses fills only for the current element:

```yaml
# ~/.config/vde/tmux/config.yml
statusline:
  session_badge:
    chip:
      bg: "#303047"
      cap_left: ""
      cap_right: ""
  category:
    mode: list
    format: "{badge}{category} {name} "
    inactive_format: "{badge}{category} "
    badge_style: chip
    agent_badge:
      enabled: true
      mode: rollup
    colors:
      fg: "#ecebff"
      bg: "#453f9e"
    inactive_colors:
      fg: "#9591ad"
  sessions:
    badge_style: outer   # inline | plain | outer | chip
    current:
      format: " {session} "
      bold: true
      colors:
        fg: "#ecebff"
        bg: "#453f9e"
    other:
      format: " {session} "
      colors:
        fg: "#9591ad"
  windows:
    separator: "#[fg=#8f8ba8]│#[default]"
    badge_style: inline
    agent_badge:
      enabled: false
      mode: rollup
    current:
      format: " {index}:{window} "
      bold: false
      colors:
        fg: "#20233a"
        bg: "#9d8cf5"
      prefix: "#[fg=#9d8cf5]"
      suffix: "#[fg=#9d8cf5,bg=default]#[default]"
    other:
      format: " {index}:{window} "
      colors:
        fg: "#9591ad"
    bell:
      fg: "#ff6b6b"
    activity:
      fg: "#ff6b6b"
  panes:
    current:
      format: " {pane}  {detail} "
      colors:
        fg: "#e7e3f6"
        bg: "#4a4a70"
        outer_bg: "#1C1C1C"
      prefix: "#[fg=#4a4a70,bg=#1C1C1C]"
      suffix: "#[fg=#4a4a70,bg=#1C1C1C]#[default]"
    other:
      format: " {pane} #[fg=#9696CE]#[fg=#BDC4E3] {detail} "
      colors:
        fg: "#BDC4E3"
        bg: "#373A56"
        outer_bg: "#1C1C1C"
      prefix: "#[fg=#373A56,bg=#1C1C1C]"
      suffix: "#[fg=#373A56,bg=#1C1C1C]#[default]"
```

```tmux
# ~/.tmux.conf
run-shell -b 'vt daemon ensure'
set -ga terminal-overrides ',*:Tc'
set -g status-style 'bg=#1a1926,fg=#9591ad'
set -g status-left-length 10000
set -g status-left '#{@vde_status_category}#[fg=#8f8ba8] │ #[default]#{@vde_status_sessions}#[fg=#8f8ba8] │ #[default]#{@vde_status_windows}'
set -g status-right '#{@vde_status_attention} #{@vde_status_summary}'
setw -g window-status-format ''
setw -g window-status-current-format ''
set -g window-status-separator ''
set -g pane-border-status bottom
set -g pane-border-format '#{?#{@vde_status_pane},#{@vde_status_pane},#{pane_index} #{pane_current_command}}'
bind-key -n MouseDown1Status run-shell "vt statusline-click --client-name #{q:client_name} --session-id #{q:session_id} #{q:mouse_status_range}"
```

Hex colors require tmux truecolor support (the `terminal-overrides` line above).
`statusline.panes.*.format` supports `{pane}` / `{id}` / `{process}` / `{agent}` / `{name}` / `{badge}` / `{status}` / `{time}` / `{detail}`.
`{detail}` is the compact default: agent panes show the colored badge, agent name, status, and elapsed time; non-agent panes show the process name.
The configured pane format is used unchanged at every pane width, including the 31/32 and 63/64 column boundaries.

## Files and runtime paths

- Config: `~/.config/vde/tmux/config.yml`
- Sidebar preferences: `$XDG_STATE_HOME/vde/tmux/sidebar-state/` (falls back to `~/.local/state/vde/tmux/sidebar-state/`). `sidebar-order-v1.json` stores manual repository/chat order plus view mode/filter, while `sidebar-expansion-v1.json` stores expansion state shared by every session; both use CAS-protected atomic saves. Selection, scroll, return target, toast, and connection state remain local.
- Per-incarnation lifecycle record and logs: `$XDG_STATE_HOME/vde-tmux/<tmux-incarnation-hash>/` (falls back under `~/.local/state`; directory mode `0700`, files mode `0600`)
- Daemon socket: `/tmp/vt-<uid>/v2/<tmux-incarnation-hash>.sock`, namespaced by the canonical tmux socket path, server PID, and server start time
- Per-sidebar control socket: a short hashed name below `/tmp/vt-<uid>/v2/sidebar-control/`, namespaced by the tmux server and exact sidebar pane instance

## Optional UI/UX preflight

`scripts/preflight-ui-ux.sh` is a local, optional tmux preflight rather than a required CI check. It requires `bash`, `cargo`, `tmux`, `python3`, and `lsof`; build the debug binary first and run it from the repository root:

```sh
cargo build
scripts/preflight-ui-ux.sh
```

Each run preserves its summary, logs, terminal captures, and sandbox under `target/preflight/<UTC-stamp>-<pid>-<random>/`. Set `VDE_VT_BIN` to test a different binary.

## Known limitations

- Agent detection prefers hook-reported events; without agent hooks it falls back to the pane's running command (`claude`, `codex`, `opencode`) and can mark a pane as blocked only when a permission prompt is recognizable in the visible pane content.
- If the daemon crashes, the last pushed status values remain frozen; tmux does not immediately detect the crash. The next agent hook or view hook starts a new daemon and refreshes the display. Use `vt daemon ensure` or `vt daemon reload` (`restart` is an alias) for explicit recovery when no such event occurs.
- Closing the sidebar preview with `Esc` relies on `less` supporting `LESSKEYIN`; very old `less` versions may need `q` instead.

## License

[MIT](./LICENSE)
