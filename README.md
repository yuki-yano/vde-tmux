# vde-tmux

**English** | [日本語](./README.ja.md)

vde-tmux shows the state of AI coding agents running in tmux.
It tracks Claude Code, Codex, and opencode panes and renders their state in the tmux status line and a dedicated sidebar.

![vde-tmux sidebar](https://github.com/user-attachments/assets/e912448f-b657-49d9-b175-39a0cbad04f2)

## Features

- Classifies agents across all tmux sessions as `Blocked`, `Limited`, `Working`, `Done`, or `Idle`
- Shows agents that need attention directly in the tmux status line
- Displays task summaries, elapsed time, tasks, subagents, and worktree activity in a sidebar
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
set -g status-right-length 80
set -g status-left '#{@vde_status_summary}#[fg=#8f8ba8] │ #[default]#{@vde_status_category}#[fg=#8f8ba8] │ #[default]#{@vde_status_sessions} #{@vde_status_attention}'
set -g status-right '#{@vde_status_windows}'

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
- `Blocked`, `Limited`, `Working`, and `Done` agent panes fill the rest of their bottom pane border with a line in the badge color.
- The `window-status-*` settings replace tmux's native window list with the vde-tmux session and window segments.
- The Window map belongs in `status-right`, so a long Session list does not hide the current Window. If you override `status-format`, include its right-aligned `#{T:status-right}` area as well.
- `--client-name` and `--session-id` keep actions scoped to the client that triggered them when multiple tmux clients are attached.

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

The default `<C-h/j/k/l>` mappings move between Neovim windows and, at an edge, ask the daemon to move to the adjacent tmux pane. When the destination pane runs Neovim, the plugin selects the window aligned with the source cursor.

Existing mappings can call the API directly, such as `require('vde-tmux').navigate('h')`. `setup()` accepts `keybindings = false`, `modes`, `debug`, `disable_when_floating`, and `navigate_from_floating`.

The daemon switches panes through one persistent tmux control-mode client. vde-tmux ignores it when deciding whether a session has an attached client, but it is visible in `tmux list-clients`. While no regular client is attached, tmux treats the session hosting this client as attached for its own alerts and `destroy-unattached`.

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
        "matcher": "^(update_plan|request_user_input_async)$",
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

Start Codex in tmux with `codex --no-daemon`.
vde-tmux accepts hooks only from this Embedded mode: hooks from a shared app-server cannot identify the pane they came from, so they are rejected.
The option applies only to that invocation and leaves other clients' shared daemon settings unchanged; `features.daemon_auto_start=false` alone does not prevent attaching to an already running server.
Shared-server commands such as `codex queue` are unavailable with `--no-daemon`, and an active shared thread should not be opened in both modes.

Permission requests, plans, subagents, worktree activity, and [question notices](#codex-question-notices) then appear in the sidebar.

Without accepted hooks, a Codex pane shows the state observed on its screen: Working, or Blocked for an approval or synchronous question.
A screen that cannot be recognized shows `?` (Unknown).
Screen observation never completes a run or acknowledges a question notice.
`summary.presentation` in `vt agent get --json` explains why a badge was chosen.

### 5. Verify

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
| `▲` | Blocked | The agent needs input or hit an error |
| `⋄` | Limited | The provider usage or session allowance is exhausted; excluded from Needs action |
| `●` | Working | The agent is running |
| `✓` | Done | The run completed and has not been acknowledged |
| `○` | Idle | No work is active, or the completed run was acknowledged |
| `?` | Unknown | The screen of a Codex pane without accepted hooks could not be recognized |

A `Done` agent becomes `Idle` when its exact pane is active for a tmux client.
Viewing another split in the same window does not acknowledge it.
Read state survives daemon restarts and is shared by every tmux client and sidebar.

`unread-latest` jumps to the pane with the newest unread event (waiting for input, error, or completion) across all panes.
The jump itself does not mark anything read; the destination becomes read once it is observed as the active pane.

Limited comes from Claude Code's `StopFailure` hook with `error=rate_limit`, or from the provider's usage-limit message on the screen.
Other Claude Code API failures become Blocked.
vde-tmux does not retry a failed turn automatically because the turn may already have had side effects.
The agent leaves Limited on its next `SessionStart` or `UserPromptSubmit`, or when its process exits.
Use `vt pane read` to inspect the provider's message.

## Sidebar

The sidebar opens in the current tmux window.

```bash
vt sidebar open --width 40
vt sidebar open --width 20%
vt sidebar toggle
vt sidebar toggle --all
vt sidebar rail    # toggle between a narrow rail and the normal width
vt sidebar close
```

`vt sidebar focus-toggle` opens a missing sidebar, focuses a visible one, and closes it when it already has focus.

### Views

The sidebar has two independent view settings:

- Scope: `Current` shows the category of the sidebar's session, and `All` shows every category.
- Presentation: `Tree` groups Current as Repository → Agent and All as Category → Repository → Agent. `Priority` groups agents as Pinned, Needs Input, Questions, Limited, Unread Done, Running, then Idle. `Flat` removes grouping.

The Needs action filter includes Blocked agents and agents with unacknowledged [question notices](#codex-question-notices).
Unread Done agents appear under the Done filter.

Agents pinned with `p` come first in Priority and Flat, and their Category and Repository come first in Tree.
A pin is independent of unread, badge, and notification state, and is removed when the pane disappears.

A cyan `▎` marks agents that belong to an active session.
The agent pane focused by a tmux client uses the yellow `selection_bar` color instead.
Keyboard selection is shown by the row background.
An editprompt editor pane counts as focusing its target agent when the `@editprompt_is_editor`, `@editprompt_target_panes`, and `@editprompt_editor_pane` options link the two panes in both directions.

Repository and linked-worktree branch labels show upstream divergence as `↑N` / `↓N` and staged and unstaged line changes from `HEAD` as `+N` / `-N`.
Untracked text files that Git does not ignore count toward `+N`.
Zero counts are omitted, and binary files are not counted.

Expanded agents show the branch or worktree, task status, ahead/behind counts, listening TCP ports, background commands that Claude Code reports with `run_in_background`, and a `▷` preview of the last response.

Scope, presentation, filter, manual order, expansion, selection, and scrolling are shared by all sidebars on the same tmux server.
Each sidebar keeps its own Current category and return target, which follow its source session.
Scope, presentation, filter, manual order, and expansion are saved per tmux socket under `$XDG_STATE_HOME/vde/tmux/sidebar-state/`.

### Keys

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
| `n` / `N` | Move to the next or previous visible pane needing action or question-notice acknowledgement |
| `p` | Pin or unpin the selected agent |
| `d` | Mark the selected run as complete |
| `Q` | Acknowledge the displayed question notice; does not answer or skip the question |
| `a` | Open the add-category dialog |
| `m` | Open the dialog for moving the selected repository |
| `r` | Open the dialog for renaming the selected dynamic category |
| `D` | Open the dialog for deleting the selected dynamic category |
| `J` / `K` | Change manual ordering |
| `q` / `Esc` | Close the sidebar |

Click the first line of an agent to expand or collapse it, or a later line to jump to its pane.
The mouse wheel scrolls without moving the selection.

In the category dialogs, choose an entry with `j`/`k`, the arrow keys, or `gg`/`G`, save with `Enter`, and cancel with `Esc`.
A failed save keeps the dialog open and shows the error.

### Controlling the sidebar from tmux

An open sidebar can also be controlled without focusing it:

```tmux
bind-key -n M-v run-shell "vt sidebar input v --window #{q:window_id}"
bind-key -n M-f run-shell "vt sidebar input tab --window #{q:window_id}"
bind-key -n C-M-j run-shell "vt sidebar input agent-next --window #{q:window_id} --client-pid #{client_pid}"
bind-key -n C-M-k run-shell "vt sidebar input agent-prev --window #{q:window_id} --client-pid #{client_pid}"
bind-key -n C-M-e run-shell "vt sidebar input read-current --window #{q:window_id} --client-pid #{client_pid}"
bind-key -n M-u run-shell "vt sidebar input unread-latest --window #{q:window_id}"
bind-key -n M-p run-shell "vt sidebar input pin-toggle --window #{q:window_id}"
```

`C-M-j` and `C-M-k` work only in Priority view.
They move the invoking client to the next or previous visible agent without wrapping, and keep the agent unread (peek).
`C-M-e` marks the current peek target read; in Priority, it then moves to the next unread agent below.
Peek state belongs to the invoking client: another client viewing the same pane still marks it read.
Restarting the daemon ends peek mode.

### Layout managers

Layout managers can reserve the sidebar synchronously before applying a pane layout:

```bash
vt sidebar prepare-layout --window @12 --json
```

The command prints one JSON object such as
`{"window_id":"@12","status":"ready","reserved_panes":[{"pane_id":"%23","role":"sidebar"}],"content_anchor":"%22"}`.
`ready` means every returned sidebar pane exists and has finished width reconciliation; it does not wait for the sidebar to render.
When the auto-all hook is disabled and no sidebar exists, the command returns `absent`, an empty `reserved_panes`, and an existing content pane as `content_anchor`.
The command is idempotent with the auto-all new-window hook.

The command does not start the daemon.
It exits non-zero when the daemon is not running with its configuration applied (run `vt daemon reload` first), when the window is invalid or has no content pane, or when a tmux operation fails.

## Codex question notices

Codex's `request_user_input_async` asks questions while the turn keeps running.
With stock Codex CLI 0.155.1, 0.156.1, 0.159.3, and 0.160.0, the `PostToolUse` hook above observes these questions; keep `request_user_input_async` in its matcher, or use an unfiltered `PostToolUse` hook.
Only Embedded mode (`codex --no-daemon`) is supported, and subagent questions are excluded.

- The sidebar shows `?` on an agent with an unacknowledged question notice. A parent `? N` counts panes, not questions.
- Select the agent or one of its detail rows and press `Q` to acknowledge the notices you saw. `Q` does not answer or skip the question. A question that arrives during the operation stays visible.
- Acknowledgement is shared across sidebars and does not change focus, agent input, run status, or unread Done.
- `!` (or a parent `! N`) means notice tracking is degraded; expand the agent to see the reason.

When Codex accepts an answer, vde-tmux matches its question IDs and acknowledges those notices automatically, in every Codex version.
After a partial answer, the unanswered questions stay visible; answered items are remembered across daemon restarts.

Skipping a question does not acknowledge it.
An ordinary prompt accepted in a later turn of the same session can acknowledge earlier notices once that turn becomes Idle or Done and the screen shows a normal input field.
Whenever vde-tmux cannot verify this, for example after resume, fork, `/clear`, rollback, an unknown Codex version or layout, or a clipped view, the notice stays until you press `Q`.
This check does not prove that a question was read or answered.

Question and answer text is never saved; vde-tmux keeps only bounded hashes.
Questions issued before the hook was installed are not recovered.
Notices close when their Codex process or pane is gone.
Question notices add no status-line entry or OS notification.
See [Question notices](./AGENT_API.md#question-notices) for the full contract.

## Sessions and categories

Categories group repositories by canonical project identity.
Git worktrees that share a common directory are treated as one repository:

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
vt category get --repo ~/src/temporary-project
vt category create scratch
vt category assign scratch --repo ~/src/temporary-project
vt category automatic --repo ~/src/temporary-project
vt session-cycle next
vt session-cycle prev
vt session new -c ~/src/my-project
```

Categories from the configuration file cannot be changed from vde-tmux.
Dynamic categories, explicit repository assignments, and category and repository order are stored per tmux socket.
An explicit assignment overrides the config rules until `vt category automatic` is run, and is restored when a session for the same repository is recreated.
All × Tree keeps repositories of managed sessions visible even when they have no agent panes.
vde-tmux writes the effective category to `@vde_category` for external tmux formats; changing the option does not change the membership.

`category list`, `get`, `assign`, and `automatic` accept `--json`, which lets agents change repository membership through a versioned interface.
See [Repository category membership](./AGENT_API.md#repository-category-membership).

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
  windows:
    badge_style: inline
    agent_badge:
      enabled: true
      mode: counts
      hide_idle: false
    current:
      format: " {index} {badge} "
      bold: true
      colors:
        fg: "#e8ecfb"
        bg: "#434662"
    other:
      format: " {index} {badge} "
      colors:
        fg: "#a6adc8"
        bg: "#2a2b3c"
    bell:
      fg: "#f9e2af"
    activity:
      fg: "#f9e2af"
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
    limited: "⋄"
    working: "●"
    done: "✓"
    idle: "○"
    unknown: "?"
```

The full schema is available with `vt config schema`.
Reload the daemon after changing the file:

```bash
vt daemon reload
```

`statusline.summary.format` supports the `{badge}` and `{count}` placeholders, such as `{badge}{count}` or `{badge}: {count}`.
Zero-count states stay visible so the summary width stays stable; set `hide_idle: true` to omit the idle token.
When enabled, the summary stays visible even when category or window content is long.
The Window map starts with `W N`, where `N` is the number of Windows in the current Session.
Each cell uses the real tmux index, including zero-based indices and gaps; the active index is reversed.
Only the current Window name follows the map, limited to 16 display cells.
Each Window's agents are counted separately in the same state order and colors as Session badges:
Blocked, Limited, Working, unread Done, Unknown, and Idle. Counts are enabled by default; zero-count states are omitted.
The map uses an independent 80-cell budget. When it does not fit, it keeps contiguous neighbors of the active Window and shows omitted Window counts on their respective sides.
Hidden Blocked, Limited, and unread Done agent counts remain visible next to each omitted side.
The current index and agent counts survive compaction; internal `@` IDs are only click targets.
Cells are padded to the widest cell so their positions stay stable when the active Window changes.
Bell (`♪`) and activity (`·`) indicators are separate from agent badges and do not recolor the whole Window.
If an existing configuration uses `{index}:{window}` or disables `windows.agent_badge`, update that section to the map configuration above.
The category segment lists every category that has a session with its full label, even when it exceeds the status width.

`statusline.sessions.fixed_width: true` pads the session area to the widest category, so the combined category, session, and window area keeps the same width when you switch sessions.
Content is left-aligned by default; set `fixed_width_alignment: center` to center it.
If the `current` and `other` session formats have different widths, the area can differ by a few cells.

`sidebar.task_summary.enabled` adds a short summary of the current task to agent rows.
The daemon generates it asynchronously with the CLI that matches the agent (`codex exec` for Codex, `claude -p` for Claude), without falling back to another provider.
This sends bounded, best-effort redacted prompt text in an additional model request; keep the feature disabled if that is not acceptable.
Model names are optional and default to the installed CLI's default.
The summary follows the latest four prompts of the current agent, and an older summary is hidden while a new one is pending or after it fails.
`vt agent get --json` reports `task_summary_status` (`current` or `failed`) and, on failure, `task_summary_error`.

### Codex capacity errors

vde-tmux can resume a Codex CLI 0.160.0 session started with `--no-daemon` after its turn fails with `Selected model is at capacity. Please try a different model.`
Capacity failures are always recorded as Error / Unresolved and never count as Done.
Automatic resume is disabled by default. Enable it with:

```yaml
codex:
  capacity_auto_resume:
    enabled: true
    # Omit prompt to use the English default, or set your own UTF-8 text.
    prompt: |-
      Resume only unfinished work after checking the current state. Preserve the original objective, scope, constraints, approvals, and response language. Do not repeat completed operations. This message is not a new approval.
```

vde-tmux retries the same model after 60, 120, and 300 seconds with 0–20% jitter, at most three times per failure chain.
It does not change models or grant approvals.

A resume prompt is sent only when the pane still shows the same failed turn with an empty input field, and nothing has changed since the failure: no queued input, images, dialogs, question notices, copy mode, or interaction with the pane.
A manual prompt, a session or process change, a successful completion, or another error ends the chain.
Only prompts that a person sent while the current daemon was running can start a chain.
Other Codex versions, narrow panes, and changed screens are still recorded as failures but are not resumed.
Pending retries are discarded when the daemon restarts.
If delivery of a resume prompt cannot be confirmed, vde-tmux stops sending to that Codex process, even across daemon restarts, until the process or pane is replaced.

Stock Codex cannot check its input field and submit in one atomic step: typed input or an approval prompt can still appear between the final check and the paste.
Enable the feature only if that risk is acceptable.

`vt daemon diagnostics --json` reports attempts, the next retry, and the stop reason under `codex_capacity_auto_resume`, without prompt text.
A custom prompt may contain LF and is limited to 65,536 UTF-8 bytes after trailing LFs are removed.
The daemon refuses to start or reload with an empty prompt, surrounding whitespace, unsafe control characters, a leading `/` or `!`, or a trailing `@`/`$` completion token.

## Notifications

Run an external command whenever an agent enters `Blocked`:

```yaml
notify:
  enabled: true
  command: 'terminal-notifier -title vde-tmux -message "$VDE_AGENT needs attention"'
```

The command receives `VDE_PANE_ID`, `VDE_AGENT`, and `VDE_BADGE_STATE`.
A queued notification is skipped if its Blocked state has already been resolved when the command is about to run.

## Agent JSON API

Agents can inspect panes and other agents, and wait for them, without polling tmux:

```bash
vt api schema --json
vt api snapshot --json
vt agent list --status working --json
vt agent wait %456 --until done,blocked,limited --json
vt pane read %456 --source latest --lines 120 --json

AGENT_REF="$(vt agent get %456 --json | jq -r '.result.agent.summary.agent_ref')"
REQUEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/vt-request.XXXXXX")"
printf '%s' 'Review the current diff.' >"$REQUEST_DIR/prompt.txt"
PROMPT_JSON="$(vt agent request "$AGENT_REF" \
  --state-file "$REQUEST_DIR/request.json" \
  --prompt-file "$REQUEST_DIR/prompt.txt" --json)"
RUN_REF="$(printf '%s' "$PROMPT_JSON" | jq -r '.result.run_ref')"
vt agent run wait "$RUN_REF" --json
vt agent run response "$RUN_REF" --json
```

- `vt api snapshot --json` returns panes, agents, and daemon diagnostics from one consistent revision. Prefer it over combining `tmux list-panes` with separate `vt pane list` and `vt agent list` calls.
- `vt agent request` sends a prompt to a Codex agent durably. It saves its progress in the `--state-file` you choose; if the response is lost, rerun it with the same target and state file and without a prompt body to resume instead of resending. Use a new state-file path for every new prompt. Prompt text never appears in argv.
- A Codex session freshly started with `--no-daemon` can take its first `agent request` while it still shows Unknown.
- `agent send` (idle or done agents), `agent steer` (working agents), and `agent send-keys` (blocked agents) provide guarded terminal input. `pane split` and `agent start` create panes and start agents.
- Mutations require an exact reference. An `agent_ref` is available only while vde-tmux can identify one live agent process by its PID and start time.

See [Agent JSON API](./AGENT_API.md) for the full contract.

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
`--prompt` is display metadata passed in the process argv, so do not use it for secrets.
A waiting event also needs a reason:

```bash
vt hook emit \
  --agent myagent \
  --session-id run-42 \
  --status waiting \
  --wait-reason permission_prompt
```

Report exhausted usage with `--wait-reason usage_limit`.

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

The daemon saves pane details such as prompts, tasks, subagents, lifecycle, summaries, and read state to `$XDG_STATE_HOME/vde-tmux/<incarnation-hash>/pane-state-v10.json`, one file per tmux server.
After a daemon restart, it restores them for panes whose pane ID and PID still match.
Files for tmux servers that no longer run are not removed automatically.

If the file is corrupt or has unsafe permissions, the daemon does not start, and `vt daemon status` shows the file path in `last_transition_error`.
Remove the file only if you intend to reset all saved pane state for that tmux server, then run `vt daemon ensure`.

## Upgrading

The daemon and its clients (sidebar, status line, CLI) must run the same version; there is no cross-version compatibility.
Stop the daemon before replacing the binary, then start the new one and reopen any sidebars:

```bash
vt daemon stop
cargo install vde-tmux --locked
vt daemon ensure
```

If the binary was replaced while the old daemon was still running, `vt daemon stop --force` stops it.
Saved pane details are not migrated when the pane-state schema changes, so they reset after such an upgrade.

## Troubleshooting

### The status line or sidebar does not update

Check daemon health. After changing the configuration, reload it; `reload` validates the configuration and reports errors.

```bash
vt daemon status
vt daemon reload
```

Notification, status update, and hook delivery errors are logged to `$XDG_STATE_HOME/vde-tmux/<incarnation-hash>/daemon.log`.

### Reloading tmux configuration breaks hooks

vde-tmux owns tmux hook index `70`.
Use a different explicit index for custom handlers on the same hook:

```tmux
set-hook -g client-session-changed[0] 'your-command'
```

An unindexed `set-hook` replaces the existing hook array.

## Known limitations

- Without hooks, waiting detection is limited to states that can be inferred from visible pane output
- When the daemon stops, the last rendered status options remain until the next hook event or `vt daemon ensure`
- Codex executables or scripts whose paths contain spaces may not be identified, and their hook ownership is then reported as unverified

## License

[MIT](./LICENSE)
