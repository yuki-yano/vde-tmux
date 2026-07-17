# vde-tmux

[English](./README.md) | **日本語**

vde-tmux は、tmux で動かしている AI コーディングエージェントの状態を一覧できるツールです。
Claude Code、Codex、opencode の pane を追跡し、tmux の status line とサイドバーへ状態を表示します。

![vde-tmux sidebar](https://github.com/user-attachments/assets/e912448f-b657-49d9-b175-39a0cbad04f2)

## できること

- すべての tmux session にいるエージェントを `Blocked`、`Working`、`Done`、`Idle` に分類する
- 対応が必要なエージェントを status line に表示する
- prompt、経過時間、task、subagent、worktree activity をサイドバーに表示する
- サイドバーからエージェントの pane へ移動し、スクロールバックを preview する
- session をカテゴリで整理し、キーボードや status line のクリックで切り替える
- エージェントが入力待ちになったとき、任意の通知コマンドを実行する

## 必要なもの

- tmux 3.2 以降
- 最新の stable Rust と Cargo（インストールに使用）
- `PATH` にある git、lsof、less
- 任意：session manager を使う場合は fzf、project selector を使う場合は ghq

## インストール

```bash
cargo install vde-tmux --locked
```

`vt` と `vde-tmux` の二つの同等なコマンドがインストールされます。
以降は短い名前の `vt` を使います。

```bash
vt --version
```

## セットアップ

### 1. tmux の設定

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
set -g @vde_status_now_format '%s'
set -g pane-border-format '#{?#{@vde_status_pane},#{E:@vde_status_pane},#{pane_index} #{pane_current_command}}'

bind-key -n MouseDown1Status run-shell "vt statusline-click --client-name #{q:client_name} --session-id #{q:session_id} #{q:mouse_status_range}"
bind-key -n M-h run-shell "vt session-cycle prev --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n M-l run-shell "vt session-cycle next --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n M-e run-shell "vt sidebar focus-toggle --window #{q:window_id}"
```

設定の要点は次のとおりです。

- `vt daemon ensure` が daemon を必要に応じて起動します。
- vde-tmux は描画済みのテキストを `@vde_status_*` option へ書き込むため、status line の再描画ごとに外部プロセスは起動しません。
- `@vde_status_now_format` は pane border の経過時間表示に必要です。
- `window-status-*` の設定は、tmux 標準の window list を vde-tmux の session と window の表示へ置き換えます。
- `--client-name` と `--session-id` により、複数の tmux client を使っていても操作対象が別の client へずれません。

設定を読み込み直します。

```bash
tmux source-file ~/.tmux.conf
```

### 2. Claude Code の hook

`~/.claude/settings.json` に次の hook を追加します。

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

保存後に Claude Code を再起動すると、状態遷移と task の進捗が表示されます。

### 3. Codex の hook

`~/.codex/hooks.json` または project の `.codex/hooks.json` に次の hook を追加します。
保存後、Codex の `/hooks` で内容を確認して承認します。

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

### 4. 動作確認

tmux 内で次のコマンドを実行します。

```bash
vt daemon status
vt daemon doctor
vt sidebar open
```

hook を設定していなくても、Claude Code、Codex、opencode は pane の実行コマンドから検出できます。
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

サイドバーは現在の tmux window に開き、デフォルトではエージェントをカテゴリ別に表示します。

```bash
vt sidebar open --width 40
vt sidebar open --width 20%
vt sidebar toggle
vt sidebar toggle --all
vt sidebar rail
vt sidebar close
```

`vt sidebar focus-toggle` は、サイドバーがなければ開き、表示中ならフォーカスし、フォーカス中なら閉じます。

| キー | 動作 |
| --- | --- |
| `j` / `k`、`↓` / `↑` | 行を移動する |
| `Enter` | 選択したエージェントの pane へ移動する |
| `Space` | 選択行を開閉する |
| `v` | 表示モードを切り替える |
| `1` / `2` / `3` | Flat / ByRepo / ByCategory へ切り替える |
| `Tab` / `Shift+Tab` | 状態フィルタを切り替える |
| `n` / `N` | 次または前の要対応エージェントへ移動する |
| `d` | 選択中の run を完了としてマークする |
| `J` / `K` | 手動順序を変更する |
| `p` | スクロールバックを preview する |
| `e` | live 表示を出力とイベントで切り替える |
| `q` / `Esc` | サイドバーを閉じる |

現在の session に属するエージェントには左端へ `▎` を表示します。
表示モード、フィルタ、手動順序、開閉状態は保存され、すべてのサイドバーで共有されます。
選択位置と scroll はサイドバーごとの一時状態です。

## session とカテゴリ

カテゴリを使うと、project path または session name で tmux session をまとめられます。

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

fzf をインストールすると、session、window、pane を切り替えたり削除したりする popup を利用できます。

```bash
vt session-manager --popup
```

selector の最下段には `✕ tmux server | tmux kill-server` が表示されます。
この行は `Ctrl-Q` にのみ反応し、vde daemon の停止と残った pane プロセスの後始末を済ませてから tmux server 全体を終了します。

ghq を使っている場合は、project selector から session を作成または選択できます。

```bash
vt project selector --popup
```

## 設定ファイル

設定ファイルは `$XDG_CONFIG_HOME/vde/tmux/config.yml` に置きます。
`XDG_CONFIG_HOME` が未設定の場合は `~/.config/vde/tmux/config.yml` を使います。
すべての設定にデフォルト値があるため、設定ファイルは任意で、必要な項目だけを書けば動作します。

前節の `categories` と合わせて、よく使う設定は次のとおりです。

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

`statusline.summary.format` では `{badge}` と `{count}` の placeholder を使えます（`{badge}{count}`、`{badge}: {count}` など）。
件数が 0 の状態も表示するため、summary の表示幅は安定します。
Idle を表示したくない場合は `hide_idle: true` を指定します。

設定全体のスキーマは `vt config schema` で確認できます。

設定を変更したら daemon を読み込み直します。

```bash
vt daemon reload
```

## 通知

エージェントが `Blocked` へ移ったときに外部コマンドを実行できます。

```yaml
notify:
  enabled: true
  command: 'terminal-notifier -title vde-tmux -message "$VDE_AGENT needs attention"'
```

通知コマンドには `VDE_PANE_ID`、`VDE_AGENT`、`VDE_BADGE_STATE` が渡されます。

## その他のエージェントを接続する

Claude Code と Codex 以外のエージェントは、`vt hook emit` で状態を送れます。
`--session-id` には一つのエージェント実行中に変わらない ID を指定します。

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

## アップグレード

daemon とそのクライアント（サイドバー、status line、CLI）はバージョンが一致している必要があり、異なるバージョン間の互換はありません。
バイナリを差し替える前に daemon を止め、新しい daemon を起動してからサイドバーを開き直します。

```bash
vt daemon stop
cargo install vde-tmux --locked
vt daemon ensure
```

古い daemon が動いたままバイナリを差し替えた場合は、`vt daemon stop --force` で停止できます。

## トラブルシュート

### status line またはサイドバーが更新されない

daemon の状態を確認し、設定を変更した直後であれば読み込み直します。

```bash
vt daemon status
vt daemon doctor
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

daemon の記録と log は `$XDG_STATE_HOME/vde-tmux/` に保存されます。
サイドバーの設定は `$XDG_STATE_HOME/vde/tmux/sidebar-state/` に保存されます。

## 既知の制約

- hook がない場合、入力待ちの判定は pane に表示された内容から推測できる範囲に限られる
- daemon が停止すると最後に描画した status option が残り、次の hook event または `vt daemon ensure` まで更新されない
- 古い less では preview を `Esc` で閉じられない場合があるため、その場合は `q` を使う

## License

[MIT](./LICENSE)
