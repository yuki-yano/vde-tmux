#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/p0-provider-contract.sh --provider claude|codex --phase setup [--run-dir DIR]
  scripts/p0-provider-contract.sh --provider claude|codex --phase collect --run-dir DIR [--attach]
  scripts/p0-provider-contract.sh --provider claude|codex --phase verify --run-dir DIR [--confirm-queued]
  scripts/p0-provider-contract.sh --provider claude|codex --phase stop --run-dir DIR

The provider always runs in a dedicated `tmux -L` server. setup creates private
probe configuration and records only hashes/lengths. collect launches the real
interactive provider and prints the manual input sequence. verify refuses to
pass missing observations. stop kills only the derived scratch tmux server and
keeps the private observations. Provider-native runtime data is removed after
a normal provider exit and by stop.
EOF
}

die() {
  printf 'p0-provider-contract: %s\n' "$*" >&2
  exit 1
}

PROVIDER=""
PHASE=""
RUN_DIR=""
ATTACH=0
CONFIRM_QUEUED=0

while (($# > 0)); do
  case "$1" in
    --provider)
      (($# >= 2)) || die "--provider requires a value"
      PROVIDER="$2"
      shift 2
      ;;
    --phase)
      (($# >= 2)) || die "--phase requires a value"
      PHASE="$2"
      shift 2
      ;;
    --run-dir)
      (($# >= 2)) || die "--run-dir requires a value"
      RUN_DIR="$2"
      shift 2
      ;;
    --attach)
      ATTACH=1
      shift
      ;;
    --confirm-queued)
      CONFIRM_QUEUED=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    setup|collect|verify|stop)
      [[ -z "$PHASE" ]] || die "phase was specified more than once"
      PHASE="$1"
      shift
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[[ "$PROVIDER" == "claude" || "$PROVIDER" == "codex" ]] || die "--provider must be claude or codex"
[[ "$PHASE" == "setup" || "$PHASE" == "collect" || "$PHASE" == "verify" || "$PHASE" == "stop" ]] \
  || die "--phase must be setup, collect, verify, or stop"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
PROBE="$SCRIPT_DIR/fixtures/provider-hook-probe"
PYTHON=/usr/bin/python3

[[ -x "$PROBE" ]] || die "probe fixture is not executable: $PROBE"
command -v tmux >/dev/null 2>&1 || die "tmux is required"

if [[ "$PHASE" == "setup" && -z "$RUN_DIR" ]]; then
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/vde-p0-${PROVIDER}.XXXXXX")"
elif [[ -z "$RUN_DIR" ]]; then
  die "--run-dir is required after setup"
fi

if [[ "$PHASE" == "setup" ]]; then
  mkdir -p -- "$RUN_DIR"
else
  [[ -d "$RUN_DIR" ]] || die "run directory does not exist: $RUN_DIR"
fi
RUN_DIR="$($PYTHON - "$RUN_DIR" <<'PY'
import os
import sys
print(os.path.realpath(sys.argv[1]))
PY
)"

[[ "$RUN_DIR" != "/" ]] || die "run directory cannot be filesystem root"
chmod 700 "$RUN_DIR"

SOCKET_HASH="$(printf '%s' "$PROVIDER:$RUN_DIR" | shasum -a 256 | awk '{print substr($1, 1, 12)}')"
TMUX_SOCKET="vde-p0-${PROVIDER}-${SOCKET_HASH}"
TMUX_SESSION="p0-${PROVIDER}"
METADATA="$RUN_DIR/metadata.json"
EXPECTATIONS="$RUN_DIR/expectations.json"
EVENTS="$RUN_DIR/events.jsonl"
VERIFICATION="$RUN_DIR/verification.json"
SETTINGS="$RUN_DIR/hooks.json"
PROVIDER_RUNTIME="$RUN_DIR/provider-runtime"

provider_version() {
  if [[ "$PROVIDER" == "claude" ]]; then
    command -v claude >/dev/null 2>&1 || die "claude is not installed"
    claude --version | head -n 1
  else
    command -v codex >/dev/null 2>&1 || die "codex is not installed"
    codex --version | head -n 1
  fi
}

metadata_field() {
  local field="$1"
  "$PYTHON" - "$METADATA" "$field" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as source:
    print(json.load(source)[sys.argv[2]])
PY
}

require_matching_metadata_provider() {
  [[ -f "$METADATA" ]] || die "metadata.json is missing"
  [[ "$(metadata_field provider)" == "$PROVIDER" ]] \
    || die "--provider does not match metadata.json"
}

purge_provider_runtime() {
  [[ -e "$PROVIDER_RUNTIME" ]] || return 0
  [[ -d "$PROVIDER_RUNTIME" && ! -L "$PROVIDER_RUNTIME" ]] \
    || die "provider runtime is not a real directory: $PROVIDER_RUNTIME"
  [[ "$(stat -f '%u' "$PROVIDER_RUNTIME")" == "$(id -u)" ]] \
    || die "provider runtime is not owned by the current user"
  rm -r -- "$PROVIDER_RUNTIME"
}

phase_setup() {
  [[ ! -e "$METADATA" && ! -e "$EVENTS" && ! -e "$SETTINGS" ]] \
    || die "setup refuses to overwrite an existing probe run: $RUN_DIR"

  local version head
  version="$(provider_version)"
  head="$(git -C "$REPO_ROOT" rev-parse HEAD)"
  umask 077
  install -m 600 /dev/null "$EVENTS"

  P0_PROVIDER="$PROVIDER" \
  P0_PROVIDER_VERSION="$version" \
  P0_GIT_HEAD="$head" \
  P0_METADATA="$METADATA" \
  P0_EXPECTATIONS="$EXPECTATIONS" \
  P0_SETTINGS="$SETTINGS" \
  P0_EVENTS="$EVENTS" \
  P0_PROBE="$PROBE" \
    "$PYTHON" <<'PY'
import hashlib
import json
import os
import shlex

provider = os.environ["P0_PROVIDER"]
version = os.environ["P0_PROVIDER_VERSION"]
head = os.environ["P0_GIT_HEAD"]
events_path = os.environ["P0_EVENTS"]
probe = os.environ["P0_PROBE"]

def write_private(path, value):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        json.dump(value, output, sort_keys=True, separators=(",", ":"))
        output.write("\n")

def measure(value):
    encoded = value.encode("utf-8")
    return {
        "byte_length": len(encoded),
        "lf_count": encoded.count(b"\n"),
        "sha256": hashlib.sha256(encoded).hexdigest(),
    }

prompts = [
    ("one_line", "P0-PROBE-ONE reply with ONE only"),
    ("multiple_lines", "P0-PROBE-MULTI line one\nline two\nreply with MULTI only"),
    ("queue_a", "P0-PROBE-QUEUE-A run sleep 5 in the shell, then reply with QUEUE-A only"),
    ("queue_b", "P0-PROBE-QUEUE-B reply with QUEUED only"),
]
write_private(
    os.environ["P0_METADATA"],
    {"provider": provider, "provider_version": version, "git_head": head},
)
write_private(
    os.environ["P0_EXPECTATIONS"],
    {"provider": provider, "prompts": [{"case": case, **measure(body)} for case, body in prompts]},
)

def command(event, mode):
    return shlex.join(
        [
            probe,
            "--provider", provider,
            "--provider-version", version,
            "--git-head", head,
            "--event", event,
            "--mode", mode,
            "--output", events_path,
        ]
    )

hooks = {}
for event in ("SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"):
    hooks[event] = [
        {
            "hooks": [
                {"type": "command", "command": command(event, "fail-first")},
                {"type": "command", "command": command(event, "collector")},
            ]
        }
    ]
write_private(os.environ["P0_SETTINGS"], {"hooks": hooks})
PY

  if [[ "$PROVIDER" == "codex" ]]; then
    local isolated_home configured_home auth_source
    isolated_home="$PROVIDER_RUNTIME/codex-home"
    mkdir -m 700 "$PROVIDER_RUNTIME"
    mkdir -m 700 "$isolated_home"
    cp -p "$SETTINGS" "$isolated_home/hooks.json"
    chmod 600 "$isolated_home/hooks.json"
    configured_home="${CODEX_HOME:-$HOME/.codex}"
    auth_source="$configured_home/auth.json"
    if [[ -f "$auth_source" ]]; then
      ln -s "$auth_source" "$isolated_home/auth.json"
    fi
  else
    mkdir -m 700 "$PROVIDER_RUNTIME"
    mkdir -m 700 "$PROVIDER_RUNTIME/claude-home"
    local claude_credentials
    claude_credentials="$HOME/.claude/.credentials.json"
    if [[ -f "$claude_credentials" ]]; then
      cp -p "$claude_credentials" "$PROVIDER_RUNTIME/claude-home/.credentials.json"
      chmod 600 "$PROVIDER_RUNTIME/claude-home/.credentials.json"
    fi
    local claude_state
    claude_state="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/.claude.json"
    if [[ -f "$claude_state" ]]; then
      cp -p "$claude_state" "$PROVIDER_RUNTIME/claude-home/.claude.json"
      chmod 600 "$PROVIDER_RUNTIME/claude-home/.claude.json"
    fi
  fi

  tmux -L "$TMUX_SOCKET" -f /dev/null new-session -d -s "$TMUX_SESSION" -c "$REPO_ROOT" \
    'exec /bin/zsh -f'
  tmux -L "$TMUX_SOCKET" set-option -g remain-on-exit on

  printf 'P0 probe prepared.\nprovider: %s\nrun-dir: %s\nscratch socket: %s\n' \
    "$PROVIDER" "$RUN_DIR" "$TMUX_SOCKET"
  printf 'Next: %q --provider %q --phase collect --run-dir %q\n' "$0" "$PROVIDER" "$RUN_DIR"
}

print_manual_sequence() {
  cat <<'EOF'

In the attached provider TUI, perform this sequence exactly:

1. Submit this single-line prompt and wait for completion:
P0-PROBE-ONE reply with ONE only

2. Paste these three lines as one prompt and wait for completion:
P0-PROBE-MULTI line one
line two
reply with MULTI only

3. Submit queue A:
P0-PROBE-QUEUE-A run sleep 5 in the shell, then reply with QUEUE-A only

4. While queue A is visibly still running, enter queue B and use the provider's
   explicit queue control (in Codex, press Tab):
P0-PROBE-QUEUE-B reply with QUEUED only

5. Wait for both turns to complete, then use /exit. Wait a few seconds so the
   SessionEnd hook has completed, and detach from tmux if it remains attached.

Do not use tmux send-keys for these inputs: this phase intentionally preserves
the provider's real interactive paste/queue behavior.
EOF
}

phase_collect() {
  [[ -f "$METADATA" && -f "$EXPECTATIONS" && -f "$EVENTS" && -f "$SETTINGS" ]] \
    || die "setup artifacts are incomplete"
  require_matching_metadata_provider
  [[ "$(provider_version)" == "$(metadata_field provider_version)" ]] \
    || die "provider version changed after setup; use a fresh run directory"
  [[ "$(git -C "$REPO_ROOT" rev-parse HEAD)" == "$(metadata_field git_head)" ]] \
    || die "git HEAD changed after setup; use a fresh run directory"
  [[ ! -s "$EVENTS" ]] || die "collect refuses to reuse a run that already contains observations"
  tmux -L "$TMUX_SOCKET" has-session -t "$TMUX_SESSION" 2>/dev/null \
    || die "scratch tmux server is not running; rerun setup with a fresh run directory"

  local command_path provider_command command_line quoted_runtime
  if [[ "$PROVIDER" == "claude" ]]; then
    command_path="$(command -v claude)"
    printf -v provider_command '%q ' env "CLAUDE_CONFIG_DIR=$PROVIDER_RUNTIME/claude-home" \
      "$command_path" --settings "$SETTINGS" --setting-sources ''
  else
    command_path="$(command -v codex)"
    printf -v provider_command '%q ' env "CODEX_HOME=$PROVIDER_RUNTIME/codex-home" \
      "$command_path" --dangerously-bypass-hook-trust -C "$REPO_ROOT"
  fi
  printf -v quoted_runtime '%q' "$PROVIDER_RUNTIME"
  command_line="${provider_command% }; provider_status=\$?; /bin/rm -r -- $quoted_runtime; exit \$provider_status"
  tmux -L "$TMUX_SOCKET" respawn-pane -k -t "$TMUX_SESSION:0.0" -c "$REPO_ROOT" "$command_line"

  printf 'Provider launched only in scratch socket %s.\n' "$TMUX_SOCKET"
  print_manual_sequence
  printf '\nAttach with:\n  env -u TMUX tmux -L %q attach -t %q\n' "$TMUX_SOCKET" "$TMUX_SESSION"
  printf 'Then verify with:\n  %q --provider %q --phase verify --run-dir %q --confirm-queued\n' \
    "$0" "$PROVIDER" "$RUN_DIR"
  if ((ATTACH == 1)); then
    [[ -t 0 && -t 1 ]] || die "--attach requires an interactive terminal"
    exec env -u TMUX tmux -L "$TMUX_SOCKET" attach -t "$TMUX_SESSION"
  fi
}

phase_verify() {
  [[ -f "$METADATA" && -f "$EXPECTATIONS" && -f "$EVENTS" ]] \
    || die "setup artifacts are incomplete"
  require_matching_metadata_provider
  local queued_value
  queued_value=false
  if ((CONFIRM_QUEUED == 1)); then
    queued_value=true
  fi

  P0_METADATA="$METADATA" \
  P0_EXPECTATIONS="$EXPECTATIONS" \
  P0_EVENTS="$EVENTS" \
  P0_VERIFICATION="$VERIFICATION" \
  P0_CONFIRM_QUEUED="$queued_value" \
    "$PYTHON" <<'PY'
import collections
import json
import os
import stat
import sys

metadata_path = os.environ["P0_METADATA"]
expectations_path = os.environ["P0_EXPECTATIONS"]
events_path = os.environ["P0_EVENTS"]
verification_path = os.environ["P0_VERIFICATION"]
queued_confirmed = os.environ["P0_CONFIRM_QUEUED"] == "true"

def load_json(path):
    with open(path, encoding="utf-8") as source:
        return json.load(source)

def is_sha(value):
    return isinstance(value, str) and len(value) == 64 and all(c in "0123456789abcdef" for c in value)

def is_git_head(value):
    return (
        isinstance(value, str)
        and len(value) in {40, 64}
        and all(c in "0123456789abcdef" for c in value)
    )

def private_regular(path):
    info = os.stat(path, follow_symlinks=False)
    return stat.S_ISREG(info.st_mode) and stat.S_IMODE(info.st_mode) == 0o600 and info.st_uid == os.getuid()

metadata = load_json(metadata_path)
expectations = load_json(expectations_path)
events = []
errors = []
if set(metadata) != {"provider", "provider_version", "git_head"}:
    errors.append("metadata.json has unexpected fields")
if metadata.get("provider") not in {"claude", "codex"}:
    errors.append("metadata.json has an invalid provider")
if not isinstance(metadata.get("provider_version"), str) or not metadata.get("provider_version"):
    errors.append("metadata.json has an invalid provider version")
if not is_git_head(metadata.get("git_head")):
    errors.append("metadata.json has an invalid git HEAD")
with open(events_path, encoding="utf-8") as source:
    for number, line in enumerate(source, 1):
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            errors.append(f"events.jsonl line {number} is invalid JSON")
            continue
        events.append(value)

for path in (metadata_path, expectations_path, events_path):
    if not private_regular(path):
        errors.append(f"{os.path.basename(path)} is not a current-user 0600 regular file")

allowed_keys = {
    "provider", "provider_version", "git_head", "event", "monotonic_ns",
    "input", "prompt", "response", "transcript_response", "stable_id_hashes",
}
metric_keys = {"byte_length", "lf_count", "sha256"}
for index, event in enumerate(events):
    extra = set(event) - allowed_keys
    if extra:
        errors.append(f"event {index} has non-whitelisted fields: {sorted(extra)}")
    if event.get("provider") != metadata.get("provider"):
        errors.append(f"event {index} provider differs from metadata")
    if event.get("provider_version") != metadata.get("provider_version"):
        errors.append(f"event {index} provider version differs from metadata")
    if event.get("git_head") != metadata.get("git_head"):
        errors.append(f"event {index} git HEAD differs from metadata")
    if not isinstance(event.get("monotonic_ns"), int):
        errors.append(f"event {index} lacks monotonic time")
    if event.get("event") not in {
        f"{hook}:{mode}"
        for hook in ("SessionStart", "UserPromptSubmit", "Stop", "SessionEnd")
        for mode in ("fail-first", "collector")
    }:
        errors.append(f"event {index} has an invalid event name")
    for key in ("input", "prompt", "response", "transcript_response"):
        if key not in event:
            continue
        value = event[key]
        if not isinstance(value, dict) or set(value) != metric_keys:
            errors.append(f"event {index} has invalid {key} metric")
        elif (
            not isinstance(value.get("byte_length"), int)
            or not isinstance(value.get("lf_count"), int)
            or value.get("lf_count", -1) < 0
            or not is_sha(value.get("sha256"))
        ):
            errors.append(f"event {index} has invalid {key} length/hash")
    ids = event.get("stable_id_hashes", {})
    if not isinstance(ids, dict) or not all(isinstance(key, str) and is_sha(value) for key, value in ids.items()):
        errors.append(f"event {index} has invalid stable ID hashes")

events.sort(key=lambda event: event.get("monotonic_ns", -1))
collectors = [event for event in events if str(event.get("event", "")).endswith(":collector")]
failures = [event for event in events if str(event.get("event", "")).endswith(":fail-first")]

def base_event(event):
    return str(event.get("event", "")).split(":", 1)[0]

expected_prompts = expectations.get("prompts", [])
expected_metrics = [
    {
        "byte_length": item.get("byte_length"),
        "lf_count": item.get("lf_count"),
        "sha256": item.get("sha256"),
    }
    for item in expected_prompts
]
prompt_events = [event for event in collectors if base_event(event) == "UserPromptSubmit" and "prompt" in event]
observed_metrics = [event["prompt"] for event in prompt_events]
gate1 = {
    "status": "pass" if observed_metrics == expected_metrics and len(expected_metrics) == 4 else "incomplete",
    "expected_prompt_count": len(expected_metrics),
    "observed_prompt_count": len(observed_metrics),
    "byte_length_lf_count_and_digest_order_matches": observed_metrics == expected_metrics,
}

stop_events = [event for event in collectors if base_event(event) == "Stop"]
matching_responses = [
    event for event in stop_events
    if "response" in event and "transcript_response" in event
    and event["response"] == event["transcript_response"]
]
gate2 = {
    "status": "pass" if len(stop_events) == 4 and len(matching_responses) == len(stop_events) else "incomplete",
    "completion_count": len(stop_events),
    "payload_transcript_exact_match_count": len(matching_responses),
    "response_source": "completion_payload_and_transcript" if matching_responses else "unconfirmed",
}

collector_by_input = collections.Counter(
    (base_event(event), event.get("input", {}).get("sha256")) for event in collectors
)
failure_by_input = collections.Counter(
    (base_event(event), event.get("input", {}).get("sha256")) for event in failures
)
fanout_pairs = all(failure_by_input[key] >= count for key, count in collector_by_input.items())
required_counts = collections.Counter(base_event(event) for event in collectors)
required_observed = (
    required_counts["SessionStart"] >= 1
    and required_counts["UserPromptSubmit"] >= 4
    and required_counts["Stop"] >= 4
    and required_counts["SessionEnd"] >= 1
)
gate4 = {
    "status": "pass" if fanout_pairs and required_observed else "incomplete",
    "all_success_collectors_have_failed_sibling": fanout_pairs,
    "collector_counts": dict(sorted(required_counts.items())),
}

session_hashes = {
    event.get("stable_id_hashes", {}).get("session_id")
    for event in collectors
    if event.get("stable_id_hashes", {}).get("session_id")
}
identity_fields = sorted({key for event in collectors for key in event.get("stable_id_hashes", {})})
queue_order_observed = (
    len(observed_metrics) == 4
    and len(expected_metrics) == 4
    and observed_metrics[2:] == expected_metrics[2:]
)
failed_attempt_counts = sorted(failure_by_input.values())
session_end_observed = required_counts["SessionEnd"] >= 1
unexpected_replays = {
    f"{event}:{digest}": failure_by_input[(event, digest)] - collector_by_input[(event, digest)]
    for event, digest in collector_by_input
    if failure_by_input[(event, digest)] != collector_by_input[(event, digest)]
}
retry_window_observed = (
    session_end_observed
    and bool(failed_attempt_counts)
    and not unexpected_replays
)
durable_turn_identity_supported = metadata.get("provider") == "codex"
prompt_turn_ids = [
    event.get("stable_id_hashes", {}).get("turn_id")
    for event in prompt_events
]
stop_turn_ids = [
    event.get("stable_id_hashes", {}).get("turn_id")
    for event in stop_events
]
prompt_turn_id_count = sum(is_sha(turn_id) for turn_id in prompt_turn_ids)
stop_turn_id_count = sum(is_sha(turn_id) for turn_id in stop_turn_ids)
prompt_turn_ids_unique = (
    prompt_turn_id_count == 4
    and len(set(prompt_turn_ids)) == 4
)
stop_turn_ids_match_prompt_order = (
    stop_turn_id_count == 4
    and stop_turn_ids == prompt_turn_ids
)
turn_identity_observed = (
    durable_turn_identity_supported
    and len(prompt_turn_ids) == 4
    and len(stop_turn_ids) == 4
    and prompt_turn_ids_unique
    and stop_turn_ids_match_prompt_order
)
gate3_ready = (
    gate1["status"] == "pass"
    and len(session_hashes) == 1
    and session_end_observed
    and queue_order_observed
    and queued_confirmed
    and retry_window_observed
    and turn_identity_observed
)
gate3 = {
    "status": "pass" if gate3_ready else "incomplete",
    "stable_id_fields_observed": identity_fields,
    "single_session_identity": len(session_hashes) == 1,
    "durable_turn_identity_supported": durable_turn_identity_supported,
    "prompt_turn_id_hash_count": prompt_turn_id_count,
    "stop_turn_id_hash_count": stop_turn_id_count,
    "prompt_turn_ids_unique": prompt_turn_ids_unique,
    "stop_turn_ids_match_prompt_order": stop_turn_ids_match_prompt_order,
    "queued_prompt_digest_order_observed": queue_order_observed,
    "queued_while_running_operator_confirmed": queued_confirmed,
    "retry_observation_window_closed_by_session_end": retry_window_observed,
    "retry_contract": "no_callback_replay_observed_through_session_end"
    if retry_window_observed else "unconfirmed",
    "unexpected_callback_replays": unexpected_replays,
    "failed_callback_attempt_counts": failed_attempt_counts,
}

gates = {
    "gate_1_prompt_bytes_and_digest": gate1,
    "gate_2_response_body_and_completeness": gate2,
    "gate_3_identity_retry_and_queued_ordering": gate3,
    "gate_4_fanout_isolation": gate4,
}
overall = "pass" if not errors and all(gate["status"] == "pass" for gate in gates.values()) else "incomplete"
result = {
    "provider": metadata.get("provider"),
    "provider_version": metadata.get("provider_version"),
    "git_head": metadata.get("git_head"),
    "status": overall,
    "event_count": len(events),
    "gates": gates,
    "validation_errors": errors,
}

temporary_path = verification_path + f".tmp.{os.getpid()}"
descriptor = os.open(
    temporary_path,
    os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
    0o600,
)
with os.fdopen(descriptor, "w", encoding="utf-8") as output:
    json.dump(result, output, indent=2, sort_keys=True)
    output.write("\n")
    output.flush()
    os.fsync(output.fileno())
os.replace(temporary_path, verification_path)
os.chmod(verification_path, 0o600)
print(json.dumps(result, indent=2, sort_keys=True))
sys.exit(0 if overall == "pass" else 1)
PY
}

phase_stop() {
  require_matching_metadata_provider
  if tmux -L "$TMUX_SOCKET" has-session -t "$TMUX_SESSION" 2>/dev/null; then
    tmux -L "$TMUX_SOCKET" kill-server
    printf 'Stopped scratch tmux socket %s.\n' "$TMUX_SOCKET"
  else
    printf 'Scratch tmux socket %s is already stopped.\n' "$TMUX_SOCKET"
  fi
  purge_provider_runtime
  printf 'Removed provider-native runtime data; hashed observations remain at %s\n' "$RUN_DIR"
}

case "$PHASE" in
  setup) phase_setup ;;
  collect) phase_collect ;;
  verify) phase_verify ;;
  stop) phase_stop ;;
esac
