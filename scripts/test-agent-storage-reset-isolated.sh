#!/usr/bin/env bash
set -euo pipefail

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/vde-agent-storage-reset.XXXXXX")"
TMUX_SOCKET="vde-agent-storage-reset-$$"
BUILD_BIN="${VDE_TMUX_TEST_BUILD_BIN:-$PWD/target/debug/vt}"
BIN="$ROOT/bin/vt"
PYTHON="/usr/bin/python3"
SCRATCH_TMUX_ENV=""
SCRATCH_DAEMON_SOCKET=""
SCRATCH_DAEMON_PID=""
DAEMON_MAY_BE_RUNNING=0

cleanup() {
  original_status=$?
  cleanup_status=0
  trap - EXIT INT TERM
  set +e
  if [[ "$DAEMON_MAY_BE_RUNNING" == "1" && -n "$SCRATCH_TMUX_ENV" ]]; then
    env -u TMUX_PANE TMUX="$SCRATCH_TMUX_ENV" VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" \
      "$BIN" daemon stop >/dev/null 2>&1 || cleanup_status=1
  fi
  if [[ -n "$SCRATCH_DAEMON_SOCKET" && -S "$SCRATCH_DAEMON_SOCKET" ]]; then
    echo "scratch daemon socket remains: $SCRATCH_DAEMON_SOCKET" >&2
    cleanup_status=1
  fi
  if [[ -n "$SCRATCH_DAEMON_PID" ]] \
    && kill -0 "$SCRATCH_DAEMON_PID" >/dev/null 2>&1; then
    echo "scratch daemon process remains: $SCRATCH_DAEMON_PID" >&2
    cleanup_status=1
  fi
  tmux -L "$TMUX_SOCKET" kill-server 2>/dev/null || true
  if [[ "${KEEP_ARTIFACTS:-0}" == "1" ]]; then
    echo "kept isolated storage reset artifacts at $ROOT" >&2
  else
    rm -rf -- "$ROOT"
  fi
  if [[ "$original_status" -ne 0 ]]; then
    exit "$original_status"
  fi
  exit "$cleanup_status"
}
trap cleanup EXIT INT TERM

export XDG_CONFIG_HOME="$ROOT/config"
export XDG_STATE_HOME="$ROOT/state"
export XDG_RUNTIME_DIR="$ROOT/runtime"
export VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET"
mkdir -p \
  "$ROOT/bin" \
  "$XDG_CONFIG_HOME/vde/tmux" \
  "$XDG_STATE_HOME" \
  "$XDG_RUNTIME_DIR"

if [[ -z "${VDE_TMUX_TEST_BUILD_BIN:-}" ]]; then
  cargo build --bin vt >/dev/null
fi
cp "$BUILD_BIN" "$BIN"
cp "$PWD/scripts/fixtures/fake-agent.py" "$ROOT/bin/codex"
chmod 700 "$ROOT/bin/codex"
export PATH="$ROOT/bin:$PATH"

tmux -L "$TMUX_SOCKET" -f /dev/null new-session -d -s storage -n work -c "$ROOT" \
  /bin/bash --noprofile --norc
tmux -L "$TMUX_SOCKET" set-option -g default-shell /bin/bash
tmux -L "$TMUX_SOCKET" set-option -g default-command '/bin/bash --noprofile --norc'
TMUX_SOCKET_PATH="$(tmux -L "$TMUX_SOCKET" display-message -p '#{socket_path}')"
TMUX_SERVER_PID="$(tmux -L "$TMUX_SOCKET" display-message -p '#{pid}')"
export TMUX="$TMUX_SOCKET_PATH,$TMUX_SERVER_PID,0"
SCRATCH_TMUX_ENV="$TMUX"

DAEMON_MAY_BE_RUNNING=1
"$BIN" daemon start >/dev/null
DAEMON_STATUS="$($BIN daemon status)"
SCRATCH_DAEMON_SOCKET="$(printf '%s\n' "$DAEMON_STATUS" | sed -n 's/^socket: //p')"
SCRATCH_DAEMON_PID="$(printf '%s\n' "$DAEMON_STATUS" | sed -n 's/^process: pid=\([0-9][0-9]*\).*/\1/p')"
[[ -n "$SCRATCH_DAEMON_SOCKET" && -n "$SCRATCH_DAEMON_PID" ]]
STORAGE_JSON="$($BIN agent storage status --json)"
GENERATION="$(printf '%s' "$STORAGE_JSON" | "$PYTHON" -c '
import json, sys
reply = json.load(sys.stdin)
assert reply["result"]["type"] == "agent_storage", reply
print(reply["result"]["usage"]["generation"])
')"

if "$BIN" agent storage reset \
  --expected-generation "$GENERATION" \
  --confirm-reset \
  --json >"$ROOT/reset-running.json" 2>"$ROOT/reset-running-error.json"; then
  echo "offline reset unexpectedly succeeded while the daemon was running" >&2
  exit 1
fi
"$PYTHON" - "$ROOT/reset-running-error.json" <<'PY'
import json, sys
error = json.load(open(sys.argv[1], encoding="utf-8"))["error"]
assert error["code"] == "recovery_not_allowed", error
assert "daemon is running" in error["message"], error
assert error["side_effect"] == "none", error
PY

PANE_ID="$(tmux -L "$TMUX_SOCKET" display-message -p -t storage:work '#{pane_id}')"
PANE_JSON="$($BIN pane get "$PANE_ID" --json)"
PANE_REF="$(printf '%s' "$PANE_JSON" | "$PYTHON" -c '
import json, sys
print(json.load(sys.stdin)["result"]["pane"]["summary"]["pane_ref"])
')"
"$BIN" agent start "$PANE_REF" --agent codex --arg 600 --timeout-ms 10000 --json \
  >"$ROOT/start-codex.json"
"$BIN" daemon stop >/dev/null
[[ ! -S "$SCRATCH_DAEMON_SOCKET" ]]
if kill -0 "$SCRATCH_DAEMON_PID" >/dev/null 2>&1; then
  echo "scratch daemon process remains after stop: $SCRATCH_DAEMON_PID" >&2
  exit 1
fi
DAEMON_MAY_BE_RUNNING=0

if "$BIN" agent storage reset \
  --expected-generation "$GENERATION" \
  --confirm-reset \
  --json >"$ROOT/reset-live-agent.json" 2>"$ROOT/reset-live-agent-error.json"; then
  echo "offline reset unexpectedly succeeded while a Codex process was live" >&2
  exit 1
fi
"$PYTHON" - "$ROOT/reset-live-agent-error.json" <<'PY'
import json, sys
error = json.load(open(sys.argv[1], encoding="utf-8"))["error"]
assert error["code"] == "recovery_not_allowed", error
assert "supported Codex occupant is live" in error["message"], error
assert error["side_effect"] == "none", error
PY

tmux -L "$TMUX_SOCKET" send-keys -t "$PANE_ID" C-c
for _ in $(seq 1 100); do
  if [[ "$(tmux -L "$TMUX_SOCKET" display-message -p -t "$PANE_ID" '#{pane_current_command}')" == "bash" ]]; then
    break
  fi
  sleep 0.05
done
[[ "$(tmux -L "$TMUX_SOCKET" display-message -p -t "$PANE_ID" '#{pane_current_command}')" == "bash" ]]

STALE_GENERATION="00000000000000000000000000000000"
[[ "$STALE_GENERATION" != "$GENERATION" ]]
if "$BIN" agent storage reset \
  --expected-generation "$STALE_GENERATION" \
  --confirm-reset \
  --json >"$ROOT/reset-stale.json" 2>"$ROOT/reset-stale-error.json"; then
  echo "offline reset unexpectedly accepted a stale generation" >&2
  exit 1
fi
"$PYTHON" - "$ROOT/reset-stale-error.json" <<'PY'
import json, sys
error = json.load(open(sys.argv[1], encoding="utf-8"))["error"]
assert error["code"] == "stale_precondition", error
assert error["side_effect"] == "none", error
PY

RESET_JSON="$($BIN agent storage reset \
  --expected-generation "$GENERATION" \
  --confirm-reset \
  --json)"
printf '%s' "$RESET_JSON" | "$PYTHON" -c '
import json, sys
reply = json.load(sys.stdin)
result = reply["result"]
assert result["type"] == "agent_storage_reset", result
assert result["previous_generation"] == sys.argv[1], result
assert result["generation"] != result["previous_generation"], result
' "$GENERATION"

echo "isolated agent storage reset daemon/live-agent/stale-generation guards and success ok"
