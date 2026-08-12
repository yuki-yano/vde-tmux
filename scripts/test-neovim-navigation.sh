#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMUX_SOCKET="vde-tmux-nvim-navigation-$$"

cleanup() {
  tmux -L "$TMUX_SOCKET" kill-server 2>/dev/null || true
}
trap cleanup EXIT

PANE_ID="$(
  tmux -L "$TMUX_SOCKET" -f /dev/null new-session -d -P \
    -F '#{pane_id}' \
    -s vde-tmux-nvim-navigation \
    'sleep 30'
)"
tmux -L "$TMUX_SOCKET" set-option -g remain-on-exit on
tmux -L "$TMUX_SOCKET" set-option -g @vde_daemon_server_identity "$(printf 'a%.0s' {1..64})"
tmux -L "$TMUX_SOCKET" set-option -g @vde_pane_switch_channel vde-pane-switch
tmux -L "$TMUX_SOCKET" respawn-pane -k -t "$PANE_ID" \
  "env VDE_TMUX_NVIM_PLUGIN_ROOT='$ROOT' nvim --clean --headless -l '$ROOT/tests/nvim-navigation-smoke.lua'"

for _ in $(seq 1 100); do
  if [[ "$(tmux -L "$TMUX_SOCKET" display-message -p -t "$PANE_ID" '#{pane_dead}')" == "1" ]]; then
    break
  fi
  sleep 0.05
done

DEAD_STATUS="$(tmux -L "$TMUX_SOCKET" display-message -p -t "$PANE_ID" '#{pane_dead_status}')"
if [[ "$DEAD_STATUS" != "0" ]]; then
  tmux -L "$TMUX_SOCKET" capture-pane -p -t "$PANE_ID" >&2
  echo "Neovim navigation smoke failed with exit status $DEAD_STATUS" >&2
  exit 1
fi

ACTIVE_PANE_PID="$(
  tmux -L "$TMUX_SOCKET" display-message -p -t "$PANE_ID" '#{@vde_nvim_active_pane_pid}'
)"
if [[ -n "$ACTIVE_PANE_PID" ]]; then
  echo "Neovim navigation left a stale pane marker: $ACTIVE_PANE_PID" >&2
  exit 1
fi

echo "Neovim navigation smoke passed"
