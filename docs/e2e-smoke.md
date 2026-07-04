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

VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt statusline-agent-badge
VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt statusline-sessions --show-index
```

期待値: agent badge は `running:1`、sessions は `1:main` を含む。

daemon 経由も確認する。

```bash
sock="/tmp/${name}.sock"
VDE_TMUX_SOCKET_NAME="$name" ./target/debug/vt daemon --socket "$sock" &
daemon_pid=$!

for _ in $(seq 1 50); do
  [ -S "$sock" ] && break
  sleep 0.1
done

VDE_TMUX_SOCKET_NAME="$name" VDE_DAEMON_SOCKET="$sock" \
  ./target/debug/vt statusline-agent-badge

kill "$daemon_pid"
rm -f "$sock"
```

期待値: `running:1`。

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
次の script で subscribe/input/jump/query/detect をまとめて確認する。

```bash
scripts/smoke-m6-runtime.sh
```

期待値:

```text
subscribe snapshot ok
capture detect ok
input redraw state ok
query response ok
M6 runtime smoke ok
```

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
sessions= 1:main
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
