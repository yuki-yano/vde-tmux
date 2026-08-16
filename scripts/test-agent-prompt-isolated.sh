#!/usr/bin/env bash
set -euo pipefail

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/vde-agent-prompt.XXXXXX")"
TMUX_SOCKET="vde-agent-prompt-$$"
BUILD_BIN="$PWD/target/debug/vt"
BIN="$ROOT/bin/vt"
FIXTURE="$PWD/scripts/fixtures/codex"
FIXTURE_BUILDER="$PWD/scripts/fixtures/build-codex-embedded"
PYTHON="/usr/bin/python3"
CODEX_FIXTURE="$ROOT/bin/codex"
PROMPT_FILE="$ROOT/prompt.txt"
PROMPT_LOG="$ROOT/prompts.jsonl"
CODEX_SESSION_ID="guarded-prompt-isolated"
CODEX_TRANSCRIPT_PATH="$ROOT/codex/sessions/2026/08/15/rollout-$CODEX_SESSION_ID.jsonl"

cleanup() {
  tmux -L "$TMUX_SOCKET" kill-server 2>/dev/null || true
  if [[ "${KEEP_ARTIFACTS:-0}" == "1" ]]; then
    echo "kept isolated prompt artifacts at $ROOT" >&2
  else
    rm -rf "$ROOT"
  fi
}
trap cleanup EXIT

export XDG_CONFIG_HOME="$ROOT/config"
export XDG_STATE_HOME="$ROOT/state"
export XDG_RUNTIME_DIR="$ROOT/runtime"
export CODEX_HOME="$ROOT/codex"
export CODEX_TRANSCRIPT_PATH
export VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET"
export VT_BIN="$BIN"
export PROMPT_LOG
mkdir -p "$XDG_CONFIG_HOME/vde-tmux" "$XDG_STATE_HOME" "$XDG_RUNTIME_DIR" "$(dirname "$CODEX_FIXTURE")" "$(dirname "$CODEX_TRANSCRIPT_PATH")"
printf '%s\n' "{\"type\":\"session_meta\",\"payload\":{\"id\":\"$CODEX_SESSION_ID\",\"thread_source\":\"user\"}}" >"$CODEX_TRANSCRIPT_PATH"

"$FIXTURE_BUILDER" "$CODEX_FIXTURE"
cargo build --bin vt >/dev/null
cp "$BUILD_BIN" "$BIN"
tmux -L "$TMUX_SOCKET" -f /dev/null new-session -d -s guarded 'exec sleep 300'
tmux -L "$TMUX_SOCKET" set-option -g remain-on-exit on
TMUX_SOCKET_PATH="$(tmux -L "$TMUX_SOCKET" display-message -p '#{socket_path}')"
TMUX_SERVER_PID="$(tmux -L "$TMUX_SOCKET" display-message -p '#{pid}')"
export TMUX="$TMUX_SOCKET_PATH,$TMUX_SERVER_PID,0"

"$BIN" daemon start >/dev/null
PANE_ID="$(tmux -L "$TMUX_SOCKET" display-message -p -t guarded: '#{pane_id}')"
tmux -L "$TMUX_SOCKET" respawn-pane -k -t "$PANE_ID" "exec '$CODEX_FIXTURE' '$FIXTURE'"

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
  echo "guarded prompt fixture was not discovered as an exact idle agent" >&2
  tmux -L "$TMUX_SOCKET" list-panes -a -F '#{pane_id} pid=#{pane_pid} dead=#{pane_dead} command=#{pane_current_command}' >&2 || true
  tmux -L "$TMUX_SOCKET" capture-pane -p -t "$PANE_ID" -S -100 >&2 || true
  "$BIN" agent get "$PANE_ID" --json >&2 || true
  "$BIN" agent list --json >&2 || true
  exit 1
fi
AGENT_REF="$("$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["result"]["agent"]["summary"]["agent_ref"])' <<<"$AGENT_JSON")"

printf 'first line\nsecond line' >"$PROMPT_FILE"
OPERATION_ID="isolated_prompt_$(printf '%08d' $$)"
if ! RESULT="$("$BIN" agent prompt "$AGENT_REF" --operation-id "$OPERATION_ID" --prompt-file "$PROMPT_FILE" --confirm-timeout-ms 5000 --json 2>"$ROOT/prompt-error.json")"; then
  cat "$ROOT/prompt-error.json" >&2
  tmux -L "$TMUX_SOCKET" list-panes -a -F '#{pane_id} pid=#{pane_pid} dead=#{pane_dead} command=#{pane_current_command}' >&2 || true
  tmux -L "$TMUX_SOCKET" capture-pane -p -t "$PANE_ID" -S -100 >&2 || true
  "$BIN" agent get "$PANE_ID" --json >&2 || true
  exit 1
fi
"$PYTHON" -c 'import json,sys; value=json.load(sys.stdin); result=value["result"]; assert value["meta"]["api_version"] == 3; assert result["type"] == "agent_prompt"; assert result["operation"]["dispatch_state"] == "prompt_confirmed"; assert result["operation_ref"].startswith("vto3:"); assert result["run_ref"].startswith("vtr3:")' <<<"$RESULT"

RUN_REF="$("$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["result"]["run_ref"])' <<<"$RESULT")"
WAIT_RESULT="$("$BIN" agent run wait "$RUN_REF" --timeout-ms 5000 --json)"
"$PYTHON" -c 'import json,sys; value=json.load(sys.stdin); result=value["result"]; assert result["run_ref"] == sys.argv[1]; assert result["run"]["execution_phase"] == "ended"; assert result["run"]["semantic_outcome"] == "completed"' "$RUN_REF" <<<"$WAIT_RESULT"
RESPONSE_RESULT="$("$BIN" agent run response "$RUN_REF" --json)"
"$PYTHON" -c 'import json,sys; value=json.load(sys.stdin); assert value["result"]["body"] == "isolated guarded prompt accepted"; assert value["result"]["metadata"]["store_completeness"] == "complete"' <<<"$RESPONSE_RESULT"

"$PYTHON" - "$PROMPT_LOG" <<'PY'
import json, sys
lines = open(sys.argv[1], encoding="utf-8").read().splitlines()
assert len(lines) == 1, lines
assert json.loads(lines[0]) == "first line\nsecond line", lines
PY

if tmux -L "$TMUX_SOCKET" list-buffers -F '#{buffer_name}' 2>/dev/null | grep -F 'vde-agent-prompt-' >/dev/null; then
  echo "guarded prompt buffer leaked" >&2
  exit 1
fi

echo "isolated durable agent prompt, run wait, response artifact, and buffer cleanup ok"
