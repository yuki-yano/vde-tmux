#!/usr/bin/env bash
set -euo pipefail

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/vde-agent-api-v4.XXXXXX")"
TMUX_SOCKET="vde-agent-api-v4-$$"
BUILD_BIN="${VDE_TMUX_TEST_BUILD_BIN:-$PWD/target/debug/vt}"
BIN="$ROOT/bin/vt"
PYTHON="/usr/bin/python3"

cleanup() {
  tmux -L "$TMUX_SOCKET" kill-server 2>/dev/null || true
  if [[ "${KEEP_ARTIFACTS:-0}" == "1" ]]; then
    echo "kept isolated API v4 artifacts at $ROOT" >&2
  else
    rm -rf "$ROOT"
  fi
}
trap cleanup EXIT

export XDG_CONFIG_HOME="$ROOT/config"
export XDG_STATE_HOME="$ROOT/state"
export XDG_RUNTIME_DIR="$ROOT/runtime"
export VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET"
export ZDOTDIR="$ROOT/zdot"
mkdir -p \
  "$ROOT/bin" \
  "$ZDOTDIR" \
  "$XDG_CONFIG_HOME/vde-tmux" \
  "$XDG_STATE_HOME" \
  "$XDG_RUNTIME_DIR"

if [[ -z "${VDE_TMUX_TEST_BUILD_BIN:-}" ]]; then
  cargo build --bin vt >/dev/null
fi
cp "$BUILD_BIN" "$BIN"
ln -s "$(command -v sleep)" "$ROOT/bin/codex"
ln -s "$(command -v sleep)" "$ROOT/bin/claude"
export PATH="$ROOT/bin:$PATH"

tmux -L "$TMUX_SOCKET" -f /dev/null new-session -d -s api4 -n work -c "$ROOT"
tmux -L "$TMUX_SOCKET" set-option -g remain-on-exit on
TMUX_SOCKET_PATH="$(tmux -L "$TMUX_SOCKET" display-message -p '#{socket_path}')"
TMUX_SERVER_PID="$(tmux -L "$TMUX_SOCKET" display-message -p '#{pid}')"
export TMUX="$TMUX_SOCKET_PATH,$TMUX_SERVER_PID,0"

"$BIN" daemon start >/dev/null
SOURCE_PANE="$(tmux -L "$TMUX_SOCKET" display-message -p -t api4:work '#{pane_id}')"

SOURCE_JSON=""
for _ in $(seq 1 100); do
  SOURCE_JSON="$("$BIN" pane get "$SOURCE_PANE" --json 2>/dev/null || true)"
  if printf '%s' "$SOURCE_JSON" | "$PYTHON" -c '
import json, sys
pane = json.load(sys.stdin)["result"]["pane"]["summary"]
assert pane["pane_ref"].startswith("vtp1:")
' 2>/dev/null; then
    break
  fi
  sleep 0.05
done
SOURCE_REF="$(printf '%s' "$SOURCE_JSON" | "$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["result"]["pane"]["summary"]["pane_ref"])')"
ACTIVE_BEFORE="$(tmux -L "$TMUX_SOCKET" display-message -p -t api4:work '#{pane_id}')"

SPLIT_JSON="$("$BIN" pane split "$SOURCE_REF" --direction right --size-percent 40 --json)"
SPLIT_REF="$(printf '%s' "$SPLIT_JSON" | "$PYTHON" -c '
import json, sys
reply = json.load(sys.stdin)
assert reply["meta"]["api_version"] == 4, reply
result = reply["result"]
assert result["type"] == "pane_split", result
split = result["split"]
assert split["direction"] == "right", split
assert split["size_percent"] == 40, split
assert split["focused"] is False, split
assert split["pane_ref"].startswith("vtp1:"), split
print(split["pane_ref"])
')"
SPLIT_PANE="$(printf '%s' "$SPLIT_JSON" | "$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["result"]["split"]["pane_id"])')"
ACTIVE_AFTER="$(tmux -L "$TMUX_SOCKET" display-message -p -t api4:work '#{pane_id}')"
[[ "$ACTIVE_AFTER" == "$ACTIVE_BEFORE" ]]

START_CODEX_JSON="$("$BIN" agent start "$SPLIT_REF" --agent codex --arg 600 --timeout-ms 10000 --json)"
CODEX_REF="$(printf '%s' "$START_CODEX_JSON" | "$PYTHON" -c '
import json, sys
result = json.load(sys.stdin)["result"]
assert result["type"] == "agent_start", result
assert result["start"]["agent"] == "codex", result
assert result["start"]["readiness"] == "durable_initial_prompt", result
assert result["agent"]["summary"]["identity"] == "exact", result
assert result["agent"]["summary"]["pane_id"] == sys.argv[1], result
print(result["start"]["agent_ref"])
' "$SPLIT_PANE")"

printf 'best effort codex steer' >"$ROOT/steer.txt"
if "$BIN" agent steer "$CODEX_REF" --prompt-file "$ROOT/steer.txt" --json \
  >"$ROOT/steer-idle.json" 2>"$ROOT/steer-idle-error.json"; then
  echo "agent steer unexpectedly accepted an idle agent" >&2
  exit 1
fi
"$PYTHON" - "$ROOT/steer-idle-error.json" <<'PY'
import json, sys
error = json.load(open(sys.argv[1], encoding="utf-8"))["error"]
assert error["code"] == "invalid_target", error
assert error["side_effect"] == "none", error
PY

TMUX_PANE="$SPLIT_PANE" "$BIN" hook emit --agent codex --session-id api-v4-codex \
  --status running --started-at "$(date +%s)"
for _ in $(seq 1 100); do
  if "$BIN" agent get "$CODEX_REF" --json 2>/dev/null | "$PYTHON" -c '
import json, sys
agent = json.load(sys.stdin)["result"]["agent"]["summary"]
assert agent["status"] == "working", agent
' 2>/dev/null; then
    break
  fi
  sleep 0.05
done
tmux -L "$TMUX_SOCKET" copy-mode -t "$SPLIT_PANE"
STEER_JSON="$("$BIN" agent steer "$CODEX_REF" --prompt-file "$ROOT/steer.txt" --json)"
printf '%s' "$STEER_JSON" | "$PYTHON" -c '
import json, sys
result = json.load(sys.stdin)["result"]
assert result["type"] == "agent_steer", result
steer = result["steer"]
assert steer["dispatch"] == "guarded_terminal_best_effort", steer
assert steer["race_policy"] == "may_start_next_turn", steer
assert steer["target"]["agent"] == "codex", steer
assert len(steer["prompt_digest"]) == 64, steer
assert "best effort codex steer" not in str(result), result
'
[[ "$(tmux -L "$TMUX_SOCKET" display-message -p -t "$SPLIT_PANE" '#{pane_in_mode}')" == "0" ]]

if "$BIN" agent start "$SPLIT_REF" --agent codex --arg 600 --timeout-ms 1000 --json \
  >"$ROOT/start-busy.json" 2>"$ROOT/start-busy-error.json"; then
  echo "agent start unexpectedly accepted an occupied pane" >&2
  exit 1
fi
"$PYTHON" - "$ROOT/start-busy-error.json" <<'PY'
import json, sys
error = json.load(open(sys.argv[1], encoding="utf-8"))["error"]
assert error["code"] == "agent_busy", error
assert error["side_effect"] == "none", error
PY

CLAUDE_SPLIT_JSON="$("$BIN" pane split "$SOURCE_REF" --direction down --json)"
CLAUDE_REF="$(printf '%s' "$CLAUDE_SPLIT_JSON" | "$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["result"]["split"]["pane_ref"])')"
CLAUDE_PANE="$(printf '%s' "$CLAUDE_SPLIT_JSON" | "$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["result"]["split"]["pane_id"])')"
"$BIN" agent start "$CLAUDE_REF" --agent claude --arg 600 --timeout-ms 10000 --json \
  >"$ROOT/start-claude.json" &
START_CLAUDE_PID=$!
for _ in $(seq 1 100); do
  if "$BIN" agent get "$CLAUDE_PANE" --json 2>/dev/null | "$PYTHON" -c '
import json, sys
agent = json.load(sys.stdin)["result"]["agent"]["summary"]
assert agent["agent"] == "claude", agent
assert agent["identity"] == "exact", agent
' 2>/dev/null; then
    break
  fi
  sleep 0.05
done
printf '%s' '{"session_id":"api-v4-claude","source":"startup"}' \
  | TMUX_PANE="$CLAUDE_PANE" "$BIN" hook claude SessionStart
wait "$START_CLAUDE_PID"
START_CLAUDE_JSON="$(<"$ROOT/start-claude.json")"
CLAUDE_AGENT_REF="$(printf '%s' "$START_CLAUDE_JSON" | "$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["result"]["start"]["agent_ref"])')"
printf '%s' "$START_CLAUDE_JSON" | "$PYTHON" -c '
import json, sys
result = json.load(sys.stdin)["result"]
assert result["start"]["readiness"] == "provider_session", result
assert result["agent"]["agent_session_id"] == "api-v4-claude", result
'

tmux -L "$TMUX_SOCKET" copy-mode -t "$CLAUDE_PANE"
[[ "$(tmux -L "$TMUX_SOCKET" display-message -p -t "$CLAUDE_PANE" '#{pane_in_mode}')" != "0" ]]
printf 'guarded terminal prompt' >"$ROOT/prompt.txt"
SEND_JSON="$("$BIN" agent send "$CLAUDE_AGENT_REF" --prompt-file "$ROOT/prompt.txt" --json)"
printf '%s' "$SEND_JSON" | "$PYTHON" -c '
import json, sys
result = json.load(sys.stdin)["result"]
assert result["type"] == "agent_send", result
send = result["send"]
assert send["dispatch"] == "guarded_terminal", send
assert send["target"]["agent"] == "claude", send
assert len(send["prompt_digest"]) == 64, send
assert "guarded terminal prompt" not in str(result), result
'
[[ "$(tmux -L "$TMUX_SOCKET" display-message -p -t "$CLAUDE_PANE" '#{pane_in_mode}')" == "0" ]]
if tmux -L "$TMUX_SOCKET" list-buffers -F '#{buffer_name}' 2>/dev/null | grep -F 'vde-agent-input-' >/dev/null; then
  echo "guarded terminal input buffer leaked" >&2
  exit 1
fi

TMUX_PANE="$CLAUDE_PANE" "$BIN" hook emit --agent claude --session-id api-v4-claude \
  --status waiting --wait-reason permission_prompt
BLOCKED_JSON=""
for _ in $(seq 1 100); do
  BLOCKED_JSON="$("$BIN" agent get "$CLAUDE_PANE" --json 2>/dev/null || true)"
  if printf '%s' "$BLOCKED_JSON" | "$PYTHON" -c '
import json, sys
agent = json.load(sys.stdin)["result"]["agent"]
assert agent["summary"]["status"] == "blocked", agent
assert agent["summary"]["identity"] == "exact", agent
' 2>/dev/null; then
    break
  fi
  sleep 0.05
done
BLOCKED_REF="$(printf '%s' "$BLOCKED_JSON" | "$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["result"]["agent"]["summary"]["agent_ref"])')"
KEY_JSON="$("$BIN" agent send-keys "$BLOCKED_REF" --key y --key Enter --json)"
printf '%s' "$KEY_JSON" | "$PYTHON" -c '
import json, sys
result = json.load(sys.stdin)["result"]
assert result["type"] == "agent_send_keys", result
assert result["send"]["keys"] == ["y", "Enter"], result
'

SCHEMA_JSON="$("$BIN" api schema --json)"
printf '%s' "$SCHEMA_JSON" | "$PYTHON" -c '
import json, sys
reply = json.load(sys.stdin)
providers = reply["result"]["contract"]["providers"]
assert reply["meta"]["api_version"] == 4, reply
assert providers["codex"]["capabilities"]["prompt_dispatch"] == "durable", providers
assert providers["codex"]["capabilities"]["steer"] == "guarded_terminal_best_effort", providers
assert providers["claude"]["capabilities"]["prompt_dispatch"] == "guarded_terminal", providers
assert providers["claude"]["capabilities"]["steer"] == "guarded_terminal_best_effort", providers
assert providers["claude"]["capabilities"]["start"] == "provider_session", providers
assert providers["opencode"]["capabilities"]["prompt_confirmation"] == "none", providers
assert providers["opencode"]["capabilities"]["steer"] == "disabled", providers
'

echo "isolated API v4 split/start/send/steer/send-keys and copy-mode guards ok"
