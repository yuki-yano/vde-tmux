# vde-tmux

[English](./README.md) | **日本語**

vde-tmux は、tmux で動かしている AI コーディングエージェントの状態を一覧できるツールです。
Claude Code、Codex、opencode の pane を追跡し、status line とサイドバーへ表示します。

![vde-tmux sidebar](https://github.com/user-attachments/assets/e912448f-b657-49d9-b175-39a0cbad04f2)

## できること

- 複数の tmux session にいるエージェントを `Blocked`、`Working`、`Done`、`Idle` に分類する
- status line で、対応が必要なエージェントと作業中のエージェントを確認する
- サイドバーから prompt、経過時間、task、subagent、worktree activity を確認する
- サイドバーからエージェントの pane へ移動し、スクロールバックを preview する
- session をカテゴリで整理し、キーボードやマウスで切り替える
- エージェントが入力待ちになったとき、任意の通知コマンドを実行する

## 必要なもの

- tmux 3.2 以降
- 最新の stable Rust と Cargo（インストールに使用）
- git（repository と branch の表示に使用）
- lsof（daemon socket の検証に使用）
- less（サイドバーの preview に使用）
- fzf（session manager を使う場合）
- ghq（project selector を使う場合）

## インストール

crates.io からインストールします。

```bash
cargo install vde-tmux --locked
```

`vt` と `vde-tmux` の二つのコマンドがインストールされます。
以降は短い名前の `vt` を使います。

```bash
vt --version
```

## 最初の設定

### tmux に表示とキーバインドを追加する

`~/.tmux.conf` に次の設定を追加します。

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

vde-tmux が `@vde_status_*` option を更新するため、status line の再描画ごとに外部プロセスは起動しません。
上の設定は tmux 標準の window list を vde-tmux の表示へ置き換えます。

設定を読み込み直します。

```bash
tmux source-file ~/.tmux.conf
```

複数の tmux client を使う場合、session や category を操作する binding には `--client-name` と `--session-id` の両方が必要です。
上の例をそのまま使えば、操作対象が別の client へずれることはありません。

### Claude Code の hook を設定する

`~/.claude/settings.json` の `hooks` に次の設定を追加します。

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

Claude Code を再起動すると、状態遷移と task の進捗が表示されます。

### Codex の hook を設定する

`~/.codex/hooks.json` または project の `.codex/hooks.json` に次の設定を追加します。
追加後、Codex の `/hooks` で内容を確認して承認します。

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

Codex を再起動すると、permission request、plan、subagent、worktree activity がサイドバーへ反映されます。

### 動作を確認する

tmux 内で daemon の状態を確認します。

```bash
vt daemon status
vt daemon doctor
```

サイドバーを開きます。

```bash
vt sidebar open
```

hook を設定していないエージェントも、pane の実行コマンドから検出できる場合があります。
ただし、prompt、完了時刻、入力待ちを正確に表示するには hook が必要です。

## 状態の読み方

| 表示 | 状態 | 意味 |
| --- | --- | --- |
| `▲` | Blocked | 許可や回答など、利用者の入力を待っている |
| `●` | Working | エージェントが作業している |
| `✓` | Done | 作業が完了し、まだ確認されていない |
| `○` | Idle | 作業がない、または完了を確認済み |

`Done` は、対象の pane または window を確認すると `Idle` になります。
確認範囲は `daemon.done_clear_on` で変更できます。

```yaml
daemon:
  done_clear_on: window # window | pane
```

確認状態は daemon の再起動後も保持され、すべての tmux client とサイドバーで共有されます。

## サイドバー

サイドバーは現在の tmux window に開きます。
デフォルトの表示モードはカテゴリ別です。

```bash
vt sidebar open --width 40
vt sidebar open --width 20%
vt sidebar toggle
vt sidebar toggle --all
vt sidebar rail
vt sidebar close
```

`vt sidebar focus-toggle` は、サイドバーがなければ開き、表示中ならフォーカスし、フォーカス中なら閉じます。
開閉状態はすべての session で共有されます。

| キー | 動作 |
| --- | --- |
| `j` / `k`、`↓` / `↑` | 行を移動する |
| `Enter` | 選択したエージェントの pane へ移動する |
| `Space` | 選択行を開閉する |
| `v` | 表示モードを切り替える |
| `1` / `2` / `3` | Flat / ByRepo / ByCategory へ切り替える |
| `Tab` | 状態フィルタを切り替える |
| `n` / `N` | 次または前の要対応エージェントへ移動する |
| `d` | 完了状態を確認済みにする |
| `J` / `K` | 手動順序を変更する |
| `p` | スクロールバックを preview する |
| `e` | live output を切り替える |
| `q` / `Esc` | サイドバーを閉じる |

カーソル位置は `›` で示します。
選択中のエージェントを開いた場合、カーソルは先頭行だけに表示され、背景色は展開内容全体へ適用されます。
現在の session に属するエージェントには左端へ `▎` を表示します。

表示モード、フィルタ、手動順序、開閉状態は保存されます。
選択位置、scroll、通知メッセージはサイドバーごとの一時状態です。

## session と category

category を使うと、project path または session name で tmux session をまとめられます。

```yaml
categories:
  default_category: misc
  rules:
    - category: work
      path_patterns:
        - github.com/acme/*
```

主なコマンドは次のとおりです。

```bash
vt category next
vt category prev
vt category use work
vt session-cycle next
vt session-cycle prev
vt session new -c ~/src/my-project
vt session set-category my-session work
```

fzf をインストールすると、session、window、pane を選ぶ popup を利用できます。

```bash
vt session-manager --popup
```

ghq を使っている場合は、project selector から session を作成または選択できます。

```bash
vt project selector --popup
```

## 設定ファイル

設定ファイルは `$XDG_CONFIG_HOME/vde/tmux/config.yml` に置きます。
`XDG_CONFIG_HOME` が未設定の場合は `~/.config/vde/tmux/config.yml` を使います。
設定ファイルがなくてもデフォルト値で動作します。

最初は必要な項目だけを指定してください。

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

設定を変更したら daemon を読み込み直します。

```bash
vt daemon reload
```

## 通知

エージェントが `Blocked` へ移ったときに外部コマンドを実行できます。

```yaml
notify:
  enabled: true
  command: 'terminal-notifier -title vde-tmux -message "$VDE_AGENT $VDE_BADGE_STATE"'
```

通知コマンドには `VDE_PANE_ID`、`VDE_AGENT`、`VDE_BADGE_STATE` が渡されます。

## その他のエージェントを接続する

Claude Code と Codex 以外のエージェントは、`vt hook emit` で状態を送れます。
`--session-id` には一つのエージェント実行中に変わらないIDを指定します。

```bash
vt hook emit \
  --agent myagent \
  --session-id run-42 \
  --status running \
  --prompt "fix the build" \
  --prompt-source user
```

`--status` は `running`、`waiting`、`idle`、`error` を受け取ります。
入力待ちを送る場合は理由も指定します。

```bash
vt hook emit \
  --agent myagent \
  --session-id run-42 \
  --status waiting \
  --wait-reason permission_prompt
```

## daemon の操作

通常は tmux 設定の `vt daemon ensure` だけで起動を管理できます。

| コマンド | 用途 |
| --- | --- |
| `vt daemon ensure` | daemon が必要なら起動する |
| `vt daemon reload` | 設定を検証して再起動する |
| `vt daemon stop` | daemon を一時停止する |
| `vt daemon disable` | 自動起動を無効にして停止する |
| `vt daemon enable` | 自動起動を有効にして起動する |
| `vt daemon status` | daemon と hook の状態を表示する |
| `vt daemon doctor` | 設定、hook、表示、通知を診断する |
| `vt daemon logs daemon --lines 100` | daemon log の末尾を表示する |

`stop` は自動起動を無効にしません。
停止状態を維持したい場合は `disable` を使います。

## トラブルシュート

### status line またはサイドバーが更新されない

daemon の状態と診断結果を確認します。

```bash
vt daemon status
vt daemon doctor
```

設定を変更した直後であれば、daemon を読み込み直します。

```bash
vt daemon reload
```

### tmux の設定を読み込むと hook が壊れる

vde-tmux は tmux hook の index `70` を使います。
同じ hook に独自の処理を追加する場合は、別の index を明示してください。

```tmux
set-hook -g client-session-changed[0] 'your-command'
```

index を付けない `set-hook` は既存の hook 配列を置き換えます。

### 設定エラーを確認する

```bash
vt daemon doctor
vt daemon logs daemon --lines 100
```

daemon の記録とlogは `$XDG_STATE_HOME/vde-tmux/` に保存されます。
サイドバーの設定は `$XDG_STATE_HOME/vde/tmux/sidebar-state/` に保存されます。

## 開発時の確認

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
scripts/preflight-ui-ux.sh
```

UI/UX preflight は専用の tmux server と一時ディレクトリを使います。
通常の tmux server と設定ファイルは変更しません。
結果は `target/preflight/` に保存されます。

## 既知の制約

- hook がない場合、入力待ちの判定は pane に表示された内容から推測できる範囲に限られる
- daemon が停止すると最後に描画した status option が残り、次の hook event または `vt daemon ensure` まで更新されない
- 古い less では preview を `Esc` で閉じられない場合があるため、その場合は `q` を使う

## License

[MIT](./LICENSE)
