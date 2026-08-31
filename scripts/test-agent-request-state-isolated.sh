#!/usr/bin/env bash
set -euo pipefail

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/vde-agent-request-state.XXXXXX")"
TMUX_SOCKET="vde-agent-request-state-$$"
BUILD_BIN="${VDE_TMUX_TEST_BUILD_BIN:-$PWD/target/debug/vt}"
BIN="$ROOT/bin/vt"
FIXTURE="$PWD/scripts/fixtures/codex"
FIXTURE_BUILDER="$PWD/scripts/fixtures/build-codex-embedded"
PYTHON="/usr/bin/python3"
CODEX_FIXTURE="$ROOT/bin/codex"
PROMPT_FILE="$ROOT/prompt.txt"
PROMPT_LOG="$ROOT/prompts.jsonl"
REQUEST_STATE="$ROOT/request.json"
FAULT_DIR="$ROOT/faults"
HOOK_GATE="$ROOT/release-prompt-hook"
SESSION_ID="request-state-isolated"
TRANSCRIPT="$ROOT/codex/sessions/2026/08/25/rollout-$SESSION_ID.jsonl"
PROMPT='request-state prompt must be delivered exactly once'

cleanup() {
  original_status=$?
  trap - EXIT INT TERM
  set +e
  if [[ -x "$BIN" ]] && tmux -L "$TMUX_SOCKET" display-message -p '#{pid}' >/dev/null 2>&1; then
    "$BIN" daemon start >/dev/null 2>&1 || true
    "$BIN" daemon stop --force >/dev/null 2>&1 || true
  fi
  tmux -L "$TMUX_SOCKET" kill-server 2>/dev/null || true
  if [[ "${KEEP_ARTIFACTS:-0}" == "1" ]]; then
    echo "kept isolated request-state artifacts at $ROOT" >&2
  else
    rm -rf -- "$ROOT"
  fi
  exit "$original_status"
}
trap cleanup EXIT INT TERM

export XDG_CONFIG_HOME="$ROOT/config"
export XDG_STATE_HOME="$ROOT/state"
export XDG_RUNTIME_DIR="$ROOT/runtime"
export CODEX_HOME="$ROOT/codex"
export CODEX_TRANSCRIPT_PATH="$TRANSCRIPT"
export CODEX_SESSION_ID="$SESSION_ID"
export VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET"
export VDE_TMUX_TEST_AGENT_OPERATION_FAULT_DIR="$FAULT_DIR"
export VT_BIN="$BIN"
export PROMPT_LOG
export PROMPT_HOOK_GATE="$HOOK_GATE"
SKIP_CRASH_FAULT="${VDE_TMUX_TEST_SKIP_CRASH_FAULT:-0}"
if [[ "$SKIP_CRASH_FAULT" != "0" && "$SKIP_CRASH_FAULT" != "1" ]]; then
  echo "VDE_TMUX_TEST_SKIP_CRASH_FAULT must be 0 or 1" >&2
  exit 1
fi
mkdir -p \
  "$XDG_CONFIG_HOME/vde-tmux" \
  "$XDG_STATE_HOME" \
  "$XDG_RUNTIME_DIR" \
  "$FAULT_DIR" \
  "$(dirname "$CODEX_FIXTURE")" \
  "$(dirname "$TRANSCRIPT")"
printf '%s\n' "{\"type\":\"session_meta\",\"payload\":{\"id\":\"$SESSION_ID\",\"thread_source\":\"user\"}}" >"$TRANSCRIPT"
printf '%s' "$PROMPT" >"$PROMPT_FILE"

"$FIXTURE_BUILDER" "$CODEX_FIXTURE"
if [[ -z "${VDE_TMUX_TEST_BUILD_BIN:-}" ]]; then
  cargo build --bin vt >/dev/null
fi
cp "$BUILD_BIN" "$BIN"
tmux -L "$TMUX_SOCKET" -f /dev/null new-session -d -s request-state 'exec sleep 300'
tmux -L "$TMUX_SOCKET" set-option -g remain-on-exit on
TMUX_SOCKET_PATH="$(tmux -L "$TMUX_SOCKET" display-message -p '#{socket_path}')"
TMUX_SERVER_PID="$(tmux -L "$TMUX_SOCKET" display-message -p '#{pid}')"
export TMUX="$TMUX_SOCKET_PATH,$TMUX_SERVER_PID,0"

"$BIN" daemon start >/dev/null
PANE_ID="$(tmux -L "$TMUX_SOCKET" display-message -p -t request-state: '#{pane_id}')"
tmux -L "$TMUX_SOCKET" respawn-pane -k -t "$PANE_ID" \
  "exec '$CODEX_FIXTURE' '$FIXTURE'"

AGENT_JSON=""
for _ in $(seq 1 100); do
  if AGENT_JSON="$("$BIN" agent get "$PANE_ID" --json 2>/dev/null)" \
    && printf '%s' "$AGENT_JSON" | "$PYTHON" -c 'import json,sys; value=json.load(sys.stdin); assert value["result"]["agent"]["summary"]["identity"] == "exact"; assert value["result"]["agent"]["summary"]["status"] == "idle"' 2>/dev/null
  then
    break
  fi
  AGENT_JSON=""
  sleep 0.05
done
if [[ -z "$AGENT_JSON" ]]; then
  echo "request-state fixture was not discovered as an exact idle agent" >&2
  exit 1
fi
AGENT_REF="$("$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["result"]["agent"]["summary"]["agent_ref"])' <<<"$AGENT_JSON")"

if "$BIN" agent request "$AGENT_REF" --state-file "$ROOT/missing-body.json" --json >"$ROOT/missing-body-output.json" 2>"$ROOT/missing-body-error.json"; then
  echo "initial bodyless request unexpectedly succeeded" >&2
  exit 1
fi
"$PYTHON" -c 'import json,sys; error=json.load(open(sys.argv[1], encoding="utf-8"))["error"]; assert error["code"] == "invalid_arguments"; assert error["side_effect"] == "none"' "$ROOT/missing-body-error.json"
if [[ -e "$ROOT/missing-body.json" ]]; then
  echo "initial bodyless request unexpectedly created durable state" >&2
  exit 1
fi

# Persist the intent while the daemon is unavailable, so the test can arm a fault for vt's ID.
if [[ "$SKIP_CRASH_FAULT" == "0" ]]; then
"$BIN" daemon disable >/dev/null
if "$BIN" agent request "$AGENT_REF" \
  --state-file "$REQUEST_STATE" \
  --prompt-file "$PROMPT_FILE" \
  --confirm-timeout-ms 1 \
  --json >"$ROOT/unexpected-offline-result.json" 2>"$ROOT/offline-error.json"
then
  echo "offline request unexpectedly succeeded" >&2
  exit 1
fi
"$PYTHON" - "$ROOT/offline-error.json" "$REQUEST_STATE" "$PROMPT" <<'PY'
import json, sys
error = json.load(open(sys.argv[1], encoding="utf-8"))["error"]
state = json.load(open(sys.argv[2], encoding="utf-8"))
assert error["code"] == "timeout", error
assert error["stage"] == "before_dispatch", error
assert error["side_effect"] == "none", error
assert sys.argv[3] not in json.dumps(error), error
assert state["phase"] == "active", state
assert state["prompt_body"] == sys.argv[3], state
assert state.get("operation_ref") is None, state
PY
if [[ "$(stat -f '%Lp' "$REQUEST_STATE")" != "600" ]] \
  || [[ "$(stat -f '%u' "$REQUEST_STATE")" != "$(id -u)" ]] \
  || [[ "$(stat -f '%Lp' "$REQUEST_STATE.lock")" != "600" ]]
then
  echo "request-state or sidecar lock is not private" >&2
  exit 1
fi
OPERATION_ID="$("$PYTHON" -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["operation_id"])' "$REQUEST_STATE")"
touch "$FAULT_DIR/$OPERATION_ID.after_dispatch_submitted"
"$BIN" daemon enable >/dev/null

# The daemon aborts after terminal submission. No receipt reaches the caller, so state stays active.
if "$BIN" agent request "$AGENT_REF" \
  --state-file "$REQUEST_STATE" \
  --confirm-timeout-ms 250 \
  --json >"$ROOT/unexpected-fault-result.json" 2>"$ROOT/fault-error.json"
then
  echo "faulted request unexpectedly succeeded" >&2
  exit 1
fi
"$PYTHON" - "$ROOT/fault-error.json" "$REQUEST_STATE" "$PROMPT" <<'PY'
import json, sys
error = json.load(open(sys.argv[1], encoding="utf-8"))["error"]
state = json.load(open(sys.argv[2], encoding="utf-8"))
assert error["code"] == "delivery_unknown", error
assert error.get("receipt") is None, error
assert sys.argv[3] not in json.dumps(error), error
assert state["phase"] == "active", state
assert state["prompt_body"] == sys.argv[3], state
PY

# Await a hook-started replacement, then normalize restart state as the crash regression does.
"$BIN" daemon start >/dev/null
"$BIN" daemon stop >/dev/null
"$BIN" daemon start >/dev/null

# Exact replay recovers the same operation. The receipt makes vt prune the prompt body.
if "$BIN" agent request "$AGENT_REF" \
  --state-file "$REQUEST_STATE" \
  --confirm-timeout-ms 5000 \
  --json >"$ROOT/unexpected-unknown-result.json" 2>"$ROOT/unknown-error.json"
then
  echo "delivery-unknown request unexpectedly succeeded before the hook gate opened" >&2
  exit 1
fi
OPERATION_REF="$("$PYTHON" - "$ROOT/unknown-error.json" "$REQUEST_STATE" "$PROMPT" <<'PY'
import json, sys
error = json.load(open(sys.argv[1], encoding="utf-8"))["error"]
state = json.load(open(sys.argv[2], encoding="utf-8"))
assert error["code"] == "delivery_unknown", error
assert error["receipt"]["operation"]["dispatch_state"] == "delivery_unknown", error
assert sys.argv[3] not in json.dumps(error), error
assert state["phase"] == "operation_known", state
assert "prompt_body" not in state, state
assert state["operation_ref"] == error["receipt"]["operation_ref"], (state, error)
print(state["operation_ref"])
PY
)"

# Repeated resume is observation-only and does not deliver a second terminal prompt.
if "$BIN" agent request "$AGENT_REF" \
  --state-file "$REQUEST_STATE" \
  --confirm-timeout-ms 5000 \
  --json >"$ROOT/unexpected-repeat-result.json" 2>"$ROOT/repeat-error.json"
then
  echo "delivery-unknown resume unexpectedly succeeded before the hook gate opened" >&2
  exit 1
fi
"$PYTHON" -c 'import json,sys; error=json.load(open(sys.argv[1], encoding="utf-8"))["error"]; assert error["code"] == "delivery_unknown"; assert error["receipt"]["operation_ref"] == sys.argv[2]' "$ROOT/repeat-error.json" "$OPERATION_REF"
"$PYTHON" - "$PROMPT_LOG" "$PROMPT" <<'PY'
import json, sys
lines = open(sys.argv[1], encoding="utf-8").read().splitlines()
assert [json.loads(line) for line in lines] == [sys.argv[2]], lines
PY

# Supplied body and target must match the persisted intent before any daemon operation.
printf 'different prompt' >"$ROOT/different.txt"
if "$BIN" agent request "$AGENT_REF" --state-file "$REQUEST_STATE" --prompt-file "$ROOT/different.txt" --json >"$ROOT/mismatch-output.json" 2>"$ROOT/prompt-mismatch.json"; then
  echo "prompt mismatch unexpectedly succeeded" >&2
  exit 1
fi
"$PYTHON" -c 'import json,sys; assert json.load(open(sys.argv[1], encoding="utf-8"))["error"]["code"] == "request_state_mismatch"' "$ROOT/prompt-mismatch.json"
if "$BIN" agent request "${AGENT_REF}x" --state-file "$REQUEST_STATE" --json >"$ROOT/target-mismatch-output.json" 2>"$ROOT/target-mismatch.json"; then
  echo "target mismatch unexpectedly succeeded" >&2
  exit 1
fi
"$PYTHON" -c 'import json,sys; assert json.load(open(sys.argv[1], encoding="utf-8"))["error"]["code"] == "request_state_mismatch"' "$ROOT/target-mismatch.json"

printf '{' >"$ROOT/corrupt.json"
chmod 600 "$ROOT/corrupt.json"
if "$BIN" agent request "$AGENT_REF" --state-file "$ROOT/corrupt.json" --json >"$ROOT/corrupt-output.json" 2>"$ROOT/corrupt-error.json"; then
  echo "corrupt request-state unexpectedly succeeded" >&2
  exit 1
fi
"$PYTHON" -c 'import json,sys; assert json.load(open(sys.argv[1], encoding="utf-8"))["error"]["code"] == "request_state_invalid"' "$ROOT/corrupt-error.json"

touch "$HOOK_GATE"
RESULT=""
for _ in $(seq 1 100); do
  if RESULT="$("$BIN" agent request "$AGENT_REF" --state-file "$REQUEST_STATE" --confirm-timeout-ms 5000 --json 2>"$ROOT/late-error.json")"; then
    break
  fi
  RESULT=""
  sleep 0.05
done
if [[ -z "$RESULT" ]]; then
  cat "$ROOT/late-error.json" >&2
  echo "request-state did not recover late prompt confirmation" >&2
  exit 1
fi
RUN_REF="$("$PYTHON" -c 'import json,sys; value=json.load(sys.stdin); result=value["result"]; assert result["type"] == "agent_prompt"; assert result["operation_ref"] == sys.argv[1]; assert result["operation"]["dispatch_state"] == "prompt_confirmed"; print(result["run_ref"])' "$OPERATION_REF" <<<"$RESULT")"

# A matching body on terminal resume is accepted but never dispatched again.
MATCHED_RESULT="$("$BIN" agent request "$AGENT_REF" --state-file "$REQUEST_STATE" --prompt-file "$PROMPT_FILE" --json)"
"$PYTHON" -c 'import json,sys; result=json.load(sys.stdin)["result"]; assert result["operation_ref"] == sys.argv[1]; assert result["run_ref"] == sys.argv[2]' "$OPERATION_REF" "$RUN_REF" <<<"$MATCHED_RESULT"
"$PYTHON" - "$PROMPT_LOG" "$PROMPT" <<'PY'
import json, sys
lines = open(sys.argv[1], encoding="utf-8").read().splitlines()
assert [json.loads(line) for line in lines] == [sys.argv[2]], lines
PY
if grep -F "$PROMPT" "$REQUEST_STATE" "$ROOT/unknown-error.json" "$ROOT/repeat-error.json" >/dev/null; then
  echo "prompt body leaked after operation receipt became durable" >&2
  exit 1
fi

WAIT_RESULT="$("$BIN" agent run wait "$RUN_REF" --until completed --timeout-ms 5000 --json)"
"$PYTHON" -c 'import json,sys; run=json.load(sys.stdin)["result"]["run"]; assert run["execution_phase"] == "ended"; assert run["semantic_outcome"] == "completed"' <<<"$WAIT_RESULT"
RESPONSE_RESULT="$("$BIN" agent run response "$RUN_REF" --json)"
"$PYTHON" -c 'import json,sys; assert json.load(sys.stdin)["result"]["body"] == "isolated guarded prompt accepted"' <<<"$RESPONSE_RESULT"
else
  # Release builds intentionally omit daemon crash injection. Exercise the installed binary's
  # initial dispatch and bodyless observation-only resume without relying on that debug-only hook.
  touch "$HOOK_GATE"
  FIRST_RESULT="$("$BIN" agent request "$AGENT_REF" \
    --state-file "$REQUEST_STATE" \
    --prompt-file "$PROMPT_FILE" \
    --confirm-timeout-ms 5000 \
    --json)"
  printf '%s' "$FIRST_RESULT" >"$ROOT/first-result.json"
  read -r OPERATION_REF RUN_REF < <("$PYTHON" - "$ROOT/first-result.json" "$REQUEST_STATE" "$PROMPT" <<'PY'
import json, sys
result = json.load(open(sys.argv[1], encoding="utf-8"))["result"]
state = json.load(open(sys.argv[2], encoding="utf-8"))
assert result["type"] == "agent_prompt", result
assert result["operation"]["dispatch_state"] == "prompt_confirmed", result
assert state["phase"] == "operation_known", state
assert state["operation_ref"] == result["operation_ref"], (state, result)
assert "prompt_body" not in state, state
assert sys.argv[3] not in json.dumps(result), result
print(result["operation_ref"], result["run_ref"])
PY
  )
  RESUMED_RESULT="$("$BIN" agent request "$AGENT_REF" \
    --state-file "$REQUEST_STATE" \
    --confirm-timeout-ms 5000 \
    --json)"
  "$PYTHON" -c 'import json,sys; result=json.load(sys.stdin)["result"]; assert result["operation_ref"] == sys.argv[1]; assert result["run_ref"] == sys.argv[2]' "$OPERATION_REF" "$RUN_REF" <<<"$RESUMED_RESULT"
  "$BIN" agent run wait "$RUN_REF" --until completed --timeout-ms 5000 --json >/dev/null
  RESPONSE_RESULT="$("$BIN" agent run response "$RUN_REF" --json)"
  "$PYTHON" -c 'import json,sys; assert json.load(sys.stdin)["result"]["body"] == "isolated guarded prompt accepted"' <<<"$RESPONSE_RESULT"
  "$PYTHON" - "$PROMPT_LOG" "$PROMPT" <<'PY'
import json, sys
lines = open(sys.argv[1], encoding="utf-8").read().splitlines()
assert [json.loads(line) for line in lines] == [sys.argv[2]], lines
PY
fi

# A normal dispatch-started timeout must carry a structured receipt and become observation-only.
rm -f "$HOOK_GATE"
SECOND_PROMPT='dispatch-started timeout must retain its operation reference'
SECOND_PROMPT_FILE="$ROOT/second-prompt.txt"
SECOND_STATE="$ROOT/second-request.json"
printf '%s' "$SECOND_PROMPT" >"$SECOND_PROMPT_FILE"
if "$BIN" agent request "$AGENT_REF" \
  --state-file "$SECOND_STATE" \
  --prompt-file "$SECOND_PROMPT_FILE" \
  --confirm-timeout-ms 3000 \
  --json >"$ROOT/unexpected-dispatch-timeout.json" 2>"$ROOT/dispatch-timeout.json"
then
  echo "gated dispatch unexpectedly confirmed before timeout" >&2
  exit 1
fi
SECOND_OPERATION_REF="$("$PYTHON" - "$ROOT/dispatch-timeout.json" "$SECOND_STATE" "$SECOND_PROMPT" <<'PY'
import json, sys
error = json.load(open(sys.argv[1], encoding="utf-8"))["error"]
state = json.load(open(sys.argv[2], encoding="utf-8"))
assert error["code"] == "delivery_unknown", error
assert error["receipt"]["operation"]["dispatch_state"] == "dispatch_started", error
assert sys.argv[3] not in json.dumps(error), error
assert state["phase"] == "operation_known", state
assert "prompt_body" not in state, state
assert state["operation_ref"] == error["receipt"]["operation_ref"], (state, error)
print(state["operation_ref"])
PY
)"
touch "$HOOK_GATE"
SECOND_RESULT=""
for _ in $(seq 1 100); do
  if SECOND_RESULT="$("$BIN" agent request "$AGENT_REF" --state-file "$SECOND_STATE" --confirm-timeout-ms 5000 --json 2>"$ROOT/second-late-error.json")"; then
    break
  fi
  SECOND_RESULT=""
  sleep 0.05
done
if [[ -z "$SECOND_RESULT" ]]; then
  cat "$ROOT/second-late-error.json" >&2
  echo "dispatch-started request did not recover late confirmation" >&2
  exit 1
fi
SECOND_RUN_REF="$("$PYTHON" -c 'import json,sys; result=json.load(sys.stdin)["result"]; assert result["operation_ref"] == sys.argv[1]; assert result["operation"]["dispatch_state"] == "prompt_confirmed"; print(result["run_ref"])' "$SECOND_OPERATION_REF" <<<"$SECOND_RESULT")"
"$BIN" agent run wait "$SECOND_RUN_REF" --until completed --timeout-ms 5000 --json >/dev/null
"$PYTHON" - "$PROMPT_LOG" "$PROMPT" "$SECOND_PROMPT" <<'PY'
import json, sys
lines = open(sys.argv[1], encoding="utf-8").read().splitlines()
assert [json.loads(line) for line in lines] == [sys.argv[2], sys.argv[3]], lines
PY

# The common path must parse a fresh immediate success and prune its body in the same call.
THIRD_PROMPT='fresh request should confirm without a recovery round trip'
THIRD_STATE="$ROOT/third-request.json"
THIRD_RESULT="$(printf '%s' "$THIRD_PROMPT" | "$BIN" agent request "$AGENT_REF" \
  --state-file "$THIRD_STATE" \
  --stdin \
  --confirm-timeout-ms 5000 \
  --json)"
printf '%s' "$THIRD_RESULT" >"$ROOT/third-result.json"
THIRD_RUN_REF="$("$PYTHON" - "$ROOT/third-result.json" "$THIRD_STATE" "$THIRD_PROMPT" <<'PY'
import json, sys
result = json.load(open(sys.argv[1], encoding="utf-8"))["result"]
state = json.load(open(sys.argv[2], encoding="utf-8"))
assert result["type"] == "agent_prompt", result
assert result["operation"]["dispatch_state"] == "prompt_confirmed", result
assert state["phase"] == "operation_known", state
assert state["operation_ref"] == result["operation_ref"], (state, result)
assert "prompt_body" not in state, state
assert sys.argv[3] not in json.dumps(result), result
print(result["run_ref"])
PY
)"
"$BIN" agent run wait "$THIRD_RUN_REF" --until completed --timeout-ms 5000 --json >/dev/null
"$PYTHON" - "$PROMPT_LOG" "$PROMPT" "$SECOND_PROMPT" "$THIRD_PROMPT" <<'PY'
import json, sys
lines = open(sys.argv[1], encoding="utf-8").read().splitlines()
assert [json.loads(line) for line in lines] == sys.argv[2:], lines
PY

if [[ "$SKIP_CRASH_FAULT" == "0" ]]; then
  echo "isolated request-state immediate success, persistence, exact replay, structured timeout receipt, no-redispatch resume, mismatch guards, and late confirmation ok"
else
  echo "isolated installed request-state initial success, persistence, structured timeout receipt, no-redispatch resume, and late confirmation ok"
fi
