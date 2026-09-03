# Agent JSON API

This document defines the current API v4 contract. The v4 mutation boundary and rollout gates are
maintained in [AGENT_API_V4.md](AGENT_API_V4.md). The durable state design inherited from v3 is
recorded in [AGENT_API_V3.md](AGENT_API_V3.md).

`vt` exposes a versioned JSON interface for terminal agents. The command tree is the public API.
Read-only topology and state commands coexist with exact-reference mutations for durable prompt
dispatch, guarded terminal input, pane split, and agent start. The daemon Unix-socket protocol is
internal and changes independently.

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

PANE_JSON="$(vt pane get %456 --json)"
PANE_REF="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.summary.pane_ref')"
SPLIT_JSON="$(vt pane split "$PANE_REF" --direction right --size-percent 50 --json)"
NEW_PANE_REF="$(printf '%s' "$SPLIT_JSON" | jq -r '.result.split.pane_ref')"
vt agent start "$NEW_PANE_REF" --agent claude --json

vt agent list --status working --json
vt agent list --needs-action --json
vt agent get %456 --json
vt agent wait %456 --until done,blocked,limited --timeout-ms 120000 --json

AGENT_JSON="$(vt agent get %456 --json)"
AGENT_REF="$(printf '%s' "$AGENT_JSON" | jq -r '.result.agent.summary.agent_ref')"
REQUEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/vt-request.XXXXXX")"
printf '%s' 'Review the current diff and report must-fix findings.' >"$REQUEST_DIR/prompt.txt"
PROMPT_JSON="$(vt agent request "$AGENT_REF" \
  --state-file "$REQUEST_DIR/request.json" \
  --prompt-file "$REQUEST_DIR/prompt.txt" --json)"
OPERATION_REF="$(printf '%s' "$PROMPT_JSON" | jq -r '.result.operation_ref')"
RUN_REF="$(printf '%s' "$PROMPT_JSON" | jq -r '.result.run_ref')"
vt agent run wait "$RUN_REF" --json
vt agent run response "$RUN_REF" --json
vt agent read %456 --source latest --lines 120 --json

CLAUDE_JSON="$(vt agent get %537 --json)"
CLAUDE_REF="$(printf '%s' "$CLAUDE_JSON" | jq -r '.result.agent.summary.agent_ref')"
SEND_JSON="$(printf '%s' 'Inspect the current diff.' \
  | vt agent send "$CLAUDE_REF" --stdin --json)"
SEND_BASELINE="$(printf '%s' "$SEND_JSON" | jq -r '.result.send.baseline_completed_seq')"
vt agent wait "$CLAUDE_REF" \
  --until working --until blocked --until done \
  --after-completed-seq "$SEND_BASELINE" --timeout-ms 10000 --json

# Best-effort input while the exact Codex or Claude occupant is working.
# A concurrent completion may cause this to start the next turn.
WORKING_REF="$(vt agent get %539 --json | jq -r '.result.agent.summary.agent_ref')"
printf '%s' 'Also check the error path.' \
  | vt agent steer "$WORKING_REF" --stdin --json

# For an independently resolved exact blocked occupant only:
BLOCKED_REF="$(vt agent get %538 --json | jq -r '.result.agent.summary.agent_ref')"
vt agent send-keys "$BLOCKED_REF" --key y --key Enter --json
```

`api snapshot` is the one-call inventory endpoint. It groups the live canonical panes, resolved
agent occupants, and daemon diagnostics from one snapshot revision. Prefer it over composing raw
`tmux list-panes` output with separate `pane list` and `agent list` calls, which can observe
different revisions. Use the narrower list commands when their filters are useful.

The snapshot does not inspect arbitrary paths supplied to `--prompt-file`. Those files are request
inputs owned by the caller, not transport or delivery state. `--state-file` is different: its path
is the caller-chosen intent handle, while vt exclusively owns its opaque contents and update order.
After dispatch, use the returned Operation, Run, or terminal-send receipt instead of prompt-file
metadata as the acceptance signal.

API commands always emit JSON. `--json` is accepted so callers can state the expected format. A
successful command writes one envelope to stdout. A failed command writes one error envelope to
stderr and exits non-zero. `api schema` uses the same success envelope and includes JSON Schemas for
the conceptual request command, success envelope, and error envelope.

```json
{
  "meta": {
    "api_version": 4,
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
ANSI, wait defaults are `done,blocked,limited` and 120,000 ms, read lines are 1..2,000, and wait timeout is
1..86,400,000 ms. Prompt confirmation defaults to 7,000 ms and is limited to 1..60,000 ms. Prompt
bytes are supplied out-of-band through stdin or a file and therefore do not appear in the conceptual
request schema. The `agent_request` schema includes `state_file` because the stable path identifies
one logical intent; it does not expose the input source or state contents. Repeated and comma-separated
`--until` argv forms normalize to the same set.
The prompt deadline covers the whole operation from daemon connection and preflight through digest
confirmation; it does not start only after submission.

## Repository category membership

The Category Agent API exposes the ordered catalog and repository membership without exposing
catalog mutation or manual ordering:

```bash
vt category list --json
vt category get --repo /absolute/project/path --json
vt category assign work --repo /absolute/project/path --json
vt category automatic --repo /absolute/project/path --json
```

`list` returns one-based `index`, `name`, `display_name`, the closed `source` enum
(`configured`, `dynamic`, or `system`), and `category_state_revision`. `get` returns the canonical
repository `key`, rule path, display name, effective category, and `explicit`. A Git main worktree
and linked worktrees that share the same common directory return the same repository key.

JSON `list` and `get` require an already-Serving daemon and never start it. All four commands
require the strictly loaded disk config to match the daemon's active config hash; a mismatch is
`stale_precondition` with reload guidance. A missing or non-directory path is `invalid_target`, and
failure to establish its canonical Git or path identity is `identity_verification_failed`.

`assign` sets an explicit membership in an existing category. `automatic` removes that override so
config rules, the configured default, and finally `Uncategorized` determine the effective
category. Both mutations ensure the daemon and return a `category_mutation` receipt containing
`accepted_seq`, canonical `repo`, typed `requested`, effective `before` and `after`, `changed`, and
the persisted `category_state_revision`. Reapplying the current explicit category or automatic
state succeeds with `changed: false` and does not advance the Category state revision.
`meta.snapshot_revision` is the daemon revision carried by the same mutation result.

An unknown category is `daemon_invalid_request` with `side_effect: none`. If the complete mutation
request was sent but its receipt could not be read, the result is `delivery_unknown` at
`after_dispatch`, with `side_effect: possible` and `retry_action: inspect_manually`. Do not resend
automatically. Run `category get` once to inspect current membership, while retaining that the
original receipt was not recovered.

Category creation, rename, deletion, catalog/repository ordering, category navigation, session
switching, and pane Agent operations are outside this API boundary. They do not gain JSON behavior
through the Category commands above.

`agent steer` accepts only an exact Codex or Claude occupant whose initial canonical status is
`working`. It uses the same guarded copy-mode exit, pane/process identity, and foreground input-owner
fences as `agent send`. It does not wait for provider hooks or prove active-turn attribution. Its
receipt therefore reports `dispatch=guarded_terminal_best_effort` and
`race_policy=may_start_next_turn`: a completion racing with input may make the text the next turn.
Success means tmux applied the input, not that the provider accepted it or interrupted the current
turn. `opencode` advertises `steer=disabled` until its behavior is verified.

## Agent state

The public `status` describes durable agent activity and is independent of the sidebar's unread UI
projection:

| Status | Meaning |
| --- | --- |
| `blocked` | The lifecycle is Waiting for user action, or Error |
| `limited` | The lifecycle is Waiting because provider usage is exhausted |
| `working` | A run is active |
| `done` | At least one run completed and no run is active, whether read or unread |
| `idle` | No run has started in the current agent epoch |

`badge` contains the current sidebar badge. A read completion therefore has `status: done`,
`badge: idle`, and `unread: false`. A limited agent has `status: limited`, `badge: limited`, and
`needs_action: false`. `needs_action` is derived from canonical triage state and does
not disappear merely because a pane is visible.

For Claude Code, `StopFailure(error=rate_limit)` projects the open run as Limited. Other official
`StopFailure` errors project it as Blocked with `lifecycle.state=error`; `error=overloaded` and a
529 overload have the stable reason `provider_overloaded`. Other hook reasons preserve the official
`error` value. A same-session failure received after canonical completion schedules one terminal
verification and opens a new failed run only when the current pane still confirms the failure. If
the hook is missing or lost, the daemon's five-second supplementary scan uses
`provider_api_error` for a provider-rendered `⏺ API Error:` only when its `· done` turn summary is
the latest semantic line before the input area. A later prompt, retry spinner, tool output, or
assistant output suppresses the inference. Neither path marks the run completed or automatically
retries it.

`agent list` returns present agents. Historical records retained for unread/sidebar behavior are
not reported as current occupants. Results are ordered by canonical pane identity; consumers must
not infer activity order from array position.

A tmux pane is emitted once per server even when its window is linked into multiple sessions.
`sessions[]` describes those session views; `window_active` and `window_last` apply to each view.
Pane-level `active` is the selected pane within the shared window, not a particular client's focus.
`pane current` and `pane read` with no target read `TMUX_PANE`; `pane current` returns the same
`pane_get` result shape as `pane get`.

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
transition history preserves transient `blocked`, `limited`, `working`, and `idle` matches across coalesced
snapshots. If bounded history can no longer prove a transient result, the command fails with
`event_history_lost` instead of silently timing out.

If `--until` is omitted, the completion set is `done,blocked,limited`. The initial state is tested before
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

## Guarded prompt dispatch

`agent request` is the normal durable submission command for an exact Codex occupant. Claude Code
remains visible through the legacy pane projection, but durable mutation is disabled until its
isolated provider contract probe passes. Give each new prompt intent a new file path inside a
caller-owned directory with mode 0700:

```bash
REQUEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/vt-request.XXXXXX")"
printf '%s' 'Review the current diff.' >"$REQUEST_DIR/prompt.txt"

PROMPT_JSON="$(vt agent request "$AGENT_REF" \
  --state-file "$REQUEST_DIR/request.json" \
  --prompt-file "$REQUEST_DIR/prompt.txt" \
  --json)"

# Resume the same logical request after CLI exit or response loss.
PROMPT_JSON="$(vt agent request "$AGENT_REF" \
  --state-file "$REQUEST_DIR/request.json" \
  --json)"
```

The first call requires `--stdin` or `--prompt-file`. A later call may omit the body. If it is
supplied again, its normalized digest must match. vt creates a 0600 state file and a stable 0600
sidecar lock, persists the generated Operation ID and body before daemon mutation, and uses that
same request for safe replay. Once an Operation receipt is known, vt records its reference and
removes the body, except for a retryable pre-dispatch timeout, which keeps the request active. The
state contents are opaque: callers choose and retain the path, but must not
read, edit, copy between intents, or reuse it for a new prompt. A private directory can be removed
after the Operation and any linked Run have reached the caller's required terminal state.

`request_state_busy` asks the caller to wait before reusing the same path.
`request_state_mismatch` and `request_state_invalid` are fail-closed request-validation errors and
must be corrected rather than retried as a different Operation. A receiptless transport error keeps
the request active; only an explicit invocation with the same state path replays the same ID/body.
Once the state holds an Operation reference, resume is observation-only and never terminal dispatch.

`agent prompt` is the lower-level primitive for callers that deliberately manage a stable
`operation_id` and a byte-identical private body source themselves. The daemon, not either CLI
surface, owns staging, fencing, and tmux dispatch:

```bash
OPERATION_ID="$(uuidgen)"
printf '%s' 'Review the current diff.' \
  | vt agent prompt "$AGENT_REF" \
      --operation-id "$OPERATION_ID" \
      --stdin \
      --json

vt agent prompt "$AGENT_REF" \
  --operation-id "$OPERATION_ID" \
  --prompt-file /tmp/review-request.txt \
  --json
```

The prompt is valid UTF-8, non-empty, NUL-free, and at most 65,536 bytes. One terminal LF or CRLF
from stdin or a prompt file is treated as a text-record terminator and removed before hashing and
dispatch; internal line breaks and an additional trailing line break are preserved. The prompt is
staged in a private 0600 body file before the durable Operation becomes `prepared`; prompt bytes
never appear in argv, the Operation record, JSON output, daemon errors, or logs. The stored request
identity contains only the exact target, domain-separated prompt digest, dispatch option, and caller
operation ID.

Before dispatch the daemon re-resolves the exact Agent Binding, requires healthy daemon-owned tmux
hooks, acquires the per-pane dispatch lock, and verifies that the agent process owns foreground
input. It then persists `dispatch_started` before spawning tmux. A matching `UserPromptSubmit` hook
creates the Run Record first and advances the Operation to `prompt_confirmed`.

If the exact Codex process is present but its startup `SessionStart` was not observed, the Operation
is staged with a pending provider session. The daemon still fences the exact pane, process, input
owner, and prompt digest. Only the first matching `UserPromptSubmit` from that same process may bind
the real session and confirm the Operation; an unbound session is never stored in a Run Record.

Codex may also emit `SessionStart` for a fresh provider session in the same TUI process as the
dispatched prompt arrives. That event advances the canonical Agent epoch and resets its run sequence.
The immediately following epoch's first `UserPromptSubmit` may still confirm the Operation only when
the server, pane instance, pane state, agent kind, process identity, prompt digest, and confirmation
window all match. Confirmation records the new provider session and epoch. A process replacement,
skipped epoch, later run in the new epoch, or prompt mismatch never crosses this fence.

The same `operation_id`, target, and prompt bytes are idempotent. A retry of an unexpired
`prepared` Operation resumes the guarded dispatch. An unattended `prepared` Operation is rejected
without a side effect when its original confirmation deadline expires. A settled Operation is
returned without another side effect. Reusing an ID with a different request returns
`operation_conflict`. After `dispatch_started`, restart or an ambiguous transport result advances
the Operation to `delivery_unknown` and never resends it automatically.

Use the durable references instead of pane capture to follow the result:

```bash
OPERATION_REF="$(printf '%s' "$PROMPT_JSON" | jq -r '.result.operation_ref')"
vt agent operation wait "$OPERATION_REF" --until prompt-confirmed --json

RUN_REF="$(vt agent operation get "$OPERATION_REF" --json | jq -r '.result.run_ref')"
vt agent run get "$RUN_REF" --json
vt agent run wait "$RUN_REF" --until completed --json
vt agent run response "$RUN_REF" --json
```

`delivery_unknown` is a typed ambiguous result. Do not call `agent prompt` with a new operation ID.
Use `agent operation get`; pass `--follow-unknown` only when waiting for a possible late matching
provider hook. `agent prompt` and `agent operation wait` return non-zero typed error envelopes with
the durable Operation receipt for `delivery_unknown` and `rejected`; `agent operation get` remains
a successful state query. `agent run response` reads the bounded provider Response Artifact and
never falls back to terminal capture.

`agent list` and `agent get` expose the exact occupant's current durable `run_ref`, execution phase,
semantic outcome, and public status. This is also how an agent discovers a manually-started Run.
Historical Runs remain available to `run get` and `run wait` while retained. Run execution and
semantic completion are separate: an ended process may remain `ended_unconfirmed` until a late
provider completion or explicit operator recovery resolves it.

Recovery uses a two-step compare-and-swap flow:

```bash
vt agent run check "$RUN_REF" --json > /tmp/run-check.json
vt agent run resolve "$RUN_REF" \
  --outcome completed \
  --precondition-file /tmp/run-check.json \
  --resolution-id "$(uuidgen)" \
  --reason 'Provider completion was lost after the exact process exited.' \
  --json
```

`check` only accepts the Run identified by the Pane's current durable-run pointer. Historical Runs
are read-only after occupant replacement. For the current Run, `check` observes Run, Pane, and exact
process state twice, two seconds apart. It issues a 60-second precondition for a stable absent or
replaced process, or for the exact foreground process when the ANSI-free visible viewport and pane
dimensions are unchanged across both observations. The latter is content-agnostic: it does not
recognize provider prompt text and never infers semantic completion. The operator still decides
whether the unchanged screen is sufficient evidence.

`resolve` revalidates the generation, complete binding, Run revision, evidence digest, Pane state
ID/revision/current Run pointer/lifecycle/subagent count, expiry, fresh process ownership, and any
viewport fingerprint inside the daemon sequencer. It stores the auditable `operator_completed` Run
first, then projects the completed current Run to the Pane. A matching resolution retry repairs a
failed Pane projection without creating another resolution. It never sends a key or silently
retargets a replacement.

Provider adapters project only bounded, normalized UI previews into PaneState v10: a manually entered
prompt can feed the sidebar prompt and task-summary context, and a completion can feed the latest
response preview. The full response body remains available solely through the explicit bounded
`agent run response` read. A prompt linked to guarded dispatch is omitted from PaneState and every
public snapshot; its private body contract is unchanged.

## Storage status and offline reset

`agent storage status` reports the private state generation, format version, bounded usage, and
in-flight counts:

```bash
vt agent storage status --json
```

When the store cannot accept new records or a crash left a durable `resetting` marker, reset is an
explicit offline operation. Stop the daemon and quiesce durable work first, then bind the request to
the generation returned by the last status response:

```bash
vt agent storage reset \
  --expected-generation "$GENERATION" \
  --confirm-reset \
  --json
```

Reset fails closed if the recorded daemon is live, a supported-provider process still occupies the
tmux server, or the store contains an active Run or in-flight Operation. It writes a durable reset
marker first and can resume the same generation-bound reset after interruption. It does not decode
old formats, migrate references, or fall back to another state root.

If metadata is missing, corrupt, or from an unsupported future format, the generation-bound command
cannot authenticate the old generation. After the same daemon and supported-provider quiescence
checks, explicitly discard that unreadable private state instead:

```bash
vt agent storage reset \
  --recover-uninitialized \
  --confirm-reset \
  --json
```

This recovery does not decode or migrate the unreadable state. It creates only the exact private
state root and its four `0700` regions when they are absent; existing paths must pass ownership,
mode, and symlink validation. It then clears only those four bounded regions and publishes a fresh
generation last. It refuses `--recover-uninitialized` when metadata is valid; use the generation-
bound reset in that case.

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

Every error contains a closed-enum `code`, human-readable `message`, `stage`, `side_effect`, and
`retry_action`. Mutation errors may also contain a receipt. Public codes are:

- Input/identity: `invalid_arguments`, `invalid_target`, `invalid_reference`, `no_current_pane`,
  `pane_not_found`, `agent_not_found`, `exact_identity_unavailable`, `stale_reference`.
- Runtime: `tmux_server_unavailable`, `daemon_unavailable`, `daemon_not_ready`,
  `daemon_query_failed`, `daemon_stream_error`, `daemon_invalid_request`, `stale_daemon`, `timeout`,
  `event_history_lost`, `identity_verification_failed`, `control_unavailable`.
- Contract/resource: `protocol_mismatch`, `invalid_daemon_response`, `resource_limit`,
  `capture_failed`, `daemon_error`, `internal_error`.
- Durable state: `operation_conflict`, `operation_not_found`, `operation_store_full`,
  `operation_generation_replaced`, `request_state_busy`, `request_state_mismatch`,
  `request_state_invalid`, `run_not_found`, `run_generation_replaced`, `run_unresolved`,
  `run_already_resolved`, `target_replaced`, `unsupported_provider`, `provider_event_conflict`,
  `recovery_not_allowed`, `stale_precondition`, `resolution_conflict`,
  `storage_capacity_exceeded`, `state_uninitialized`, `artifact_unavailable`, `artifact_expired`.
- Prompt mutation: `agent_busy`, `agent_blocked`, `agent_limited`, `prompt_confirmation_unavailable`,
  `agent_not_input_owner`, `prompt_dispatch_busy`, `dispatch_rejected`, `delivery_unknown`.

`stage` is one of `request_validation`, `target_resolution`, `observation`, `before_dispatch`,
`dispatch`, or `after_dispatch`. `side_effect` is `none`, `possible`, or `confirmed`.
`retry_action` is a closed enum:

| Action | Caller behavior |
| --- | --- |
| `retry_same_request` | The operation failed before any side effect; the same request may be retried |
| `refresh_target` | Resolve a new reference before retrying |
| `wait_then_retry` | Preserve the request and retry only after capacity/state changes |
| `restart_observation` | Establish a new daemon observation/baseline |
| `inspect_manually` | A side effect may have happened; do not resend automatically |
| `never` | Fix the request/configuration instead of retrying |

`event_history_lost` requires a new observation. `delivery_unknown` always requires manual
inspection; it is never permission to resend the prompt.

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

The daemon accepts at most 64 simultaneous socket handlers. At most 48 may be streaming
subscriptions, reserving 16 slots for short queries and hook/mutation traffic. Overload is returned
as `resource_limit` with `wait_then_retry` without spawning another handler thread. The daemon
durable Run and Operation waits use one-request query connections rather than streaming slots;
their reconnect interval backs off from 50 ms to a one-second ceiling instead of polling at 20 Hz
for the full timeout. The daemon
observation capture coordinator has a separate bounded queue of eight requests; this does not bound
the direct `capture-pane` subprocess used by `pane read` and `agent read`. Each daemon observation
tmux process drains all output but retains at most 8 MiB stdout and 64 KiB stderr, and one coalesced
observation group retains at most 16 MiB of parsed pane tails. Exceeding any of those bounds produces
a typed capture output-limit failure.

## Definition of Done for API v4 rollout

The measurable functional, test, and operational completion checklist is maintained in
[AGENT_API_V4.md](AGENT_API_V4.md#definition-of-done). The API is not rollout-complete while any
item in that checklist remains unchecked.
