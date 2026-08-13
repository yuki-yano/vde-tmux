#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/vt"
TMUX_SOCKET="vde-pane-switch-control-$$"

cleanup() {
  if [[ -n "${TMUX_ENV:-}" ]]; then
    env TMUX="$TMUX_ENV" VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" "$BIN" daemon stop >/dev/null 2>&1 || true
  fi
  tmux -L "$TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
}
trap cleanup EXIT

SOURCE="$({ tmux -L "$TMUX_SOCKET" -f /dev/null new-session -d -P -F '#{pane_id}' -s main 'sleep 30'; })"
TARGET="$(tmux -L "$TMUX_SOCKET" split-window -h -d -P -F '#{pane_id}' -t "$SOURCE" 'sleep 30')"
tmux -L "$TMUX_SOCKET" select-pane -t "$SOURCE"
TMUX_PATH="$(tmux -L "$TMUX_SOCKET" display-message -p '#{socket_path}')"
TMUX_ENV="$TMUX_PATH,$(tmux -L "$TMUX_SOCKET" display-message -p '#{pid}'),0"
env TMUX="$TMUX_ENV" VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" "$BIN" daemon ensure >/dev/null

for _ in $(seq 1 100); do
  CONTROL_HEALTH="$(env TMUX="$TMUX_ENV" VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" "$BIN" daemon status | sed -n 's/^control: //p')"
  [[ "$CONTROL_HEALTH" == "Ready" ]] && break
  sleep 0.02
done
[[ "$CONTROL_HEALTH" == "Ready" ]]

CONTROL_ROW="$(tmux -L "$TMUX_SOCKET" list-clients -F '#{client_control_mode}|#{client_flags}')"
[[ "$CONTROL_ROW" == *'1|'* ]]
[[ "$CONTROL_ROW" == *'ignore-size'* ]]
[[ "$CONTROL_ROW" == *'no-output'* ]]
[[ "$CONTROL_ROW" == *'no-detach-on-destroy'* ]]
# The persistent control client intentionally counts as attached in tmux's native alert and
# destroy-unattached bookkeeping; vde's regular-client projection below must still be detached.
[[ "$(tmux -L "$TMUX_SOCKET" display-message -p -t main '#{session_attached}')" == "1" ]]

SOURCE_PID="$(tmux -L "$TMUX_SOCKET" display-message -p -t "$SOURCE" '#{pane_pid}')"
DAEMON_SOCKET="$(tmux -L "$TMUX_SOCKET" show-option -gqv @vde_daemon_socket)"
SERVER_IDENTITY="$(tmux -L "$TMUX_SOCKET" show-option -gqv @vde_daemon_server_identity)"

# A Neovim process marker is validated against the pane's process tree. A stale marker left by an
# abnormal editor exit must not make a later Codex/Claude process consume C-h/j/k/l itself.
tmux -L "$TMUX_SOCKET" set-option -p -t "$SOURCE" @vde_nvim_process_pid "$SOURCE_PID"
for _ in $(seq 1 100); do
  STALE_NVIM_MARKER="$(tmux -L "$TMUX_SOCKET" show-option -pqv -t "$SOURCE" @vde_nvim_process_pid)"
  [[ -z "$STALE_NVIM_MARKER" ]] && break
  sleep 0.05
done
[[ -z "$STALE_NVIM_MARKER" ]]

python3 - "$DAEMON_SOCKET" <<'PY'
import json
import socket
import sys

connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
connection.connect(sys.argv[1])
stream = connection.makefile("rwb", buffering=0)
stream.write(b'{"op":"hello","proto":10}\n')
assert json.loads(stream.readline())["type"] == "hello_ack"
stream.write(b'{"op":"query_status_snapshot","proto":10,"context":"global"}\n')
response = json.loads(stream.readline())
assert response["type"] == "status_snapshot_result", response
assert response["snapshot"]["sessions"][0]["attached"] is False, response
connection.close()
PY

env TMUX="$TMUX_ENV" VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" "$BIN" pane-switch right \
  --pane-id "$SOURCE" --pane-pid "$SOURCE_PID" \
  --daemon-socket "$DAEMON_SOCKET" --server-identity "$SERVER_IDENTITY"
[[ "$(tmux -L "$TMUX_SOCKET" display-message -p -t main '#{pane_id}')" == "$TARGET" ]]

TARGET_PID="$(tmux -L "$TMUX_SOCKET" display-message -p -t "$TARGET" '#{pane_pid}')"
PANE_SWITCH_CHANNEL="$(tmux -L "$TMUX_SOCKET" show-option -gqv @vde_pane_switch_channel)"
tmux -L "$TMUX_SOCKET" set-option -g @vde_pane_switch_request \
  "${SERVER_IDENTITY}__vde_pane_switch_request__left__vde_pane_switch_request__${TARGET}__vde_pane_switch_request__${TARGET_PID}"
tmux -L "$TMUX_SOCKET" wait-for -S "$PANE_SWITCH_CHANNEL"
for _ in $(seq 1 100); do
  [[ "$(tmux -L "$TMUX_SOCKET" display-message -p -t main '#{pane_id}')" == "$SOURCE" ]] && break
  sleep 0.01
done
[[ "$(tmux -L "$TMUX_SOCKET" display-message -p -t main '#{pane_id}')" == "$SOURCE" ]]

ORIGINAL_CONTROL_PID="$(tmux -L "$TMUX_SOCKET" list-clients -F '#{client_pid}|#{client_control_mode}' | awk -F'|' '$2 == 1 {print $1}')"
kill "$ORIGINAL_CONTROL_PID"
REPLACEMENT_CONTROL_PID=""
for _ in $(seq 1 150); do
  CONTROL_HEALTH="$(env TMUX="$TMUX_ENV" VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" "$BIN" daemon status | sed -n 's/^control: //p')"
  REPLACEMENT_CONTROL_PID="$(tmux -L "$TMUX_SOCKET" list-clients -F '#{client_pid}|#{client_control_mode}' \
    | awk -F'|' -v original="$ORIGINAL_CONTROL_PID" '$2 == 1 && $1 != original {print $1}')"
  [[ "$CONTROL_HEALTH" == "Ready" && -n "$REPLACEMENT_CONTROL_PID" ]] && break
  sleep 0.02
done
[[ "$CONTROL_HEALTH" == "Ready" ]]
[[ -n "$REPLACEMENT_CONTROL_PID" ]]

# Stopping the daemon must close the control client before removing the daemon socket. Otherwise
# the resulting client-detached hook can race with shutdown and restart an enabled daemon.
env TMUX="$TMUX_ENV" VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" "$BIN" daemon stop >/dev/null
for _ in $(seq 1 100); do
  DAEMON_CONTROL_COUNT="$(tmux -L "$TMUX_SOCKET" list-clients -F '#{client_control_mode}' | grep -c '^1$' || true)"
  [[ ! -S "$DAEMON_SOCKET" && "$DAEMON_CONTROL_COUNT" == "0" ]] && break
  sleep 0.02
done
[[ ! -S "$DAEMON_SOCKET" ]]
[[ "$DAEMON_CONTROL_COUNT" == "0" ]]

echo "Pane switch control smoke passed"
