#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/vt"
STAMP="$(date +%s)-$$-$RANDOM"
TMUX_SOCKET="vde-pane-state-smoke-$STAMP"
SECONDARY_TMUX_SOCKET="vde-pane-state-smoke-secondary-$STAMP"
RUNTIME_DIR="${TMPDIR:-/tmp}/vde-pane-state-smoke-$STAMP"
STATE_HOME="$RUNTIME_DIR/state"
CONFIG_HOME="$RUNTIME_DIR/config"
HOME_DIR="$RUNTIME_DIR/home"
TMUX_CONF="$RUNTIME_DIR/tmux.conf"
QUERY_JSON="$RUNTIME_DIR/query.json"
CLIENT_LOG_1="$RUNTIME_DIR/client-1.log"
CLIENT_LOG_2="$RUNTIME_DIR/client-2.log"
CLIENT_LOG_3="$RUNTIME_DIR/client-3.log"
CLIENT_FIFO_1="$RUNTIME_DIR/client-1.in"
CLIENT_FIFO_2="$RUNTIME_DIR/client-2.in"
CLIENT_FIFO_3="$RUNTIME_DIR/client-3.in"
PTY_CLIENT="$RUNTIME_DIR/pty-client.py"
HOOK_BIN_DIR="$RUNTIME_DIR/bin"
HOOK_LOG="$RUNTIME_DIR/hook-delivery.log"
TMUX_PROCESS_LOG="$RUNTIME_DIR/tmux-process.log"
SYSTEM_TMUX="$(command -v tmux)"

cleanup() {
  set +e
  local cleanup_pids="${CLIENT_PID_1:-} ${CLIENT_PID_2:-} ${CLIENT_PID_3:-} ${CONTROL_PID:-} ${HOOK_CLI_PID:-} ${DAEMON_PIDS:-}"
  [[ -n "${STOPPED_DAEMON_PID:-}" ]] && kill -CONT "$STOPPED_DAEMON_PID" 2>/dev/null
  for cleanup_pid in $cleanup_pids; do
    kill "$cleanup_pid" 2>/dev/null
  done
  if [[ -n "${TMUX_ENV:-}" ]]; then
    env TMUX="$TMUX_ENV" VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" \
      XDG_STATE_HOME="$STATE_HOME" XDG_CONFIG_HOME="$CONFIG_HOME" HOME="$HOME_DIR" \
      "$BIN" daemon stop >/dev/null 2>&1
  fi
  "$SYSTEM_TMUX" -L "$TMUX_SOCKET" kill-server >/dev/null 2>&1
  "$SYSTEM_TMUX" -L "$SECONDARY_TMUX_SOCKET" kill-server >/dev/null 2>&1
  for _ in $(seq 1 40); do
    local any_alive=0
    for cleanup_pid in $cleanup_pids; do
      if kill -0 "$cleanup_pid" 2>/dev/null; then
        any_alive=1
      fi
    done
    [[ "$any_alive" == 0 ]] && break
    sleep 0.05
  done
  for cleanup_pid in $cleanup_pids; do
    if kill -0 "$cleanup_pid" 2>/dev/null; then
      kill -KILL "$cleanup_pid" 2>/dev/null
    fi
    wait "$cleanup_pid" 2>/dev/null
  done
  rm -rf "$RUNTIME_DIR"
}
trap cleanup EXIT

mkdir -p "$STATE_HOME" "$CONFIG_HOME/vde/tmux" "$HOME_DIR" "$HOOK_BIN_DIR"
chmod 700 "$RUNTIME_DIR" "$STATE_HOME" "$CONFIG_HOME" "$HOME_DIR"

cargo build --locked

cat >"$HOOK_BIN_DIR/vt" <<EOF
#!/usr/bin/env bash
exec 3>&2
TIMEFORMAT='%R'
duration="\$({ time "$BIN" "\$@" 2>&3; } 2>&1)"
status=\$?
printf 'duration=%s status=%s args=' "\$duration" "\$status" >>"$HOOK_LOG"
printf ' %q' "\$@" >>"$HOOK_LOG"
printf '\n' >>"$HOOK_LOG"
exit "\$status"
EOF
chmod +x "$HOOK_BIN_DIR/vt"
: >"$HOOK_LOG"

# Only daemon and owned-hook children inherit this PATH. Direct smoke-driver tmux commands use the
# real binary, so this log counts one entry per tmux client process launched by the v2 runtime.
cat >"$HOOK_BIN_DIR/tmux" <<EOF
#!/usr/bin/env bash
printf -v rendered ' %q' "\$@"
printf '%s\n' "\$rendered" >>"$TMUX_PROCESS_LOG"
exec "$SYSTEM_TMUX" "\$@"
EOF
chmod +x "$HOOK_BIN_DIR/tmux"
: >"$TMUX_PROCESS_LOG"

cat >"$CONFIG_HOME/vde/tmux/config.yml" <<'YAML'
categories:
  default_category: smoke
daemon:
  done_clear_on: pane
YAML

# This config is intentionally self-contained. Every format is an option reference and contains
# no #() command. The daemon is started while the scratch server still has zero sessions.
cat >"$TMUX_CONF" <<EOF
set -g exit-empty off
set -g status-left '#{@vde_status_category}#{@vde_status_sessions}#{@vde_status_windows}'
set -g status-right '#{@vde_status_attention}#{@vde_status_summary}'
set -g pane-border-status top
set -g pane-border-format '#{@vde_status_pane}'
set-hook -g 'client-session-changed[0]' 'run-shell "vt hooks on-client-session-changed '\''#{client_pid}'\'' '\''#{session_name}'\''"'
set-hook -g 'window-pane-changed[71]' 'set-option -g @vde_smoke_user_hook preserved'
run-shell -b 'env PATH="$HOOK_BIN_DIR:$ROOT/target/debug:$PATH" XDG_STATE_HOME="$STATE_HOME" XDG_CONFIG_HOME="$CONFIG_HOME" HOME="$HOME_DIR" "$BIN" daemon ensure'
EOF

V2_SOCKET_ROOT="/tmp/vt-$(id -u)/v2"
PATH="$HOOK_BIN_DIR:$ROOT/target/debug:$PATH" HOME="$HOME_DIR" XDG_CONFIG_HOME="$CONFIG_HOME" \
  XDG_STATE_HOME="$STATE_HOME" tmux -L "$TMUX_SOCKET" -f "$TMUX_CONF" start-server
TMUX_PATH="$(tmux -L "$TMUX_SOCKET" display-message -p '#{socket_path}')"
TMUX_PID="$(tmux -L "$TMUX_SOCKET" display-message -p '#{pid}')"
TMUX_START_TIME="$(tmux -L "$TMUX_SOCKET" display-message -p '#{start_time}')"
TMUX_ENV="$TMUX_PATH,$TMUX_PID,0"
SERVER_HASH="$(python3 - "$TMUX_PATH" "$TMUX_PID" "$TMUX_START_TIME" <<'PY'
import hashlib, sys
digest = hashlib.sha256()
digest.update(__import__("os").path.realpath(sys.argv[1]).encode())
digest.update(b"\0")
digest.update(sys.argv[2].encode())
digest.update(b"\0")
digest.update(sys.argv[3].encode())
print(digest.hexdigest())
PY
)"

run_vt() {
  local pane="${VT_PANE:-${AGENT_PANE:-}}"
  env TMUX="$TMUX_ENV" TMUX_PANE="$pane" \
    VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" XDG_STATE_HOME="$STATE_HOME" \
    XDG_CONFIG_HOME="$CONFIG_HOME" HOME="$HOME_DIR" "$BIN" "$@"
}

record_daemon_pid() {
  local daemon_pid
  daemon_pid="$(lsof -t "$DAEMON_SOCKET" 2>/dev/null | head -n 1 || true)"
  if [[ -n "$daemon_pid" ]]; then
    DAEMON_PIDS="${DAEMON_PIDS:-} $daemon_pid"
  fi
}

# Confirm the config-load run-shell itself created a new daemon socket and reached Serving on an
# empty topology. No explicit `daemon ensure` is allowed before this proof.
DAEMON_SOCKET="$V2_SOCKET_ROOT/$SERVER_HASH.sock"
for _ in $(seq 1 80); do
  [[ -S "$DAEMON_SOCKET" ]] && break
  sleep 0.1
done
[[ -n "$DAEMON_SOCKET" && -S "$DAEMON_SOCKET" ]]

python3 - "$DAEMON_SOCKET" <<'PY'
import json, socket, sys, time
path = sys.argv[1]
deadline = time.time() + 8
while True:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(5)
    s.connect(path)
    s.sendall(b'{"op":"hello","proto":4}\n')
    reader = s.makefile("rb")
    hello = json.loads(reader.readline())
    assert hello["type"] == "hello_ack" and hello["proto"] == 4, hello
    if hello["phase"] == "serving":
        break
    s.close()
    assert time.time() < deadline, hello
    time.sleep(0.1)
s.sendall(b'{"op":"query_resolved_snapshot","proto":4}\n')
reply = json.loads(reader.readline())
assert reply["type"] == "resolved_snapshot_result", reply
assert reply["snapshot"]["panes"] == [], reply
print("empty-topology v2 Serving ok")
PY
record_daemon_pid
for hook in window-pane-changed session-window-changed client-session-changed client-attached client-detached; do
  tmux -L "$TMUX_SOCKET" show-hooks -g "${hook}[70]" | grep -F "${hook}[70]" >/dev/null
done
tmux -L "$TMUX_SOCKET" show-options -gqv @vde_status_summary >/dev/null
echo "empty-topology hook install and display initialization ok"

# A second zero-topology server runs concurrently with the first and must acquire an independent
# v2 socket/writer lease namespace. Both reach Serving before either one is stopped.
PATH="$HOOK_BIN_DIR:$ROOT/target/debug:$PATH" HOME="$HOME_DIR" XDG_CONFIG_HOME="$CONFIG_HOME" \
  XDG_STATE_HOME="$STATE_HOME" tmux -L "$SECONDARY_TMUX_SOCKET" -f "$TMUX_CONF" start-server
SECONDARY_TMUX_PATH="$(tmux -L "$SECONDARY_TMUX_SOCKET" display-message -p '#{socket_path}')"
SECONDARY_TMUX_PID="$(tmux -L "$SECONDARY_TMUX_SOCKET" display-message -p '#{pid}')"
SECONDARY_TMUX_START="$(tmux -L "$SECONDARY_TMUX_SOCKET" display-message -p '#{start_time}')"
SECONDARY_HASH="$(python3 - "$SECONDARY_TMUX_PATH" "$SECONDARY_TMUX_PID" "$SECONDARY_TMUX_START" <<'PY'
import hashlib, os, sys
digest = hashlib.sha256()
for index, value in enumerate((os.path.realpath(sys.argv[1]), sys.argv[2], sys.argv[3])):
    if index:
        digest.update(b"\0")
    digest.update(value.encode())
print(digest.hexdigest())
PY
)"
SECONDARY_DAEMON_SOCKET="$V2_SOCKET_ROOT/$SECONDARY_HASH.sock"
[[ "$SECONDARY_DAEMON_SOCKET" != "$DAEMON_SOCKET" ]]
for _ in $(seq 1 80); do
  [[ -S "$SECONDARY_DAEMON_SOCKET" ]] && break
  sleep 0.1
done
python3 - "$DAEMON_SOCKET" "$SECONDARY_DAEMON_SOCKET" <<'PY'
import json, socket, sys, time
identities = []
for path in sys.argv[1:]:
    deadline = time.time() + 8
    while True:
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(5)
        client.connect(path)
        client.sendall(b'{"op":"hello","proto":4}\n')
        reply = json.loads(client.makefile("rb").readline())
        assert reply["type"] == "hello_ack", reply
        if reply["phase"] == "serving":
            break
        assert time.time() < deadline, reply
        client.close()
        time.sleep(0.05)
    identities.append(reply["server_identity"])
assert identities[0] != identities[1], identities
PY
SECONDARY_DAEMON_PID="$(lsof -t "$SECONDARY_DAEMON_SOCKET" 2>/dev/null | head -n 1)"
[[ -n "$SECONDARY_DAEMON_PID" && "$SECONDARY_DAEMON_PID" != "$(lsof -t "$DAEMON_SOCKET" 2>/dev/null | head -n 1)" ]]
DAEMON_PIDS="${DAEMON_PIDS:-} $SECONDARY_DAEMON_PID"
env TMUX="$SECONDARY_TMUX_PATH,$SECONDARY_TMUX_PID,0" \
  VDE_TMUX_SOCKET_NAME="$SECONDARY_TMUX_SOCKET" XDG_STATE_HOME="$STATE_HOME" \
  XDG_CONFIG_HOME="$CONFIG_HOME" HOME="$HOME_DIR" "$BIN" daemon stop >/dev/null 2>&1 || true
for _ in $(seq 1 40); do
  ! kill -0 "$SECONDARY_DAEMON_PID" 2>/dev/null && break
  sleep 0.05
done
if kill -0 "$SECONDARY_DAEMON_PID" 2>/dev/null; then
  kill "$SECONDARY_DAEMON_PID"
fi
for _ in $(seq 1 40); do
  ! kill -0 "$SECONDARY_DAEMON_PID" 2>/dev/null && break
  sleep 0.05
done
if kill -0 "$SECONDARY_DAEMON_PID" 2>/dev/null; then
  kill -KILL "$SECONDARY_DAEMON_PID"
fi
wait "$SECONDARY_DAEMON_PID" 2>/dev/null || true
tmux -L "$SECONDARY_TMUX_SOCKET" kill-server
[[ -S "$DAEMON_SOCKET" ]]
echo "concurrent scratch server socket and writer-lease isolation ok"

tmux -L "$TMUX_SOCKET" new-session -d -s main -n work -c "$ROOT" "sleep 600"
AGENT_PANE="$(tmux -L "$TMUX_SOCKET" display-message -p -t main:work '#{pane_id}')"
tmux -L "$TMUX_SOCKET" split-window -d -t main:work -c "$ROOT" "sleep 600"
OTHER_PANE="$(tmux -L "$TMUX_SOCKET" list-panes -t main:work -F '#{pane_id}' | grep -v -F "$AGENT_PANE" | head -n 1)"
tmux -L "$TMUX_SOCKET" new-session -d -s aux -n own -c "$ROOT" "sleep 600"
tmux -L "$TMUX_SOCKET" link-window -s main:work -t aux:
tmux -L "$TMUX_SOCKET" rename-window -t aux:work linked
tmux -L "$TMUX_SOCKET" set-option -t main @vde_category smoke
tmux -L "$TMUX_SOCKET" set-option -t aux @vde_category smoke
tmux -L "$TMUX_SOCKET" bind-key -T prefix A select-pane -t "$AGENT_PANE"
tmux -L "$TMUX_SOCKET" bind-key -T prefix O select-pane -t "$OTHER_PANE"
MAIN_SESSION_ID="$(tmux -L "$TMUX_SOCKET" display-message -p -t main '#{session_id}')"
AUX_SESSION_ID="$(tmux -L "$TMUX_SOCKET" display-message -p -t aux '#{session_id}')"
WINDOW_ID="$(tmux -L "$TMUX_SOCKET" display-message -p -t main:work '#{window_id}')"

query_v2() {
  local request="$1"
  python3 - "$DAEMON_SOCKET" "$request" "$QUERY_JSON" <<'PY'
import json, socket, sys
path, raw, output = sys.argv[1:]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(5)
s.connect(path)
reader = s.makefile("rb")
s.sendall(b'{"op":"hello","proto":4}\n')
hello = json.loads(reader.readline())
assert hello["type"] == "hello_ack" and hello["phase"] == "serving", hello
s.sendall(json.dumps(json.loads(raw), separators=(",", ":")).encode() + b"\n")
reply = json.loads(reader.readline())
with open(output, "w", encoding="utf-8") as handle:
    json.dump(reply, handle)
PY
}

wait_for_topology() {
  for _ in $(seq 1 80); do
    query_v2 '{"op":"query_resolved_snapshot","proto":4}'
    if python3 - "$QUERY_JSON" "$AGENT_PANE" "$MAIN_SESSION_ID" "$AUX_SESSION_ID" 2>/dev/null <<'PY'
import json, sys
reply = json.load(open(sys.argv[1], encoding="utf-8"))
panes = reply.get("snapshot", {}).get("panes", [])
target = next((p for p in panes if p["pane_instance"]["pane_id"] == sys.argv[2]), None)
assert target is not None
links = {link["session_id"] for link in target["session_links"]}
assert {sys.argv[3], sys.argv[4]} <= links
PY
    then return 0; fi
    sleep 0.1
  done
  echo "canonical topology did not converge" >&2
  return 1
}
wait_for_topology

# Each observation poll must launch one tmux client containing every pane capture in a single
# command group. Linked windows are deduplicated, so this topology has exactly three pane
# instances even though the agent window belongs to two sessions.
: >"$TMUX_PROCESS_LOG"
sleep 3.2
CAPTURE_PROCESS_COUNT="$(grep -c ' capture-pane' "$TMUX_PROCESS_LOG" || true)"
[[ "$CAPTURE_PROCESS_COUNT" -ge 2 && "$CAPTURE_PROCESS_COUNT" -le 5 ]]
EXPECTED_CAPTURE_PANES=3
while IFS= read -r capture_command; do
  CAPTURES_IN_PROCESS="$(grep -o 'capture-pane' <<<"$capture_command" | wc -l | tr -d ' ')"
  [[ "$CAPTURES_IN_PROCESS" == "$EXPECTED_CAPTURE_PANES" ]]
done < <(grep ' capture-pane' "$TMUX_PROCESS_LOG")
echo "capture batching ok: $CAPTURE_PROCESS_COUNT polls, 1 process/poll, $EXPECTED_CAPTURE_PANES panes/process"

# v1 and unknown protocol are rejected at Hello, before any side effect.
python3 - "$DAEMON_SOCKET" <<'PY'
import json, socket, sys
for proto in (1, 999):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(5)
    s.connect(sys.argv[1])
    s.sendall(json.dumps({"op":"hello", "proto":proto}).encode() + b"\n")
    reply = json.loads(s.makefile("rb").readline())
    assert reply["type"] == "error" and reply["code"] == "unsupported_protocol", reply
print("protocol rejection ok")
PY

# vde-tmux owns index 70 only; the pre-existing index 71 remains byte-for-byte present.
for hook in window-pane-changed session-window-changed client-session-changed client-attached client-detached; do
  tmux -L "$TMUX_SOCKET" show-hooks -g "${hook}[70]" | grep -F "${hook}[70]" >/dev/null
done
USER_HOOK="$(tmux -L "$TMUX_SOCKET" show-hooks -g 'window-pane-changed[71]')"
grep -F 'window-pane-changed[71]' <<<"$USER_HOOK" >/dev/null
grep -F '@vde_smoke_user_hook preserved' <<<"$USER_HOOK" >/dev/null
echo "hook index 70 coexistence ok"
SERVER_PATH="$(tmux -L "$TMUX_SOCKET" show-environment -g PATH | sed 's/^PATH=//')"
[[ ":$SERVER_PATH:" == *":$ROOT/target/debug:"* ]]
[[ "$(PATH="$SERVER_PATH" command -v vt)" == "$HOOK_BIN_DIR/vt" ]]
grep -F "\"$BIN\"" "$HOOK_BIN_DIR/vt" >/dev/null
echo "owned hook resolves current target/debug/vt"

# Two PTY-backed attach-session processes are normal clients (not control or active-pane clients).
cat >"$PTY_CLIENT" <<'PY'
import os, pty, select, subprocess, sys
fifo, log, socket_name, session, *mode = sys.argv[1:]
master, slave = pty.openpty()
env = dict(os.environ, TERM="xterm-256color")
command = ["tmux", "-L", socket_name, "attach-session"]
if mode == ["readonly"]:
    command.append("-r")
command += ["-t", session]
proc = subprocess.Popen(
    command,
    stdin=slave, stdout=slave, stderr=slave, env=env,
)
os.close(slave)
fifo_fd = os.open(fifo, os.O_RDWR | os.O_NONBLOCK)
with open(log, "wb", buffering=0) as output:
    while proc.poll() is None:
        readable, _, _ = select.select([master, fifo_fd], [], [], 0.2)
        for fd in readable:
            try:
                data = os.read(fd, 4096)
            except OSError:
                data = b""
            if not data:
                continue
            if fd == fifo_fd:
                os.write(master, data)
            else:
                output.write(data)
PY
mkfifo "$CLIENT_FIFO_1" "$CLIENT_FIFO_2"
python3 "$PTY_CLIENT" "$CLIENT_FIFO_1" "$CLIENT_LOG_1" "$TMUX_SOCKET" main &
CLIENT_PID_1=$!
python3 "$PTY_CLIENT" "$CLIENT_FIFO_2" "$CLIENT_LOG_2" "$TMUX_SOCKET" aux &
CLIENT_PID_2=$!
exec 7>"$CLIENT_FIFO_1"
exec 8>"$CLIENT_FIFO_2"
for _ in $(seq 1 50); do
  [[ "$(tmux -L "$TMUX_SOCKET" list-clients -F '#{client_control_mode}' 2>/dev/null | grep -c '^0$' || true)" -ge 2 ]] && break
  sleep 0.1
done
[[ "$(tmux -L "$TMUX_SOCKET" list-clients -F '#{client_control_mode}' | grep -c '^0$')" -ge 2 ]]
CLIENT_FIELD_SEP=$'\037'
client_for_session() {
  local session_name="$1"
  tmux -L "$TMUX_SOCKET" list-clients -F "#{client_name}${CLIENT_FIELD_SEP}#{client_session}" |
    awk -F "$CLIENT_FIELD_SEP" -v session_name="$session_name" '$2 == session_name { print $1; found = 1; exit } END { exit !found }'
}
client_session_value() {
  local client_name="$1"
  local field="$2"
  tmux -L "$TMUX_SOCKET" list-clients -F "#{client_name}${CLIENT_FIELD_SEP}${field}" |
    awk -F "$CLIENT_FIELD_SEP" -v client_name="$client_name" '$1 == client_name { print $2; found = 1; exit } END { exit !found }'
}

CLIENT_1="$(client_for_session main)"
CLIENT_2="$(client_for_session aux)"
[[ -n "$CLIENT_1" && -n "$CLIENT_2" ]]
tmux -L "$TMUX_SOCKET" select-window -t aux:own

# Category mutations use one session snapshot, commit the target client switch first, and leave
# remembered-session/topology/status work to the foreground hook path. Exercise three categories
# on this isolated server so two rapid Next operations have an unambiguous two-category result.
tmux -L "$TMUX_SOCKET" new-session -d -s category-fast -n work "sleep 600"
CATEGORY_FAST_SESSION_ID="$(tmux -L "$TMUX_SOCKET" display-message -p -t category-fast '#{session_id}')"
tmux -L "$TMUX_SOCKET" set-option -t main @vde_category_override category-a
tmux -L "$TMUX_SOCKET" set-option -t aux @vde_category_override category-b
tmux -L "$TMUX_SOCKET" set-option -t category-fast @vde_category_override category-c
tmux -L "$TMUX_SOCKET" switch-client -c "$CLIENT_1" -t '=main:'
CATEGORY_TIMINGS="$RUNTIME_DIR/category-timings.txt"
: >"$CATEGORY_TIMINGS"

run_category_action() {
  local direction="$1"
  local source_session
  local switched_session
  local started_ns
  local switched_ns
  local action_pid
  local action_error="$RUNTIME_DIR/category-action-error.txt"
  source_session="$(client_session_value "$CLIENT_1" '#{session_id}')"
  started_ns="$(python3 -c 'import time; print(time.time_ns())')"
  VT_PANE="$AGENT_PANE" run_vt category "$direction" \
    --client-name "$CLIENT_1" --session-id "$source_session" \
    > /dev/null 2>"$action_error" &
  action_pid=$!
  switched_session="$source_session"
  for _ in $(seq 1 100); do
    switched_session="$(client_session_value "$CLIENT_1" '#{session_id}')"
    [[ "$switched_session" != "$source_session" ]] && break
    sleep 0.005
  done
  switched_ns="$(python3 -c 'import time; print(time.time_ns())')"
  if [[ "$switched_session" == "$source_session" ]]; then
    echo "category client did not switch before the measurement deadline" >&2
    cat "$action_error" >&2
    wait "$action_pid" || true
    return 1
  fi
  if ! wait "$action_pid"; then
    cat "$action_error" >&2
    return 1
  fi
  python3 - "$started_ns" "$switched_ns" >>"$CATEGORY_TIMINGS" <<'PY'
import sys
started_ns, switched_ns = map(int, sys.argv[1:])
print((switched_ns - started_ns) / 1_000_000_000)
PY
}

# Warm every relevant binary, daemon query, and tmux hook path before measuring.
run_category_action next
tmux -L "$TMUX_SOCKET" switch-client -c "$CLIENT_1" -t '=main:'
HOOK_CALLS_BEFORE_CATEGORY="$(wc -l <"$HOOK_LOG")"
for _ in $(seq 1 30); do
  run_category_action next
done
[[ "$(client_session_value "$CLIENT_1" '#{client_session}')" == main ]]
[[ "$(client_session_value "$CLIENT_2" '#{client_session}')" == aux ]]

run_category_action next
[[ "$(client_session_value "$CLIENT_1" '#{client_session}')" == aux ]]
run_category_action next
[[ "$(client_session_value "$CLIENT_1" '#{session_id}')" == "$CATEGORY_FAST_SESSION_ID" ]]
run_category_action prev
[[ "$(client_session_value "$CLIENT_1" '#{client_session}')" == aux ]]
[[ "$(client_session_value "$CLIENT_2" '#{client_session}')" == aux ]]

HOOK_CALLS_AFTER_CATEGORY="$(wc -l <"$HOOK_LOG")"
CATEGORY_HOOK_LOG="$(tail -n "$((HOOK_CALLS_AFTER_CATEGORY - HOOK_CALLS_BEFORE_CATEGORY))" "$HOOK_LOG")"
[[ "$(grep -c 'pane-state-view client-session-changed' <<<"$CATEGORY_HOOK_LOG")" -ge 33 ]]
if grep 'pane-state-view client-session-changed' <<<"$CATEGORY_HOOK_LOG" | grep -v 'status=0 '; then
  echo "category owned hook failed before ViewQueued receipt" >&2
  exit 1
fi
[[ "$(grep -c 'hooks on-client-session-changed' <<<"$CATEGORY_HOOK_LOG")" -ge 33 ]]
if grep 'hooks on-client-session-changed' <<<"$CATEGORY_HOOK_LOG" | grep -v 'status=0 '; then
  echo "category remembered-session hook failed" >&2
  exit 1
fi
python3 - "$CATEGORY_TIMINGS" <<'PY'
import math, sys

durations = [float(line) for line in open(sys.argv[1], encoding="utf-8") if line.strip()]
# Exclude the initial warmup sample; the following 30 Next plus 3 order checks are measured.
durations = durations[1:]
assert len(durations) == 33, durations
ordered = sorted(durations)
p95 = ordered[math.ceil(len(ordered) * 0.95) - 1]
assert p95 <= 0.150, (p95, ordered)
print(f"category warm switch SLA ok: n={len(durations)} p95={p95 * 1000:.1f}ms max={ordered[-1] * 1000:.1f}ms")
PY

tmux -L "$TMUX_SOCKET" switch-client -c "$CLIENT_1" -t '=main:'
tmux -L "$TMUX_SOCKET" set-option -u -t main @vde_category_override
tmux -L "$TMUX_SOCKET" set-option -u -t aux @vde_category_override
tmux -L "$TMUX_SOCKET" kill-session -t '=category-fast:'
for category in category-a category-b category-c; do
  key="$(python3 - "$CLIENT_1" "$category" <<'PY'
import sys
client, category = sys.argv[1:]
print("@vde_client_" + client.encode().hex() + "_" + category)
PY
)"
  tmux -L "$TMUX_SOCKET" set-option -gu "$key"
done
run_vt sessions refresh-category
sleep 1
echo "category consecutive order, multi-client pin, and foreground SLA ok"

wait_badge() {
  wait_pane_badge "$AGENT_PANE" "$1"
}

wait_pane_badge() {
  local pane_id="$1"
  local expected="$2"
  for _ in $(seq 1 80); do
    query_v2 '{"op":"query_resolved_snapshot","proto":4}'
    if python3 - "$QUERY_JSON" "$pane_id" "$expected" 2>/dev/null <<'PY'
import json, sys
reply = json.load(open(sys.argv[1], encoding="utf-8"))
pane = next((p for p in reply["snapshot"]["panes"] if p["pane_instance"]["pane_id"] == sys.argv[2]), None)
assert pane and pane.get("resolved"), pane
assert pane["resolved"]["badge"] == sys.argv[3], pane["resolved"]
PY
    then return 0; fi
    sleep 0.1
  done
  echo "pane badge did not become $expected" >&2
  return 1
}

tmux -L "$TMUX_SOCKET" select-pane -t "$OTHER_PANE"
NOW="$(date +%s)"
VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status running --started-at "$NOW" --prompt 'literal #[x] #(false)' --prompt-source smoke
wait_badge Working
VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status idle --completed-at "$((NOW + 1))"
wait_badge Done

# Correct ownership/protocol markers do not make malformed or oversized inline frames acceptable.
# Both failures are logged outside canonical state and must leave the hidden Done pane untouched.
AGENT_PANE_PID="$(tmux -L "$TMUX_SOCKET" display-message -p -t "$AGENT_PANE" '#{pane_pid}')"
PANE_STATE_HOOK_LOG="$STATE_HOME/vde-tmux/$SERVER_HASH/pane-state-hook.log"
HOOK_FAILURES_BEFORE=0
if [[ -f "$PANE_STATE_HOOK_LOG" ]]; then
  HOOK_FAILURES_BEFORE="$(wc -l <"$PANE_STATE_HOOK_LOG")"
fi
if run_vt hooks pane-state-view window-pane-changed \
  --owner vde-tmux-pane-state --protocol 2 --hook-window="$WINDOW_ID" \
  --snapshot-session="$MAIN_SESSION_ID" --snapshot-window="$WINDOW_ID" \
  --snapshot-pane="$AGENT_PANE" --snapshot-pane-pid="$AGENT_PANE_PID" \
  --snapshot-panes=malformed --snapshot-clients= >/dev/null 2>&1; then
  echo "malformed inline hook snapshot was accepted" >&2
  exit 1
fi
OVERSIZED_HOOK_PANES="$(python3 - <<'PY'
print("x" * (64 * 1024 + 1), end="")
PY
)"
if run_vt hooks pane-state-view window-pane-changed \
  --owner vde-tmux-pane-state --protocol 2 --hook-window="$WINDOW_ID" \
  --snapshot-session="$MAIN_SESSION_ID" --snapshot-window="$WINDOW_ID" \
  --snapshot-pane="$AGENT_PANE" --snapshot-pane-pid="$AGENT_PANE_PID" \
  --snapshot-panes="$OVERSIZED_HOOK_PANES" --snapshot-clients= >/dev/null 2>&1; then
  echo "oversized inline hook snapshot was accepted" >&2
  exit 1
fi
wait_badge Done
HOOK_FAILURES_AFTER="$(wc -l <"$PANE_STATE_HOOK_LOG")"
[[ "$((HOOK_FAILURES_AFTER - HOOK_FAILURES_BEFORE))" == 2 ]]
tail -n 2 "$PANE_STATE_HOOK_LOG" | grep -F 'unterminated hook panes row' >/dev/null
tail -n 2 "$PANE_STATE_HOOK_LOG" | grep -F 'exceeds byte limit' >/dev/null
echo "malformed and oversized inline hook snapshots rejected without acknowledgment"

# A pane/window change issued by either normal client acknowledges globally.
# Enter and leave within one poll period; the foreground hook acknowledgment must not be lost.
HOOK_CALLS_BEFORE="$(wc -l <"$HOOK_LOG" 2>/dev/null || echo 0)"
printf '\002A' >&7
for _ in $(seq 1 20); do
  [[ "$(tmux -L "$TMUX_SOCKET" display-message -p -t main:linked '#{pane_id}')" == "$AGENT_PANE" ]] && break
  sleep 0.01
done
[[ "$(tmux -L "$TMUX_SOCKET" display-message -p -t main:linked '#{pane_id}')" == "$AGENT_PANE" ]]
printf '\002O' >&7
if ! wait_badge Idle; then
  echo "owned hook delivery log:" >&2
  cat "$HOOK_LOG" >&2 || true
  query_v2 '{"op":"query_resolved_snapshot","proto":4}'
  python3 - "$QUERY_JSON" <<'PY' >&2
import json, sys
reply = json.load(open(sys.argv[1], encoding="utf-8"))
print(json.dumps(reply.get("snapshot", {}).get("diagnostics", []), indent=2))
PY
  exit 1
fi
HOOK_CALLS_AFTER="$(wc -l <"$HOOK_LOG")"
[[ "$((HOOK_CALLS_AFTER - HOOK_CALLS_BEFORE))" -ge 2 ]]
if tail -n "$((HOOK_CALLS_AFTER - HOOK_CALLS_BEFORE))" "$HOOK_LOG" | grep -v 'status=0 '; then
  echo "owned hook command failed" >&2
  exit 1
fi
RAPID_HOOK_LOG="$(tail -n "$((HOOK_CALLS_AFTER - HOOK_CALLS_BEFORE))" "$HOOK_LOG")"
for argument in --snapshot-session= --snapshot-window= --snapshot-pane= --snapshot-pane-pid= --snapshot-panes= --snapshot-clients=; do
  grep -F -- "$argument" <<<"$RAPID_HOOK_LOG" >/dev/null
done
grep -F -- "--snapshot-pane=$AGENT_PANE" <<<"$RAPID_HOOK_LOG" >/dev/null
grep -F -- "--snapshot-pane=$OTHER_PANE" <<<"$RAPID_HOOK_LOG" >/dev/null
if grep -F -- '--hook-pane=' <<<"$RAPID_HOOK_LOG" >/dev/null; then
  echo "owned hook unexpectedly used the unsupported hook_pane format" >&2
  exit 1
fi
echo "immutable hook-time snapshot arguments ok"
VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status running --started-at "$((NOW + 2))"
wait_badge Working
VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status idle --completed-at "$((NOW + 3))"
wait_badge Done
echo "rapid focus acknowledgment and later completion ok"

# A script changes the active pane while the linked window is not displayed by either client. It
# must not acknowledge; switching a normal client back to that session must acknowledge.
tmux -L "$TMUX_SOCKET" switch-client -c "$CLIENT_1" -t '=aux:'
tmux -L "$TMUX_SOCKET" select-pane -t "$AGENT_PANE"
sleep 1
wait_badge Done
echo "detached select did not acknowledge"
tmux -L "$TMUX_SOCKET" switch-client -c "$CLIENT_1" -t '=main:'
wait_badge Idle
echo "client session change acknowledged globally"

# Window-level acknowledgment: prepare the agent pane while its window is hidden, then expose it
# through a normal client's session-window change.
printf '\002O' >&7
VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status running --started-at "$((NOW + 4))"
VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status idle --completed-at "$((NOW + 5))"
wait_badge Done
tmux -L "$TMUX_SOCKET" new-window -d -t main -n hidden "sleep 600"
tmux -L "$TMUX_SOCKET" select-window -t main:hidden
tmux -L "$TMUX_SOCKET" select-pane -t "$AGENT_PANE"
sleep 1
wait_badge Done
echo "hidden window pane change did not acknowledge"
tmux -L "$TMUX_SOCKET" select-window -t main:linked
wait_badge Idle
echo "session window change acknowledged"

# Control-mode attach is excluded from acknowledgment. A read-only normal client is eligible and
# its client-attached hook acknowledges the visible Done pane.
printf '\002O' >&7
VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status running --started-at "$((NOW + 6))"
VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status idle --completed-at "$((NOW + 7))"
wait_badge Done
tmux -L "$TMUX_SOCKET" switch-client -c "$CLIENT_1" -t '=aux:'
tmux -L "$TMUX_SOCKET" select-pane -t "$AGENT_PANE"
CONTROL_FIFO="$RUNTIME_DIR/control.in"
mkfifo "$CONTROL_FIFO"
exec 9<>"$CONTROL_FIFO"
tmux -L "$TMUX_SOCKET" -C attach-session -f active-pane -t main <"$CONTROL_FIFO" >"$RUNTIME_DIR/control.log" 2>&1 &
CONTROL_PID=$!
CONTROL_CLIENT=""
for _ in $(seq 1 30); do
  CONTROL_CLIENT="$(tmux -L "$TMUX_SOCKET" list-clients -F '#{client_name} #{client_control_mode}' | awk '$2 != "0" { print $1; exit }')"
  [[ -n "$CONTROL_CLIENT" ]] && break
  sleep 0.05
done
[[ -n "$CONTROL_CLIENT" ]]
tmux -L "$TMUX_SOCKET" refresh-client -t "$CONTROL_CLIENT" -A "$AGENT_PANE:on"
CONTROL_FLAGS=""
for _ in $(seq 1 50); do
  CONTROL_FLAGS="$(tmux -L "$TMUX_SOCKET" list-clients -F '#{client_name} #{client_flags}' | awk -v client="$CONTROL_CLIENT" '$1 == client { $1=""; sub(/^ /, ""); print; exit }')"
  [[ "$CONTROL_FLAGS" == *active-pane* ]] && break
  sleep 0.01
done
echo "control client flags: $CONTROL_FLAGS"
[[ -n "$CONTROL_FLAGS" ]]
grep -F 'control-mode' <<<"$CONTROL_FLAGS" >/dev/null
grep -F 'active-pane' <<<"$CONTROL_FLAGS" >/dev/null
sleep 1
wait_badge Done
echo "control/active-pane client did not acknowledge"
kill "$CONTROL_PID" 2>/dev/null || true
CONTROL_PID=""
mkfifo "$CLIENT_FIFO_3"
python3 "$PTY_CLIENT" "$CLIENT_FIFO_3" "$CLIENT_LOG_3" "$TMUX_SOCKET" main readonly &
CLIENT_PID_3=$!
exec 10>"$CLIENT_FIFO_3"
wait_badge Idle
echo "read-only client attach acknowledged"
kill "$CLIENT_PID_3" 2>/dev/null || true
CLIENT_PID_3=""

# The debug build is already warm before measurement. The wrapper uses bash's built-in `time`, so
# measuring foreground delivery does not add another process to the path under test.
python3 - "$HOOK_LOG" <<'PY'
import math, re, sys

durations = []
for line in open(sys.argv[1], encoding="utf-8"):
    match = re.match(r"duration=([0-9]+(?:\.[0-9]+)?) status=([0-9]+) ", line)
    assert match, line
    if " args= hooks pane-state-view " not in line:
        continue
    assert match.group(2) == "0", line
    durations.append(float(match.group(1)))
assert len(durations) >= 8, durations
durations.sort()
p95 = durations[math.ceil(len(durations) * 0.95) - 1]
maximum = durations[-1]
assert p95 <= 0.100, (p95, durations)
assert maximum <= 0.500, (maximum, durations)
print(f"foreground hook SLA ok: n={len(durations)} p95={p95 * 1000:.1f}ms max={maximum * 1000:.1f}ms")
PY

# Restore a hidden Done state for restart, display and cleanup checks.
tmux -L "$TMUX_SOCKET" select-pane -t "$OTHER_PANE"
VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status running --started-at "$((NOW + 10))"
VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status idle --completed-at "$((NOW + 11))"
wait_badge Done

# The linked agent contributes once to global/category counts, while each linked session and the
# shared window receive the same canonical Done count. Window pane_count is likewise deduplicated.
python3 - "$DAEMON_SOCKET" "$MAIN_SESSION_ID" "$AUX_SESSION_ID" "$WINDOW_ID" <<'PY'
import json, socket, sys

socket_path, main_id, aux_id, window_id = sys.argv[1:]

def query(context):
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(5)
    client.connect(socket_path)
    reader = client.makefile("rb")
    client.sendall(b'{"op":"hello","proto":4}\n')
    assert json.loads(reader.readline())["type"] == "hello_ack"
    request = {"op":"query_status_snapshot", "proto":4, "context":context}
    client.sendall(json.dumps(request, separators=(",", ":")).encode() + b"\n")
    response = json.loads(reader.readline())
    assert response["type"] == "status_snapshot_result", response
    return response["snapshot"]

global_status = query("global")
assert global_status["summary"]["done"] == 1, global_status
category = next(entry for entry in global_status["categories"] if entry["category"] == "smoke")
assert category["counts"]["done"] == 1, category
assert set(category["session_ids"]) == {main_id, aux_id}, category
global_linked = next(entry for entry in global_status["windows"] if entry["window_id"] == window_id)
assert global_linked["counts"]["done"] == 1, global_linked
assert global_linked["pane_count"] == 2, global_linked
assert set(global_linked["session_ids"]) == {main_id, aux_id}, global_linked

for session_id in (main_id, aux_id):
    status = query({"session":{"session_id":session_id}})
    session = next(entry for entry in status["sessions"] if entry["session_id"] == session_id)
    assert session["counts"]["done"] == 1, session
    linked = next(entry for entry in status["windows"] if entry["window_id"] == window_id)
    assert linked["counts"]["done"] == 1, linked
    assert linked["pane_count"] == 2, linked
    assert linked["session_ids"] == [session_id], linked
print("linked session/category/window canonical counts ok")
PY

# Exercise linked session/window context with the two long-lived normal clients.
tmux -L "$TMUX_SOCKET" switch-client -c "$CLIENT_1" -t '=main:'
tmux -L "$TMUX_SOCKET" select-window -t aux:linked
tmux -L "$TMUX_SOCKET" select-window -t aux:own
echo "linked active pane after session exercise: $(tmux -L "$TMUX_SOCKET" display-message -p -t main:linked '#{pane_id}')"
wait_badge Done

# Status format and every pushed display surface use only daemon-owned option values.
STATUS_LEFT="$(tmux -L "$TMUX_SOCKET" show-options -gv status-left)"
STATUS_RIGHT="$(tmux -L "$TMUX_SOCKET" show-options -gv status-right)"
PANE_BORDER="$(tmux -L "$TMUX_SOCKET" show-options -gv pane-border-format)"
[[ "$STATUS_LEFT$STATUS_RIGHT$PANE_BORDER" != *'#('* ]]
grep -F '#{@vde_status_' <<<"$STATUS_LEFT$STATUS_RIGHT$PANE_BORDER" >/dev/null

wait_display_options() {
  for _ in $(seq 1 80); do
    local summary sessions windows pane
    summary="$(tmux -L "$TMUX_SOCKET" show-options -v -t main @vde_status_summary 2>/dev/null || true)"
    sessions="$(tmux -L "$TMUX_SOCKET" show-options -v -t main @vde_status_sessions 2>/dev/null || true)"
    windows="$(tmux -L "$TMUX_SOCKET" show-options -v -t main @vde_status_windows 2>/dev/null || true)"
    pane="$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$AGENT_PANE" @vde_status_pane 2>/dev/null || true)"
    [[ "$summary" == *'✓ 1'* && -n "$sessions" && -n "$windows" && -n "$pane" ]] && return 0
    sleep 0.1
  done
  return 1
}
wait_display_options
OTHER_STATUS=""
OTHER_EXPECTED=""
for _ in $(seq 1 20); do
  OTHER_STATUS="$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$OTHER_PANE" @vde_status_pane)"
  OTHER_EXPECTED="$(VT_PANE="$OTHER_PANE" run_vt statusline-pane --target "$OTHER_PANE")"
  [[ "$OTHER_STATUS" == "$OTHER_EXPECTED" ]] && break
  sleep 0.05
done
echo "display nonagent pane: $OTHER_STATUS"
[[ -n "$OTHER_STATUS" && "$OTHER_STATUS" != *generic* ]]
[[ "$OTHER_STATUS" == "$OTHER_EXPECTED" ]]
MAIN_WINDOWS="$(tmux -L "$TMUX_SOCKET" show-options -v -t main @vde_status_windows)"
AUX_WINDOWS="$(tmux -L "$TMUX_SOCKET" show-options -v -t aux @vde_status_windows)"
echo "display main windows: $MAIN_WINDOWS"
echo "display aux windows: $AUX_WINDOWS"
grep -F linked <<<"$MAIN_WINDOWS" >/dev/null
grep -F own <<<"$AUX_WINDOWS" >/dev/null
[[ "$MAIN_WINDOWS" != "$AUX_WINDOWS" ]]
MAIN_SESSIONS="$(tmux -L "$TMUX_SOCKET" show-options -v -t main @vde_status_sessions)"
echo "display main sessions: $MAIN_SESSIONS"
grep -F main <<<"$MAIN_SESSIONS" >/dev/null
grep -F aux <<<"$MAIN_SESSIONS" >/dev/null
SUMMARY_VALUE="$(tmux -L "$TMUX_SOCKET" show-options -v -t main @vde_status_summary)"
echo "display summary: $SUMMARY_VALUE"
for token in '▲ 0' '● 0' '✓ 1' '○ 0'; do
  grep -F "$token" <<<"$SUMMARY_VALUE" >/dev/null
done
CLIENT_ATTACHMENTS="$(tmux -L "$TMUX_SOCKET" list-clients -F '#{client_name} #{client_session}')"
grep -F "$CLIENT_1 main" <<<"$CLIENT_ATTACHMENTS" >/dev/null
grep -F "$CLIENT_2 aux" <<<"$CLIENT_ATTACHMENTS" >/dev/null
CLIENT_1_WINDOWS="$(tmux -L "$TMUX_SOCKET" display-message -p -c "$CLIENT_1" -t main: '#{@vde_status_windows}')"
CLIENT_2_WINDOWS="$(tmux -L "$TMUX_SOCKET" display-message -p -c "$CLIENT_2" -t aux: '#{@vde_status_windows}')"
echo "client1 expanded windows: $CLIENT_1_WINDOWS"
echo "client2 expanded windows: $CLIENT_2_WINDOWS"
[[ "$CLIENT_1_WINDOWS" == "$MAIN_WINDOWS" ]]
[[ "$CLIENT_2_WINDOWS" == "$AUX_WINDOWS" ]]

for surface in \
  "statusline-summary" \
  "statusline-attention --session-id $MAIN_SESSION_ID" \
  "statusline-category --session-id $MAIN_SESSION_ID" \
  "statusline-sessions --session-id $MAIN_SESSION_ID" \
  "statusline-windows --session-id $MAIN_SESSION_ID" \
  "statusline-pane --target $AGENT_PANE"; do
  echo "checking CLI display surface: $surface"
  # shellcheck disable=SC2086
  run_vt $surface >/dev/null
done

# A production-sized topology must keep the status push argv small. The daemon writes every scope
# through one guarded `source-file` tmux client process, even when the command body exceeds tmux's
# direct command-length limit.
: >"$TMUX_PROCESS_LOG"
LARGE_WINDOWS=()
LAST_LARGE_PANE=""
LARGE_TARGET_PANES=58
BASELINE_PANE_COUNT="$(tmux -L "$TMUX_SOCKET" list-panes -a -F '#{pane_id}' | sort -u | wc -l | tr -d ' ')"
LARGE_WINDOW_COUNT="$((LARGE_TARGET_PANES - BASELINE_PANE_COUNT))"
[[ "$LARGE_WINDOW_COUNT" -gt 0 ]]
for index in $(seq 1 "$LARGE_WINDOW_COUNT"); do
  large_window="$(tmux -L "$TMUX_SOCKET" new-window -d -P -F '#{window_id}' -t aux: \
    -n "load-$index" "sleep 600")"
  LARGE_WINDOWS+=("$large_window")
  LAST_LARGE_PANE="$(tmux -L "$TMUX_SOCKET" display-message -p -t "$large_window" '#{pane_id}')"
done
[[ "$(tmux -L "$TMUX_SOCKET" list-panes -a -F '#{pane_id}' | sort -u | wc -l | tr -d ' ')" == "$LARGE_TARGET_PANES" ]]
for _ in $(seq 1 120); do
  LARGE_PANE_STATUS="$(tmux -L "$TMUX_SOCKET" show-options -pqv -t "$LAST_LARGE_PANE" \
    @vde_status_pane 2>/dev/null || true)"
  LARGE_WINDOWS_STATUS="$(tmux -L "$TMUX_SOCKET" show-options -qv -t aux \
    @vde_status_windows 2>/dev/null || true)"
  [[ -n "$LARGE_PANE_STATUS" && "$LARGE_WINDOWS_STATUS" == *'+'* ]] && break
  sleep 0.1
done
[[ -n "$LARGE_PANE_STATUS" && "$LARGE_WINDOWS_STATUS" == *'+'* ]]
[[ "$LARGE_WINDOWS_STATUS" != *"load-$LARGE_WINDOW_COUNT"* ]]
STATUS_SOURCE_PROCESSES="$(grep -c 'status-batches.*source-file\|source-file.*status-batches' \
  "$TMUX_PROCESS_LOG" || true)"
[[ "$STATUS_SOURCE_PROCESSES" -ge 1 ]]
if grep 'status-batches' "$TMUX_PROCESS_LOG" | grep -F '@vde_status_pane' >/dev/null; then
  echo "file-backed status push leaked pane payload into tmux argv" >&2
  exit 1
fi
STATUS_BATCH_DIR="$V2_SOCKET_ROOT/$SERVER_HASH.status-batches"
[[ -d "$STATUS_BATCH_DIR" && "$(find "$STATUS_BATCH_DIR" -type f | wc -l | tr -d ' ')" == 0 ]]
echo "large status projection ok: $LARGE_TARGET_PANES panes, guarded source-file, 1 process/batch"
for large_window in "${LARGE_WINDOWS[@]}"; do
  tmux -L "$TMUX_SOCKET" kill-window -t "$large_window"
done

sidebar_snapshot() {
  VT_PANE="$OTHER_PANE" run_vt sidebar attach --once |
    sed -E \
      -e 's/[0-9]+s ago/<elapsed>/g' \
      -e 's/ +done <elapsed>/ done <elapsed>/g'
}

echo "checking sidebar snapshot surface"
VT_PANE="$OTHER_PANE" run_vt sidebar attach --once >/dev/null
sleep 6
SIDEBAR_BEFORE="$(sidebar_snapshot)"
[[ -n "$SIDEBAR_BEFORE" ]]

# Restart must hydrate canonical state and reproduce every display surface and sidebar projection.
capture_display_surface() {
  local value
  for session in main aux; do
    for option in @vde_status_summary @vde_status_category @vde_status_sessions @vde_status_windows @vde_status_attention; do
      value="$(tmux -L "$TMUX_SOCKET" show-options -v -t "$session" "$option" 2>/dev/null || true)"
      printf '%s\037' "$value"
    done
  done
  for pane in "$AGENT_PANE" "$OTHER_PANE"; do
    value="$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$pane" @vde_status_pane 2>/dev/null || true)"
    printf '%s\037' "$value"
  done
}

capture_stable_display_surface() {
  capture_display_surface | python3 -c '
import re, sys
value = sys.stdin.read()
duration = r"(?<![A-Za-z0-9])(?:[0-9]+d|[0-9]+h[0-9]+m|[0-9]+m(?:[0-9]{2}s)?|[0-9]+s)(?![A-Za-z0-9])"
sys.stdout.write(re.sub(duration, "<elapsed>", value))
'
}

sleep 2
DISPLAY_BEFORE="$(capture_stable_display_surface)"
run_vt daemon restart >/dev/null
record_daemon_pid
for _ in $(seq 1 80); do
  DISPLAY_AFTER="$(capture_stable_display_surface)"
  [[ "$DISPLAY_AFTER" == "$DISPLAY_BEFORE" ]] && break
  sleep 0.1
done
if [[ "$DISPLAY_AFTER" != "$DISPLAY_BEFORE" ]]; then
  echo "display restart mismatch" >&2
  echo "before=$DISPLAY_BEFORE" >&2
  echo "after=$DISPLAY_AFTER" >&2
  exit 1
fi
SIDEBAR_AFTER=""
for _ in $(seq 1 80); do
  SIDEBAR_AFTER="$(sidebar_snapshot)"
  [[ "$SIDEBAR_AFTER" == "$SIDEBAR_BEFORE" ]] && break
  sleep 0.25
done
if [[ "$SIDEBAR_AFTER" != "$SIDEBAR_BEFORE" ]]; then
  echo "sidebar restart mismatch" >&2
  echo "before=$SIDEBAR_BEFORE" >&2
  echo "after=$SIDEBAR_AFTER" >&2
  exit 1
fi
wait_badge Done
echo "daemon restart parity ok"

# Deliberately drop foreground view hooks. A still-visible pane must reconcile to Idle on the next
# poll, while a pane focused out before completion must remain Done. Restart restores owned hooks.
run_vt pane-state hooks uninstall >/dev/null
printf '\002A' >&7
wait_badge Idle
printf '\002O' >&7
VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status running --started-at "$((NOW + 12))"
VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status idle --completed-at "$((NOW + 13))"
sleep 1
wait_badge Done
run_vt daemon restart >/dev/null
record_daemon_pid
for hook in window-pane-changed session-window-changed client-session-changed client-attached client-detached; do
  tmux -L "$TMUX_SOCKET" show-hooks -g "${hook}[70]" | grep -F "${hook}[70]" >/dev/null
done
echo "dropped-view reconciliation ok"

# Window scope uses only the pane membership frozen into the occurrence. Pane C is already Done in
# canonical state but is joined after the A/B snapshot; moving the witnessing client away before
# the daemon resumes also prevents periodic reconciliation from obscuring this event-level proof.
tmux -L "$TMUX_SOCKET" new-session -d -s late -n source "sleep 600"
LATE_PANE="$(tmux -L "$TMUX_SOCKET" display-message -p -t late:source '#{pane_id}')"
cat >"$CONFIG_HOME/vde/tmux/config.yml" <<'YAML'
categories:
  default_category: smoke
daemon:
  done_clear_on: window
  poll_ms: 60000
YAML
run_vt daemon restart >/dev/null
record_daemon_pid
run_vt pane-state hooks uninstall >/dev/null
tmux -L "$TMUX_SOCKET" switch-client -c "$CLIENT_1" -t '=aux:'
tmux -L "$TMUX_SOCKET" switch-client -c "$CLIENT_2" -t '=aux:'
tmux -L "$TMUX_SOCKET" select-window -t aux:own

VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status running --started-at "$((NOW + 14))"
VT_PANE="$AGENT_PANE" run_vt hook emit --agent generic --session-id smoke-session \
  --status idle --completed-at "$((NOW + 15))"
wait_badge Done
echo "window occurrence pane A prepared"
VT_PANE="$OTHER_PANE" run_vt hook emit --agent generic --session-id smoke-window-b \
  --status running --started-at "$((NOW + 16))"
VT_PANE="$OTHER_PANE" run_vt hook emit --agent generic --session-id smoke-window-b \
  --status idle --completed-at "$((NOW + 17))"
wait_pane_badge "$OTHER_PANE" Done
echo "window occurrence pane B prepared"
VT_PANE="$LATE_PANE" run_vt hook emit --agent generic --session-id smoke-window-c \
  --status running --started-at "$((NOW + 18))"
VT_PANE="$LATE_PANE" run_vt hook emit --agent generic --session-id smoke-window-c \
  --status idle --completed-at "$((NOW + 19))"
wait_pane_badge "$LATE_PANE" Done
echo "post-occurrence pane C prepared"

STOPPED_DAEMON_PID="$(lsof -t "$DAEMON_SOCKET" 2>/dev/null | head -n 1)"
[[ -n "$STOPPED_DAEMON_PID" ]]
kill -STOP "$STOPPED_DAEMON_PID"
tmux -L "$TMUX_SOCKET" select-window -t main:linked
tmux -L "$TMUX_SOCKET" select-pane -t "$AGENT_PANE"
tmux -L "$TMUX_SOCKET" switch-client -c "$CLIENT_1" -t '=main:'

WINDOW_SNAPSHOT_SESSION="$(tmux -L "$TMUX_SOCKET" display-message -p -c "$CLIENT_1" '#{session_id}')"
WINDOW_SNAPSHOT_WINDOW="$(tmux -L "$TMUX_SOCKET" display-message -p -c "$CLIENT_1" '#{window_id}')"
WINDOW_SNAPSHOT_PANE="$(tmux -L "$TMUX_SOCKET" display-message -p -c "$CLIENT_1" '#{pane_id}')"
WINDOW_SNAPSHOT_PANE_PID="$(tmux -L "$TMUX_SOCKET" display-message -p -c "$CLIENT_1" '#{pane_pid}')"
WINDOW_SNAPSHOT_PANES="$(tmux -L "$TMUX_SOCKET" display-message -p -t "$WINDOW_ID" '#{P:#{pane_id}__vde_hook_pane_field_v2__#{pane_pid}__vde_hook_pane_row_v2__,#{pane_id}__vde_hook_pane_field_v2__#{pane_pid}__vde_hook_pane_row_v2__}')"
WINDOW_SNAPSHOT_CLIENTS="$(tmux -L "$TMUX_SOCKET" display-message -p -c "$CLIENT_1" '#{L:#{S:#{?#{==:#{client_session},#{session_name}},#{client_pid}__vde_hook_client_field_v2__#{session_id}__vde_hook_client_field_v2__#{window_id}__vde_hook_client_field_v2__#{pane_id}__vde_hook_client_field_v2__#{pane_pid}__vde_hook_client_field_v2__#{client_control_mode}__vde_hook_client_field_v2__#{client_flags}__vde_hook_client_row_v2__,}}}')"
WINDOW_SOURCE_CLIENT_PID="$(tmux -L "$TMUX_SOCKET" display-message -p -c "$CLIENT_1" '#{client_pid}')"
[[ "$WINDOW_SNAPSHOT_PANES" == *"$AGENT_PANE"* && "$WINDOW_SNAPSHOT_PANES" == *"$OTHER_PANE"* ]]
[[ "$WINDOW_SNAPSHOT_PANES" != *"$LATE_PANE"* ]]

run_vt hooks pane-state-view window-pane-changed \
  --owner vde-tmux-pane-state --protocol 2 --hook-window="$WINDOW_ID" \
  --snapshot-session="$WINDOW_SNAPSHOT_SESSION" --snapshot-window="$WINDOW_SNAPSHOT_WINDOW" \
  --snapshot-pane="$WINDOW_SNAPSHOT_PANE" --snapshot-pane-pid="$WINDOW_SNAPSHOT_PANE_PID" \
  --snapshot-panes="$WINDOW_SNAPSHOT_PANES" --snapshot-clients="$WINDOW_SNAPSHOT_CLIENTS" \
  --hook-client="$WINDOW_SOURCE_CLIENT_PID" >"$RUNTIME_DIR/window-occurrence.log" 2>&1 &
HOOK_CLI_PID=$!
sleep 0.05
tmux -L "$TMUX_SOCKET" join-pane -d -s "$LATE_PANE" -t "$WINDOW_ID"
tmux -L "$TMUX_SOCKET" switch-client -c "$CLIENT_1" -t '=aux:'
kill -CONT "$STOPPED_DAEMON_PID"
STOPPED_DAEMON_PID=""
wait "$HOOK_CLI_PID"
HOOK_CLI_PID=""
wait_pane_badge "$AGENT_PANE" Idle
wait_pane_badge "$OTHER_PANE" Idle
wait_pane_badge "$LATE_PANE" Done
echo "window occurrence excludes pane joined after immutable snapshot"

cat >"$CONFIG_HOME/vde/tmux/config.yml" <<'YAML'
categories:
  default_category: smoke
daemon:
  done_clear_on: pane
YAML

# Cleanup precedes reset. Exactly the fixed legacy keys are removed; canonical, display and
# unrelated category/sidebar options survive. Reset then replaces canonical Active with a tombstone.
PANE_LEGACY_KEYS=(
  @vde_agent @vde_status @vde_prompt @vde_prompt_source @vde_wait_reason @vde_attention
  @vde_started_at @vde_completed_at @vde_tasks @vde_task_items @vde_task_item_ids
  @vde_subagents @vde_worktree_activity
)
SESSION_LEGACY_KEYS=(@vde_session_status @vde_session_state @vde_session_agent_counts)
WINDOW_LEGACY_KEYS=(@vde_window_status @vde_window_state @vde_window_agent_counts)
for key in "${PANE_LEGACY_KEYS[@]}"; do
  tmux -L "$TMUX_SOCKET" set-option -p -t "$AGENT_PANE" "$key" legacy
done
for key in "${SESSION_LEGACY_KEYS[@]}"; do
  tmux -L "$TMUX_SOCKET" set-option -t main "$key" legacy
done
for key in "${WINDOW_LEGACY_KEYS[@]}"; do
  tmux -L "$TMUX_SOCKET" set-option -w -t "$WINDOW_ID" "$key" legacy
done
tmux -L "$TMUX_SOCKET" set-option -p -t "$AGENT_PANE" @vde_sidebar 1
tmux -L "$TMUX_SOCKET" set-option -t main @vde_category smoke-category
CANONICAL_BEFORE="$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$AGENT_PANE" @vde_pane_state)"
DISPLAY_PANE_BEFORE="$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$AGENT_PANE" @vde_status_pane)"
run_vt pane-state cleanup-legacy --all >/dev/null
for key in "${PANE_LEGACY_KEYS[@]}"; do
  [[ -z "$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$AGENT_PANE" "$key" 2>/dev/null || true)" ]]
done
for key in "${SESSION_LEGACY_KEYS[@]}"; do
  [[ -z "$(tmux -L "$TMUX_SOCKET" show-options -v -t main "$key" 2>/dev/null || true)" ]]
done
for key in "${WINDOW_LEGACY_KEYS[@]}"; do
  [[ -z "$(tmux -L "$TMUX_SOCKET" show-options -wv -t "$WINDOW_ID" "$key" 2>/dev/null || true)" ]]
done
[[ "$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$AGENT_PANE" @vde_pane_state)" == "$CANONICAL_BEFORE" ]]
DISPLAY_PANE_AFTER="$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$AGENT_PANE" @vde_status_pane)"
# Cleanup preserves the display option itself, while the live daemon may legitimately refresh its
# rendered value from the unchanged canonical state during this assertion window.
[[ -n "$DISPLAY_PANE_BEFORE" && -n "$DISPLAY_PANE_AFTER" && "$DISPLAY_PANE_AFTER" == *"$AGENT_PANE"* ]]
[[ "$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$AGENT_PANE" @vde_sidebar)" == 1 ]]
[[ "$(tmux -L "$TMUX_SOCKET" show-options -v -t main @vde_category)" == smoke-category ]]

run_vt pane-state reset --target "$AGENT_PANE" >/dev/null
CANONICAL_AFTER="$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$AGENT_PANE" @vde_pane_state)"
[[ -n "$CANONICAL_AFTER" && "$CANONICAL_AFTER" != "$CANONICAL_BEFORE" ]]
grep -F 'reset' <<<"$CANONICAL_AFTER" >/dev/null
[[ -z "$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$AGENT_PANE" @vde_agent 2>/dev/null || true)" ]]
[[ "$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$AGENT_PANE" @vde_sidebar)" == 1 ]]
echo "legacy cleanup ordering and reset preservation ok"

# Recreate the scratch tmux server at the same socket path. Ask the old daemon to mutate legacy
# state if it is still reachable; its server-side incarnation/PID guards must leave the new pane's
# sentinel untouched, and the config run-shell must start a distinct Serving daemon generation.
OLD_DAEMON_SOCKET="$DAEMON_SOCKET"
OLD_DAEMON_PID="$(lsof -t "$OLD_DAEMON_SOCKET" 2>/dev/null | head -n 1 || true)"
[[ -n "$OLD_DAEMON_PID" ]]
OLD_WRITE_PANE="$AGENT_PANE"
OLD_WRITE_PANE_PID="$(tmux -L "$TMUX_SOCKET" display-message -p -t "$OLD_WRITE_PANE" '#{pane_pid}')"
OLD_MUTATION_RESULT="$RUNTIME_DIR/old-mutation-result.log"
OLD_MUTATION_TRIGGER="$RUNTIME_DIR/old-mutation-trigger"
python3 - "$OLD_DAEMON_SOCKET" "$OLD_WRITE_PANE" "$OLD_WRITE_PANE_PID" "$NOW" "$OLD_MUTATION_TRIGGER" >"$OLD_MUTATION_RESULT" <<'PY' &
import json, os, socket, sys, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(5)
s.connect(sys.argv[1])
s.sendall(b'{"op":"hello","proto":4}\n')
reader = s.makefile("rb")
hello_line = reader.readline()
assert hello_line, "old daemon closed before persistent Hello response"
hello = json.loads(hello_line)
assert hello["type"] == "hello_ack", hello
print("hello_ack", flush=True)
while not os.path.exists(sys.argv[5]):
    time.sleep(0.005)
request = {
    "op":"submit_pane_event", "proto":4,
    "envelope": {
    "daemon_instance_id":hello["daemon_instance_id"],
    "event_id":"00112233445566778899aabbccddeeff",
    "pane_instance":{"pane_id":sys.argv[2], "pane_pid":int(sys.argv[3])},
    "agent":"generic",
    "agent_session_id":"incarnation-guard-smoke",
    "event":{"type":"begin_run", "data":{"started_at":int(sys.argv[4]) + 100, "prompt":None}},
    },
}
s.sendall(json.dumps(request, separators=(",", ":")).encode() + b"\n")
print("mutation_attempt_sent", flush=True)
mutation_line = reader.readline()
if mutation_line:
    mutation = json.loads(mutation_line)
    print("mutation_response=" + json.dumps(mutation, sort_keys=True), flush=True)
else:
    print("mutation_response=eof_after_fail_stop", flush=True)
PY
OLD_MUTATION_PID=$!
for _ in $(seq 1 100); do
  grep -F 'hello_ack' "$OLD_MUTATION_RESULT" >/dev/null 2>&1 && break
  sleep 0.01
done
grep -F 'hello_ack' "$OLD_MUTATION_RESULT" >/dev/null
kill -STOP "$OLD_DAEMON_PID"
STOPPED_DAEMON_PID="$OLD_DAEMON_PID"
echo "old daemon paused with a handshaken mutation connection: $OLD_DAEMON_PID"
: >"$OLD_MUTATION_TRIGGER"
for _ in $(seq 1 20); do
  grep -F 'mutation_attempt_sent' "$OLD_MUTATION_RESULT" >/dev/null 2>&1 && break
  sleep 0.01
done
grep -F 'mutation_attempt_sent' "$OLD_MUTATION_RESULT" >/dev/null
echo "old mutation bytes queued while daemon is paused"
OLD_TMUX_PID="$TMUX_PID"
tmux -L "$TMUX_SOCKET" kill-server
for _ in $(seq 1 50); do
  if ! kill -0 "$OLD_TMUX_PID" 2>/dev/null && [[ ! -S "$TMUX_PATH" ]]; then
    break
  fi
  sleep 0.05
done
RECREATE_CONF="$RUNTIME_DIR/recreate.conf"
cat >"$RECREATE_CONF" <<'EOF'
set -g exit-empty off
EOF
PATH="$HOOK_BIN_DIR:$ROOT/target/debug:$PATH" HOME="$HOME_DIR" XDG_CONFIG_HOME="$CONFIG_HOME" \
  XDG_STATE_HOME="$STATE_HOME" tmux -L "$TMUX_SOCKET" -f "$RECREATE_CONF" start-server
echo "same socket path scratch server recreated without starting new daemon"
TMUX_PATH="$(tmux -L "$TMUX_SOCKET" display-message -p '#{socket_path}')"
TMUX_PID="$(tmux -L "$TMUX_SOCKET" display-message -p '#{pid}')"
TMUX_START_TIME="$(tmux -L "$TMUX_SOCKET" display-message -p '#{start_time}')"
TMUX_ENV="$TMUX_PATH,$TMUX_PID,0"
SERVER_HASH="$(python3 - "$TMUX_PATH" "$TMUX_PID" "$TMUX_START_TIME" <<'PY'
import hashlib, os, sys
digest = hashlib.sha256()
for index, value in enumerate((os.path.realpath(sys.argv[1]), sys.argv[2], sys.argv[3])):
    if index:
        digest.update(b"\0")
    digest.update(value.encode())
print(digest.hexdigest())
PY
)"
DAEMON_SOCKET="$V2_SOCKET_ROOT/$SERVER_HASH.sock"
tmux -L "$TMUX_SOCKET" new-session -d -s recreated -n new "sleep 600"
NEW_PANE="$(tmux -L "$TMUX_SOCKET" display-message -p -t recreated:new '#{pane_id}')"
NEW_PANE_PID="$(tmux -L "$TMUX_SOCKET" display-message -p -t "$NEW_PANE" '#{pane_pid}')"
echo "recreated pane identity: old=$OLD_WRITE_PANE/$OLD_WRITE_PANE_PID new=$NEW_PANE/$NEW_PANE_PID"
[[ "$NEW_PANE" == "$OLD_WRITE_PANE" && "$NEW_PANE_PID" != "$OLD_WRITE_PANE_PID" ]]
tmux -L "$TMUX_SOCKET" set-option -p -t "$NEW_PANE" @vde_pane_state incarnation-sentinel
tmux -L "$TMUX_SOCKET" set-option -g @vde_status_summary incarnation-summary-sentinel
tmux -L "$TMUX_SOCKET" set-option -p -t "$NEW_PANE" @vde_status_pane incarnation-pane-display-sentinel

echo "old mutation remained queued through same-socket recreation"
kill -CONT "$OLD_DAEMON_PID"
STOPPED_DAEMON_PID=""
wait "$OLD_MUTATION_PID"
cat "$OLD_MUTATION_RESULT"
grep -F 'hello_ack' "$OLD_MUTATION_RESULT" >/dev/null
grep -F 'mutation_response=' "$OLD_MUTATION_RESULT" >/dev/null
if ! grep -Eq 'tmux server (incarnation changed|identity mismatch)|mutation_response=eof_after_fail_stop' "$OLD_MUTATION_RESULT"; then
  echo "old daemon did not report a recognized fail-stop outcome" >&2
  exit 1
fi
sleep 1
[[ "$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$NEW_PANE" @vde_pane_state)" == incarnation-sentinel ]]
[[ "$(tmux -L "$TMUX_SOCKET" show-options -gv @vde_status_summary)" == incarnation-summary-sentinel ]]
[[ "$(tmux -L "$TMUX_SOCKET" show-options -pv -t "$NEW_PANE" @vde_status_pane)" == incarnation-pane-display-sentinel ]]
echo "new-server canonical and display sentinels survived old writer attempt"
if kill -0 "$OLD_DAEMON_PID" 2>/dev/null; then
  echo "old daemon survived same-socket server recreation" >&2
  exit 1
fi
PATH="$HOOK_BIN_DIR:$ROOT/target/debug:$PATH" HOME="$HOME_DIR" XDG_CONFIG_HOME="$CONFIG_HOME" \
  XDG_STATE_HOME="$STATE_HOME" tmux -L "$TMUX_SOCKET" source-file "$TMUX_CONF"
for _ in $(seq 1 80); do
  [[ -S "$DAEMON_SOCKET" ]] && break
  sleep 0.1
done
record_daemon_pid
python3 - "$DAEMON_SOCKET" <<'PY'
import json, socket, sys, time
deadline = time.time() + 8
while True:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(5)
    s.connect(sys.argv[1])
    s.sendall(b'{"op":"hello","proto":4}\n')
    reply = json.loads(s.makefile("rb").readline())
    assert reply["type"] == "hello_ack", reply
    if reply["phase"] == "serving":
        break
    assert time.time() < deadline, reply
    s.close()
    time.sleep(0.05)
PY
echo "same-socket incarnation guard ok"

echo "pane-state v2 scratch smoke ok"
