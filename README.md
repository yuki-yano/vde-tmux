# vde-tmux

tmux 上の state と UI の管理ツール(セッション/カテゴリ管理、statusline、エージェントサイドバー)。

[vde-tmux-manager](https://github.com/yuki-yano/vde-tmux-manager) と
[vde-tmux-sidebar](https://github.com/yuki-yano/vde-tmux-sidebar) を参考実装として
スクラッチで書き直し中。バイナリは `vt`(常用)と `vde-tmux`(正式名)の 2 つで、同一 CLI を提供する。

設計・進捗はローカルの設計書(`2026-07-04-vde-tmux-rewrite-design.md`)を参照。

## 機能

- session/category: `statusline-sessions`、`statusline-sessions --show-index`、`statusline-category`、category 切替、session cycle、project switch、session-manager。
- hook: `vt hook claude`、`vt hook codex`、`vt hook emit` で agent 状態を `@vde_*` pane option に書く。
- daemon/statusline: `vt daemon` と `vt statusline-agent-badge`。daemon が使えない場合は tmux option snapshot を直接読む。
- sidebar: `vt sidebar open`、`toggle`、`toggle --all`、`close`、`rail`、`rebaseline`、`layout-applied`、`attach`、`input`、`jump`。
- git badge: sidebar の repo 行に branch と ahead/behind を表示する。upstream が無い場合は branch のみを表示する。

## Config

設定ファイルは `~/.config/vde/tmux/config.yml`。
旧 vtm/sidebar config からの自動移行はしない。
JSON Schema は `vt config schema` で出力できる。

```yaml
ghq_root: ~/repos
categories:
  default_category: misc
statusline:
  agent_badge:
    enabled: true
sidebar:
  width: 40
daemon:
  poll_ms: 1000
```

## State / Socket

sidebar state は `$XDG_STATE_HOME/vde/tmux/state.json`、未設定なら
`~/.local/state/vde/tmux/state.json` を使う。
daemon socket は `VDE_DAEMON_SOCKET` 明示指定を優先し、次に
`$XDG_RUNTIME_DIR/vde-tmux/daemon.sock`、最後に `/tmp/vde-tmux-<uid>/daemon.sock` を使う。
socket directory は 0700 の通常ディレクトリであることを検証する。

## Option Bus

新実装は `@vde_*` の個別 pane/window/session option を使う。
旧 `@pane_*` は書かない。

- pane: `@vde_agent`、`@vde_status`、`@vde_prompt`、`@vde_wait_reason`、`@vde_attention`、`@vde_sidebar`
- window: `@vde_layout_baseline`、`@vde_layout_panes`
- session: `@vde_category`、`@vde_category_override`、`@vde_project_path`

## Docs

- [E2E smoke](docs/e2e-smoke.md)
- [Migration](docs/migration.md)
- [vde-monitor compatibility](docs/vde-monitor-compat.md)
