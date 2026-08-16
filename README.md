# vde-tmux

**English** | [日本語](./README.ja.md)

vde-tmux shows the state of AI coding agents running in tmux.
It tracks Claude Code, Codex, and opencode panes and renders their state in the tmux status line and a dedicated sidebar.

![vde-tmux sidebar](https://github.com/user-attachments/assets/e912448f-b657-49d9-b175-39a0cbad04f2)

## Features

- Classifies agents across all tmux sessions as `Blocked`, `Working`, `Done`, or `Idle`
- Shows agents that need attention directly in the tmux status line
- Displays prompts, elapsed time, tasks, subagents, and worktree activity in a sidebar
- Jumps to agent panes directly from the sidebar
- Groups sessions into categories and switches them from the keyboard or status line
- Runs a notification command when an agent starts waiting for input

## Requirements

- tmux 3.2 or later
- The latest stable Rust and Cargo for installation
- git and lsof on `PATH`
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
- The daemon stores the absolute path of the running `vt` executable in `@vde_executable`. Neovim pane navigation uses that exact binary instead of searching `PATH`.
- vde-tmux pushes rendered text into the `@vde_status_*` options, so tmux does not start a process on every status redraw.
- `@vde_status_now_format` is required for the elapsed time shown on pane borders.
- `Blocked`, `Working`, and `Done` agent panes fill the unused pane-statusline width with a plain single-line rail in the badge color. With `pane-border-status bottom`, only the bottom edge is highlighted and no content cells on the left or right are covered. `Idle` and non-agent panes get no additional rail.
- The `window-status-*` settings replace tmux's native window list with the vde-tmux session and window segments.
- `--client-name` and `--session-id` keep session and category bindings scoped to the client that triggered them, which matters when multiple tmux clients are attached.

Reload the configuration:

```bash
tmux source-file ~/.tmux.conf
```

### 2. Neovim pane navigation (optional)

This repository also provides a Neovim plugin. Load it with lazy.nvim:

```lua
{
  'yuki-yano/vde-tmux',
  lazy = false,
  config = function()
    require('vde-tmux').setup()
  end,
}
```

The default `<C-h/j/k/l>` mappings move between Neovim windows and signal the daemon through a lightweight tmux channel at an edge to enter another tmux pane. The tmux root binding spawns no per-key `vt` or `tmux` process; a Neovim edge uses one tmux client only to send the signal. When the destination runs Neovim, the plugin selects the window aligned with the source cursor. Selection metadata is stored on the destination pane together with its PID, preventing another client or a reused pane from consuming it.

Pane switching is executed through one persistent tmux control-mode client owned by the daemon. The client is attached to an existing session with `ignore-size`, `no-output`, and `no-detach-on-destroy`; vde-tmux excludes it when deciding whether a session has a regular attached client. It is still visible to native `tmux list-clients`, and while no regular client is attached, tmux's native alert and `destroy-unattached` bookkeeping for the one session hosting the control client follows tmux's attached-client semantics.

Existing mappings can call the API directly, such as `require('vde-tmux').navigate('h')`. `setup()` accepts `keybindings = false`, `modes`, `debug`, `disable_when_floating`, and `navigate_from_floating`.

### 3. Claude Code hooks

Add these hooks to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "vt hook claude SessionStart" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "vt hook claude UserPromptSubmit" }] }],
    "PreToolUse": [{ "hooks": [{ "type": "command", "command": "vt hook claude PreToolUse" }] }],
    "PostToolUse": [{ "hooks": [{ "type": "command", "command": "vt hook claude PostToolUse" }] }],
    "Notification": [{ "hooks": [{ "type": "command", "command": "vt hook claude Notification" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "vt hook claude Stop" }] }],
    "StopFailure": [{ "hooks": [{ "type": "command", "command": "vt hook claude StopFailure" }] }]
  }
}
```

Restart Claude Code after saving the file.
Its lifecycle and task progress will then appear in vde-tmux.

### 4. Codex hooks

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

### 5. Verify

Run these commands inside tmux:

```bash
vt daemon status
vt sidebar open
```

vde-tmux can detect Claude Code, Codex, and opencode from the command running in a pane even without hooks.
Hooks are still required for accurate prompts, completion times, and waiting states.

## Agent JSON API

Agents can inspect the daemon's cached canonical topology and wait for live process-identified exact
agent occupants without polling tmux topology:

```bash
vt api schema --json
vt agent list --status working --json
vt agent wait %456 --until done,blocked --json
vt pane read %456 --source latest --lines 120 --json

AGENT_REF="$(vt agent get %456 --json | jq -r '.result.agent.summary.agent_ref')"
OPERATION_ID="$(uuidgen)"
PROMPT_JSON="$(printf '%s' 'Review the current diff.' \
  | vt agent prompt "$AGENT_REF" --operation-id "$OPERATION_ID" --stdin --json)"
OPERATION_REF="$(printf '%s' "$PROMPT_JSON" | jq -r '.result.operation_ref')"
vt agent operation wait "$OPERATION_REF" --until prompt-confirmed --json
RUN_REF="$(vt agent operation get "$OPERATION_REF" --json | jq -r '.result.run_ref')"
vt agent run wait "$RUN_REF" --until completed --json
vt agent run response "$RUN_REF" --json
```

See [Agent JSON API](./AGENT_API.md) for the response envelope, stable occupant references,
durable run completion, filters, and capture bounds. An exact `agent_ref` is emitted only when one
unique live agent process can be pinned by PID and OS start token. Hooks remain necessary for
accurate lifecycle details, but hookless agents can use `agent wait` and `agent read` when that live
process identity is available. API v3 stores durable Run and Operation records separately from the
bounded pane projection. Guarded prompt dispatch is daemon-owned, requires healthy tmux hooks and
foreground input ownership, and never places prompt bytes in argv. Reusing the same operation ID
performs an idempotent lookup/resume; `delivery_unknown` is never auto-retried. Historical unresolved
runs remain readable while retained. CAS recovery is restricted to the Pane's current durable Run,
which is checked twice against Pane, process, foreground ownership, and visible viewport state.
Prompt input treats one terminal LF or CRLF from stdin or a file as a text-record terminator and
removes it before hashing and dispatch, while preserving all internal line breaks.

When Claude Code or Codex exhausts its allowance, the pane remains queryable as
`status=blocked`, `lifecycle.state=waiting`, and `lifecycle.reason=usage_limit`, even if the agent
process exits. Claude Code's `StopFailure` rate-limit event is authoritative. A bounded,
five-second supplementary pane-tail scan recognizes only the provider messages `You've hit your
session limit` and `You've hit your usage limit`; generic rate-limit text and status-line warnings
do not change state. Use `vt pane read` to inspect the provider's reset text. vde-tmux does not
retry automatically or infer recovery from the clock; a later `SessionStart` or
`UserPromptSubmit` is recovery evidence.

## Agent states

| Badge | State | Meaning |
| --- | --- | --- |
| `▲` | Blocked | The agent needs input, hit an error, or exhausted its usage allowance |
| `●` | Working | The agent is running |
| `✓` | Done | The run completed and has not been acknowledged |
| `○` | Idle | No work is active, or the completed run was acknowledged |

A `Done` agent becomes `Idle` when its exact pane is active for an eligible tmux client.
Viewing another split in the same window does not acknowledge it. Read state survives daemon
restarts and is shared by every tmux client and sidebar. The daemon periodically reconciles current
client views, so a missed view hook is repaired by the next observation poll.

`unread-latest` jumps to the newest unread Waiting, Error, or Completed occurrence across all panes.
The daemon owns the global ordering and retries the next unread pane if the newest target disappears
during the jump. The jump itself does not mark anything read; the destination becomes read after it
is observed as the active pane.

## Sidebar

The sidebar opens in the current tmux window with two independent view axes. `Current` limits the
rows to the category of that sidebar's source session, while `All` spans every category. `Tree`
groups Current as Repository→Agent and All as Category→Repository→Agent. `Priority` groups the
selected scope as Pinned, Needs Input, Unread Done, Running, then Idle. `Flat` removes grouping.
Press `p` on any agent to toggle its persistent pane pin without changing unread, badge, or
notification state. `Priority` places pinned agents in the first `PINNED` zone, `Flat` places them
first, and `Tree` promotes their enclosing Category and Repository while keeping the hierarchy.
Pinned agents remain pinned when they become read or their lifecycle changes, and stale pins are
removed when the pane disappears.
The Needs action filter and red triangle match only Blocked agents waiting for user input.
Unread Done agents remain separate under the Done filter. `unread-latest` navigation also includes
unread Blocked occurrences.

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
| `gg` / `G` | Move to the first or last row |
| `Ctrl-D` / `Ctrl-U` | Move down or up by half a page |
| `Ctrl-F` / `Ctrl-B` | Move down or up by a full page |
| `Enter` | Jump to the selected agent pane |
| `Space` | Expand or collapse the selected row |
| `c` | Toggle Current / All category scope |
| `v` | Cycle Tree / Priority / Flat presentation |
| `1` / `2` / `3` | Select Tree / Priority / Flat presentation |
| `Tab` / `Shift+Tab` | Cycle the state filter |
| `n` / `N` | Move to the next or previous Blocked agent waiting for user input |
| `p` | Pin or unpin the selected agent |
| `d` | Mark the selected run as complete |
| `J` / `K` | Change manual ordering |
| `q` / `Esc` | Close the sidebar |

Agents belonging to an active session have a cyan `▎` marker on the left. The exact agent pane
focused by an eligible tmux client uses the yellow `selection_bar` color instead; this marker follows
pane, session, and category changes and disappears when the focused pane is not a live agent.
Keyboard selection remains visible through the row background and does not create a current-agent
marker by itself.
Click the first rendered line of an agent to expand or collapse it. Click its
second or later line to jump to the agent pane without selecting it first.
Use `Space` to expand or collapse the selected agent from the keyboard.
The mouse wheel scrolls overflow without moving the selected cursor.
An agent with no activity yet uses a single line while collapsed.
Expanded agents show a compact signal row with the pane branch or worktree, task status glyphs,
ahead/behind counts, and listening TCP ports. Claude Bash calls are shown as background processes
only when the hook explicitly reports `run_in_background`; command text is never guessed, and the
process row clears after the command leaves the pane process tree. Codex does not currently report a
background flag, but its listening ports are still discovered from the pane process tree. A Stop
payload's `last_assistant_message` appears as a muted `▷` response preview below the latest prompt.
Category scope, presentation, filter, manual order, expansion state, selection, and scrolling are
synchronized across all open sidebars on the same tmux server. The concrete Current category and
return target remain local to each sidebar and follow its source session.

An open sidebar can also be controlled without focusing it. The input source updates that sidebar's
Current category context; axis, filter, selection, and scrolling changes use shared state.

```tmux
bind-key -n M-v run-shell "vt sidebar input v --window #{q:window_id}"
bind-key -n M-f run-shell "vt sidebar input tab --window #{q:window_id}"
bind-key -n M-j run-shell "vt sidebar input agent-next --window #{q:window_id}"
bind-key -n M-k run-shell "vt sidebar input agent-prev --window #{q:window_id}"
bind-key -n M-u run-shell "vt sidebar input unread-latest --window #{q:window_id}"
bind-key -n M-p run-shell "vt sidebar input pin-toggle --window #{q:window_id}"
```

## Sessions and categories

Categories group repositories by canonical project identity. Git worktrees that
share a common directory are treated as one repository:

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
vt category list
vt category create scratch
vt category assign scratch --repo ~/src/temporary-project
vt category automatic --repo ~/src/temporary-project
vt session-cycle next
vt session-cycle prev
vt session new -c ~/src/my-project
```

Configured categories remain the read-only baseline. Dynamic categories, explicit
repository assignments, and category/repository order are stored per tmux socket.
An explicit assignment wins over config rules until `vt category automatic` is
used. Recreating a session for the same repository restores its assignment.
In the sidebar, use `a` to add a category, `m` to move a repository, `r` to
rename a dynamic category, `D` to delete one, and `J`/`K` to reorder categories
or repositories. All × Tree keeps repositories from managed sessions visible
even when they currently have no agent panes. `@vde_category` remains a derived,
write-only mirror for external tmux formats.

With fzf installed, open a popup for switching or removing sessions, windows, and panes:

```bash
vt session-manager --popup
```

The final selector row is `✕ tmux server | tmux kill-server`.
Selecting it with `Enter` or `Ctrl-Q` shuts down the whole tmux server after stopping the vde daemon and cleaning up the remaining pane processes.

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
sidebar:
  width: "20%"
  min_width: 40
  task_summary:
    enabled: false
    debounce_ms: 750
    timeout_ms: 90000
    # codex_model: optional-model-name
    # claude_model: optional-model-name

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

`sidebar.task_summary.enabled` replaces the collapsed agent row's latest-prompt line with a short
persistent-task summary. The daemon generates it asynchronously with an isolated CLI matching the
pane agent (`codex exec` for Codex and `claude -p` for Claude). Expanded rows keep the summary on
the second line and show the latest prompt below it. No cross-provider fallback is used. Prompt
evidence is bounded and best-effort redacted before the additional model request; keep the feature
disabled if that extra request is not acceptable. Model names are optional and otherwise follow the
installed CLI's default.

The category segment publishes every category that contains a session, including categories with no agent panes. Each category keeps its full label and action target; category entries are never collapsed into `+N` or `cat:N`, even when the segment exceeds the shared status width budget.

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
`hook emit --prompt` is public display metadata and is passed in the process argv. Do not use it for
secrets. Provider adapters should accept private bodies on stdin, and agent dispatch should use
`vt agent prompt --stdin` or `--prompt-file`, which never places the prompt body in argv.
A waiting event also needs a reason:

```bash
vt hook emit \
  --agent myagent \
  --session-id run-42 \
  --status waiting \
  --wait-reason permission_prompt
```

Provider integrations can report exhausted usage explicitly with `--wait-reason usage_limit`.

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
`$XDG_STATE_HOME/vde-tmux/<incarnation-hash>/pane-state-v9.json`. A daemon restart restores the
prompt, task progress and items, subagents, worktree activity, lifecycle, timestamps, agent
identity, task context and generated summaries, the latest response preview, explicitly reported
background processes, listening ports, and Done/acknowledgement state for panes
whose pane ID and PID still match.

If this snapshot is corrupt or insecure, daemon startup stops instead of repairing it or falling
back. `vt daemon status` reports the snapshot path in `last_transition_error`; remove that file only when you
intend to reset all saved pane state for that tmux server, then run `vt daemon ensure`.

Production startup does not migrate snapshots from older pane-state schemas. Without a separate
one-shot migration, pane details reset after a schema upgrade. Snapshots for other tmux server
incarnations are not removed automatically.

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
Sidebar order, category scope, presentation, filter, and row expansion are stored atomically below
`$XDG_STATE_HOME/vde/tmux/sidebar-state/`, isolated by tmux socket. These values are shared live by
sidebars on the same tmux server. Selection and scrolling are shared while the daemon is running but
are not persisted. The concrete Current category and return target remain instance-local and are not
persisted.

## Known limitations

- Without hooks, waiting detection is limited to states that can be inferred from visible pane output
- When the daemon stops, the last rendered status options remain until the next hook event or `vt daemon ensure`

## License

[MIT](./LICENSE)
