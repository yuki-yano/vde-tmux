#!/usr/bin/env bash
set -euo pipefail

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/vde-agent-operation-crash.XXXXXX")"
TMUX_SOCKET="vde-agent-operation-crash-$$"
BUILD_BIN="$PWD/target/debug/vt"
BIN="$ROOT/bin/vt"
FIXTURE="$PWD/scripts/fixtures/codex"
FIXTURE_BUILDER="$PWD/scripts/fixtures/build-codex-embedded"
PYTHON="/usr/bin/python3"
CODEX_FIXTURE="$ROOT/bin/codex"
FAULT_DIR="$ROOT/faults"
PREPARED_PROMPT_FILE="$ROOT/prepared-prompt.txt"
PREPARED_PROMPT_LOG="$ROOT/prepared-prompts.jsonl"
PREPARED_SESSION_ID="operation-crash-prepared"
PREPARED_TRANSCRIPT="$ROOT/codex/sessions/prepared/rollout-$PREPARED_SESSION_ID.jsonl"
UNKNOWN_PROMPT_FILE="$ROOT/unknown-prompt.txt"
UNKNOWN_PROMPT_LOG="$ROOT/unknown-prompts.jsonl"
UNKNOWN_SESSION_ID="operation-crash-unknown"
UNKNOWN_TRANSCRIPT="$ROOT/codex/sessions/unknown/rollout-$UNKNOWN_SESSION_ID.jsonl"
UNKNOWN_HOOK_GATE="$ROOT/release-unknown-hook"

cleanup() {
  original_status=$?
  cleanup_status=0
  trap - EXIT INT TERM
  set +e
  if [[ "${KEEP_ARTIFACTS:-0}" == "1" ]]; then
    tmux -L "$TMUX_SOCKET" capture-pane -p -t prepared: -S -100 >"$ROOT/prepared-pane.txt" 2>/dev/null || true
    tmux -L "$TMUX_SOCKET" capture-pane -p -t unknown: -S -100 >"$ROOT/unknown-pane.txt" 2>/dev/null || true
  fi
  if [[ -x "$BIN" ]] && tmux -L "$TMUX_SOCKET" display-message -p '#{pid}' >/dev/null 2>&1; then
    # A fault abort disconnects the daemon control client. Let any hook-started replacement
    # finish bootstrap before stopping it, so cleanup does not race a replaced socket identity.
    "$BIN" daemon start >/dev/null 2>&1 || cleanup_status=1
    "$BIN" daemon stop --force >/dev/null 2>&1 || cleanup_status=1
  fi
  tmux -L "$TMUX_SOCKET" kill-server 2>/dev/null || true
  if [[ "${KEEP_ARTIFACTS:-0}" == "1" ]]; then
    echo "kept isolated operation crash artifacts at $ROOT" >&2
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
export CODEX_HOME="$ROOT/codex"
export VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET"
export VDE_TMUX_TEST_AGENT_OPERATION_FAULT_DIR="$FAULT_DIR"
export VT_BIN="$BIN"
mkdir -p \
  "$XDG_CONFIG_HOME/vde-tmux" \
  "$XDG_STATE_HOME" \
  "$XDG_RUNTIME_DIR" \
  "$FAULT_DIR" \
  "$(dirname "$CODEX_FIXTURE")" \
  "$(dirname "$PREPARED_TRANSCRIPT")" \
  "$(dirname "$UNKNOWN_TRANSCRIPT")"
printf '%s\n' "{\"type\":\"session_meta\",\"payload\":{\"id\":\"$PREPARED_SESSION_ID\",\"thread_source\":\"user\"}}" >"$PREPARED_TRANSCRIPT"
printf '%s\n' "{\"type\":\"session_meta\",\"payload\":{\"id\":\"$UNKNOWN_SESSION_ID\",\"thread_source\":\"user\"}}" >"$UNKNOWN_TRANSCRIPT"

"$FIXTURE_BUILDER" "$CODEX_FIXTURE"
cargo build --bin vt >/dev/null
cp "$BUILD_BIN" "$BIN"
tmux -L "$TMUX_SOCKET" -f /dev/null new-session -d -s prepared 'exec sleep 300'
tmux -L "$TMUX_SOCKET" -f /dev/null new-session -d -s unknown 'exec sleep 300'
tmux -L "$TMUX_SOCKET" set-option -g remain-on-exit on
TMUX_SOCKET_PATH="$(tmux -L "$TMUX_SOCKET" display-message -p '#{socket_path}')"
TMUX_SERVER_PID="$(tmux -L "$TMUX_SOCKET" display-message -p '#{pid}')"
export TMUX="$TMUX_SOCKET_PATH,$TMUX_SERVER_PID,0"

"$BIN" daemon start >/dev/null
PREPARED_PANE_ID="$(tmux -L "$TMUX_SOCKET" display-message -p -t prepared: '#{pane_id}')"
UNKNOWN_PANE_ID="$(tmux -L "$TMUX_SOCKET" display-message -p -t unknown: '#{pane_id}')"
tmux -L "$TMUX_SOCKET" respawn-pane -k -t "$PREPARED_PANE_ID" \
  "exec env CODEX_SESSION_ID='$PREPARED_SESSION_ID' CODEX_TRANSCRIPT_PATH='$PREPARED_TRANSCRIPT' PROMPT_LOG='$PREPARED_PROMPT_LOG' '$CODEX_FIXTURE' '$FIXTURE'"
tmux -L "$TMUX_SOCKET" respawn-pane -k -t "$UNKNOWN_PANE_ID" \
  "exec env CODEX_SESSION_ID='$UNKNOWN_SESSION_ID' CODEX_TRANSCRIPT_PATH='$UNKNOWN_TRANSCRIPT' PROMPT_LOG='$UNKNOWN_PROMPT_LOG' PROMPT_HOOK_GATE='$UNKNOWN_HOOK_GATE' '$CODEX_FIXTURE' '$FIXTURE'"

wait_for_agent() {
  local pane_id="$1"
  local agent_json=""
  for _ in $(seq 1 100); do
    if agent_json="$("$BIN" agent get "$pane_id" --json 2>/dev/null)" \
      && printf '%s' "$agent_json" | "$PYTHON" -c 'import json,sys; value=json.load(sys.stdin); assert value["result"]["agent"]["summary"]["identity"] == "exact"; assert value["result"]["agent"]["summary"]["status"] == "idle"' 2>/dev/null
    then
      printf '%s' "$agent_json"
      return 0
    fi
    sleep 0.05
  done
  echo "operation crash fixture was not discovered as an exact idle agent: $pane_id" >&2
  tmux -L "$TMUX_SOCKET" list-panes -a -F '#{pane_id} pid=#{pane_pid} dead=#{pane_dead} command=#{pane_current_command}' >&2 || true
  tmux -L "$TMUX_SOCKET" capture-pane -p -t "$pane_id" -S -100 >&2 || true
  "$BIN" agent get "$pane_id" --json >&2 || true
  return 1
}

PREPARED_AGENT_JSON="$(wait_for_agent "$PREPARED_PANE_ID")"
UNKNOWN_AGENT_JSON="$(wait_for_agent "$UNKNOWN_PANE_ID")"
PREPARED_AGENT_REF="$("$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["result"]["agent"]["summary"]["agent_ref"])' <<<"$PREPARED_AGENT_JSON")"
UNKNOWN_AGENT_REF="$("$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["result"]["agent"]["summary"]["agent_ref"])' <<<"$UNKNOWN_AGENT_JSON")"
PREPARED_DAEMON_ID="$("$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["meta"]["daemon_instance_id"])' <<<"$PREPARED_AGENT_JSON")"

expect_prompt_connection_loss() {
  local agent_ref="$1"
  local operation_id="$2"
  local prompt_file="$3"
  local error_file="$4"
  if "$BIN" agent prompt "$agent_ref" \
    --operation-id "$operation_id" \
    --prompt-file "$prompt_file" \
    --confirm-timeout-ms 250 \
    --json >"$ROOT/unexpected-prompt-result.json" 2>"$error_file"
  then
    echo "faulted prompt unexpectedly returned a successful response" >&2
    cat "$ROOT/unexpected-prompt-result.json" >&2
    return 1
  fi
  "$PYTHON" -c 'import json,sys; value=json.load(open(sys.argv[1], encoding="utf-8")); assert value["error"]["code"] == "delivery_unknown"; assert value["error"]["side_effect"] == "possible"' "$error_file"
}

restart_after_fault() {
  # Aborting the daemon disconnects its tmux control client, which can fire an installed hook and
  # start a replacement. Await that replacement (or start one when no hook won the race) before
  # stopping it; stopping during Hydrating can observe a new socket with the old process record.
  "$BIN" daemon start >/dev/null
  "$BIN" daemon stop >/dev/null
  tmux -L "$TMUX_SOCKET" set-buffer \
    -b vde-agent-prompt-0123456789abcdef01234567 \
    'stale guarded prompt'
  "$BIN" daemon start >/dev/null
  if tmux -L "$TMUX_SOCKET" list-buffers -F '#{buffer_name}' 2>/dev/null \
    | grep -Fx 'vde-agent-prompt-0123456789abcdef01234567' >/dev/null
  then
    echo "daemon restart did not clean the stale guarded prompt buffer" >&2
    return 1
  fi
}

printf 'resume prepared operation after restart' >"$PREPARED_PROMPT_FILE"
PREPARED_OPERATION_ID="operation_crash_prepared_$(printf '%08d' $$)"
touch "$FAULT_DIR/$PREPARED_OPERATION_ID.after_prepared"
expect_prompt_connection_loss \
  "$PREPARED_AGENT_REF" \
  "$PREPARED_OPERATION_ID" \
  "$PREPARED_PROMPT_FILE" \
  "$ROOT/prepared-connection-loss.json"
if [[ -e "$FAULT_DIR/$PREPARED_OPERATION_ID.after_prepared" ]]; then
  echo "prepared fault marker was not consumed" >&2
  exit 1
fi
restart_after_fault
PREPARED_RESULT="$("$BIN" agent prompt "$PREPARED_AGENT_REF" \
  --operation-id "$PREPARED_OPERATION_ID" \
  --prompt-file "$PREPARED_PROMPT_FILE" \
  --confirm-timeout-ms 20000 \
  --json)"
"$PYTHON" -c 'import json,sys; value=json.load(sys.stdin); result=value["result"]; assert result["operation"]["dispatch_state"] == "prompt_confirmed"; assert result["run_ref"].startswith("vtr3:"); assert value["meta"]["daemon_instance_id"] != sys.argv[1]' "$PREPARED_DAEMON_ID" <<<"$PREPARED_RESULT"
"$PYTHON" - "$PREPARED_PROMPT_LOG" <<'PY'
import json, sys
lines = open(sys.argv[1], encoding="utf-8").read().splitlines()
assert [json.loads(line) for line in lines] == ["resume prepared operation after restart"], lines
PY

printf 'confirm dispatch after delayed hook' >"$UNKNOWN_PROMPT_FILE"
UNKNOWN_OPERATION_ID="operation_crash_unknown_$(printf '%08d' $$)"
UNKNOWN_BEFORE_JSON="$("$BIN" agent get "$UNKNOWN_PANE_ID" --json)"
UNKNOWN_DAEMON_ID="$("$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["meta"]["daemon_instance_id"])' <<<"$UNKNOWN_BEFORE_JSON")"
touch "$FAULT_DIR/$UNKNOWN_OPERATION_ID.after_dispatch_submitted"
expect_prompt_connection_loss \
  "$UNKNOWN_AGENT_REF" \
  "$UNKNOWN_OPERATION_ID" \
  "$UNKNOWN_PROMPT_FILE" \
  "$ROOT/unknown-connection-loss.json"
if [[ -e "$FAULT_DIR/$UNKNOWN_OPERATION_ID.after_dispatch_submitted" ]]; then
  echo "dispatch_started fault marker was not consumed" >&2
  exit 1
fi
restart_after_fault
UNKNOWN_RESTARTED_JSON="$("$BIN" agent get "$UNKNOWN_PANE_ID" --json)"
UNKNOWN_RESTARTED_DAEMON_ID="$("$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["meta"]["daemon_instance_id"])' <<<"$UNKNOWN_RESTARTED_JSON")"
if [[ "$UNKNOWN_RESTARTED_DAEMON_ID" == "$UNKNOWN_DAEMON_ID" ]]; then
  echo "dispatch_started fault did not replace the daemon instance" >&2
  exit 1
fi
if "$BIN" agent prompt "$UNKNOWN_AGENT_REF" \
  --operation-id "$UNKNOWN_OPERATION_ID" \
  --prompt-file "$UNKNOWN_PROMPT_FILE" \
  --confirm-timeout-ms 20000 \
  --json >"$ROOT/unexpected-unknown-result.json" 2>"$ROOT/unknown-result.json"
then
  echo "delivery_unknown operation unexpectedly returned a success envelope" >&2
  cat "$ROOT/unexpected-unknown-result.json" >&2
  exit 1
fi
UNKNOWN_RESULT="$(<"$ROOT/unknown-result.json")"
"$PYTHON" -c 'import json,sys; value=json.load(sys.stdin); error=value["error"]; receipt=error["receipt"]; operation=receipt["operation"]; assert error["code"] == "delivery_unknown"; assert error["stage"] == "after_dispatch"; assert error["side_effect"] == "possible"; assert error["retry_action"] == "inspect_manually"; assert operation["dispatch_state"] == "delivery_unknown"; assert operation["result_receipt"]["code"] == "daemon_restarted_during_dispatch"' <<<"$UNKNOWN_RESULT"
UNKNOWN_OPERATION_REF="$("$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["error"]["receipt"]["operation_ref"])' <<<"$UNKNOWN_RESULT")"

UNKNOWN_GET="$("$BIN" agent operation get "$UNKNOWN_OPERATION_REF" --json)"
"$PYTHON" -c 'import json,sys; value=json.load(sys.stdin); assert value["result"]["operation"]["dispatch_state"] == "delivery_unknown"; assert value["result"].get("run_ref") is None' <<<"$UNKNOWN_GET"

if "$BIN" agent prompt "$UNKNOWN_AGENT_REF" \
  --operation-id "$UNKNOWN_OPERATION_ID" \
  --prompt-file "$UNKNOWN_PROMPT_FILE" \
  --confirm-timeout-ms 5000 \
  --json >"$ROOT/unexpected-unknown-retry.json" 2>"$ROOT/unknown-retry.json"
then
  echo "delivery_unknown retry unexpectedly returned a success envelope" >&2
  cat "$ROOT/unexpected-unknown-retry.json" >&2
  exit 1
fi
UNKNOWN_RETRY="$(<"$ROOT/unknown-retry.json")"
"$PYTHON" -c 'import json,sys; value=json.load(sys.stdin); error=value["error"]; assert error["code"] == "delivery_unknown"; assert error["retry_action"] == "inspect_manually"; assert error["receipt"]["operation_ref"] == sys.argv[1]; assert error["receipt"]["operation"]["dispatch_state"] == "delivery_unknown"' "$UNKNOWN_OPERATION_REF" <<<"$UNKNOWN_RETRY"
"$PYTHON" - "$UNKNOWN_PROMPT_LOG" <<'PY'
import json, sys
lines = open(sys.argv[1], encoding="utf-8").read().splitlines()
assert [json.loads(line) for line in lines] == ["confirm dispatch after delayed hook"], lines
PY

"$BIN" agent operation wait "$UNKNOWN_OPERATION_REF" \
  --until prompt-confirmed \
  --follow-unknown \
  --timeout-ms 5000 \
  --json >"$ROOT/late-confirmation.json" &
WAIT_PID=$!
sleep 0.1
touch "$UNKNOWN_HOOK_GATE"
wait "$WAIT_PID"
LATE_RESULT="$(<"$ROOT/late-confirmation.json")"
"$PYTHON" -c 'import json,sys; value=json.load(sys.stdin); result=value["result"]; operation=result["operation"]; assert operation["dispatch_state"] == "prompt_confirmed"; assert operation["result_receipt"]["confirmation_basis"] == "guarded_window_digest"; assert operation["result_receipt"]["source_attribution"] == "non_exclusive"; assert result["run_ref"].startswith("vtr3:")' <<<"$LATE_RESULT"

UNKNOWN_RUN_REF="$("$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["result"]["run_ref"])' <<<"$LATE_RESULT")"
RUN_RESULT="$("$BIN" agent run wait "$UNKNOWN_RUN_REF" --until completed --timeout-ms 5000 --json)"
"$PYTHON" -c 'import json,sys; value=json.load(sys.stdin); run=value["result"]["run"]; assert run["execution_phase"] == "ended"; assert run["semantic_outcome"] == "completed"' <<<"$RUN_RESULT"
RESPONSE_RESULT="$("$BIN" agent run response "$UNKNOWN_RUN_REF" --json)"
"$PYTHON" -c 'import json,sys; value=json.load(sys.stdin); assert value["result"]["body"] == "isolated guarded prompt accepted"; assert value["result"]["metadata"]["store_completeness"] == "complete"' <<<"$RESPONSE_RESULT"

if tmux -L "$TMUX_SOCKET" list-buffers -F '#{buffer_name}' 2>/dev/null | grep -F 'vde-agent-prompt-' >/dev/null; then
  echo "guarded prompt buffer leaked" >&2
  exit 1
fi

echo "isolated prepared restart, stale buffer cleanup, dispatch ambiguity, no-redispatch retry, and late hook recovery ok"
