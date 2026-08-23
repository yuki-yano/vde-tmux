#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SYSTEM_TMUX="$(command -v tmux)"
SOCKET_NAME="vde-category-api-test-$$-$RANDOM"
RUNTIME_DIR="$(mktemp -d "${TMPDIR:-/tmp}/vde-category-api-test.XXXXXX")"
STATE_HOME="$RUNTIME_DIR/state"
CONFIG_HOME="$RUNTIME_DIR/config"
HOME_DIR="$RUNTIME_DIR/home"
REPO="$RUNTIME_DIR/repo"
LINKED="$RUNTIME_DIR/linked"
BIN="${VDE_TMUX_BIN:-$ROOT/target/debug/vt}"
TMUX_ENV=""

cleanup() {
  local status=$?
  set +e
  if [[ -n "$TMUX_ENV" ]]; then
    run_vt daemon disable >/dev/null 2>&1
  fi
  "$SYSTEM_TMUX" -L "$SOCKET_NAME" kill-server >/dev/null 2>&1
  rm -rf "$RUNTIME_DIR"
  exit "$status"
}
trap cleanup EXIT INT TERM

if [[ -z "${VDE_TMUX_BIN:-}" ]]; then
  cargo build --quiet --locked --manifest-path "$ROOT/Cargo.toml" --bins
fi
[[ -x "$BIN" ]]

mkdir -p "$STATE_HOME" "$CONFIG_HOME/vde/tmux" "$HOME_DIR" "$REPO"
chmod 700 "$RUNTIME_DIR" "$STATE_HOME" "$CONFIG_HOME" "$HOME_DIR"
cat >"$CONFIG_HOME/vde/tmux/config.yml" <<'YAML'
categories:
  default_category: misc
  display_names:
    misc: Misc
    work: Work
  order:
    work: 1
    misc: 2
YAML

git -C "$REPO" init --quiet
git -C "$REPO" config user.email category-api@example.invalid
git -C "$REPO" config user.name "Category API Test"
touch "$REPO/README"
git -C "$REPO" add README
git -C "$REPO" commit --quiet -m initial
git -C "$REPO" worktree add --quiet -b linked "$LINKED"

"$SYSTEM_TMUX" -L "$SOCKET_NAME" -f /dev/null new-session -d -s main -c "$REPO"
TMUX_PATH="$($SYSTEM_TMUX -L "$SOCKET_NAME" display-message -p '#{socket_path}')"
TMUX_PID="$($SYSTEM_TMUX -L "$SOCKET_NAME" display-message -p '#{pid}')"
TMUX_ENV="$TMUX_PATH,$TMUX_PID,0"
[[ "$TMUX_PATH" == *"$SOCKET_NAME"* ]]
[[ "$STATE_HOME" == "$RUNTIME_DIR/"* ]]
[[ "$CONFIG_HOME" == "$RUNTIME_DIR/"* ]]

run_vt() {
  env TMUX="$TMUX_ENV" VDE_TMUX_SOCKET_NAME="$SOCKET_NAME" \
    XDG_STATE_HOME="$STATE_HOME" XDG_CONFIG_HOME="$CONFIG_HOME" HOME="$HOME_DIR" \
    "$BIN" "$@"
}

# JSON reads must not start a stopped daemon.
if run_vt category list --json >"$RUNTIME_DIR/stopped.out" 2>"$RUNTIME_DIR/stopped.err"; then
  echo "category list unexpectedly started or reached a stopped daemon" >&2
  exit 1
fi
[[ ! -s "$RUNTIME_DIR/stopped.out" ]]
python3 - "$RUNTIME_DIR/stopped.err" <<'PY'
import json, sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
assert value["error"]["code"] == "daemon_unavailable", value
assert value["error"]["side_effect"] == "none", value
PY

run_vt daemon ensure
run_vt category list --json >"$RUNTIME_DIR/list.json"
python3 - "$RUNTIME_DIR/list.json" <<'PY'
import json, sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
assert value["meta"]["api_version"] == 4, value
assert value["result"]["type"] == "category_list", value
assert value["result"]["category_state_revision"] == 0, value
categories = value["result"]["categories"]
assert [item["index"] for item in categories] == list(range(1, len(categories) + 1)), categories
assert {item["name"] for item in categories} == {"work", "misc", "Uncategorized"}, categories
assert {item["source"] for item in categories} == {"configured", "system"}, categories
PY

cp "$CONFIG_HOME/vde/tmux/config.yml" "$RUNTIME_DIR/config.original.yml"
cat >"$CONFIG_HOME/vde/tmux/config.yml" <<'YAML'
categories:
  default_category: misc
  display_names:
    misc: Misc
    work: Changed Work
  order:
    work: 1
    misc: 2
YAML
if run_vt category list --json >"$RUNTIME_DIR/mismatch.out" 2>"$RUNTIME_DIR/mismatch.err"; then
  echo "category list accepted a disk/active config mismatch" >&2
  exit 1
fi
[[ ! -s "$RUNTIME_DIR/mismatch.out" ]]
python3 - "$RUNTIME_DIR/mismatch.err" <<'PY'
import json, sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
assert value["error"]["code"] == "stale_precondition", value
assert "daemon reload" in value["error"]["message"], value
PY
cp "$RUNTIME_DIR/config.original.yml" "$CONFIG_HOME/vde/tmux/config.yml"

run_vt category get --repo "$REPO" --json >"$RUNTIME_DIR/get-before.json"
python3 - "$RUNTIME_DIR/get-before.json" <<'PY'
import json, sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
placement = value["result"]["placement"]
assert value["result"]["type"] == "category_get", value
assert placement["category"] == "misc", placement
assert placement["explicit"] is False, placement
PY

run_vt category assign work --repo "$REPO" --json >"$RUNTIME_DIR/assign.json"
python3 - "$RUNTIME_DIR/assign.json" <<'PY'
import json, sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
receipt = value["result"]["receipt"]
assert value["result"]["type"] == "category_mutation", value
assert isinstance(value["meta"]["snapshot_revision"], int), value
assert receipt["requested"] == {"type": "category", "category": "work"}, receipt
assert receipt["before"] == {"category": "misc", "explicit": False}, receipt
assert receipt["after"] == {"category": "work", "explicit": True}, receipt
assert receipt["changed"] is True, receipt
assert receipt["category_state_revision"] == 1, receipt
PY

run_vt category assign work --repo "$REPO" --json >"$RUNTIME_DIR/assign-noop.json"
python3 - "$RUNTIME_DIR/assign-noop.json" <<'PY'
import json, sys
receipt = json.load(open(sys.argv[1], encoding="utf-8"))["result"]["receipt"]
assert receipt["changed"] is False, receipt
assert receipt["category_state_revision"] == 1, receipt
assert receipt["before"] == receipt["after"] == {"category": "work", "explicit": True}, receipt
PY

run_vt category get --repo "$LINKED" --json >"$RUNTIME_DIR/get-linked.json"
python3 - "$RUNTIME_DIR/get-before.json" "$RUNTIME_DIR/get-linked.json" <<'PY'
import json, sys
before = json.load(open(sys.argv[1], encoding="utf-8"))["result"]["placement"]
linked = json.load(open(sys.argv[2], encoding="utf-8"))["result"]["placement"]
assert before["repo"]["key"] == linked["repo"]["key"], (before, linked)
assert linked["category"] == "work" and linked["explicit"] is True, linked
PY

if run_vt category assign missing --repo "$REPO" --json \
  >"$RUNTIME_DIR/unknown.out" 2>"$RUNTIME_DIR/unknown.err"; then
  echo "unknown category mutation unexpectedly succeeded" >&2
  exit 1
fi
[[ ! -s "$RUNTIME_DIR/unknown.out" ]]
python3 - "$RUNTIME_DIR/unknown.err" <<'PY'
import json, sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
assert value["error"]["code"] == "daemon_invalid_request", value
assert value["error"]["side_effect"] == "none", value
PY

run_vt daemon restart
run_vt category get --repo "$REPO" --json >"$RUNTIME_DIR/get-restarted.json"
python3 - "$RUNTIME_DIR/get-restarted.json" <<'PY'
import json, sys
placement = json.load(open(sys.argv[1], encoding="utf-8"))["result"]["placement"]
assert placement["category"] == "work" and placement["explicit"] is True, placement
PY

run_vt category automatic --repo "$REPO" --json >"$RUNTIME_DIR/automatic.json"
python3 - "$RUNTIME_DIR/automatic.json" <<'PY'
import json, sys
receipt = json.load(open(sys.argv[1], encoding="utf-8"))["result"]["receipt"]
assert receipt["requested"] == {"type": "automatic"}, receipt
assert receipt["before"] == {"category": "work", "explicit": True}, receipt
assert receipt["after"] == {"category": "misc", "explicit": False}, receipt
assert receipt["changed"] is True, receipt
assert receipt["category_state_revision"] == 2, receipt
PY

run_vt category automatic --repo "$REPO" --json >"$RUNTIME_DIR/automatic-noop.json"
python3 - "$RUNTIME_DIR/automatic-noop.json" <<'PY'
import json, sys
receipt = json.load(open(sys.argv[1], encoding="utf-8"))["result"]["receipt"]
assert receipt["changed"] is False, receipt
assert receipt["category_state_revision"] == 2, receipt
assert receipt["after"] == {"category": "misc", "explicit": False}, receipt
PY

run_vt category list >"$RUNTIME_DIR/human-list.tsv"
awk -F '\t' 'NF != 3 { exit 1 } END { if (NR < 3) exit 1 }' "$RUNTIME_DIR/human-list.tsv"
run_vt category get --repo "$REPO" >"$RUNTIME_DIR/human-get.tsv"
awk -F '\t' '$2 == "misc" && $3 == "automatic" { ok = 1 } END { exit !ok }' \
  "$RUNTIME_DIR/human-get.tsv"
run_vt category assign work --repo "$REPO" >"$RUNTIME_DIR/human-assign.out"
[[ ! -s "$RUNTIME_DIR/human-assign.out" ]]
run_vt category automatic --repo "$REPO" >"$RUNTIME_DIR/human-automatic.out"
[[ ! -s "$RUNTIME_DIR/human-automatic.out" ]]

echo "isolated Category Agent API ok"
