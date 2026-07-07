# E2E Smoke 手順

この手順は本番 tmux server に触れない。
すべて `tmux -L <scratch>` の隔離 server と `target/debug/vt` で実行する。
`cargo install`、dotfiles 更新、mise 更新は M7 の承認後まで実行しない。

## 共通準備

```bash
name="vde-smoke-$(date +%s)"
tmux -L "$name" -f /dev/null new-session -d -s main -c /tmp
window="$(tmux -L "$name" list-windows -F '#{window_id}' | head -n 1)"
pane="$(tmux -L "$name" list-panes -t "$window" -F '#{pane_id}' | head -n 1)"
trap 'tmux -L "$name" kill-server >/dev/null 2>&1 || true' EXIT
```

`show-hooks -g` で user config 由来の hook が入っていないことを確認する。

```bash
tmux -L "$name" show-hooks -g | rg '^after-new-window\[' || true
```

## Hook / Statusline / Daemon

```bash
VDE_TMUX_SOCKET_NAME="$name" TMUX_PANE="$pane" \
  ./target/debug/vt hook emit --agent codex --status running --prompt smoke

VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt statusline-summary
VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt statusline-sessions --show-index
```

期待値: summary は `#[fg=green]●1#[default]`、sessions は `1 main` を含む。session badge がある場合、バッジとラベルは suffix なしで `○1 main` のように連結される。

daemon 経由も確認する。

```bash
sock_dir="/private/tmp/${name}-daemon"
mkdir -p "$sock_dir"
chmod 700 "$sock_dir"
sock="$sock_dir/daemon.sock"
VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt daemon --socket "$sock" &

for _ in $(seq 1 50); do
  [ -S "$sock" ] && break
  sleep 0.1
done

VDE_TMUX_SOCKET_NAME="$name" VDE_DAEMON_SOCKET="$sock" \
  ./target/debug/vt statusline-summary

VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt daemon stop --socket "$sock"
rm -rf "$sock_dir"
```

期待値: `#[fg=green]●1#[default]`。

### Plan 19 statusline smoke (2026-07-05)

scratch tmux server `vde-plan19-smoke-1783256207` で確認。
daemon は default config で起動後、`hide_idle: true` を設定して再起動した。

- idle agent の session option: `@vde_session_status=○`
- default `statusline-sessions`: `#[bold] ○main #[default]`
- `hide_idle: true` 後の session option: 空
- pill 設定(`current.prefix/suffix` + `bg=24`)の `statusline-sessions`: `[#[bg=24] main #[default]]`

### Plan 20 statusline summary smoke (2026-07-05)

scratch tmux server `vde-plan20-smoke-1783256800` で確認。
daemon 起動後に summary を確認し、daemon 停止後に fallback も確認した。

- daemon 経由 `statusline-summary`: `#[fg=green]●1#[default]`
- fallback `statusline-summary`: `#[fg=green]●1#[default]`

### Plan 21 statusline inline/count smoke (2026-07-05)

scratch tmux server `vde-plan21-smoke-1783257469` で確認。
daemon 起動後に `@vde_session_state`、inline 表示、daemon stop 後の clear、config 切替後の plain / `{count}` を確認した。

- `@vde_session_status=●`
- `@vde_session_state=working`
- inline `statusline-sessions`: ` #[fg=green]●#[fg=default]main #[bold] sub #[default]`
- daemon stop 後: `@vde_session_status` / `@vde_session_state` とも空
- `badge_style: plain` の `statusline-sessions`: ` ●main #[bold] sub #[default]`
- `format: "{category} {count} "` の `statusline-category`: `work 2 `

### Plan 22 statusline attention/heartbeat smoke (2026-07-05)

scratch tmux server `vde-plan22-smoke-1783258255` で確認。
daemon 起動後の attention、`kill -9` 後の stale、daemon 再起動後の復帰、graceful stop 後の heartbeat clear を確認した。

- heartbeat: `1783258255`
- `statusline-attention`: `#[fg=red]▲ proxy · perm 2m#[default]`
- fresh `statusline-sessions`: ` #[fg=green]●#[fg=default]main #[bold] #[fg=red]▲#[fg=default]proxy #[default]`
- daemon 強制終了後 stale: ` ?main #[bold] ?proxy #[default]`
- daemon 再起動後: ` #[fg=green]●#[fg=default]main #[bold] #[fg=red]▲#[fg=default]proxy #[default]`
- graceful stop 後 heartbeat: 空
- graceful stop 後 `statusline-sessions`: ` main #[bold] proxy #[default]`

追実施 scratch tmux server `vde-plan22-followup-1783259654` で、permission 承認後の attention clear を確認した。

- checked=permission appears before approval
- result=`#[fg=red]▲ proxy · perm 2m#[default]`
- checked=running emit clears attention after debounce
- result=`<empty>`

可視 session 除外は scratch tmux server `vde-plan22-visible-1783259681` で control-mode client を使って再現を試みたが、headless 環境では `session_attached=0` のままで成立しなかった。
過剰主張を避けるため smoke では未実施として記録し、可視除外は unit test(`attention_names_oldest_hidden_blocked_session` / `attention_is_empty_without_hidden_blocked`)で担保する。

- checked=visible session attached/window_active probe
- result=`main:0:1`
- checked=visible blocked attention output
- result=`#[fg=red]▲ main · perm 2m#[default]`

## Session / Category / Project

```bash
VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt sessions refresh-category
VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt session set-category main work
VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt statusline-category
VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt statusline-sessions
VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt project switch /tmp
```

期待値:

- `statusline-category` が `work` を含む。
- `statusline-sessions` が `main` を含む。
- `project switch /tmp` 後に `tmp` session が作成される。

## Sidebar

```bash
VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt sidebar open \
  --window "$window" --width 30 --delay-ms 0

tmux -L "$name" list-panes -t "$window" \
  -F '#{pane_id} #{pane_width} #{@vde_sidebar}'
```

期待値: `@vde_sidebar` が `1` の pane が 1 つある。

```bash
sidebar_pane="$(tmux -L "$name" list-panes -t "$window" \
  -F '#{pane_id} #{@vde_sidebar}' | awk '$2 == "1" { print $1; exit }')"

VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt sidebar rail \
  --window "$window" --width 30
tmux -L "$name" list-panes -t "$sidebar_pane" -F '#{pane_width}'

VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt sidebar rebaseline --window "$window"
tmux -L "$name" show-options -w -t "$window" -qv '@vde_layout_panes'

VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt sidebar layout-applied \
  --window "$window" --width 30

VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt sidebar close --window "$window"
tmux -L "$name" list-panes -t "$window" -F '#{pane_id} #{@vde_sidebar}'
```

期待値:

- `rail` 後の sidebar pane 幅が `2`。
- `rebaseline` 後の `@vde_layout_panes` に sidebar pane ID が含まれない。
- `close` 後に `@vde_sidebar = 1` の pane が残らない。

toggle / schema も確認する。

```bash
VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt sidebar toggle \
  --window "$window" --width 30

VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt sidebar toggle \
  --window "$window" --width 30

VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt sidebar toggle \
  --all --width 30

VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt sidebar toggle \
  --all --width 30

./target/debug/vt config schema | grep 'https://json-schema.org/draft/2020-12/schema'
```

期待値:

- window 指定の `toggle` と `toggle --all` が sidebar pane を開閉する。
- `config schema` が JSON Schema draft 2020-12 を出力する。

`vt sidebar input <key>` と `vt sidebar jump <pane>` は M6 runtime daemon への
client event として送る。
次の script で subscribe/input/jump/query/detect/session badge をまとめて確認する。

```bash
scripts/smoke-m6-runtime.sh
```

期待値:

```text
subscribe snapshot ok
capture detect ok
session badge blocked ok
session badge done ok
statusline badge render ok
input redraw state ok
query response ok
session badge cleanup ok
M6 runtime smoke ok
```

## Sidebar UI parity

本番 tmux server は使わず、scratch server と隔離 daemon だけで確認する。
`VDE_DAEMON_SOCKET`、`XDG_STATE_HOME`、`XDG_CONFIG_HOME` は scratch directory に向ける。

確認項目:

- `vt hook emit` で running / waiting+permission / idle / attention の pane を作る。
- `vt sidebar open` で daemon subscribe TUI を開き、header の現在値表示(` repo · all`)を確認する。
- `v` で `category` に切り替わることを確認する。
- `Tab` で `attention` filter に切り替わり、attention 不要な idle pane が消えることを確認する。
- Chat 行で `Space` を押し、chat 行が `${agent}: ${state} · ${time}` になり、prompt と場所行(`session · %pane`)の Detail 行が出ることを確認する。
- `p` または Detail 行クリックで preview が floating pane として開くことを確認する。

### Sidebar header rich smoke

Plan 26 の header 表示は `docs/sidebar-header-proposals.html` の状態 1〜3 と照合する。
TUI の alt-screen capture が安定しない場合は目視確認を優先し、filter/count 遷移は daemon snapshot と unit test で補助確認する。

確認手順:

- `vt hook emit` で blocked 1 / working 1 / idle 5 相当の agent pane を作り、sidebar を開く。
- 状態 1: `all` filter で 1 行目が `≣ category  7 tasks `、2 行目が `≡ all 7  ▲ 1  ● 1  ✓ 0  ○ 5` 相当であることを確認する。
- `▲ 1` chip をクリックし、状態 2: attn filter に切り替わり、他状態の件数が filter 前のまま残ることを確認する。
- `✓ 0` chip をクリックしても filter が変わらないことを確認する。
- `Tab` を押し、0件の done をスキップして次の非0 filter に進むことを確認する。
- attn filter 適用中に blocked pane を working/idle へ遷移させ、状態 3: header が残り、active chip が `▲ 0` の反転表示を維持し、rows 領域に `no attn agents` と `tab: next filter · click ≡ all to reset` が出ることを確認する。
- 空状態で `≡ all` chip をクリックし、all filter に戻って rows が再表示されることを確認する。
- `sidebar.header.suffix: ""` の scratch config で再起動し、1 行目から powerline 矢印が消えることを確認する。

実施メモ(2026-07-07):

- scratch tmux + isolated daemon/config/state で確認。header/rows の capture を安定させるため `sidebar.live.enabled: false` を scratch config に設定し、`sidebar attach` pane へ `VDE_TMUX_SOCKET_NAME` / `VDE_DAEMON_SOCKET` / `XDG_STATE_HOME` / `XDG_CONFIG_HOME` を明示して起動した。
- 初期状態: counts `total=7, blocked=1, working=1, done=0, idle=5`、1 行目に `≣ category` / `7 tasks` / ``、2 行目に `≡ all 7  ▲ 1  ● 1  ✓ 0  ○ 5` を確認。
- attn filter、0 件 done no-op、`tab` の done skip、attn 空状態(`▲ 0` + `no attn agents` + 復帰ヒント)、all 復帰、`suffix: ""` の矢印なしを確認。

## Session Manager popup

本番 tmux server は使わず、scratch server だけを使う。
`vt session-manager --popup` は `display-popup` 固定。

```bash
rtk cargo build
name="vde-session-manager-popup-$(date +%s)"
tmux -L "$name" -f /dev/null new-session -d -s main -n work -c /tmp
trap 'tmux -L "$name" kill-server >/dev/null 2>&1 || true' EXIT

VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt session-manager --popup
```

期待値: `display-popup -E -w 80% -h 70%` 経路で session picker が開く。

## 2026-07-04 実行記録

M5 sidebar smoke は pass。

```text
scratch=vde-m5-1783130526
window=@0
main_pane=%0
sidebar_pane=%1
open_width=30
rail_width=2
baseline_panes=%0
remaining_panes=1
EXIT=0
```

M6 追加 smoke も pass。

```text
scratch=vde-m6-1783132904
window=@0
main_pane=%0
sessions= 1 main
sidebar_pane=%1
open_width=30
all_sidebar_count=1
remaining_sidebar_count=0
selection=repo::misc::tmp
schema_ok=https://json-schema.org/draft/2020-12/schema
```

M6 runtime smoke も pass。

```text
scratch=vde-m6-runtime-<timestamp>
socket=/tmp/vde-m6-runtime-<timestamp>/daemon.sock
subscribe snapshot ok
capture detect ok
input redraw state ok
query response ok
M6 runtime smoke ok
```

M6 runtime smoke cleanup 修正後の再実行も pass。

```text
executed_at=2026-07-04 17:40:50 JST
script=scripts/smoke-m6-runtime.sh
subscribe snapshot ok
capture detect ok
input redraw state ok
query response ok
M6 runtime smoke ok
scratch tmux socket residual=0
```

M6 runtime smoke session badge 追加後の再実行も pass。

```text
executed_at=2026-07-04 21:42:06 JST
script=scripts/smoke-m6-runtime.sh
subscribe snapshot ok
capture detect ok
session badge blocked ok
session badge done ok
statusline badge render ok
input redraw state ok
query response ok
session badge cleanup ok
M6 runtime smoke ok
scratch tmux socket residual=0
/tmp runtime residual=0
```

Plan 10 で smoke 用 `XDG_CONFIG_HOME` を隔離した後の再実行も pass。

```text
executed_at=2026-07-04 22:08:22 JST
script=scripts/smoke-m6-runtime.sh
subscribe snapshot ok
capture detect ok
session badge blocked ok
session badge done ok
statusline badge render ok
input redraw state ok
query response ok
session badge cleanup ok
M6 runtime smoke ok
scratch tmux socket residual=0
/tmp runtime residual=0
```

Sidebar UI parity smoke も pass。

```text
executed_at=2026-07-04 22:51:05 JST
scratch=vde-ui-parity-1783173039
window=@0
sidebar=%4
checked=header repo/all, view cycle category, attention filter, Chat Detail status/session, preview floating pane path
result=sidebar ui parity smoke ok
```

Session Manager popup は `display-popup` 固定に戻した。

```text
executed_at=2026-07-04 23:36:45 JST
checked=unit test popup_uses_display_popup_directly / popup_does_not_probe_tmux_version
result=session manager popup fixed to display-popup
```

Sidebar preview floating pane の中央配置と Codex alt-screen capture も pass。

```text
executed_at=2026-07-04 23:36:45 JST
position=left=10 top=4 width=80 height=32 floating=1 on 100x40 scratch window
capture=alt-screen text codex-alt-screen captured via capture-pane -a fallback
result=sidebar preview centered floating pane and codex capture ok
```

Sidebar header の statusline category 風 default / pill 設定と preview repeat も pass。

```text
executed_at=2026-07-04 23:54:32 JST
header_default=repo/all rendered as statusline-like fixed segments without bg
header_config=pill style via sidebar.header prefix/suffix/format/separator/bold/colors
preview_repeat=3 runs all left=10 top=4 width=80 height=32 floating=1 on 100x40 scratch window
result=sidebar header style and preview stable center ok
```

Plan 12 後の M6 runtime smoke 再実行も pass。

```text
executed_at=2026-07-05 01:11:13 JST
script=scripts/smoke-m6-runtime.sh
subscribe snapshot ok
capture detect ok
session badge blocked ok
session badge done ok
statusline badge render ok
input redraw state ok
query response ok
session badge cleanup ok
M6 runtime smoke ok
```

Plan 12 固有の scratch runtime smoke も pass。

```text
executed_at=2026-07-05 01:11:13 JST
scratch=vde-m12-runtime-<timestamp>
checked=stale agent exclusion with zsh pane retaining @vde_agent
checked=badge glyphs ▲/●/✓/○ and unread -> viewed transition via control-mode attach
checked=path_patterns with ${WORK_OWNER} expansion resolves category work
checked=preview floating pane width matches target pane, centered by pane_left, q/Esc close
checked=preview source capture includes scrollback via capture-pane -S -2000
checked=double-click dispatch covered by sidebar::tui pseudo-time tests in quality gate
result=Plan 12 scratch smoke ok
```

Plan 13 sidebar UI Phase 1 smoke も pass。
daemon は scratch socket で新 binary を起動し直して確認した。

```text
executed_at=2026-07-05 18:26:37 JST
scratch=vde-p13-once-<timestamp>
checked=attach --once width40 padding/right-column/no [running:N]
checked=attach --once width2 rail glyph
checked=daemon restarted on scratch socket and snapshot RowMeta attention_count
checked=header/footer height threshold covered by sidebar::tui TestBackend tests in quality gate
result=Plan 13 sidebar smoke ok
```

Plan 14 sidebar UI Phase 2 smoke も pass。
daemon は scratch socket で新 binary を起動し直して確認した。

```text
executed_at=2026-07-05 18:37:04 JST
scratch=vde-p14-<timestamp>
checked=selected chat row shows inline meta: 13m · task 2/5 · sub 2
checked=Space full detail expansion suppresses inline meta row
checked=n/N cycles between two permission blocked chat rows
checked=jump from done(✓) pane immediately clears unread to idle(○)
checked=vt sidebar focus selects current window sidebar pane
result=Plan 14 sidebar smoke ok
```

Plan 15 sidebar UI Phase 3 smoke も pass。
daemon は scratch socket で新 binary を起動し直して確認した。
TUI の `capture-pane -a` は alt-screen が空になるため、daemon subscribe snapshot の rows で表示状態を確認した。

```text
executed_at=2026-07-05 18:56:30 JST
scratch=vde-p15-smoke-direct
checked=permission pane appears as top zone: zone::triage / TRIAGE 1 / chat label codex · app / rollup permission
checked=FLEET repo row keeps meta.attention_count=1 while blocked pane is triaged
checked=attention filter keeps TRIAGE visible
checked=n selects triaged chat and inline meta contains origin misc/app
checked=Enter on selected TRIAGE chat is accepted by daemon input path
checked=after blocked clears, first calm poll keeps TRIAGE and second calm poll removes it back to FLEET
checked=blocked again re-enters TRIAGE without flicker
result=Plan 15 TRIAGE snapshot smoke ok
```

Plan 16 sidebar UI Phase 4 smoke も pass。
daemon は scratch socket で新 binary を起動し直して確認した。
TUI の `capture-pane -a` は alt-screen が空になるため、fisheye/pin/state は daemon subscribe snapshot、幅ティアは `attach --once`、scroll offset は unit test で確認した。

```text
executed_at=2026-07-05 19:18:09 JST
scratch=vde-p16-smoke
checked=selected chat row expands to full detail rows and jump row
checked=Space pins selected chat; after selection moves away, pinned row keeps one meta row and no full detail rows
checked=12 agent panes are present; selection movement path exercised, scroll offset covered by sidebar::tui::tests::scroll_follows_selection
checked=state.json contains pinned and daemon restart keeps pinned medium row
checked=sidebar width 30 renders dense, width 20 renders micro, width 2 renders rail count + separator + glyph rows via attach --once
result=Plan 16 fisheye width-tier snapshot smoke ok
```

Plan 17 sidebar UI Phase 5 smoke も pass。
daemon は scratch socket で新 binary を起動し直して確認した。
TUI の `capture-pane -a` はこれまで同様に alt-screen が空になるため、filter/events/flash/notify/elapsed は daemon subscribe snapshot、LIVE tail の source は scratch pane の `capture-pane -p`、LIVE area/tail は unit test で確認した。

```text
executed_at=2026-07-05 19:45:38 JST
scratch=vde-p17-smoke-1783248338
checked=badge counts include Blocked/Working/Done/Idle and transition events include %0 -> Blocked
checked=Tab cycles filters attention_only -> working_only -> done_only -> idle_only -> all
checked=blocked transition sets row meta.flash, then flash clears after subsequent polls
checked=notify.enabled command ran with VDE_PANE_ID/VDE_AGENT/VDE_BADGE_STATE: %0 codex Blocked
checked=running elapsed_secs advanced via daemon snapshot
checked=LIVE source pane capture contains live-a/live-b; LIVE area/tail behavior covered by sidebar::tui tests
result=Plan 17 snapshot smoke ok
```

Plan 18 sidebar review fixes smoke も pass。
`cargo build` で `target/debug/vt` を最新化し、scratch socket で daemon を新規起動し直して確認した。
TUI の alt-screen capture はこれまで同様に安定しないため、scratch daemon subscribe snapshot と Plan 18 の回帰 unit test で確認を分担した。

```text
executed_at=2026-07-05 20:55:22 JST
scratch=vde-p18-smoke-1783252512
checked=daemon restarted with rebuilt target/debug/vt on scratch socket
checked=permission pane appears in TRIAGE and FLEET repo row keeps meta.attention_count=1
checked=n selects triaged chat and selected TRIAGE row shows origin detail
checked=after approval, first calm/debounce snapshot keeps TRIAGE row and repo attention_count=1
checked=rail double-count regression covered by sidebar::render::tests::rail_does_not_double_count_expanded_chat
checked=event log format covered by sidebar::tui::tests::event_tail_formats_ago_agent_and_glyphs
checked=expanded chat j/k teleport regression covered by daemon::runtime::tests::moving_through_expanded_chat_does_not_teleport_selection
checked=TRIAGE/FLEET selection follow regression covered by daemon::runtime::tests::selection_follows_pane_across_triage_and_fleet
checked=Task 11 live capture worker smoke at 2026-07-05 21:06:42 JST: scratch=vde-p18-live-1783253186, LIVE source pane capture contains live-a/live-b/live-c
checked=Task 11 daemon disconnect after sidebar TUI launch completed without hang; message was not captured because tmux alt-screen capture remains unreliable
checked=Task 11 non-blocking result path covered by sidebar::tui::tests::live_capture_result_updates_only_current_pane
result=Plan 18 smoke ok
```

Plan 23 sidebar polish smoke も pass。
`cargo install --path . --force` で `vt` / `vde-tmux` を反映し、TUI の alt-screen capture は既存計画同様に安定しないため、表示・クリック仕様は対応する render/tui/runtime/tree の回帰テストで確認した。

```text
executed_at=2026-07-06 13:59:35 JST
scratch=plan23-installed-binary
checked=rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test passed
checked=rtk cargo install --path . --force replaced vt and vde-tmux
checked=pin marker ✦ and colors.pin covered by sidebar::render::tests::pinned_chat_row_shows_pin_glyph / pin_color_is_configurable
checked=header ≣ mode, spaced badges (≡ 0 etc.), category ◆ peach, repo blue covered by sidebar::render header/color tests
checked=ByCategory category rule filler covered by sidebar::render::tests::category_row_fills_remaining_width_with_rule
checked=LIVE rounded card wide/narrow and compute_areas chrome rows covered by sidebar::tui live_card tests
checked=jump/preview action row hit-test, immediate click dispatch, detail toggle covered by sidebar::render::tests::jump_row_hit_test_maps_columns_to_actions, sidebar::tui::tests::detail_click_toggles_row_immediately, daemon::runtime::tests::toggle_on_detail_row_toggles_manual_expand_of_parent_chat
checked=active lineage derivation and left bar/chat bg covered by sidebar::tree::tests::active_pane_marks_chat_row_and_ancestors and sidebar::render::tests::active_rows_render_left_bar_and_chat_bg
result=Plan 23 sidebar polish smoke ok
```

Plan 24 sidebar detail view smoke も pass。
`rtk cargo install --path . --force` で `vt` / `vde-tmux` を反映した。
TUI の alt-screen capture は既存計画同様に安定しないため、展開ビューの表示仕様は tree/render/runtime の回帰テストと品質ゲートで確認した。

```text
executed_at=2026-07-06 16:20:24 JST
scratch=plan24-installed-binary
checked=rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test passed
checked=rtk cargo install --path . --force replaced vt and vde-tmux
checked=branch default Indexed 73 and colors.branch override path covered by sidebar::render::tests::branch_defaults_to_muted_cyan / repo_branch_is_rendered_in_branch_color
checked=expanded chat row state/time label and right-label suppression covered by sidebar::tree::tests::expanded_chat_row_shows_agent_state_and_time and sidebar::render::tests::expanded_chat_row_suppresses_right_label
checked=expanded chat row carries state/time, detail state row is omitted, place row remains, state idle done {t} ago, blocked wait_reason, and completed_at missing omission covered by sidebar::tree detail/state_line tests
checked=expanded chat row colors state/context in label, covered by sidebar::render::tests::expanded_chat_row_colors_status_and_context_in_label
checked=Enter on selected detail row still previews parent pane via daemon::runtime::tests::enter_on_detail_returns_preview_effect
result=Plan 24 sidebar detail view smoke ok
```
