#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/vt"
STAMP="$(date +%s)"
TMUX_SOCKET="vde-m6-runtime-$STAMP"
RUNTIME_DIR="/tmp/vde-m6-runtime-$STAMP"
DAEMON_SOCKET="$RUNTIME_DIR/daemon.sock"
STATE_HOME="$RUNTIME_DIR/state"
LOG="$RUNTIME_DIR/daemon.log"

cleanup() {
  set +e
  if [[ -n "${DAEMON_PID:-}" ]]; then
    kill "$DAEMON_PID" 2>/dev/null
    wait "$DAEMON_PID" 2>/dev/null
  fi
  tmux -L "$TMUX_SOCKET" kill-server 2>/dev/null
  rm -rf "$RUNTIME_DIR"
}
trap cleanup EXIT

mkdir -p "$RUNTIME_DIR" "$STATE_HOME"
chmod 700 "$RUNTIME_DIR"

cargo build

tmux -L "$TMUX_SOCKET" -f /dev/null new-session -d -s main -n work -c "$ROOT" "/bin/sh"
PANE_ID="$(tmux -L "$TMUX_SOCKET" list-panes -a -F '#{pane_id}' | head -n 1)"

tmux -L "$TMUX_SOCKET" set-option -p -t "$PANE_ID" @vde_agent codex
tmux -L "$TMUX_SOCKET" set-option -p -t "$PANE_ID" @vde_status running
tmux -L "$TMUX_SOCKET" send-keys -t "$PANE_ID" "printf '? Allow command to run?\n  y) yes\n  n) no\n'; sleep 600" C-m

VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" \
VDE_DAEMON_SOCKET="$DAEMON_SOCKET" \
XDG_STATE_HOME="$STATE_HOME" \
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
while time.time() < deadline:
    line = reader.readline()
    assert line, "no subscribe snapshot"
    msg = json.loads(line)
    assert msg["type"] == "snapshot", msg
    snapshot = msg["snapshot"]
    last = snapshot
    if snapshot["agent_count"] >= 1 and snapshot.get("sidebar", {}).get("rows"):
        print("subscribe snapshot ok")
        break
else:
    raise AssertionError(last)
PY

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
while time.time() < deadline:
    line = reader.readline()
    assert line, "no detect snapshot"
    msg = json.loads(line)
    snapshot = msg["snapshot"]
    last = snapshot
    panes = snapshot.get("panes", [])
    if any(pane.get("wait_reason") == "permission_prompt" for pane in panes):
        print("capture detect ok")
        break
else:
    raise AssertionError(last)
PY

VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" \
VDE_DAEMON_SOCKET="$DAEMON_SOCKET" \
XDG_STATE_HOME="$STATE_HOME" \
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
"$BIN" sidebar jump "$PANE_ID"

python3 - "$DAEMON_SOCKET" <<'PY'
import json
import socket
import sys

path = sys.argv[1]
client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
client.connect(path)
client.sendall(json.dumps({"op": "query", "proto": 1, "what": "statusline"}).encode() + b"\n")
client.settimeout(5)
line = client.makefile("rb").readline()
assert line, "no query response"
msg = json.loads(line)
assert msg["type"] == "statusline", msg
print("query response ok")
PY

echo "M6 runtime smoke ok"
