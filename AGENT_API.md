# Agent JSON API

`vt` exposes a versioned, read-only JSON interface for terminal agents. The command tree is the
public API. The daemon Unix-socket protocol is internal and changes independently.

Compatibility exists only within the same `api_version`. Breaking public changes increment that
version; old versions and fallback behavior are not kept in parallel. Callers must reject an
unknown version before interpreting the result.

```bash
vt api schema --json
vt api snapshot --json

vt pane list --json
vt pane get %456 --json
vt pane current --json
vt pane read %456 --source latest --lines 120 --json

vt agent list --status working --json
vt agent list --needs-action --json
vt agent get %456 --json
vt agent wait %456 --until done,blocked --timeout-ms 120000 --json

AGENT_JSON="$(vt agent get %456 --json)"
AGENT_REF="$(printf '%s' "$AGENT_JSON" | jq -r '.result.agent.summary.agent_ref')"
COMPLETED_SEQ="$(printf '%s' "$AGENT_JSON" | jq -r '.result.agent.completed_seq')"
vt agent wait "$AGENT_REF" --until done --after-completed-seq "$COMPLETED_SEQ" --json
vt agent read %456 --source latest --lines 120 --json
```

API commands always emit JSON. `--json` is accepted so callers can state the expected format. A
successful command writes one envelope to stdout. A failed command writes one error envelope to
stderr and exits non-zero. `api schema` uses the same success envelope and includes JSON Schemas for
the conceptual request command, success envelope, and error envelope.

```json
{
  "meta": {
    "api_version": 1,
    "server_identity": "...",
    "daemon_instance_id": "...",
    "snapshot_revision": 42,
    "started_at": 1730000000,
    "emitted_at": 1730000000,
    "diagnostic_count": 0
  },
  "result": {
    "type": "agent_list",
    "agents": []
  }
}
```

`started_at` is the CLI operation start time. `emitted_at` is the envelope serialization time, not
a claim about topology freshness. Consumers that require
a continuous observation must pin the three-part cursor `(server_identity, daemon_instance_id,
snapshot_revision)` and reject an identity change.
`diagnostic_count` is the number of daemon diagnostics attached to the observed snapshot; use
`api snapshot` to retrieve their grouped details when it is non-zero.

The request schema describes the normalized command contract rather than raw argv syntax. Defaults
and limits match the CLI: pane-read target is optional, read defaults are `latest`, 120 lines, and no
ANSI, wait defaults are `done,blocked` and 120,000 ms, read lines are 1..2,000, and wait timeout is
1..86,400,000 ms. Repeated and comma-separated `--until` argv forms normalize to the same set.

## Agent state

The public `status` describes durable agent activity and is independent of the sidebar's unread UI
projection:

| Status | Meaning |
| --- | --- |
| `blocked` | The lifecycle is Waiting or Error |
| `working` | A run is active |
| `done` | At least one run completed and no run is active, whether read or unread |
| `idle` | No run has started in the current agent epoch |

`badge` contains the current sidebar badge. A read completion therefore has `status: done`,
`badge: idle`, and `unread: false`. `needs_action` is derived from canonical triage state and does
not disappear merely because a pane is visible.

`agent list` returns only present agents. Historical records retained for unread/sidebar behavior
are not reported as current occupants. Results are ordered by canonical pane identity; consumers
must not infer activity order from array position.

A tmux pane is emitted once per server even when its window is linked into multiple sessions.
`sessions[]` describes those session views; `window_active` and `window_last` apply to each view.
Pane-level `active` is the selected pane within the shared window, not a particular client's focus.
`pane current` and `pane read` with no target read `TMUX_PANE`; `pane current` returns the same
`pane_get` result shape as `pane get`. Every agent list/get result has `present: true` by
construction.

Filters are exact except for `--cwd-prefix`, which compares normalized path components:

- `--session` matches session ID or name.
- `--agent` matches the normalized agent kind.
- `--status` matches the public durable status above.
- `--cwd-prefix` matches a path and its descendants.
- `--unread` returns unread agents.
- `--needs-action` returns agents waiting for user action.

## Identity and waiting

A `pane_ref` pins the tmux server, pane ID, and pane PID. An `agent_ref` additionally pins the
pane-state ID, agent epoch, live agent PID, and a digest of the OS process start token. An agent
receives an `agent_ref` only while the daemon has observed one unique live process for that agent
kind. Ambiguous or temporarily unverifiable occupants report `identity: inferred` and omit it.

`agent get` can inspect an inferred current agent by pane ID. Exact `agent read` and `agent wait`
fail with `exact_identity_unavailable` until a unique live process identity is available. Hooks are
still required for accurate prompts, completion times, and waiting states, but are not the source of
exact process identity. Pinning both the process and the canonical epoch prevents a same-kind direct
replacement from being misidentified as the previous occupant.

Process scans can temporarily make a live occupant `inferred`; while that lasts, list/get omit its
`agent_ref` and a new exact read/wait fails with `exact_identity_unavailable`. An already-running
wait keeps its pinned state ID, epoch, and process identity through that observation gap. It never
retargets, and a subsequently verified replacement fails with `stale_reference`.

A wait may also start after the pinned process has exited but before its completion is recorded.
It proceeds while the retained state ID, epoch, and process identity still match, and rejects the
operation only when a different live process is verified. This lets a delayed completion event
resolve the original occupant without ever rebinding the wait to its replacement.

`agent wait` subscribes to daemon revisions and never polls topology. The initially resolved exact
occupant stays pinned. `done` follows the baseline run through `run_seq` and `completed_seq`, so a
completion remains detectable after it is read or after the next run starts. Identity-bearing
transition history preserves transient `blocked`, `working`, and `idle` matches across coalesced
snapshots. If bounded history can no longer prove a transient result, the command fails with
`event_history_lost` instead of silently timing out.

If `--until` is omitted, the completion set is `done,blocked`. The initial state is tested before
waiting, so an already completed run matches `done` immediately. Pass `--after-completed-seq N` to
require completion sequence `N + 1` or later instead of matching that existing completion. This
cursor is useful after `agent get`, because it closes the race between observing `completed_seq` and
starting the wait. The cursor form requires the exact `agent_ref` returned by that same `agent get`;
using a pane ID is rejected, so a replacement occupant cannot consume another agent's cursor. If
the pinned process exits after recording the requested completion but before subscription starts,
the retained canonical state can still satisfy the exact reference and cursor.

The wait result identifies the pinned occupant in `target`. `baseline_completed_seq` records the
input baseline, and `matched_completed_seq` is the safe cursor for a subsequent wait.
`match_source` distinguishes direct current-state evidence from a retained transition event;
`matched_state_revision` is the state version that supplied that evidence, while `matched_at` is
present only when an exact event time or completion time exists. `current_agent` is populated on a
best-effort live verification of the same exact occupant and is otherwise omitted. `waited_ms`
reports elapsed monotonic wait time rounded down to milliseconds. A durable completion may
therefore succeed after its process exits; callers can use `target.pane_ref` to inspect the terminal
pane without accidentally targeting a replacement agent.

## Terminal read

`pane read` and `agent read` are the only API commands that execute `capture-pane`.

- `--source latest` (default) returns the latest requested lines across history and the visible
  screen.
- `--source visible` limits the source to the current screen and still returns at most `--lines`.
- The default is 120 lines and the maximum is 2,000.
- At most 1 MiB is retained. On overflow, the latest UTF-8-aligned suffix is kept and `truncated` is
  true.
- `--ansi` preserves terminal escape sequences.

When `--ansi` output is truncated at the 1 MiB tail boundary, the retained suffix can begin inside
an escape sequence. Consumers must sanitize or reset terminal state before rendering it.

The live tmux server identity, pane ID, and pane PID are checked before and after capture. For
`agent read`, the live agent PID and OS start token are also checked before and after capture. The
daemon instance and canonical pane/agent identity are then checked again. A verified replacement
fails closed with `stale_reference`; a daemon restart or connection loss is reported as
`stale_daemon`, `daemon_unavailable`, or `daemon_stream_error`, depending on when it occurs.

## Errors

Every error contains a closed-enum `code`, human-readable `message`, and `retryable` boolean. Public
codes are:

- Input/identity: `invalid_arguments`, `invalid_target`, `invalid_reference`, `no_current_pane`,
  `pane_not_found`, `agent_not_found`, `exact_identity_unavailable`, `stale_reference`.
- Runtime: `tmux_server_unavailable`, `daemon_unavailable`, `daemon_not_ready`,
  `daemon_query_failed`, `daemon_stream_error`, `daemon_invalid_request`, `stale_daemon`, `timeout`,
  `event_history_lost`, `identity_verification_failed`, `control_unavailable`.
- Contract/resource: `protocol_mismatch`, `invalid_daemon_response`, `resource_limit`,
  `capture_failed`, `daemon_error`, `internal_error`.

`retryable` is true only for `exact_identity_unavailable`, `tmux_server_unavailable`,
`daemon_unavailable`, `daemon_not_ready`, `daemon_query_failed`, `daemon_stream_error`,
`stale_daemon`, `timeout`, `identity_verification_failed`, and `control_unavailable`.
`event_history_lost` is terminal for that wait baseline; start a new observation instead of retrying
the same reference and cursor.

## Query cost

`api snapshot`, `pane list/get/current`, and `agent list/get` project the daemon's cached canonical
snapshot. They do not run `list-panes`, inspect process trees, or capture terminal history. Each CLI
invocation still resolves the tmux server incarnation with one `display-message`; this is identity
verification, not topology polling. A single get currently receives the full cached snapshot before
projecting its result, so callers should avoid unnecessary high-frequency process loops.

`pane read` additionally invokes guarded `capture-pane`. Exact `agent read` and `agent wait` scan
the process table to verify the pinned process at their live verification fences; a waiting command
also receives each subscribed full snapshot. Do not use exact reads or waits as a high-frequency
polling loop: use one subscription wait and bounded reads at the points where terminal text is
actually needed.
