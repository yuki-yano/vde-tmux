#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${VDE_VT_BIN:-$ROOT/target/debug/vt}"
SYSTEM_TMUX="$(command -v tmux)"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)-$$-$RANDOM"
ARTIFACT_DIR="$ROOT/target/preflight/$STAMP"
SANDBOX="$ARTIFACT_DIR/sandbox"
STATE_HOME="$SANDBOX/state"
CONFIG_HOME="$SANDBOX/config"
HOME_DIR="$SANDBOX/home"
TMUX_CONF="$SANDBOX/tmux.conf"
NOTIFY_BIN="$SANDBOX/notify-recorder.sh"
NOTIFY_LOG="$ARTIFACT_DIR/notification.log"
SUMMARY="$ARTIFACT_DIR/summary.tsv"
PTY_CLIENT="$SANDBOX/pty-client.py"
QUERY_JSON="$ARTIFACT_DIR/resolved-snapshot.json"
CLIENT_LOG_1="$ARTIFACT_DIR/client-1.ansi"
CLIENT_LOG_2="$ARTIFACT_DIR/client-2.ansi"
ACTION_LOG="$ARTIFACT_DIR/session-actions.log"
CLIENT_FIFO_1="$SANDBOX/client-1.in"
CLIENT_FIFO_2="$SANDBOX/client-2.in"
TMUX_SOCKET="vde-ui-preflight-$STAMP"
RUNTIME_SOCKET_ROOT="/tmp/vt-$(id -u)/v2"
PROTOCOL_VERSION="${VDE_PROTOCOL_VERSION:-$(sed -n 's/^pub const PROTOCOL_VERSION: u16 = \([0-9][0-9]*\);/\1/p' "$ROOT/src/daemon/protocol/v2.rs")}"
FAILED_LINE=""
TMUX_ENV=""
DAEMON_SOCKET=""
CLIENT_PID_1=""
CLIENT_PID_2=""
CONTROL_PID=""

mkdir -p "$ARTIFACT_DIR" "$STATE_HOME" "$CONFIG_HOME/vde/tmux" "$HOME_DIR"
chmod 700 "$ARTIFACT_DIR" "$SANDBOX" "$STATE_HOME" "$CONFIG_HOME" "$HOME_DIR"
: >"$SUMMARY"
: >"$NOTIFY_LOG"
: >"$ACTION_LOG"

record() {
  printf '%s\t%s\n' "$1" "$2" | tee -a "$SUMMARY"
}

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  return 1
}

on_error() {
  FAILED_LINE="$1"
}
trap 'on_error "$LINENO"' ERR

cleanup() {
  local exit_code=$?
  trap - ERR
  set +e
  if [[ -n "$TMUX_ENV" && -x "$BIN" ]]; then
    env TMUX="$TMUX_ENV" TMUX_PANE="" VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" \
      XDG_STATE_HOME="$STATE_HOME" XDG_CONFIG_HOME="$CONFIG_HOME" HOME="$HOME_DIR" \
      "$BIN" daemon stop >"$ARTIFACT_DIR/cleanup-daemon.log" 2>&1
  fi
  for pid in "$CONTROL_PID" "$CLIENT_PID_1" "$CLIENT_PID_2"; do
    if [[ -n "$pid" ]]; then
      kill "$pid" 2>/dev/null
    fi
  done
  "$SYSTEM_TMUX" -L "$TMUX_SOCKET" kill-server >/dev/null 2>&1
  for pid in "$CONTROL_PID" "$CLIENT_PID_1" "$CLIENT_PID_2"; do
    if [[ -n "$pid" ]]; then
      wait "$pid" 2>/dev/null
    fi
  done
  if [[ "$exit_code" -eq 0 ]]; then
    printf 'PASS: UI/UX preflight artifacts: %s\n' "$ARTIFACT_DIR"
  else
    printf 'FAIL: UI/UX preflight stopped at line %s; artifacts preserved: %s\n' \
      "${FAILED_LINE:-unknown}" "$ARTIFACT_DIR" >&2
  fi
  exit "$exit_code"
}
trap cleanup EXIT

tmux_cmd() {
  "$SYSTEM_TMUX" -L "$TMUX_SOCKET" "$@"
}

run_vt() {
  local pane="${VT_PANE:-${AGENT_PANE:-}}"
  env TMUX="$TMUX_ENV" TMUX_PANE="$pane" VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" \
    XDG_STATE_HOME="$STATE_HOME" XDG_CONFIG_HOME="$CONFIG_HOME" HOME="$HOME_DIR" \
    "$BIN" "$@"
}

daemon_pid() {
  lsof -t "$DAEMON_SOCKET" 2>/dev/null | head -n 1 || true
}

wait_daemon_running() {
  local output=""
  for _ in $(seq 1 100); do
    output="$(run_vt daemon status 2>/dev/null || true)"
    if grep -F 'daemon: running' <<<"$output" >/dev/null \
      && grep -F 'phase: Serving' <<<"$output" >/dev/null; then
      printf '%s\n' "$output"
      return 0
    fi
    sleep 0.1
  done
  printf '%s\n' "$output" >&2
  return 1
}

wait_daemon_stopped() {
  for _ in $(seq 1 60); do
    if [[ ! -S "$DAEMON_SOCKET" && -z "$(daemon_pid)" ]]; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

query_snapshot() {
  python3 - "$DAEMON_SOCKET" "$QUERY_JSON" "$PROTOCOL_VERSION" <<'PY'
import json, socket, sys
path, output, protocol = sys.argv[1:]
protocol = int(protocol)
client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
client.settimeout(5)
client.connect(path)
reader = client.makefile("rb")
client.sendall((json.dumps({"op": "hello", "proto": protocol}) + "\n").encode())
hello = json.loads(reader.readline())
assert hello["type"] == "hello_ack" and hello["phase"] == "serving", hello
client.sendall((json.dumps({"op": "query_resolved_snapshot", "proto": protocol}) + "\n").encode())
reply = json.loads(reader.readline())
assert reply["type"] == "resolved_snapshot_result", reply
with open(output, "w", encoding="utf-8") as handle:
    json.dump(reply, handle, ensure_ascii=False, indent=2)
PY
}

snapshot_revision() {
  query_snapshot
  python3 - "$QUERY_JSON" <<'PY'
import json, sys
print(json.load(open(sys.argv[1], encoding="utf-8"))["snapshot"]["snapshot_revision"])
PY
}

stable_snapshot_revision() {
  local previous=""
  local current=""
  local matches=0
  for _ in $(seq 1 40); do
    current="$(snapshot_revision)"
    if [[ -n "$previous" && "$current" == "$previous" ]]; then
      matches=$((matches + 1))
      if [[ "$matches" -ge 3 ]]; then
        printf '%s\n' "$current"
        return 0
      fi
    else
      matches=0
    fi
    previous="$current"
    sleep 0.1
  done
  return 1
}

wait_badge() {
  local pane="$1"
  local expected="$2"
  for _ in $(seq 1 100); do
    query_snapshot
    if python3 - "$QUERY_JSON" "$pane" "$expected" 2>/dev/null <<'PY'
import json, sys
reply = json.load(open(sys.argv[1], encoding="utf-8"))
pane = next((p for p in reply["snapshot"]["panes"] if p["pane_instance"]["pane_id"] == sys.argv[2]), None)
assert pane and pane.get("resolved") and pane["resolved"]["badge"] == sys.argv[3], pane
PY
    then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

wait_attention() {
  local _pane="$1"
  local expected="$2"
  local value=""
  for _ in $(seq 1 100); do
    value="$(tmux_cmd show-options -qv -t A @vde_status_attention 2>/dev/null || true)"
    if [[ "$expected" == present && -n "$value" ]]; then
      printf '%s\n' "$value"
      return 0
    fi
    if [[ "$expected" == absent && -z "$value" ]]; then
      printf '%s\n' "$value"
      return 0
    fi
    sleep 0.1
  done
  printf 'attention=%s\n' "$value" >&2
  return 1
}

wait_notification_count() {
  local expected="$1"
  for _ in $(seq 1 60); do
    if [[ "$(wc -l <"$NOTIFY_LOG" | tr -d ' ')" -ge "$expected" ]]; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

client_field() {
  local client="$1"
  local field="$2"
  local sep=$'\x1f'
  tmux_cmd list-clients -F "#{client_name}${sep}#{${field}}" 2>/dev/null \
    | awk -F "$sep" -v client="$client" '$1 == client { print $2; exit }'
}

wait_client_session() {
  local client="$1"
  local expected="$2"
  for _ in $(seq 1 60); do
    if [[ "$(client_field "$client" session_name || true)" == "$expected" ]]; then
      return 0
    fi
    sleep 0.05
  done
  return 1
}

wait_sidebar() {
  local window="$1"
  local pane=""
  for _ in $(seq 1 100); do
    pane="$(tmux_cmd list-panes -t "$window" -F '#{pane_id} #{@vde_sidebar}' \
      | awk '$2 == "1" { print $1; exit }')"
    if [[ -n "$pane" ]] && tmux_cmd capture-pane -p -t "$pane" | grep -v '^$' >/dev/null; then
      printf '%s\n' "$pane"
      return 0
    fi
    sleep 0.1
  done
  return 1
}

capture_sidebar_normalized() {
  local pane="$1"
  local output="$2"
  tmux_cmd capture-pane -ep -t "$pane" >"$output.ansi"
  python3 - "$output.ansi" "$output" <<'PY'
import re, sys
raw = open(sys.argv[1], "rb").read().decode("utf-8", "replace")
raw = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", raw)
raw = re.sub(r"\b\d+m\d{2}s\b", "<age>", raw)
raw = re.sub(r"\b\d+[smhd]\b", "<age>", raw)
raw = re.sub(r"\b\d+h\d+m\b", "<age>", raw)
open(sys.argv[2], "w", encoding="utf-8").write(raw.rstrip() + "\n")
PY
}

fingerprint() {
  shasum -a 256 "$1" | awk '{print $1}'
}

metadata_fingerprint() {
  {
    tmux_cmd show-hooks -g
    tmux_cmd show-options -g | grep -v '^@vde_status_' || true
    tmux_cmd show-options -gw | grep -v '^@vde_status_' || true
    tmux_cmd list-sessions -F '#{session_id} #{session_name} #{@vde_category}'
    tmux_cmd list-windows -a -F '#{window_id} #{window_name}'
    tmux_cmd list-panes -a -F '#{pane_id} #{@vde_pane_state} #{@vde_sidebar}'
  } | shasum -a 256 | awk '{print $1}'
}

cat >"$NOTIFY_BIN" <<EOF
#!/usr/bin/env bash
printf '%s\t%s\t%s\n' "\${VDE_PANE_ID:-}" "\${VDE_AGENT:-}" "\${VDE_BADGE_STATE:-}" >>"$NOTIFY_LOG"
EOF
chmod 700 "$NOTIFY_BIN"

cat >"$CONFIG_HOME/vde/tmux/config.yml" <<EOF
categories:
  default_category: preflight
daemon:
  done_clear_on: pane
  poll_ms: 250
sidebar:
  width: 35
  min_width: 10
  live:
    enabled: true
    lines: 3
notify:
  enabled: true
  command: '$NOTIFY_BIN'
EOF
cp "$CONFIG_HOME/vde/tmux/config.yml" "$ARTIFACT_DIR/config.valid.yml"

cat >"$TMUX_CONF" <<'EOF'
set -g exit-empty off
set -g status on
set -g status-left '#{@vde_status_category}#{@vde_status_sessions}#{@vde_status_windows}'
set -g status-right '#{@vde_status_attention}#{@vde_status_summary}'
set -g pane-border-status top
set -g pane-border-format '#{@vde_status_pane}'
EOF

cat >"$PTY_CLIENT" <<'PY'
import fcntl, os, pty, select, struct, subprocess, sys, termios
fifo, log, socket_name, session = sys.argv[1:]
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
env = dict(os.environ, TERM="xterm-256color", COLORTERM="truecolor")
proc = subprocess.Popen(
    ["tmux", "-L", socket_name, "attach-session", "-t", session],
    stdin=slave, stdout=slave, stderr=slave, env=env,
)
os.close(slave)
fifo_fd = os.open(fifo, os.O_RDWR | os.O_NONBLOCK)
with open(log, "wb", buffering=0) as output:
    while proc.poll() is None:
        readable, _, _ = select.select([master, fifo_fd], [], [], 0.2)
        for fd in readable:
            try:
                data = os.read(fd, 65536)
            except OSError:
                data = b""
            if not data:
                continue
            if fd == fifo_fd:
                os.write(master, data)
            else:
                output.write(data)
PY

if [[ "${VDE_SKIP_BUILD:-0}" != 1 ]]; then
  (cd "$ROOT" && cargo build --locked) >"$ARTIFACT_DIR/cargo-build.log" 2>&1
fi
[[ -x "$BIN" ]] || fail "current binary is not executable: $BIN"
export PATH="$(dirname "$BIN"):$PATH"
record build PASS

env XDG_STATE_HOME="$STATE_HOME" XDG_CONFIG_HOME="$CONFIG_HOME" HOME="$HOME_DIR" \
  "$SYSTEM_TMUX" -L "$TMUX_SOCKET" -f "$TMUX_CONF" start-server
TMUX_PATH="$(tmux_cmd display-message -p '#{socket_path}')"
TMUX_PID="$(tmux_cmd display-message -p '#{pid}')"
TMUX_START_TIME="$(tmux_cmd display-message -p '#{start_time}')"
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
DAEMON_SOCKET="$RUNTIME_SOCKET_ROOT/$SERVER_HASH.sock"
printf '%s\n' "$TMUX_PATH" >"$ARTIFACT_DIR/tmux-socket-path.txt"

# Populate the server before daemon startup, matching tmux's normal config-load path where
# `daemon ensure` runs from an existing session.
for session in A a a10 a2 漢; do
  tmux_cmd new-session -d -s "$session" -n own -c "$ROOT" "sleep 900"
  tmux_cmd set-option -t "$session" @vde_category preflight
done
A_PANE="$(tmux_cmd display-message -p -t A:own '#{pane_id}')"
AGENT_PANE="$A_PANE"
OTHER_PANE="$(tmux_cmd split-window -d -P -F '#{pane_id}' -t A:own -c "$ROOT" "sleep 900")"
A_WINDOW="$(tmux_cmd display-message -p -t A:own '#{window_id}')"
tmux_cmd link-window -s A:own -t a:
tmux_cmd rename-window -t "$A_WINDOW" linked
A_ID="$(tmux_cmd display-message -p -t A '#{session_id}')"
A10_ID="$(tmux_cmd display-message -p -t a10 '#{session_id}')"
A2_ID="$(tmux_cmd display-message -p -t a2 '#{session_id}')"

run_vt daemon ensure >"$ARTIFACT_DIR/daemon-ensure-initial.log"
wait_daemon_running >"$ARTIFACT_DIR/daemon-status-initial.log"

STATUS_SESSIONS=""
for _ in $(seq 1 100); do
  STATUS_SESSIONS="$(tmux_cmd show-options -qv -t A @vde_status_sessions 2>/dev/null || true)"
  [[ "$(grep -o 'range=user|session:' <<<"$STATUS_SESSIONS" | wc -l | tr -d ' ')" == 5 ]] && break
  sleep 0.1
done
printf '%s\n' "$STATUS_SESSIONS" >"$ARTIFACT_DIR/session-ranges.txt"
tmux_cmd list-sessions -F '#{session_id} #{session_name}' >"$ARTIFACT_DIR/session-id-name.txt"
python3 - "$ARTIFACT_DIR/session-ranges.txt" "$ARTIFACT_DIR/session-id-name.txt" <<'PY'
import re, sys
rendered = open(sys.argv[1], encoding="utf-8").read()
mapping = {}
for line in open(sys.argv[2], encoding="utf-8"):
    target, name = line.rstrip("\n").split(" ", 1)
    mapping[target] = name
targets = re.findall(r"#\[range=user\|session:(\$\d+)\]", rendered)
actual = [mapping[target] for target in targets]
assert actual == ["A", "a", "a10", "a2", "漢"], actual
assert not re.search(r"(?:^|\s)\+\d+(?:\s|$)", rendered), rendered
PY
record session-range-order PASS-A-a-a10-a2-CJK-all-visible

mkfifo "$CLIENT_FIFO_1"
python3 "$PTY_CLIENT" "$CLIENT_FIFO_1" "$CLIENT_LOG_1" "$TMUX_SOCKET" A &
CLIENT_PID_1=$!
exec 7>"$CLIENT_FIFO_1"
for _ in $(seq 1 60); do
  [[ "$(tmux_cmd list-clients -F '#{client_control_mode}' 2>/dev/null | grep -c '^0$' || true)" -ge 1 ]] && break
  sleep 0.1
done
CLIENT_1="$(tmux_cmd list-clients -F '#{client_name} #{session_name}' | awk '$2 == "A" { print $1; exit }')"
[[ -n "$CLIENT_1" ]]

VT_BIND_ENV="XDG_STATE_HOME=$STATE_HOME XDG_CONFIG_HOME=$CONFIG_HOME HOME=$HOME_DIR VDE_TMUX_SOCKET_NAME=$TMUX_SOCKET"
VT_BIND_SCOPE="--client-name '#{client_name}' --session-id '#{session_id}'"
tmux_cmd bind-key -n x run-shell "$VT_BIND_ENV $BIN statusline-sessions $VT_BIND_SCOPE switch 4 >>'$ACTION_LOG' 2>&1"
tmux_cmd bind-key -n y run-shell "$VT_BIND_ENV $BIN session-cycle next >>'$ACTION_LOG' 2>&1"
tmux_cmd bind-key -n z run-shell "$VT_BIND_ENV $BIN statusline-click $VT_BIND_SCOPE 'session:$A10_ID' >>'$ACTION_LOG' 2>&1"
printf x >&7
wait_client_session "$CLIENT_1" a2
printf y >&7
wait_client_session "$CLIENT_1" 漢
printf z >&7
wait_client_session "$CLIENT_1" a10
tmux_cmd bind-key -n r run-shell "$VT_BIND_ENV $BIN statusline-click $VT_BIND_SCOPE 'session:$A_ID' >>'$ACTION_LOG' 2>&1"
printf r >&7
wait_client_session "$CLIENT_1" A
record session-actions PASS-numeric-argument-free-cycle-click-stable-targets

mkfifo "$CLIENT_FIFO_2"
tmux_cmd select-window -t a:linked
tmux_cmd select-pane -t "$OTHER_PANE"
python3 "$PTY_CLIENT" "$CLIENT_FIFO_2" "$CLIENT_LOG_2" "$TMUX_SOCKET" a &
CLIENT_PID_2=$!
exec 8>"$CLIENT_FIFO_2"
for _ in $(seq 1 60); do
  [[ "$(tmux_cmd list-clients -F '#{client_control_mode}' 2>/dev/null | grep -c '^0$' || true)" -ge 2 ]] && break
  sleep 0.1
done
CLIENT_2="$(tmux_cmd list-clients -F '#{client_name} #{session_name}' | awk '$2 == "a" { print $1; exit }')"
[[ -n "$CLIENT_2" ]]
printf '%s %s %s\n' \
  "$(client_field "$CLIENT_2" session_name)" \
  "$(client_field "$CLIENT_2" window_name)" \
  "$(client_field "$CLIENT_2" pane_id)" >"$ARTIFACT_DIR/linked-client-initial.txt"
grep -F "a linked $OTHER_PANE" "$ARTIFACT_DIR/linked-client-initial.txt" >/dev/null

# Put both regular clients on the same pane, where TMUX_PANE alone is ambiguous. The binding
# captures client and source session together, and every action must mutate only its invoker.
tmux_cmd switch-client -c "$CLIENT_1" -t "$A_ID"
tmux_cmd switch-client -c "$CLIENT_2" -t "$A_ID"
wait_client_session "$CLIENT_1" A
wait_client_session "$CLIENT_2" A
tmux_cmd bind-key -n u run-shell "$VT_BIND_ENV $BIN session-cycle next $VT_BIND_SCOPE >>'$ACTION_LOG' 2>&1"
tmux_cmd list-keys -T root >"$ARTIFACT_DIR/multiclient-cycle-binding.txt"
printf u >&8
wait_client_session "$CLIENT_2" a
tmux_cmd list-clients -F '#{client_name} #{session_id} #{session_name} #{pane_id} #{client_control_mode}' \
  >"$ARTIFACT_DIR/multiclient-after-cycle.txt"
[[ "$(client_field "$CLIENT_1" session_name)" == A ]]

tmux_cmd switch-client -c "$CLIENT_2" -t "$A_ID"
wait_client_session "$CLIENT_2" A
tmux_cmd bind-key -n v run-shell "$VT_BIND_ENV $BIN statusline-sessions $VT_BIND_SCOPE switch 4 >>'$ACTION_LOG' 2>&1"
printf v >&8
wait_client_session "$CLIENT_2" a2
[[ "$(client_field "$CLIENT_1" session_name)" == A ]]

tmux_cmd switch-client -c "$CLIENT_2" -t "$A_ID"
wait_client_session "$CLIENT_2" A
tmux_cmd bind-key -n w run-shell "$VT_BIND_ENV $BIN statusline-click $VT_BIND_SCOPE 'session:$A10_ID' >>'$ACTION_LOG' 2>&1"
printf w >&8
wait_client_session "$CLIENT_2" a10
[[ "$(client_field "$CLIENT_1" session_name)" == A ]]
record session-multiclient PASS-shared-pane-cycle-numeric-click-pinned-client-and-source
tmux_cmd switch-client -c "$CLIENT_2" -t '=a:linked'
wait_client_session "$CLIENT_2" a
[[ "$(client_field "$CLIENT_2" pane_id)" == "$OTHER_PANE" ]]

NOW="$(date +%s)"
VT_PANE="$A_PANE" run_vt hook emit --agent generic --session-id attention-preflight \
  --status running --started-at "$NOW"
VT_PANE="$A_PANE" run_vt hook emit --agent generic --session-id attention-preflight \
  --status waiting --wait-reason permission_prompt
wait_badge "$A_PANE" Blocked
wait_notification_count 1
wait_attention "$A_PANE" present >"$ARTIFACT_DIR/attention-visible-nonfocus.txt"
grep -E '[0-9]+(m[0-9]{2})?s' "$ARTIFACT_DIR/attention-visible-nonfocus.txt" >/dev/null
ELAPSED_REVISION_BEFORE="$(stable_snapshot_revision)"
python3 - "$SYSTEM_TMUX" "$TMUX_SOCKET" "$ARTIFACT_DIR/elapsed-clock.txt" <<'PY'
import json, re, subprocess, sys, time

tmux, socket_name, artifact = sys.argv[1:]
deadline = time.monotonic() + 5.5
samples = []
last = None
while time.monotonic() < deadline and len(samples) < 4:
    rendered = subprocess.check_output(
        [tmux, "-L", socket_name, "show-options", "-qv", "-t", "A", "@vde_status_attention"],
        text=True,
    ).rstrip("\n")
    matches = re.findall(r"(?<![0-9])([0-9]+)s(?![A-Za-z])", rendered)
    assert matches, rendered
    elapsed = int(matches[-1])
    if elapsed != last:
        samples.append({"at": time.monotonic(), "elapsed": elapsed, "rendered": rendered})
        last = elapsed
    time.sleep(0.05)

assert len(samples) == 4, samples
assert [sample["elapsed"] for sample in samples] == list(
    range(samples[0]["elapsed"], samples[0]["elapsed"] + 4)
), samples
gaps = [samples[index]["at"] - samples[index - 1]["at"] for index in range(1, 4)]
# Sampling can observe one delayed update near the next boundary, shortening the following gap.
# Check the complete four-value span so scheduler jitter cannot hide a sustained 2 Hz clock.
assert all(0.0 < gap <= 1.75 for gap in gaps), gaps
assert 1.75 <= sum(gaps) <= 4.50, gaps
with open(artifact, "w", encoding="utf-8") as handle:
    json.dump({"samples": samples, "gaps": gaps}, handle, ensure_ascii=False, indent=2)
PY
[[ "$(snapshot_revision)" == "$ELAPSED_REVISION_BEFORE" ]]
record elapsed-clock PASS-four-consecutive-values-and-two-full-1Hz-gaps-without-snapshot-revision

tmux_cmd select-pane -t "$A_PANE"
wait_attention "$A_PANE" absent >"$ARTIFACT_DIR/attention-exact-focus.txt"
VT_PANE="$A_PANE" run_vt hook emit --agent generic --session-id attention-preflight \
  --status running --started-at "$((NOW + 1))"
VT_PANE="$A_PANE" run_vt hook emit --agent generic --session-id attention-preflight \
  --status waiting --wait-reason permission_prompt
wait_notification_count 2

tmux_cmd select-pane -t "$OTHER_PANE"
CONTROL_FIFO="$SANDBOX/control.in"
mkfifo "$CONTROL_FIFO"
exec 9<>"$CONTROL_FIFO"
tmux_cmd -C attach-session -f active-pane -t A <"$CONTROL_FIFO" >"$ARTIFACT_DIR/control-client.log" 2>&1 &
CONTROL_PID=$!
CONTROL_CLIENT=""
for _ in $(seq 1 60); do
  CONTROL_CLIENT="$(tmux_cmd list-clients -F '#{client_name} #{client_control_mode}' \
    | awk '$2 != "0" { print $1; exit }')"
  [[ -n "$CONTROL_CLIENT" ]] && break
  sleep 0.05
done
[[ -n "$CONTROL_CLIENT" ]]
tmux_cmd refresh-client -t "$CONTROL_CLIENT" -A "$A_PANE:on"
VT_PANE="$A_PANE" run_vt hook emit --agent generic --session-id attention-preflight \
  --status running --started-at "$((NOW + 2))"
VT_PANE="$A_PANE" run_vt hook emit --agent generic --session-id attention-preflight \
  --status waiting --wait-reason permission_prompt
wait_notification_count 3
wait_attention "$A_PANE" present >"$ARTIFACT_DIR/attention-control-linked.txt"
awk -F '\t' -v pane="$A_PANE" 'NF == 3 && $1 == pane && $3 == "Blocked" { count++ } END { exit count == 3 ? 0 : 1 }' \
  "$NOTIFY_LOG"
printf '%s %s %s\n' \
  "$(client_field "$CLIENT_2" session_name)" \
  "$(client_field "$CLIENT_2" window_name)" \
  "$(client_field "$CLIENT_2" pane_id)" >"$ARTIFACT_DIR/linked-client-witness.txt"
grep -F "linked $OTHER_PANE" "$ARTIFACT_DIR/linked-client-witness.txt" >/dev/null
record attention PASS-visible-nonfocus-exact-control-linked
record notifications PASS-focus-independent-three-transitions

# Resolve the blocker before sidebar-local screenshots so flashing does not alter fingerprints.
VT_PANE="$A_PANE" run_vt hook emit --agent generic --session-id attention-preflight \
  --status running --started-at "$((NOW + 3))"

# status and doctor are read-only, while lifecycle transitions remain distinct.
PID_BEFORE="$(daemon_pid)"
META_BEFORE="$(metadata_fingerprint)"
run_vt daemon status >"$ARTIFACT_DIR/status-readonly.txt"
run_vt daemon doctor >"$ARTIFACT_DIR/doctor-readonly.txt"
PID_AFTER="$(daemon_pid)"
META_AFTER="$(metadata_fingerprint)"
[[ -n "$PID_BEFORE" && "$PID_BEFORE" == "$PID_AFTER" && "$META_BEFORE" == "$META_AFTER" ]]

run_vt daemon ensure >"$ARTIFACT_DIR/daemon-ensure-idempotent.log"
[[ "$(daemon_pid)" == "$PID_BEFORE" ]]
run_vt daemon stop >"$ARTIFACT_DIR/daemon-stop.log"
wait_daemon_stopped
for hook in window-pane-changed session-window-changed client-session-changed client-attached client-detached; do
  tmux_cmd show-hooks -g "${hook}[70]" | grep -F "${hook}[70]" >/dev/null
done
run_vt daemon ensure >"$ARTIFACT_DIR/daemon-ensure-after-stop.log"
wait_daemon_running >"$ARTIFACT_DIR/daemon-status-after-stop.log"

run_vt daemon disable >"$ARTIFACT_DIR/daemon-disable.log"
wait_daemon_stopped
run_vt daemon status >"$ARTIFACT_DIR/daemon-status-disabled.log"
grep -F 'mode: Disabled' "$ARTIFACT_DIR/daemon-status-disabled.log" >/dev/null
if run_vt daemon start >"$ARTIFACT_DIR/daemon-start-disabled.log" 2>&1; then
  fail "daemon start unexpectedly succeeded while disabled"
fi
run_vt daemon ensure >"$ARTIFACT_DIR/daemon-ensure-disabled.log"
grep -F 'ensure made no changes' "$ARTIFACT_DIR/daemon-ensure-disabled.log" >/dev/null
tmux_cmd select-pane -t "$A_PANE"
VT_PANE="$A_PANE" run_vt hook emit --agent generic --session-id disabled-event \
  --status running --started-at "$((NOW + 4))" >"$ARTIFACT_DIR/disabled-agent-event.log" 2>&1 || true
wait_daemon_stopped
run_vt daemon enable >"$ARTIFACT_DIR/daemon-enable.log"
wait_daemon_running >"$ARTIFACT_DIR/daemon-status-enabled.log"
record lifecycle PASS-ensure-stop-disable-enable-status-doctor

PID_BEFORE_RELOAD="$(daemon_pid)"
run_vt daemon status >"$ARTIFACT_DIR/status-before-invalid-reload.txt"
CONFIG_HASH_BEFORE="$(awk '/^config_hash:/ { print $2; exit }' "$ARTIFACT_DIR/status-before-invalid-reload.txt")"
cp "$CONFIG_HOME/vde/tmux/config.yml" "$SANDBOX/config.before-invalid.yml"
printf 'daemon: [invalid\n' >"$CONFIG_HOME/vde/tmux/config.yml"
if run_vt daemon reload >"$ARTIFACT_DIR/daemon-invalid-reload.log" 2>&1; then
  fail "invalid config reload unexpectedly succeeded"
fi
[[ "$(daemon_pid)" == "$PID_BEFORE_RELOAD" ]]
cmp "$SANDBOX/config.before-invalid.yml" "$ARTIFACT_DIR/config.valid.yml"
cp "$ARTIFACT_DIR/config.valid.yml" "$CONFIG_HOME/vde/tmux/config.yml"
run_vt daemon status >"$ARTIFACT_DIR/status-after-invalid-reload.txt"
grep -F 'daemon: running' "$ARTIFACT_DIR/status-after-invalid-reload.txt" >/dev/null
CONFIG_HASH_AFTER="$(awk '/^config_hash:/ { print $2; exit }' "$ARTIFACT_DIR/status-after-invalid-reload.txt")"
[[ -n "$CONFIG_HASH_BEFORE" && "$CONFIG_HASH_BEFORE" == "$CONFIG_HASH_AFTER" ]]
record invalid-reload PASS-running-daemon-unchanged

# A valid config can still fail during child bootstrap. An intentionally insecure order file is
# rejected only by the new child after the old daemon has stopped, proving no automatic rollback.
ORDER_STATE_DIR="$STATE_HOME/vde/tmux/sidebar-state"
ORDER_STATE_FILE="$ORDER_STATE_DIR/sidebar-order-v1.json"
mkdir -p "$ORDER_STATE_DIR"
chmod 700 "$ORDER_STATE_DIR"
printf '{}\n' >"$ORDER_STATE_FILE"
chmod 644 "$ORDER_STATE_FILE"
if run_vt daemon reload >"$ARTIFACT_DIR/daemon-valid-reload-startup-failure.log" 2>&1; then
  fail "reload unexpectedly succeeded with insecure bootstrap state"
fi
wait_daemon_stopped
run_vt daemon status >"$ARTIFACT_DIR/status-after-startup-failure.txt"
grep -F 'daemon: unavailable' "$ARTIFACT_DIR/status-after-startup-failure.txt" >/dev/null
grep -E 'startup failed|remains stopped|insecure sidebar state file' \
  "$ARTIFACT_DIR/daemon-valid-reload-startup-failure.log" >/dev/null
rm "$ORDER_STATE_FILE"
run_vt daemon ensure >"$ARTIFACT_DIR/daemon-ensure-after-startup-failure.log"
wait_daemon_running >"$ARTIFACT_DIR/status-after-startup-recovery.txt"
record startup-failure PASS-valid-config-no-rollback-and-explicit-recovery

# Two independent live sidebar processes receive instance-local input.
S1_WINDOW="$(tmux_cmd new-window -d -P -F '#{window_id}' -t A: -n side-one -c "$ROOT" "sleep 900")"
S1_AGENT="$(tmux_cmd display-message -p -t "$S1_WINDOW" '#{pane_id}')"
S1_PEER="$(tmux_cmd split-window -d -P -F '#{pane_id}' -t "$S1_WINDOW" -c "$ROOT" "sleep 900")"
S2_WINDOW="$(tmux_cmd new-window -d -P -F '#{window_id}' -t a10: -n side-two -c "$ROOT" "sleep 900")"
S2_AGENT="$(tmux_cmd display-message -p -t "$S2_WINDOW" '#{pane_id}')"
S2_PEER="$(tmux_cmd split-window -d -P -F '#{pane_id}' -t "$S2_WINDOW" -c "$ROOT" "sleep 900")"
SIDEBAR_NOW="$(date +%s)"
VT_PANE="$S1_AGENT" run_vt hook emit --agent 'ascii-one' --session-id side-one-a \
  --status running --started-at "$((SIDEBAR_NOW - 125))" --prompt 'ASCII live target' --prompt-source preflight
VT_PANE="$S1_PEER" run_vt hook emit --agent 'cjk-agent' --session-id side-one-b \
  --status running --started-at "$((SIDEBAR_NOW - 185))" --prompt '漢字の確認🙂' --prompt-source preflight
VT_PANE="$S1_PEER" run_vt hook emit --agent 'cjk-agent' --session-id side-one-b \
  --status idle --completed-at "$((SIDEBAR_NOW - 180))"
VT_PANE="$S2_AGENT" run_vt hook emit --agent 'ascii-two' --session-id side-two-a \
  --status running --started-at "$((SIDEBAR_NOW - 245))"
VT_PANE="$S2_PEER" run_vt hook emit --agent 'emoji-agent' --session-id side-two-b \
  --status running --started-at "$((SIDEBAR_NOW - 305))" --prompt 'emoji fleet 🚀🙂' --prompt-source preflight
wait_badge "$S1_PEER" Done

VT_PANE="$S1_AGENT" run_vt sidebar open --window "$S1_WINDOW" --width 35
VT_PANE="$S2_AGENT" run_vt sidebar open --window "$S2_WINDOW" --width 35
SIDEBAR_1="$(wait_sidebar "$S1_WINDOW")"
SIDEBAR_2="$(wait_sidebar "$S2_WINDOW")"
printf '%s\n%s\n' "$SIDEBAR_1" "$SIDEBAR_2" >"$ARTIFACT_DIR/sidebar-panes.txt"

REVISION_BEFORE_LOCAL="$(stable_snapshot_revision)"
capture_sidebar_normalized "$SIDEBAR_1" "$ARTIFACT_DIR/sidebar-1-before.txt"
capture_sidebar_normalized "$SIDEBAR_2" "$ARTIFACT_DIR/sidebar-2-before.txt"
SIDE2_STABLE="$(fingerprint "$ARTIFACT_DIR/sidebar-2-before.txt")"

VT_PANE="$SIDEBAR_1" run_vt sidebar input j
sleep 0.15
capture_sidebar_normalized "$SIDEBAR_1" "$ARTIFACT_DIR/sidebar-1-selection.txt"
capture_sidebar_normalized "$SIDEBAR_2" "$ARTIFACT_DIR/sidebar-2-after-selection.txt"
[[ "$(fingerprint "$ARTIFACT_DIR/sidebar-1-selection.txt")" != "$(fingerprint "$ARTIFACT_DIR/sidebar-1-before.txt")" ]]
[[ "$(fingerprint "$ARTIFACT_DIR/sidebar-2-after-selection.txt")" == "$SIDE2_STABLE" ]]

VT_PANE="$SIDEBAR_1" run_vt sidebar input 1
sleep 0.15
capture_sidebar_normalized "$SIDEBAR_1" "$ARTIFACT_DIR/sidebar-1-view-flat.txt"
capture_sidebar_normalized "$SIDEBAR_2" "$ARTIFACT_DIR/sidebar-2-after-view.txt"
[[ "$(fingerprint "$ARTIFACT_DIR/sidebar-1-view-flat.txt")" != "$(fingerprint "$ARTIFACT_DIR/sidebar-1-selection.txt")" ]]
[[ "$(fingerprint "$ARTIFACT_DIR/sidebar-2-after-view.txt")" == "$SIDE2_STABLE" ]]

VT_PANE="$SIDEBAR_1" run_vt sidebar input 'done'
sleep 0.15
capture_sidebar_normalized "$SIDEBAR_1" "$ARTIFACT_DIR/sidebar-1-filter-done.txt"
capture_sidebar_normalized "$SIDEBAR_2" "$ARTIFACT_DIR/sidebar-2-after-filter.txt"
[[ "$(fingerprint "$ARTIFACT_DIR/sidebar-1-filter-done.txt")" != "$(fingerprint "$ARTIFACT_DIR/sidebar-1-view-flat.txt")" ]]
[[ "$(fingerprint "$ARTIFACT_DIR/sidebar-2-after-filter.txt")" == "$SIDE2_STABLE" ]]

tmux_cmd send-keys -t "$SIDEBAR_1" e
sleep 0.15
capture_sidebar_normalized "$SIDEBAR_1" "$ARTIFACT_DIR/sidebar-1-live-toggle.txt"
capture_sidebar_normalized "$SIDEBAR_2" "$ARTIFACT_DIR/sidebar-2-after-live.txt"
[[ "$(fingerprint "$ARTIFACT_DIR/sidebar-1-live-toggle.txt")" != "$(fingerprint "$ARTIFACT_DIR/sidebar-1-filter-done.txt")" ]]
[[ "$(fingerprint "$ARTIFACT_DIR/sidebar-2-after-live.txt")" == "$SIDE2_STABLE" ]]
REVISION_AFTER_LOCAL="$(snapshot_revision)"
(( REVISION_AFTER_LOCAL > REVISION_BEFORE_LOCAL ))
python3 - "$QUERY_JSON" <<'PY'
import json, sys
order = json.load(open(sys.argv[1], encoding="utf-8"))["snapshot"]["sidebar_model"]["order"]
assert order["view_mode"] == "flat", order
assert order["filter"] == "done_only", order
assert order["version"] >= 2, order
PY
record sidebar-local-state PASS-selection-live-noninterference-and-view-filter-defaults-persisted

# Focus records a pane-instance return target, focus-toggle closes back onto content, and a
# reopened instance still jumps through a stable PaneInstance.
tmux_cmd switch-client -c "$CLIENT_1" -t "$S1_WINDOW"
tmux_cmd select-pane -t "$S1_AGENT"
VT_PANE="$S1_AGENT" run_vt sidebar focus --window "$S1_WINDOW"
[[ "$(tmux_cmd display-message -p -t "$S1_WINDOW" '#{pane_id}')" == "$SIDEBAR_1" ]]
capture_sidebar_normalized "$SIDEBAR_1" "$ARTIFACT_DIR/sidebar-1-focused-return-marker.txt"
VT_PANE="$S1_AGENT" run_vt sidebar focus-toggle --window "$S1_WINDOW"
[[ "$(tmux_cmd display-message -p -t "$S1_WINDOW" '#{pane_id}')" == "$S1_AGENT" ]]
[[ -z "$(tmux_cmd list-panes -t "$S1_WINDOW" -F '#{@vde_sidebar}' | grep -F '1' || true)" ]]
VT_PANE="$S1_AGENT" run_vt sidebar open --window "$S1_WINDOW" --width 35
SIDEBAR_1="$(wait_sidebar "$S1_WINDOW")"
printf 'reopened %s\n' "$SIDEBAR_1" >>"$ARTIFACT_DIR/sidebar-panes.txt"
VT_PANE="$S1_AGENT" run_vt sidebar focus --window "$S1_WINDOW"
for _ in $(seq 1 60); do
  [[ "$(client_field "$CLIENT_1" pane_id)" == "$SIDEBAR_1" ]] && break
  sleep 0.05
done
[[ "$(client_field "$CLIENT_1" pane_id)" == "$SIDEBAR_1" ]]
VT_PANE="$SIDEBAR_1" run_vt sidebar jump "$S1_PEER"
for _ in $(seq 1 60); do
  [[ "$(tmux_cmd display-message -p -t "$S1_WINDOW" '#{pane_id}')" == "$S1_PEER" ]] && break
  sleep 0.05
done
[[ "$(tmux_cmd display-message -p -t "$S1_WINDOW" '#{pane_id}')" == "$S1_PEER" ]]
record sidebar-targets PASS-return-marker-close-and-stable-jump

# Done is pane-global: one eligible client focus acknowledges it for every sidebar projection.
tmux_cmd switch-client -c "$CLIENT_1" -t '=A:'
tmux_cmd select-window -t "$S1_WINDOW"
tmux_cmd select-pane -t "$S1_PEER"
wait_badge "$S1_PEER" Idle
capture_sidebar_normalized "$SIDEBAR_1" "$ARTIFACT_DIR/sidebar-1-after-global-ack.txt"
capture_sidebar_normalized "$SIDEBAR_2" "$ARTIFACT_DIR/sidebar-2-after-global-ack.txt"
record sidebar-ack PASS-pane-global

# Window mode must acknowledge inside the completion mutation when another split in the same
# window is focused; a transient Done publication is a contract violation.
awk '{
  if ($0 == "  done_clear_on: pane") print "  done_clear_on: window";
  else print $0;
}' "$CONFIG_HOME/vde/tmux/config.yml" >"$SANDBOX/config.window.yml"
mv "$SANDBOX/config.window.yml" "$CONFIG_HOME/vde/tmux/config.yml"
cp "$CONFIG_HOME/vde/tmux/config.yml" "$ARTIFACT_DIR/config.window.yml"
run_vt daemon reload >"$ARTIFACT_DIR/daemon-reload-window-mode.log"
wait_daemon_running >"$ARTIFACT_DIR/daemon-status-window-mode.log"
tmux_cmd switch-client -c "$CLIENT_1" -t "$S1_WINDOW"
tmux_cmd select-pane -t "$S1_PEER"
WINDOW_ACK_NOW="$((NOW + 100))"
VT_PANE="$S1_AGENT" run_vt hook emit --agent 'ascii-one' --session-id side-one-a \
  --status running --started-at "$WINDOW_ACK_NOW"
VT_PANE="$S1_AGENT" run_vt hook emit --agent 'ascii-one' --session-id side-one-a \
  --status idle --completed-at "$((WINDOW_ACK_NOW + 1))"
query_snapshot
cp "$QUERY_JSON" "$ARTIFACT_DIR/window-ack-snapshot.json"
python3 - "$ARTIFACT_DIR/window-ack-snapshot.json" "$S1_AGENT" <<'PY'
import json, sys
reply = json.load(open(sys.argv[1], encoding="utf-8"))
pane = next(p for p in reply["snapshot"]["panes"] if p["pane_instance"]["pane_id"] == sys.argv[2])
assert pane["resolved"]["badge"] == "Idle", pane
PY
record window-ack PASS-immediate-same-window-nonfocus-split

# Save ANSI evidence at all required sidebar widths and enforce terminal cell bounds.
VT_PANE="$SIDEBAR_1" run_vt sidebar input all
VT_PANE="$SIDEBAR_1" run_vt sidebar input 1
for width in 16 24 35 36; do
  tmux_cmd resize-pane -t "$SIDEBAR_1" -x "$width"
  for _ in $(seq 1 30); do
    [[ "$(tmux_cmd display-message -p -t "$SIDEBAR_1" '#{pane_width}')" == "$width" ]] && break
    sleep 0.05
  done
  sleep 0.1
  tmux_cmd capture-pane -ep -t "$SIDEBAR_1" >"$ARTIFACT_DIR/sidebar-${width}.ansi"
  python3 - "$ARTIFACT_DIR/sidebar-${width}.ansi" "$width" <<'PY'
import re, sys, unicodedata
raw = open(sys.argv[1], "rb").read().decode("utf-8", "replace")
raw = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", raw)
limit = int(sys.argv[2])
def cells(text):
    total = 0
    for char in text:
        if unicodedata.combining(char):
            continue
        total += 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
    return total
for line in raw.splitlines():
    assert cells(line) <= limit, (limit, cells(line), line)
PY
done

tmux_cmd refresh-client -t "$CLIENT_1" -S
sleep 0.2
tail -c 262144 "$CLIENT_LOG_1" >"$ARTIFACT_DIR/status-80.ansi"
[[ "$(client_field "$CLIENT_1" client_width)" == 80 ]]
for option in @vde_status_category @vde_status_sessions @vde_status_windows @vde_status_attention; do
  tmux_cmd show-options -qv -t A "$option" >>"$ARTIFACT_DIR/status-options-80.txt"
done
tmux_cmd show-options -qv -t A @vde_status_summary >>"$ARTIFACT_DIR/status-options-80.txt"
python3 - "$ARTIFACT_DIR/status-options-80.txt" <<'PY'
import re, sys, unicodedata
def cells(text):
    total = 0
    for char in text:
        if unicodedata.combining(char):
            continue
        total += 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
    return total
widths = []
lines = list(open(sys.argv[1], encoding="utf-8"))
for line in lines:
    visible = re.sub(r"#\[[^]]*\]", "", line.rstrip("\n"))
    visible = visible.replace("##", "#")
    widths.append(cells(visible))
assert len(lines) == 5, len(lines)
sessions = lines[1]
assert len(re.findall(r"#\[range=user\|session:\$\d+\]", sessions)) == 5, sessions
assert not re.search(r"(?:^|\s)\+\d+(?:\s|$)", sessions), sessions
non_session_total = widths[0] + widths[2] + widths[3] + widths[4]
assert non_session_total <= 80, non_session_total
PY
record width-captures PASS-16-24-35-36-sidebar-all-sessions-and-80-other-status

query_snapshot
run_vt daemon status >"$ARTIFACT_DIR/final-daemon-status.txt"
tmux_cmd list-clients -F '#{client_name} #{session_name} #{window_name} #{pane_id} #{client_control_mode} #{client_flags}' \
  >"$ARTIFACT_DIR/final-clients.txt"
tmux_cmd list-sessions -F '#{session_id} #{session_name}' >"$ARTIFACT_DIR/final-sessions.txt"
record preflight PASS
