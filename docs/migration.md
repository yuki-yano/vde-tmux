# 移行メモ

M7 の一括切替で使う作業メモ。
この文書は手順の準備であり、dotfiles、mise、本番 tmux、実 config はまだ変更しない。

## コマンド対応表

| 旧コマンド | 新コマンド |
|---|---|
| `vtm statusline-category` | `vt statusline-category` |
| `vtm statusline-agent-badge` | `vt statusline-summary` |
| `vtm statusline-sessions` | `vt statusline-sessions` |
| `vtm statusline-sessions --show-index` | `vt statusline-sessions --show-index` |
| `vtm statusline-sessions switch <n>` | `vt statusline-sessions switch <n>` |
| `vtm category next` / `prev` / `use <name>` | `vt category next` / `prev` / `use <name>` |
| `vtm session-cycle next` / `prev` | `vt session-cycle next` / `prev` |
| `vtm session set-category <session> <category>` | `vt session set-category <session> <category>` |
| `vtm sessions refresh-category` | `vt sessions refresh-category` |
| `vtm project switch <path>` | `vt project switch <path>` |
| `vtm session-manager --popup` | `vt session-manager --popup` |
| `vde-tmux-sidebar open` | `vt sidebar open` |
| `vde-tmux-sidebar toggle` | `vt sidebar toggle` |
| `vde-tmux-sidebar toggle --all` | `vt sidebar toggle --all` |
| `vde-tmux-sidebar close` | `vt sidebar close` |
| `vde-tmux-sidebar rail` | `vt sidebar rail` |
| `vde-tmux-sidebar rebaseline` | `vt sidebar rebaseline` |
| `vde-tmux-sidebar layout-applied` | `vt sidebar layout-applied` |
| `vde-tmux-sidebar attach` | `vt sidebar attach` |
| `vde-tmux-sidebar hook claude <event>` | `vt hook claude <event>` |
| `vde-tmux-sidebar hook codex [arg]` | `vt hook codex [arg]` |

## Config

新 config は `~/.config/vde/tmux/config.yml`。
旧 vtm config と旧 sidebar config は自動移行しない。
JSON Schema は `vt config schema` で確認する。
主な対応は次のとおり。

```yaml
categories:
  display_names:
    work: W
  order:
    work: 10
  default_category: misc
  rules:
    - category: work
      path_patterns:
        - github.com/acme/*
  session_name_rules:
    - category: misc
      patterns:
        - scratch-*
statusline:
  sessions:
    show_index: true
    badge_style: inline
  category:
    format: "{category} {count} "
  summary:
    enabled: true
  session_badge:
    enabled: true
    # 既定は空文字。旧表示のようにバッジとラベルの間に空白を残す場合だけ指定する。
    suffix: " "
    hide_idle: false
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
  git:
    timeout_ms: 500
    poll_interval_ms: 10000
```

`ghq_root` は新実装では使わないため削除する。
`~/.config/vde/tmux/config.yml` に残っている場合は、M7 切替前に消す。
`categories.rules[].ghq_patterns` は `path_patterns` へリネームする。
`statusline.session_badge.glyphs` は `badge.glyphs` へ移動する。
`statusline.session_badge.suffix` の既定は空文字になる。
旧表示のようにバッジとラベルの間の空白を維持したい場合は、`statusline.session_badge.suffix: " "` を明示する。
`statusline.session_badge.hide_idle: true` を指定すると idle(○)バッジを非表示にできる。
`statusline.agent_badge` は `statusline.summary` へ置き換える。
tmux.conf の `#(vtm statusline-agent-badge)` / `#(vt statusline-agent-badge)` 相当は `#(vt statusline-summary)` へ書き換える。
`statusline.sessions.badge_style` は `inline`(既定)または `plain` を指定できる。
`statusline.category.format` では `{count}` を使える。
`sidebar.colors.attention` / `selection_active_bg` は未使用のため削除する。

## Pane Option Bus

新実装は `@vde_*` 名前空間だけを使う。
旧 `@pane_*` は書かない。

| 用途 | 新キー |
|---|---|
| agent 名 | `@vde_agent` |
| 状態 | `@vde_status` |
| prompt preview | `@vde_prompt` |
| wait reason | `@vde_wait_reason` |
| attention | `@vde_attention` |
| sidebar marker | `@vde_sidebar` |
| layout baseline | `@vde_layout_baseline` / `@vde_layout_panes` |
| session category | `@vde_category` / `@vde_category_override` |
| project path | `@vde_project_path` |

## M7 で承認が必要な作業

- `cargo install`。
- dotfiles の `.tmux.conf`、hook 設定、ghq project selector の更新。
- `tmux source-file ~/.tmux.conf`。
- mise 登録/旧エントリ削除。
- GitHub push、crates.io publish、旧 repo archive。

M7 では変更前に grep 棚卸しを実行する。

```bash
rg --hidden -n '\bvtm\b|vde-tmux-sidebar' ~/dotfiles ~/.claude/settings.json ~/.codex/hooks.json ~/.config/vde/
```
