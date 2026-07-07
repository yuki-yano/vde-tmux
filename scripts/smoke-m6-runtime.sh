#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/vt"
STAMP="$(date +%s)"
TMUX_SOCKET="vde-m6-runtime-$STAMP"
RUNTIME_DIR="/tmp/vde-m6-runtime-$STAMP"
DAEMON_SOCKET="$RUNTIME_DIR/daemon.sock"
STATE_HOME="$RUNTIME_DIR/state"
CONFIG_HOME="$RUNTIME_DIR/config"
LOG="$RUNTIME_DIR/daemon.log"
AGENT_BIN="$RUNTIME_DIR/codex"

cleanup() {
  set +e
  if [[ -n "${DAEMON_PID:-}" ]]; then
    kill "$DAEMON_PID" 2>/dev/null
    wait "$DAEMON_PID" 2>/dev/null
  fi
  tmux -L "$TMUX_SOCKET" kill-server 2>/dev/null
  rm -f "/private/tmp/tmux-501/$TMUX_SOCKET"
  rm -rf "$RUNTIME_DIR"
}
trap cleanup EXIT

mkdir -p "$RUNTIME_DIR" "$STATE_HOME" "$CONFIG_HOME"
chmod 700 "$RUNTIME_DIR"

cargo build

cat >"$RUNTIME_DIR/codex.c" <<'C'
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
  char line[256];
  signal(SIGINT, SIG_IGN);
  printf("? Allow command to run?\n  y) yes\n  n) no\n");
  fflush(stdout);
  while (fgets(line, sizeof(line), stdin) != NULL) {
    if (strncmp(line, "clear", 5) == 0) {
      printf("\033[2J\033[Hidle\n");
      fflush(stdout);
    }
  }
  for (;;) {
    pause();
  }
}
C
cc -o "$AGENT_BIN" "$RUNTIME_DIR/codex.c"

tmux -L "$TMUX_SOCKET" -f /dev/null new-session -d -s main -n work -c "$ROOT" "$AGENT_BIN"
PANE_ID="$(tmux -L "$TMUX_SOCKET" list-panes -a -F '#{pane_id}' | head -n 1)"

tmux -L "$TMUX_SOCKET" set-option -p -t "$PANE_ID" @vde_agent codex

VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" \
VDE_DAEMON_SOCKET="$DAEMON_SOCKET" \
XDG_STATE_HOME="$STATE_HOME" \
XDG_CONFIG_HOME="$CONFIG_HOME" \
"$BIN" daemon --socket "$DAEMON_SOCKET" >"$LOG" 2>&1 &
DAEMON_PID="$!"

for _ in $(seq 1 50); do
  [[ -S "$DAEMON_SOCKET" ]] && break
  sleep 0.1
done
[[ -S "$DAEMON_SOCKET" ]]

python3 - "$DAEMON_SOCKET" <<'PY'
import json
import socket
import sys
import time

path = sys.argv[1]
client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
client.connect(path)
client.sendall(json.dumps({"op": "subscribe", "proto": 1}).encode() + b"\n")
client.settimeout(5)
reader = client.makefile("rb")
deadline = time.time() + 5
last = None
seen_snapshot = False
seen_permission = False
while time.time() < deadline:
    line = reader.readline()
    assert line, "no subscribe snapshot"
    msg = json.loads(line)
    assert msg["type"] == "snapshot", msg
    snapshot = msg["snapshot"]
    last = snapshot
    if not seen_snapshot and snapshot["agent_count"] >= 1 and snapshot.get("sidebar", {}).get("rows"):
        print("subscribe snapshot ok")
        seen_snapshot = True
    panes = snapshot.get("panes", [])
    if not seen_permission and any(pane.get("wait_reason") == "permission_prompt" for pane in panes):
        print("capture detect ok")
        seen_permission = True
    if seen_snapshot and seen_permission:
        break
else:
    raise AssertionError(last)
PY

# --- session badge ---
badge_wait() {
  local expect="$1"
  local got=""
  for _ in $(seq 1 50); do
    got="$(tmux -L "$TMUX_SOCKET" show-options -v -t main @vde_session_status 2>/dev/null || true)"
    [[ "$got" == "$expect" ]] && return 0
    sleep 0.1
  done
  echo "session badge mismatch: expected [$expect] got [$got]" >&2
  return 1
}

badge_wait "▲"
echo "session badge blocked ok"

tmux -L "$TMUX_SOCKET" send-keys -t "$PANE_ID" C-c
sleep 0.3
tmux -L "$TMUX_SOCKET" send-keys -t "$PANE_ID" "clear" C-m
sleep 0.1
tmux -L "$TMUX_SOCKET" clear-history -t "$PANE_ID"
tmux -L "$TMUX_SOCKET" set-option -p -t "$PANE_ID" @vde_status idle
tmux -L "$TMUX_SOCKET" set-option -p -t "$PANE_ID" -u @vde_wait_reason 2>/dev/null || true
badge_wait "✓"
echo "session badge done ok"

SESSIONS_OUT="$(VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" \
  VDE_DAEMON_SOCKET="$DAEMON_SOCKET" \
  XDG_STATE_HOME="$STATE_HOME" \
  XDG_CONFIG_HOME="$CONFIG_HOME" \
  "$BIN" statusline-sessions 2>/dev/null || true)"
case "$SESSIONS_OUT" in
  *"✓"*) echo "statusline badge render ok" ;;
  *)
    echo "statusline output missing badge: [$SESSIONS_OUT]" >&2
    exit 1
    ;;
esac

VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" \
VDE_DAEMON_SOCKET="$DAEMON_SOCKET" \
XDG_STATE_HOME="$STATE_HOME" \
XDG_CONFIG_HOME="$CONFIG_HOME" \
"$BIN" sidebar input j

python3 - "$DAEMON_SOCKET" <<'PY'
import json
import socket
import sys

path = sys.argv[1]
client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
client.connect(path)
client.sendall(json.dumps({"op": "subscribe", "proto": 1}).encode() + b"\n")
client.settimeout(5)
line = client.makefile("rb").readline()
assert line, "no snapshot after input"
msg = json.loads(line)
state = msg["snapshot"]["sidebar"]["state"]
assert state["selection"], state
print("input redraw state ok")
PY

VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" \
VDE_DAEMON_SOCKET="$DAEMON_SOCKET" \
XDG_STATE_HOME="$STATE_HOME" \
XDG_CONFIG_HOME="$CONFIG_HOME" \
"$BIN" sidebar jump "$PANE_ID"

python3 - "$DAEMON_SOCKET" <<'PY'
import json
import socket
import sys

path = sys.argv[1]
client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
client.connect(path)
client.sendall(json.dumps({"op": "query", "proto": 1, "what": "summary"}).encode() + b"\n")
client.settimeout(5)
line = client.makefile("rb").readline()
assert line, "no query response"
msg = json.loads(line)
assert msg["type"] == "summary", msg
print("query response ok")
PY

kill "$DAEMON_PID"
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""
BADGE_AFTER="$(tmux -L "$TMUX_SOCKET" show-options -v -t main @vde_session_status 2>/dev/null || true)"
[[ -z "$BADGE_AFTER" ]]
echo "session badge cleanup ok"

echo "M6 runtime smoke ok"
