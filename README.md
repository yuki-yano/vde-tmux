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
- **Status line segments** — a session list with per-session agent badges, state summary counts (`▲2 ●1`), and an attention indicator for blocked agents in sessions you are not currently looking at (`▲ session · perm 2m`).
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
set -g status-interval 1
set -g status-left-length 10000
set -g status-left '#(vt statusline-category)#[fg=#8f8ba8] │ #[default]#(vt statusline-sessions --show-index)#[fg=#8f8ba8] │ #[default]#(vt statusline-windows)'
set -g status-right '#(vt statusline-attention) #(vt statusline-summary)'
set -g pane-border-status bottom
set -g pane-border-format '#(vt statusline-pane --target #{pane_id})'
setw -g window-status-format ''
setw -g window-status-current-format ''
set -g window-status-separator ''
bind-key -n MouseDown1Status run-shell "vt statusline-click '#{mouse_status_range}'"
```

- `statusline-category` — the current category (and the other categories, depending on config)
- `statusline-sessions` — sessions in the current category, each prefixed with an agent state badge. Set `statusline.session_badge.mode: counts` to show counts such as `▲ 2 ● 1 ○ 5`
- `statusline-windows` — windows in the current session, formatted by `statusline.windows`
- `statusline-pane` — the current pane border label, formatted by `statusline.panes`
- `statusline-summary` — state counts across all agents, e.g. `▲2 ●1`
- `statusline-attention` — blocked agents you cannot currently see, e.g. `▲ session · perm 2m`

Use `statusline.sessions.badge_style: chip` to render session badges as a connected chip before each session segment. Category and window badges use `statusline.category.agent_badge` / `statusline.windows.agent_badge` for state selection, `statusline.category.badge_style` / `statusline.windows.badge_style` for placement, and the `{badge}` placeholder for inline styles. The chip color and cap glyphs are configured under `statusline.session_badge.chip`.

Set `status-left-length` to a large value to remove the artificial left segment cap; the terminal width remains the real display limit. `statusline-windows` replaces tmux's native window list, so the native `window-status-*` formats should be empty when using it.

Status updates appear within roughly `daemon.poll_ms + status-interval` (about 2 seconds with the defaults).
The background daemon that collects agent state is started automatically; you never need to launch it yourself (`vt daemon` / `vt daemon stop` / `vt daemon restart` exist for manual control).

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

`notify = ["vt", "hook", "codex"]` only reports legacy turn-complete notifications; by itself it is not enough for task, subagent, or worktree activity detail rows.
The `PostToolUse` hooks let vde-tmux read `update_plan` snapshots for the collapsed `done/total` counter and expanded task rows, and detect recognized `vw exec` Bash commands as a temporary `vw exec <worktree_name>` row under the prompt.
`SubagentStart` and `SubagentStop` add expanded `Agent - ... #id` rows.
For Codex subagents, vde-tmux resolves the session metadata when possible and prefers the Codex nickname, such as `Agent - Fermat #019f`; otherwise it shows the hook `agent_type`.
When a pane is inside a linked git worktree, expanded details start with `+ <worktree_name>`; this active worktree row is separate from `vw exec` activity, and the old `session · pane_id` place row is not shown.
Detail colors can be overridden under `sidebar.colors` with `task_done`, `task_working`, `task_pending`, `task_label`, `subagent_label`, `subagent_id`, `worktree`, and `worktree_activity`.

**Other agents** — anything can report its state through the generic low-level command:

```bash
vt hook emit --agent myagent --status running --prompt "fix the build"
```

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

## Sidebar

The sidebar is a TUI pane inside the current tmux window.
It lists agent panes grouped flat, by repository, or by category, and shows each agent's state, latest prompt, and elapsed time (`45s`, `12m`, `1h30m`, ...).
Closed agent rows use a two-line digest at standard widths: the first line keeps the state, elapsed time, task progress (`☑ done/total`), and running subagent count (`↳ n`) scannable, while the second line keeps the prompt readable and shows a shortened blocked reason such as `↩ permission` when relevant.
Expanding a row reveals the full prompt and the pane's location, and the sidebar auto-follows window layout changes.

### Key bindings

| Key | Action |
| --- | --- |
| `j` / `k`, `↓` / `↑` | Move down / up |
| `Enter` | Jump to the selected agent pane |
| `Space` | Expand / collapse the selected row |
| `v`, `1` / `2` / `3` | Cycle / set view mode (Flat / ByRepo / ByCategory) |
| `Tab` | Cycle filter (all → attn → working → done → idle; empty filters are skipped) |
| `n` / `N` | Focus next / previous row that needs attention |
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

- `vt category next` / `vt category prev` / `vt category use <name>` — switch category; vde-tmux remembers the last session you used in each category and returns to it
- `vt session-cycle next` / `vt session-cycle prev` — cycle through the sessions of the current category
- `vt session new [-c <path>]` — create a managed tmux session and initialize its project path/category metadata
- `vt session set-category <session> <category>` — manually override a session's category
- `vt session-manager --popup` — fzf UI to switch or kill sessions, windows, and panes
- `vt project switch <path>` / `vt project selector --popup` — create or switch to a session for a ghq-managed project

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
  # Runs only when an agent transitions to blocked.
  # Receives VDE_PANE_ID / VDE_AGENT / VDE_BADGE_STATE as environment variables.
  command: 'terminal-notifier -title vde-tmux -message "$VDE_AGENT $VDE_BADGE_STATE"'
```

`${ENV_VAR}` expansion is available inside `path_patterns` and session-name `patterns`.

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
set -ga terminal-overrides ',*:Tc'
set -g status-style 'bg=#1a1926,fg=#9591ad'
set -g status-left-length 10000
setw -g window-status-format ''
setw -g window-status-current-format ''
set -g window-status-separator ''
set -g pane-border-status bottom
set -g pane-border-format '#(vt statusline-pane --target #{pane_id})'
bind-key -n MouseDown1Status run-shell "vt statusline-click '#{mouse_status_range}'"
```

Hex colors require tmux truecolor support (the `terminal-overrides` line above).
`statusline.panes.*.format` supports `{pane}` / `{id}` / `{process}` / `{agent}` / `{name}` / `{badge}` / `{status}` / `{time}` / `{detail}`.
`{detail}` is the compact default: agent panes show the colored badge, agent name, status, and elapsed time; non-agent panes show the process name.

## Files and runtime paths

- Config: `~/.config/vde/tmux/config.yml`
- State: `$XDG_STATE_HOME/vde/tmux/state.json` (falls back to `~/.local/state/vde/tmux/state.json`)
- Daemon socket: `$VDE_DAEMON_SOCKET` if set, else `$XDG_RUNTIME_DIR/vde-tmux/daemon.sock`, else `/tmp/vde-tmux-<uid>/daemon.sock`

## Known limitations

- Agent detection prefers hook-provided state; without hooks it falls back to the pane's running command (`claude`, `codex`, `opencode`) and can mark a pane as blocked only when a permission prompt is recognizable in the visible pane content.
- Closing the sidebar preview with `Esc` relies on `less` supporting `LESSKEYIN`; very old `less` versions may need `q` instead.

## License

[MIT](./LICENSE)
