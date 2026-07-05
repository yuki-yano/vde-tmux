# vde-tmux

tmux 上の state と UI の管理ツール(セッション/カテゴリ管理、statusline、エージェントサイドバー)。

[vde-tmux-manager](https://github.com/yuki-yano/vde-tmux-manager) と
[vde-tmux-sidebar](https://github.com/yuki-yano/vde-tmux-sidebar) を参考実装として
スクラッチで書き直し中。バイナリは `vt`(常用)と `vde-tmux`(正式名)の 2 つで、同一 CLI を提供する。

設計・進捗はローカルの設計書(`2026-07-04-vde-tmux-rewrite-design.md`)を参照。

## 機能

- session/category: `statusline-sessions`、`statusline-sessions --show-index`、`statusline-category`、category 切替、session cycle、project switch、session-manager。
- hook: `vt hook claude`、`vt hook codex`、`vt hook emit` で agent 状態を `@vde_*` pane option に書く。
- daemon/statusline: `vt daemon`、`vt daemon stop`、`vt statusline-agent-badge`。daemon が使えない場合は tmux option snapshot を直接読む。
- session badge: daemon が session ごとの agent 状態を 4 色(🔴 Blocked、🟡 Working、🔵 Done 未読、🟢 Idle 既読)に集約し、`statusline-sessions` の session ラベルへ前置する。
- sidebar: `vt sidebar open`、`toggle`、`toggle --all`、`close`、`rail`、`rebaseline`、`layout-applied`、`attach`、`input`、`jump`、`focus`。
- git badge: sidebar の repo 行に branch と ahead/behind を表示する。upstream が無い場合は branch のみを表示する。

## Config

設定ファイルは `~/.config/vde/tmux/config.yml`。
旧 vtm/sidebar config からの自動移行はしない。
JSON Schema は `vt config schema` で出力できる。

```yaml
categories:
  default_category: misc
  rules:
    - category: work
      path_patterns:
        - github.com/acme/*
statusline:
  agent_badge:
    enabled: true
  session_badge:
    enabled: true
    suffix: " "
badge:
  glyphs:
    blocked: "🔴"
    working: "🟡"
    done: "🔵"
    idle: "🟢"
sidebar:
  width: "10%"
  min_width: 40
  preview:
    history_lines: 2000
  live:
    enabled: true
    lines: 3
    interval_ms: 2000
  header:
    format: "{label} "
    separator: ""
    # pill 風にしたい場合:
    # prefix: "["
    # suffix: "]"
    # format: " {label} "
    # separator: " "
    # bold: true
    # colors:
    #   fg: white
    #   bg: "24"
  colors:
    running: green
    waiting: yellow
    permission: light_red
    error: red
    background: dark_gray
    idle: reset
    selection_bg: "237"
daemon:
  poll_ms: 1000
notify:
  enabled: false
  # blocked 遷移時だけ実行する。環境変数 VDE_PANE_ID / VDE_AGENT / VDE_BADGE_STATE を渡す。
  command: "terminal-notifier -title vde-tmux -message \"$VDE_AGENT $VDE_BADGE_STATE\""
```

## State / Socket

sidebar state は `$XDG_STATE_HOME/vde/tmux/state.json`、未設定なら
`~/.local/state/vde/tmux/state.json` を使う。
daemon socket は `VDE_DAEMON_SOCKET` 明示指定を優先し、次に
`$XDG_RUNTIME_DIR/vde-tmux/daemon.sock`、最後に `/tmp/vde-tmux-<uid>/daemon.sock` を使う。
socket directory は 0700 の通常ディレクトリであることを検証する。

## Sidebar jump & return

サイドバーから Enter で agent pane に jump した後、tmux バインドで sidebar に戻れる。

```tmux
bind-key b run-shell "vt sidebar focus"
```

## Known Limits

- Agent pane の生存判定は `pane_current_command` が `claude`、`codex`、`opencode` のいずれかであることを見る。node ラッパー起動などで別コマンド名を返す環境では、hooks が `@vde_agent` を残していても agent として表示しない。
- Sidebar preview の Esc 終了は `less` の `LESSKEYIN` 対応を使う。現行の手元環境では Esc/q で閉じられることを scratch tmux で確認済みで、古い `less` 向けの追加フォールバックは持たない。

## Option Bus

新実装は `@vde_*` の個別 pane/window/session option を使う。
旧 `@pane_*` は書かない。

- pane: `@vde_agent`、`@vde_status`、`@vde_prompt`、`@vde_wait_reason`、`@vde_attention`、`@vde_sidebar`
- window: `@vde_layout_baseline`、`@vde_layout_panes`
- session: `@vde_category`、`@vde_category_override`、`@vde_project_path`、`@vde_session_status`
  - `@vde_session_status` の writer は daemon のみ。graceful shutdown 時に daemon が削除する。

## Docs

- [E2E smoke](docs/e2e-smoke.md)
- [Migration](docs/migration.md)
- [vde-monitor compatibility](docs/vde-monitor-compat.md)
