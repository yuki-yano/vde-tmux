# vde-tmux

tmux 上の state と UI の管理ツール(セッション/カテゴリ管理、statusline、エージェントサイドバー)。

[vde-tmux-manager](https://github.com/yuki-yano/vde-tmux-manager) と
[vde-tmux-sidebar](https://github.com/yuki-yano/vde-tmux-sidebar) を参考実装として
スクラッチで書き直し中。バイナリは `vt`(常用)と `vde-tmux`(正式名)の 2 つで、同一 CLI を提供する。

設計・進捗はローカルの設計書(`2026-07-04-vde-tmux-rewrite-design.md`)を参照。

## 機能

- session/category: `statusline-sessions`、`statusline-sessions --show-index`、`statusline-category`、category 切替、session cycle、project switch、session-manager。
- hook: `vt hook claude`、`vt hook codex`、`vt hook emit` で agent 状態を `@vde_*` pane option に書く。
- daemon/statusline: `vt daemon`、`vt daemon stop`、`vt statusline-summary`、`vt statusline-attention`。daemon が使えない場合は tmux option snapshot を直接読む。
- session badge: daemon が session ごとの agent 状態を 4 状態(▲ Blocked、● Working、✓ Done 未読、○ Idle 既読)に集約し、`statusline-sessions` の session ラベルへ前置する。
- sidebar: `vt sidebar open`、`toggle`、`toggle --all`、`close`、`rail`、`rebaseline`、`layout-applied`、`attach`、`input`、`jump`、`focus`。
- git badge: sidebar の repo 行に branch と ahead/behind を表示する。upstream が無い場合は branch のみを表示する。

sidebar の配色は 5 族の規約で運用する。状態族は `badge.colors` 由来の ▲ blocked・● working・✓ done・○ idle、構造族は repo 青太字・category ピーチ太字・branch 淡シアン(Indexed 73)、操作族は toggle/mode/active/preview のラベンダー系、本文族は通常本文・detail(246)・marker 暗灰、実況族は LIVE/EVENTS 見出し専用のマゼンタ。branch の既定色は ✓ done の明シアンとの衝突を避けるため、従来の Cyan から Indexed 73 に変更した。`sidebar.colors.branch` を設定すれば従来色にも戻せる。

## Config

設定ファイルは `~/.config/vde/tmux/config.yml`。
旧 vtm/sidebar config からの自動移行はしない。
JSON Schema は `schemas/config.schema.json` に配置する。
YAML LSP では config の先頭に `# yaml-language-server: $schema=/path/to/vde-tmux/schemas/config.schema.json` を置く。
同じ schema は `vt config schema` でも出力できる。

```yaml
categories:
  default_category: misc
  rules:
    - category: work
      path_patterns:
        - github.com/acme/*
statusline:
  category:
    format: "{category} {count} "
  sessions:
    badge_style: inline
  summary:
    enabled: true
  session_badge:
    enabled: true
    suffix: ""
    hide_idle: false
badge:
  glyphs:
    blocked: "▲"
    working: "●"
    done: "✓"
    idle: "○"
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
    format: " {label} "
    prefix: ""
    suffix: ""
    bold: true
    colors:
      fg: "16"
      bg: "147"
      outer_bg: "235"
  colors:
    selection_bg: "237"
    toggle: "147"
    category: "215"
    header_mode: "147"
    # active filter chip の文字色。未指定なら header.colors.fg と同じ解決順
    header_chip_fg: ""
    # "N tasks" チップの背景色。未指定なら active_bg
    header_total_bg: ""
    # "N tasks" チップの "tasks" ラベル文字色。未指定なら detail
    header_total_fg: ""
    active_bg: "235"
    active_bar: "147"
daemon:
  poll_ms: 1000
notify:
  enabled: false
  # blocked 遷移時だけ実行する。環境変数 VDE_PANE_ID / VDE_AGENT / VDE_BADGE_STATE を渡す。
  command: "terminal-notifier -title vde-tmux -message \"$VDE_AGENT $VDE_BADGE_STATE\""
```

## Statusline

tmux 側は `status-interval 1` を推奨する。
実際の反映遅延は、おおむね `daemon.poll_ms + status-interval` になる。

```tmux
set -g status-interval 1
set -g status-left '#(vt statusline-category)#(vt statusline-sessions --show-index)'
set -g status-right '#(vt statusline-attention) #(vt statusline-summary)'
```

`statusline-summary` は状態別カウントを `▲2 ●1` 形式で表示する。
`statusline-attention` は見えていない session の blocked agent を `▲ session · perm 2m` 形式で表示する。
daemon heartbeat が stale になると、`statusline-sessions` の既存バッジは `?` に置き換わる。

### D3改配色（推奨プリセット）

設計根拠と全案比較は `docs/statusline-color-proposals.html` を参照。
状態グリフは常にバー地の上に置き、塗りはカレント要素の名前だけに使う。

```yaml
# ~/.config/vde/tmux/config.yml
statusline:
  category:
    mode: list
    format: "{category} {name} "
    inactive_format: "{category} "
    colors:
      fg: "#ecebff"
      bg: "#453f9e"
    inactive_colors:
      fg: "#9591ad"

  sessions:
    badge_style: outer
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

# badge.colors は既定で D3改の hex（変更する場合のみ記述）
# badge:
#   colors:
#     blocked: "#ff6b6b"
#     working: "#4fd08a"
#     done: "#45cbe6"
#     idle: "#a8a8b2"
```

```tmux
# ~/.tmux.conf
set -ga terminal-overrides ',*:Tc'
set -g status-style 'bg=#1a1926,fg=#9591ad'
set -g status-left-length 60
set -g window-status-format '#[fg=#9591ad] #I:#W '
set -g window-status-current-format '#[fg=#ecebff,bg=#453f9e] #I:#W '
set -g window-status-bell-style 'fg=#ff6b6b'
set -g window-status-activity-style 'fg=#ff6b6b'
```

塗りは矩形で使う。
breadcrumb 等でバーの下に別の面を重ねている場合、その地色を `#121218` 目安まで一段暗くしないとバー地 `#1a1926` と同化する。
hex 色をそのまま使うには tmux の truecolor 設定が必要になる。

## Sidebar Header

sidebar header は 2 行構成で表示する。
1 行目は `≣ repo      7 tasks ` のような mode badge と総数セグメント、2 行目は `≡ all 7  ▲ 1  ● 1  ✓ 0  ○ 5` の filter chip 列。

chip は状態に応じて表示を分ける。
アクティブ chip は状態色 bg + 暗色 fg + bold で反転し、適用中に 0 件になっても `▲ 0` の反転表示を維持する。
非アクティブかつ非 0 件の chip は `active_bg` bg + 状態色 fg、0 件 chip は marker 色の dim 表示でクリック対象外。
`all` は常に適用でき、`tab` filter cycle は 0 件状態をスキップする。

件数は filter 適用前の全 agent pane から算出するため、filter 中も他状態の件数は変わらない。
filter 適用中に rows が空になっても header は残り、rows 領域に `no attn agents` と `tab: next filter · click ≡ all to reset` を表示する。
`all` filter で本当に 0 件のときは `no agents` のみ表示する。

`sidebar.header` は statusline の segment と同じ考え方で、`format` / `prefix` / `suffix` / `bold` / `colors` を設定できる。
`format` の `{label}` は `≣ repo` などの固定幅 mode label に置換される。
既定の `suffix: ""` は mode badge と総数セグメントを powerline 表示にし、`suffix: ""` にすると矢印なしの矩形塗りになる。
`colors.outer_bg` は suffix の遷移先背景色として使う。
`sidebar.header.powerline` と `separator` は受け付けない。

## Sidebar Detail View

sidebar の Standard 幅で chat 行を展開すると、左側は `${agent}`、右端は `${state} ${time}` を表示する。prompt は展開内の先頭 detail 行に集約する。

展開メタは `prompt 行 + 場所行` を表示し、state 情報は親 chat 行の右端に集約する。右端の state と時間は状態色で表示する。場所行は `vde-tmux · %51` の形式で session と pane id を detail 色で表示する。

時間表記は `45s` / `12m` / `1h30m` / `38h` / `2d` のように humanize する。running / blocked は `started_at` からの経過、idle / done は `completed_at` からの `done {t} ago` を表示し、`completed_at` が無い idle は時間部を省略する。

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

- Agent pane の生存判定は hooks が書いた `@vde_agent` を優先し、未設定なら `pane_current_command` が `claude`、`codex`、`opencode` のいずれかであることを見る。hook が動いていない場合でも command から agent を補完し、capture-pane で permission 画面を検出できる範囲では blocked として表示する。
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
