#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SYSTEM_TMUX="$(command -v tmux)"
SOCKET_NAME="vde-kill-server-test-$$-$RANDOM"
RUNTIME_DIR="$(mktemp -d "${TMPDIR:-/tmp}/vde-kill-server-test.XXXXXX")"
STATE_HOME="$RUNTIME_DIR/state"
CONFIG_HOME="$RUNTIME_DIR/config"
HOME_DIR="$RUNTIME_DIR/home"
FAKE_BIN="$RUNTIME_DIR/bin"
GRACEFUL_MARKER="$RUNTIME_DIR/graceful-term"
GRACEFUL_PID_FILE="$RUNTIME_DIR/graceful.pid"
STUBBORN_PID_FILE="$RUNTIME_DIR/stubborn.pid"
DAEMON_PID=""

cleanup() {
  "$SYSTEM_TMUX" -L "$SOCKET_NAME" kill-server >/dev/null 2>&1 || true
  if [[ -n "$DAEMON_PID" ]]; then
    kill "$DAEMON_PID" >/dev/null 2>&1 || true
  fi
  rm -rf "$RUNTIME_DIR"
}
trap cleanup EXIT INT TERM

mkdir -p "$STATE_HOME" "$CONFIG_HOME/vde/tmux" "$HOME_DIR" "$FAKE_BIN"

cat >"$RUNTIME_DIR/graceful.sh" <<EOF
#!/usr/bin/env sh
trap 'printf TERM >"$GRACEFUL_MARKER"; exit 0' TERM
trap ':' INT
printf '%s\n' "\$\$" >"$GRACEFUL_PID_FILE"
while :; do sleep 1; done
EOF
chmod +x "$RUNTIME_DIR/graceful.sh"

cat >"$RUNTIME_DIR/stubborn.sh" <<EOF
#!/usr/bin/env sh
trap '' INT TERM
printf '%s\n' "\$\$" >"$STUBBORN_PID_FILE"
while :; do sleep 1; done
EOF
chmod +x "$RUNTIME_DIR/stubborn.sh"

cat >"$FAKE_BIN/fzf" <<'EOF'
#!/usr/bin/env sh
row="$(awk -F '\t' '$1 == "server" { selected = $0 } END { print selected }')"
test -n "$row"
printf 'ctrl-q\n%s\n' "$row"
EOF
chmod +x "$FAKE_BIN/fzf"

cargo build --quiet --manifest-path "$ROOT/Cargo.toml" --bin vt
BIN="$ROOT/target/debug/vt"

"$SYSTEM_TMUX" -L "$SOCKET_NAME" -f /dev/null new-session -d -s graceful "$RUNTIME_DIR/graceful.sh"
"$SYSTEM_TMUX" -L "$SOCKET_NAME" new-session -d -s stubborn "$RUNTIME_DIR/stubborn.sh"
for _ in $(seq 1 100); do
  [[ -s "$GRACEFUL_PID_FILE" && -s "$STUBBORN_PID_FILE" ]] && break
  sleep 0.02
done
[[ -s "$GRACEFUL_PID_FILE" && -s "$STUBBORN_PID_FILE" ]]

SOCKET_PATH="$($SYSTEM_TMUX -L "$SOCKET_NAME" display-message -p '#{socket_path}')"
SERVER_PID="$($SYSTEM_TMUX -L "$SOCKET_NAME" display-message -p '#{pid}')"
TMUX="$SOCKET_PATH,$SERVER_PID,0" \
  VDE_TMUX_SOCKET_NAME="$SOCKET_NAME" \
  XDG_STATE_HOME="$STATE_HOME" \
  XDG_CONFIG_HOME="$CONFIG_HOME" \
  HOME="$HOME_DIR" \
  "$BIN" daemon ensure >/dev/null

for _ in $(seq 1 100); do
  LIFECYCLE_FILE="$(find "$STATE_HOME" -name lifecycle.json -type f -print -quit 2>/dev/null || true)"
  [[ -n "$LIFECYCLE_FILE" ]] && break
  sleep 0.02
done
[[ -n "${LIFECYCLE_FILE:-}" ]]
DAEMON_PID="$(python3 - "$LIFECYCLE_FILE" <<'PY'
import json, sys
record = json.load(open(sys.argv[1], encoding="utf-8"))
print(record["process"]["pid"])
PY
)"
kill -0 "$DAEMON_PID"

env -u TMUX -u TMUX_PANE \
  PATH="$FAKE_BIN:$PATH" \
  VDE_TMUX_SOCKET_NAME="$SOCKET_NAME" \
  XDG_STATE_HOME="$STATE_HOME" \
  XDG_CONFIG_HOME="$CONFIG_HOME" \
  HOME="$HOME_DIR" \
  "$BIN" session-manager

GRACEFUL_PID="$(cat "$GRACEFUL_PID_FILE")"
STUBBORN_PID="$(cat "$STUBBORN_PID_FILE")"
[[ -f "$GRACEFUL_MARKER" ]]
if kill -0 "$GRACEFUL_PID" >/dev/null 2>&1; then
  echo "graceful process is still alive: $GRACEFUL_PID" >&2
  exit 1
fi
if kill -0 "$STUBBORN_PID" >/dev/null 2>&1; then
  echo "SIGTERM-ignoring process is still alive: $STUBBORN_PID" >&2
  exit 1
fi
if kill -0 "$DAEMON_PID" >/dev/null 2>&1; then
  echo "vde daemon is still alive: $DAEMON_PID" >&2
  exit 1
fi
DAEMON_PID=""
if "$SYSTEM_TMUX" -L "$SOCKET_NAME" has-session >/dev/null 2>&1; then
  echo "isolated tmux server is still alive" >&2
  exit 1
fi
for _ in $(seq 1 100); do
  [[ ! -e "$SOCKET_PATH" ]] && break
  sleep 0.02
done
[[ ! -e "$SOCKET_PATH" ]]

echo "isolated Kill Server cleanup ok"
