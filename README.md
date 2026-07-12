# vde-tmux

**English** | [日本語](./README.ja.md)

vde-tmux shows the state of AI coding agents running in tmux.
It tracks Claude Code, Codex, and opencode panes and renders their state in the status line and a dedicated sidebar.

![vde-tmux sidebar](https://github.com/user-attachments/assets/e912448f-b657-49d9-b175-39a0cbad04f2)

## What it does

- Classifies agents across tmux sessions as `Blocked`, `Working`, `Done`, or `Idle`
- Shows agents that need attention directly in the tmux status line
- Displays prompts, elapsed time, tasks, subagents, and worktree activity in a sidebar
- Jumps to agent panes and previews their scrollback from the sidebar
- Groups sessions into categories and switches them from the keyboard or status line
- Runs a notification command when an agent starts waiting for input

## Requirements

- tmux 3.2 or later
- The latest stable Rust and Cargo for installation
- git for repository and branch metadata
- lsof for daemon socket validation
- less for sidebar previews
- fzf for the optional session manager
- ghq for the optional project selector

## Installation

Install vde-tmux from crates.io:

```bash
cargo install vde-tmux --locked
```

The package installs two equivalent commands:

- `vt`, the short name used throughout this README
- `vde-tmux`, the full package name

Verify the installation:

```bash
vt --version
```

## Initial setup

### Add the status line and key bindings

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
set -g pane-border-format '#{?#{@vde_status_pane},#{@vde_status_pane},#{pane_index} #{pane_current_command}}'

bind-key -n MouseDown1Status run-shell "vt statusline-click --client-name #{q:client_name} --session-id #{q:session_id} #{q:mouse_status_range}"
bind-key -n M-h run-shell "vt session-cycle prev --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n M-l run-shell "vt session-cycle next --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n M-e run-shell "vt sidebar focus-toggle --window #{q:window_id}"
```

vde-tmux pushes rendered text into `@vde_status_*` options, so tmux does not start a process on every status redraw.
The configuration above replaces tmux's native window list with the vde-tmux session and window segments.

Reload the configuration:

```bash
tmux source-file ~/.tmux.conf
```

Session and category bindings need both `--client-name` and `--session-id` when multiple tmux clients are attached.
The example above includes the required scope and will not move a different client accidentally.

### Configure Claude Code hooks

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

### Configure Codex hooks

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

### Verify the setup

Run these commands inside tmux:

```bash
vt daemon status
vt daemon doctor
vt sidebar open
```

vde-tmux can discover some agents from the command running in a pane without hooks.
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

## Sidebar

The sidebar opens in the current tmux window and defaults to category grouping.

```bash
vt sidebar open --width 40
vt sidebar open --width 20%
vt sidebar toggle
vt sidebar toggle --all
vt sidebar rail
vt sidebar close
```

`vt sidebar focus-toggle` opens a missing sidebar, focuses an existing sidebar, and closes it when it already has focus.
Expansion state is shared across sessions.

| Key | Action |
| --- | --- |
| `j` / `k`, `↓` / `↑` | Move between rows |
| `Enter` | Jump to the selected agent pane |
| `Space` | Expand or collapse the selected row |
| `v` | Cycle the view mode |
| `1` / `2` / `3` | Select Flat / ByRepo / ByCategory |
| `Tab` | Cycle the state filter |
| `n` / `N` | Move to the next or previous agent that needs attention |
| `d` | Acknowledge the completed run |
| `J` / `K` | Change manual ordering |
| `p` | Preview pane scrollback |
| `e` | Toggle live output |
| `q` / `Esc` | Close the sidebar |

The cursor is always rendered as `›`.
When an expanded agent is selected, only its first line shows the cursor while the selection background covers the full expanded content.
Agents belonging to the active session have a `▎` marker on the left.

View mode, filter, manual order, and expansion state are persisted.
Selection, scrolling, and transient messages remain local to each sidebar instance.

## Sessions and categories

Categories group tmux sessions by project path or session name.

```yaml
categories:
  default_category: misc
  rules:
    - category: work
      path_patterns:
        - github.com/acme/*
```

Common commands:

```bash
vt category next
vt category prev
vt category use work
vt session-cycle next
vt session-cycle prev
vt session new -c ~/src/my-project
vt session set-category my-session work
```

With fzf installed, open a popup for switching or removing sessions, windows, and panes:

```bash
vt session-manager --popup
```

With ghq installed, create or select a session from the project selector:

```bash
vt project selector --popup
```

## Configuration

The configuration file is `$XDG_CONFIG_HOME/vde/tmux/config.yml`.
When `XDG_CONFIG_HOME` is unset, vde-tmux uses `~/.config/vde/tmux/config.yml`.
Every setting has a default, so the file is optional.

Start with only the settings you need:

```yaml
categories:
  default_category: misc
  rules:
    - category: work
      path_patterns:
        - github.com/acme/*

daemon:
  done_clear_on: window

sidebar:
  width: "20%"
  min_width: 40
  live:
    enabled: true
    lines: 3

statusline:
  session_badge:
    mode: rollup # rollup | counts
  summary:
    enabled: true

badge:
  glyphs:
    blocked: "▲"
    working: "●"
    done: "✓"
    idle: "○"
```

Reload the daemon after changing the file:

```bash
vt daemon reload
```

## Notifications

Run an external command whenever an agent enters `Blocked`:

```yaml
notify:
  enabled: true
  command: 'terminal-notifier -title vde-tmux -message "$VDE_AGENT $VDE_BADGE_STATE"'
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
| `vt daemon doctor` | Diagnose configuration, hooks, display output, and notifications |
| `vt daemon logs daemon --lines 100` | Show the end of the daemon log |

`stop` does not disable automatic startup.
Use `disable` when the daemon must remain stopped.

## Troubleshooting

### The status line or sidebar does not update

Inspect daemon health:

```bash
vt daemon status
vt daemon doctor
```

Reload after changing vde-tmux configuration:

```bash
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
vt daemon doctor
vt daemon logs daemon --lines 100
```

Daemon records and logs are stored below `$XDG_STATE_HOME/vde-tmux/`.
Sidebar preferences are stored below `$XDG_STATE_HOME/vde/tmux/sidebar-state/`.

## Development checks

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
scripts/preflight-ui-ux.sh
```

The UI/UX preflight uses a dedicated tmux server and temporary directories.
It does not modify the regular tmux server or user configuration.
Artifacts are written below `target/preflight/`.

## Known limitations

- Without hooks, waiting detection is limited to states that can be inferred from visible pane output
- When the daemon stops, the last rendered status options remain until the next hook event or `vt daemon ensure`
- Older versions of less may not close a preview with `Esc`; use `q` instead

## License

[MIT](./LICENSE)
