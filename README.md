# vde-tmux

**English** | [日本語](./README.ja.md)

vde-tmux shows the state of AI coding agents running in tmux.
It tracks Claude Code, Codex, and opencode panes and renders their state in the tmux status line and a dedicated sidebar.

![vde-tmux sidebar](https://github.com/user-attachments/assets/e912448f-b657-49d9-b175-39a0cbad04f2)

## Features

- Classifies agents across all tmux sessions as `Blocked`, `Working`, `Done`, or `Idle`
- Shows agents that need attention directly in the tmux status line
- Displays prompts, elapsed time, tasks, subagents, and worktree activity in a sidebar
- Jumps to agent panes and previews their scrollback from the sidebar
- Groups sessions into categories and switches them from the keyboard or status line
- Runs a notification command when an agent starts waiting for input

## Requirements

- tmux 3.2 or later
- The latest stable Rust and Cargo for installation
- git, lsof, and less on `PATH`
- Optional: fzf for the session manager, ghq for the project selector

## Installation

```bash
cargo install vde-tmux --locked
```

The package installs two equivalent commands: `vt` and `vde-tmux`.
This README uses the short name `vt`.

```bash
vt --version
```

## Setup

### 1. tmux configuration

Add the following to `~/.tmux.conf`:

```tmux
run-shell -b 'vt daemon ensure'

set -g status-left-length 10000
set -g status-left '#{@vde_status_category}#[fg=#8f8ba8] │ #[default]#{@vde_status_sessions}#[fg=#8f8ba8] │ #[default]#{@vde_status_windows}'
set -g status-right '#{@vde_status_attention} #{@vde_status_summary}'

setw -g window-status-format ''
setw -g window-status-current-format ''
set -g window-status-separator ''

set -g pane-border-status bottom
set -g @vde_status_now_format '%s'
set -g pane-border-format '#{?#{@vde_status_pane},#{E:@vde_status_pane},#{pane_index} #{pane_current_command}}'

bind-key -n MouseDown1Status run-shell "vt statusline-click --client-name #{q:client_name} --session-id #{q:session_id} #{q:mouse_status_range}"
bind-key -n M-h run-shell "vt session-cycle prev --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n M-l run-shell "vt session-cycle next --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n M-e run-shell "vt sidebar focus-toggle --window #{q:window_id}"
```

Notes:

- `vt daemon ensure` starts the daemon on demand.
- vde-tmux pushes rendered text into the `@vde_status_*` options, so tmux does not start a process on every status redraw.
- `@vde_status_now_format` is required for the elapsed time shown on pane borders.
- The `window-status-*` settings replace tmux's native window list with the vde-tmux session and window segments.
- `--client-name` and `--session-id` keep session and category bindings scoped to the client that triggered them, which matters when multiple tmux clients are attached.

Reload the configuration:

```bash
tmux source-file ~/.tmux.conf
```

### 2. Claude Code hooks

Add these hooks to `~/.claude/settings.json`:

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

Restart Claude Code after saving the file.
Its lifecycle and task progress will then appear in vde-tmux.

### 3. Codex hooks

Add these hooks to `~/.codex/hooks.json` or the project-local `.codex/hooks.json`.
Review and trust the hooks with Codex `/hooks` after saving the file.

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|resume|clear",
        "hooks": [{ "type": "command", "command": "vt hook codex SessionStart" }]
      }
    ],
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "vt hook codex UserPromptSubmit" }] }
    ],
    "PermissionRequest": [
      { "hooks": [{ "type": "command", "command": "vt hook codex PermissionRequest" }] }
    ],
    "PostToolUse": [
      {
        "matcher": "^update_plan$",
        "hooks": [{ "type": "command", "command": "vt hook codex PostToolUse" }]
      },
      {
        "matcher": "^Bash$",
        "hooks": [{ "type": "command", "command": "vt hook codex PostToolUse" }]
      }
    ],
    "SubagentStart": [
      { "hooks": [{ "type": "command", "command": "vt hook codex SubagentStart" }] }
    ],
    "SubagentStop": [
      { "hooks": [{ "type": "command", "command": "vt hook codex SubagentStop" }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "vt hook codex Stop" }] }
    ]
  }
}
```

Restart Codex after saving the file.
Permission requests, plans, subagents, and worktree activity will then appear in the sidebar.

### 4. Verify

Run these commands inside tmux:

```bash
vt daemon status
vt sidebar open
```

vde-tmux can detect Claude Code, Codex, and opencode from the command running in a pane even without hooks.
Hooks are still required for accurate prompts, completion times, and waiting states.

## Agent states

| Badge | State | Meaning |
| --- | --- | --- |
| `▲` | Blocked | The agent is waiting for permission or an answer |
| `●` | Working | The agent is running |
| `✓` | Done | The run completed and has not been acknowledged |
| `○` | Idle | No work is active, or the completed run was acknowledged |

A `Done` agent becomes `Idle` after its pane or window is viewed.
Choose the acknowledgment scope with `daemon.done_clear_on`:

```yaml
daemon:
  done_clear_on: window # window | pane
```

Acknowledgment survives daemon restarts and is shared by every tmux client and sidebar.
The tmux view hook freezes the visible pane/window at hook time, so a brief focus followed by an
immediate move still acknowledges the `Done` state that was seen. The hook returns after the daemon
queues this event. If the daemon stops after queue acceptance but before applying it, that one
acknowledgment is best-effort and is not replayed; the badge remains `Done` until the next focus or
a later completion-time visibility check acknowledges it.

## Sidebar

The sidebar opens in the current tmux window and groups agents by category by default.

```bash
vt sidebar open --width 40
vt sidebar open --width 20%
vt sidebar toggle
vt sidebar toggle --all
vt sidebar rail
vt sidebar close
```

`vt sidebar focus-toggle` opens a missing sidebar, focuses a visible one, and closes it when it already has focus.

| Key | Action |
| --- | --- |
| `j` / `k`, `↓` / `↑` | Move between rows |
| `Enter` | Jump to the selected agent pane |
| `Space` | Expand or collapse the selected row |
| `v` | Cycle the view mode |
| `1` / `2` / `3` | Select Flat / ByRepo / ByCategory |
| `Tab` / `Shift+Tab` | Cycle the state filter |
| `n` / `N` | Move to the next or previous agent that needs attention |
| `d` | Mark the selected run as complete |
| `J` / `K` | Change manual ordering |
| `p` | Preview pane scrollback |
| `e` | Switch the live panel between output and events |
| `q` / `Esc` | Close the sidebar |

Agents belonging to the active session have a `▎` marker on the left.
View mode, filter, manual order, and expansion state are persisted and shared across sidebars.
Selection and scrolling remain local to each sidebar instance.

## Sessions and categories

Categories group tmux sessions by project path or session name:

```yaml
categories:
  default_category: misc
  rules:
    - category: work
      path_patterns:
        - github.com/acme/*
  session_name_rules:
    - category: scratch
      patterns:
        - tmp-*
```

Common commands:

```bash
vt category next
vt category prev
vt category use work
vt session-cycle next
vt session-cycle prev
vt session new -c ~/src/my-project
```

Categories are derived only from these config rules and the session/project metadata. `@vde_category` is a derived, write-only mirror for external tmux formats; changing it manually does not affect vde-tmux. Renaming a session matched by `session_name_rules` updates the runtime category immediately, while the mirror may keep its previous value until the next `vt daemon reload`.

With fzf installed, open a popup for switching or removing sessions, windows, and panes:

```bash
vt session-manager --popup
```

The final selector row is `✕ tmux server | tmux kill-server`.
It responds only to `Ctrl-Q` and shuts down the whole tmux server after stopping the vde daemon and cleaning up the remaining pane processes.

With ghq installed, create or select a session from the project selector:

```bash
vt project selector --popup
```

## Configuration

The configuration file is `$XDG_CONFIG_HOME/vde/tmux/config.yml`.
When `XDG_CONFIG_HOME` is unset, vde-tmux uses `~/.config/vde/tmux/config.yml`.
Every setting has a default, so the file is optional; start with only the settings you need.

Together with the `categories` section shown above, the commonly used settings are:

```yaml
daemon:
  done_clear_on: window

sidebar:
  width: "20%"
  min_width: 40
  live:
    enabled: true
    lines: 3

statusline:
  sessions:
    fixed_width: true
    fixed_width_alignment: center # left (default) | center
  session_badge:
    mode: rollup # rollup | counts
  summary:
    enabled: true
    hide_idle: false
    format: "{badge} {count}"

badge:
  glyphs:
    blocked: "▲"
    working: "●"
    done: "✓"
    idle: "○"
```

`statusline.summary.format` supports the `{badge}` and `{count}` placeholders, such as `{badge}{count}` or `{badge}: {count}`.
Zero-count states remain visible so the summary width stays stable; set `hide_idle: true` to omit the idle token.

`statusline.sessions.fixed_width: true` pads the active category's session segment to the widest category and keeps the combined category/session/window area at the same width across sessions. Session content is left-aligned within that fixed area by default; set `fixed_width_alignment: center` to center it. This keeps a centered status block stable when switching between sessions whose window names or process names have different lengths. Widths for inactive categories use the `other` session style; if `current.format` and `other.format` have different visual widths, the fixed width may differ by a few cells.

The full schema is available with `vt config schema`.

Reload the daemon after changing the file:

```bash
vt daemon reload
```

## Notifications

Run an external command whenever an agent enters `Blocked`:

```yaml
notify:
  enabled: true
  command: 'terminal-notifier -title vde-tmux -message "$VDE_AGENT needs attention"'
```

The command receives `VDE_PANE_ID`, `VDE_AGENT`, and `VDE_BADGE_STATE`.

## Integrating another agent

Agents other than Claude Code and Codex can report state through `vt hook emit`.
Use a stable `--session-id` for the lifetime of one agent run.

```bash
vt hook emit \
  --agent myagent \
  --session-id run-42 \
  --status running \
  --prompt "fix the build" \
  --prompt-source user
```

`--status` accepts `running`, `waiting`, `idle`, and `error`.
A waiting event also needs a reason:

```bash
vt hook emit \
  --agent myagent \
  --session-id run-42 \
  --status waiting \
  --wait-reason permission_prompt
```

## Daemon operations

For normal use, the `vt daemon ensure` line in the tmux configuration manages startup.

| Command | Purpose |
| --- | --- |
| `vt daemon ensure` | Start the daemon when needed |
| `vt daemon reload` | Validate configuration and restart |
| `vt daemon stop` | Stop temporarily |
| `vt daemon disable` | Stop and disable automatic startup |
| `vt daemon enable` | Enable automatic startup and start |
| `vt daemon status` | Show daemon and hook health |

`stop` does not disable automatic startup.
Use `disable` when the daemon must remain stopped.

### Pane-state persistence

The daemon stores one private full-state snapshot per tmux server incarnation under
`$XDG_STATE_HOME/vde-tmux/<incarnation-hash>/pane-state-v1.json`. A daemon restart restores the
prompt, task progress and items, subagents, worktree activity, lifecycle, timestamps, agent
identity, and Done/acknowledgement state for panes whose pane ID and PID still match.

If this snapshot is corrupt or insecure, daemon startup stops instead of repairing it or falling
back. `vt daemon status` reports the snapshot path in `last_transition_error`; remove that file only when you
intend to reset all saved pane state for that tmux server, then run `vt daemon ensure`.

The first upgrade from a version that stored pane state in tmux options does not migrate that old
state, so pane details reset once. Perform that upgrade only while all agents are idle and there is
no Done or Blocked state that must be retained. Snapshots for other tmux server incarnations are not
removed automatically.

## Upgrading

The daemon and its clients (sidebar, status line, CLI) must run the same version; there is no cross-version compatibility.
Stop the daemon before replacing the binary, then start the new one and reopen any sidebars:

```bash
vt daemon stop
cargo install vde-tmux --locked
vt daemon ensure
```

If the binary was replaced while the old daemon was still running, `vt daemon stop --force` stops it.

## Troubleshooting

### The status line or sidebar does not update

Inspect daemon health, and reload after configuration changes:

```bash
vt daemon status
vt daemon reload
```

### Reloading tmux configuration breaks hooks

vde-tmux owns tmux hook index `70`.
Use a different explicit index for custom handlers on the same hook:

```tmux
set-hook -g client-session-changed[0] 'your-command'
```

An unindexed `set-hook` replaces the existing hook array.

### Inspect configuration errors

```bash
vt daemon reload
vt daemon status
```

Each tmux server incarnation has one operational log at
`$XDG_STATE_HOME/vde-tmux/<incarnation-hash>/daemon.log`. Notification, status-push, and hook
delivery errors use distinct prefixes in that file.
Sidebar order, default view/filter, and row expansion are stored atomically below
`$XDG_STATE_HOME/vde/tmux/sidebar-state/`, isolated by tmux socket. Order and expansion
updates are shared live by sidebars on the same tmux server. View/filter changes stay local
to an open sidebar and become the default for sidebars opened later. Selection, scrolling,
live mode, and the return target remain instance-local and are not persisted.

## Known limitations

- Without hooks, waiting detection is limited to states that can be inferred from visible pane output
- When the daemon stops, the last rendered status options remain until the next hook event or `vt daemon ensure`
- Older versions of less may not close a preview with `Esc`; use `q` instead

## License

[MIT](./LICENSE)
