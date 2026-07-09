# vde-tmux

[English](./README.md) | **日本語**

AI コーディングエージェントのための tmux state & UI マネージャ。
vde-tmux は tmux サーバ上の Claude Code / Codex / opencode の pane をすべて追跡し、その状態を tmux の status line と専用サイドバーに表示する。
あわせて、カテゴリベースのセッション管理と、fzf によるセッション/プロジェクト切替を提供する。

![vde-tmux sidebar](https://github.com/user-attachments/assets/e912448f-b657-49d9-b175-39a0cbad04f2)

## なぜ必要か

複数の AI コーディングエージェントを tmux のセッションをまたいで並行稼働させると、監視が問題になる。
あるエージェントは許可待ちで止まり、別のエージェントは数分前に完了し、さらに別のエージェントはまだ作業中という状態は、各 pane を見に行かない限り分からない。
vde-tmux はすべての agent pane を監視し、**いまどの pane が自分を必要としているか**を一目で答える。

## 機能

- **エージェント状態の追跡**：各 agent pane を 4 状態（`▲` Blocked（入力待ち）、`●` Working、`✓` Done（完了・未読）、`○` Idle）に分類する。
- **status line セグメント**：セッションごとの状態バッジ付きセッション一覧、状態別カウント（`▲2 ●1`）、見えていないセッションの blocked agent への注意喚起（`▲ session · perm 2m`）を表示する。
- **エージェントサイドバー**：全 agent pane の状態、直近の prompt、経過時間を一覧する TUI pane。Enter で pane へジャンプでき、スクロールバックの preview や出力のライブ表示もできる。
- **セッションカテゴリ**：パスやセッション名のルールでセッションをカテゴリ（`work` / `private` など）に分類し、カテゴリ間およびカテゴリ内セッション間を巡回できる。
- **セッション/プロジェクト切替**：fzf ベースのセッションマネージャと、ghq ベースのプロジェクトセレクタを tmux popup として使える。
- **通知**：エージェントが blocked になった瞬間に任意のコマンド（`terminal-notifier` など）を実行できる。

## 動作要件

- **tmux** 3.2 以降（セッションマネージャ、プロジェクトセレクタ、サイドバー preview が popup を使う）
- **Rust toolchain**（edition 2024。Rust 1.85 以降）：ソースからのビルドに必要
- **fzf**：セッションマネージャとプロジェクトセレクタに必要
- **ghq**：プロジェクトセレクタに必要
- **git**：サイドバーの repository/branch バッジに使う
- **less**：サイドバー preview のページャに使う

## インストール

```bash
cargo install --git https://github.com/yuki-yano/vde-tmux vde-tmux
```

同一の CLI を提供する 2 つのバイナリがインストールされる。

- `vt`：常用の短縮名
- `vde-tmux`：正式名

以降の例では `vt` を使う。

## はじめかた

### 1. status line にセグメントを追加する

`~/.tmux.conf` に次を書く。

```tmux
set -g status-interval 1
set -g status-left-length 10000
set -g status-left '#(vt statusline-category)#[fg=#8f8ba8] │ #[default]#(vt statusline-sessions --show-index)#[fg=#8f8ba8] │ #[default]#(vt statusline-windows)'
set -g status-right '#(vt statusline-attention) #(vt statusline-summary)'
setw -g window-status-format ''
setw -g window-status-current-format ''
set -g window-status-separator ''
bind-key -n MouseDown1Status run-shell "vt statusline-click '#{mouse_status_range}'"
```

- `statusline-category`：現在のカテゴリ（設定によっては他カテゴリも）
- `statusline-sessions`：現在カテゴリのセッション一覧。各セッション名の前に agent 状態バッジが付く。`statusline.session_badge.mode: counts` では `▲ 2 ● 1 ○ 5` のように表示する
- `statusline-windows`：現在 session の window 一覧。`statusline.windows` で整形する
- `statusline-pane`：現在 pane の border label。`statusline.panes` で整形する
- `statusline-summary`：全 agent の状態別カウント。例 `▲2 ●1`
- `statusline-attention`：いま見えていない blocked agent の通知。例 `▲ session · perm 2m`

`statusline.sessions.badge_style: chip` を使うと、各 session セグメントの前に接続された chip として session badge を表示する。category/window の badge は `statusline.category.agent_badge` / `statusline.windows.agent_badge` で状態の集計方法を設定し、`statusline.category.badge_style` / `statusline.windows.badge_style` で見た目を設定する。inline 系の表示位置は `{badge}` placeholder で指定する。chip の色と左右 cap は `statusline.session_badge.chip` で設定する。

`status-left-length` には十分大きい値を指定し、左セグメント側の人工的な長さ制限を外す。実際の表示上限は端末幅である。`statusline-windows` は tmux native の window list を置き換えるため、併用時は native の `window-status-*` format を空にする。

表示への反映はおおむね `daemon.poll_ms + status-interval`（デフォルトで約 2 秒）以内に行われる。
agent 状態を収集するバックグラウンドの daemon は自動起動するため、自分で起動する必要はない（手動制御用に `vt daemon` / `vt daemon stop` もある）。

### 2. エージェントを接続する

hook がなくても pane の実行コマンドからエージェントは検出されるが、hook を設定すると状態遷移、prompt、時刻が正確になる。

**Claude Code**：`~/.claude/settings.json` に追加する。

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

`PostToolUse` hook により、vde-tmux は Claude Code の task tool events（`TaskCreate` / `TaskUpdate`、および emit された場合の `TodoWrite` snapshot）から collapsed の `done/total` counter と expanded task rows を表示する。

**Codex**：`~/.codex/hooks.json`（または project-local な `.codex/hooks.json`）に追加し、Codex の `/hooks` で review / trust する。

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

`notify = ["vt", "hook", "codex"]` は legacy turn-complete notification だけを報告するため、task / subagent / worktree activity の detail rows には不十分である。
`PostToolUse` hook により、vde-tmux は `update_plan` snapshot から collapsed の `done/total` counter と expanded task rows を表示し、認識できる `vw exec` Bash command を prompt 下の一時的な `vw exec <worktree_name>` row として表示する。
`SubagentStart` / `SubagentStop` からは expanded の `Agent - ... #id` rows を表示する。
Codex subagent は session metadata を解決できる場合、`Agent - Fermat #019f` のように Codex nickname を優先し、取れない場合は hook の `agent_type` を表示する。
pane が linked git worktree 上にある場合、expanded detail の先頭に `+ <worktree_name>` が表示される。この active worktree row は `vw exec` activity とは別物であり、従来の `session · pane_id` place row は表示しない。
detail row の色は `sidebar.colors` の `task_done`、`task_working`、`task_pending`、`task_label`、`subagent_label`、`subagent_id`、`worktree`、`worktree_activity` で上書きできる。

**その他のエージェント**：汎用の低レベルコマンドで任意のエージェントから状態を報告できる。

```bash
vt hook emit --agent myagent --status running --prompt "fix the build"
```

### 3. サイドバーを開く

```bash
vt sidebar toggle
```

tmux バインドの例。

```tmux
bind-key e run-shell "vt sidebar focus-toggle"   # 開く → フォーカス → 閉じる
bind-key b run-shell "vt sidebar focus"          # pane へジャンプした後にサイドバーへ戻る
bind-key -n M-C run-shell "vt session new -c ~/"  # home で管理対象セッションを作る
bind-key s run-shell "vt session-manager --popup"
bind-key g run-shell "vt project selector --popup"
```

## エージェントの状態

| バッジ | 状態 | 意味 |
| --- | --- | --- |
| `▲` | Blocked | エージェントが入力を待っている（許可プロンプト、質問） |
| `●` | Working | エージェントが実行中 |
| `✓` | Done | 完了したがまだ確認していない |
| `○` | Idle | 作業なし、または確認済み |

グリフと色は `badge.glyphs` と `badge.colors` で変更できる。

## サイドバー

サイドバーは現在の tmux window 内に開く TUI pane である。
agent pane をフラット、repository 別、カテゴリ別のいずれかでグルーピングして一覧し、各エージェントの状態、直近の prompt、経過時間（`45s`、`12m`、`1h30m` など）を表示する。
標準幅の閉じた agent 行は 2 行 digest で表示され、1 行目に状態、経過時間、task 進捗（`☑ done/total`）、実行中 subagent 数（`↳ n`）をまとめ、2 行目に prompt と必要に応じて `↩ permission` のような短縮 blocked 理由を表示する。
行を展開すると prompt の全文と pane の場所が表示され、window レイアウトの変更にはサイドバーが自動で追従する。

### キーバインド

| キー | 動作 |
| --- | --- |
| `j` / `k`、`↓` / `↑` | 下 / 上へ移動 |
| `Enter` | 選択中の agent pane へジャンプ |
| `Space` | 選択行の展開 / 折りたたみ |
| `v`、`1` / `2` / `3` | 表示モードの循環 / 直接指定（Flat / ByRepo / ByCategory） |
| `Tab` | フィルタの循環（all → attn → working → done → idle。0 件のフィルタはスキップ） |
| `n` / `N` | 次 / 前の要対応行へフォーカス |
| `J` / `K` | pin 行の並べ替え |
| `p` | 選択 pane のスクロールバックを floating pane で preview（`less`） |
| `e` | ライブ表示モードの切替 |
| `q` / `Esc` | サイドバー TUI を閉じる |

### サイドバーのコマンド

```bash
vt sidebar open [--width 40|20%]   # 現在の window に開く
vt sidebar toggle [--all]          # 開閉トグル（--all で全 window 一括）
vt sidebar focus-toggle            # 未表示なら開く、非フォーカスならフォーカス、フォーカス中なら閉じる
vt sidebar rail                    # 最小幅のレール表示に畳む
vt sidebar close
```

## セッション、カテゴリ、プロジェクト

セッションは、プロジェクトのパスまたはセッション名のルールで**カテゴリ**（`work`、`private` など）に分類される。

```yaml
categories:
  default_category: misc
  rules:
    - category: work
      path_patterns:
        - github.com/acme/*
```

- `vt category next` / `vt category prev` / `vt category use <name>`：カテゴリを切り替える。各カテゴリで最後にいたセッションを記憶していて、そこへ復帰する
- `vt session-cycle next` / `vt session-cycle prev`：現在カテゴリ内のセッションを巡回する
- `vt session new [-c <path>]`：管理対象の tmux セッションを作成し、project path / category metadata を初期化する
- `vt session set-category <session> <category>`：セッションのカテゴリを手動で上書きする
- `vt session-manager --popup`：セッション、window、pane の切替と kill ができる fzf UI
- `vt project switch <path>` / `vt project selector --popup`：ghq 管理下のプロジェクトに対応するセッションを作成、または切り替える

## 設定

設定ファイルは `~/.config/vde/tmux/config.yml`（`$XDG_CONFIG_HOME` を尊重する）。
すべての項目にデフォルト値があり、設定ファイルがなくても動く。

JSON Schema は `schemas/config.schema.json` にあり、`vt config schema` でも出力できる。
YAML の language server を使う場合は、設定ファイルの先頭に次を置く。

```yaml
# yaml-language-server: $schema=/path/to/vde-tmux/schemas/config.schema.json
```

出発点になる設定の例。

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
  width: "20%"            # 列数またはパーセント
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
  # blocked への遷移時のみ実行される。
  # 環境変数 VDE_PANE_ID / VDE_AGENT / VDE_BADGE_STATE を受け取る。
  command: 'terminal-notifier -title vde-tmux -message "$VDE_AGENT $VDE_BADGE_STATE"'
```

`path_patterns` とセッション名の `patterns` では `${ENV_VAR}` 展開が使える。

`daemon.done_clear_on` は `Done` badge を確認済みにする条件を制御する。`window` は対象 pane を含む window を表示した時点で解除し、`pane` は対象 pane 自体に focus するまで保持する。

### 推奨の status line 配色

状態グリフを常にバー地の上に置き、塗りをカレント要素だけに使う truecolor プリセット。

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
```

```tmux
# ~/.tmux.conf
set -ga terminal-overrides ',*:Tc'
set -g status-style 'bg=#1a1926,fg=#9591ad'
set -g status-left-length 10000
setw -g window-status-format ''
setw -g window-status-current-format ''
set -g window-status-separator ''
bind-key -n MouseDown1Status run-shell "vt statusline-click '#{mouse_status_range}'"
```

hex 指定の色を使うには tmux の truecolor 設定（上記の `terminal-overrides` 行）が必要になる。

## ファイルと実行時パス

- 設定：`~/.config/vde/tmux/config.yml`
- state：`$XDG_STATE_HOME/vde/tmux/state.json`（未設定なら `~/.local/state/vde/tmux/state.json`）
- daemon socket：`$VDE_DAEMON_SOCKET` があればそれを使い、次に `$XDG_RUNTIME_DIR/vde-tmux/daemon.sock`、最後に `/tmp/vde-tmux-<uid>/daemon.sock`

## 既知の制約

- エージェント検出は hook が書いた状態を優先する。hook がない場合は pane の実行コマンド（`claude`、`codex`、`opencode`）から補完し、blocked の判定は表示中の pane 内容から許可プロンプトを認識できる範囲に限られる。
- サイドバー preview を `Esc` で閉じる動作は `less` の `LESSKEYIN` 対応に依存する。古い `less` では `q` で閉じる必要がある。

## License

[MIT](./LICENSE)
