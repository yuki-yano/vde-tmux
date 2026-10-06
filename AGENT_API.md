# Agent JSON API

This document defines the current API v6 contract (daemon protocol 31, Pane State schema 10,
Question sidecar schema 1). CLI, daemon, and sidebars must be installed together; there is no
mixed-version fallback. The inherited v4 mutation boundary and rollout gates are maintained in
[AGENT_API_V4.md](AGENT_API_V4.md). The durable state design inherited from v3 is recorded in
[AGENT_API_V3.md](AGENT_API_V3.md).

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

## Response envelope

API commands always emit JSON. `--json` is accepted so callers can state the expected format. A
successful command writes one envelope to stdout. A failed command writes one error envelope to
stderr and exits non-zero. `api schema` uses the same success envelope and includes JSON Schemas for
the conceptual request command, success envelope, and error envelope.

```json
{
  "meta": {
    "api_version": 6,
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
`needs_action: false` unless it also has an unacknowledged question notice. `needs_action` is the union of current Blocked panes and unacknowledged question-notice panes.
It excludes the two-poll visual TRIAGE retention after Blocked clears and does not disappear
merely because a pane is visible. Badge totals remain mutually exclusive; notices are not an
additional execution status.

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
the server, pane instance, pane state, agent kind, process identity, prompt digest, and observation
time lower bound all match. The 10-second uncertainty deadline does not limit this adjacent-epoch
confirmation either. Confirmation records the new provider session and epoch. A process replacement,
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

`delivery_unknown` is a typed ambiguous result. Do not call `agent prompt` with a new operation ID
until delivery is confirmed or an operator follows the [manual fence release](#delayed-prompt-confirmation-and-manual-fence-release) procedure.
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

### Delayed prompt confirmation and manual fence release

A durable Operation becomes `delivery_unknown` after its 10-second confirmation deadline. That
deadline reports uncertainty; matching evidence remains valid afterward. The first
`UserPromptSubmit` with the expected Binding, run sequence, and prompt digest links the Run and
confirms the Operation even after a long queue or compaction delay. Its Response Artifact carries
the same `operation_id`. Late confirmation uses `confirmation_basis=binding_sequence_digest`;
`source_attribution=non_exclusive` still applies because hooks do not identify the sending Operation.

```bash
vt agent operation wait "$OPERATION_REF" --follow-unknown --timeout-ms 300000 --json
```

The caller's wait deadline does not cancel delivery or prevent later confirmation. Inspect the same
Operation again after a timeout. Do not resend while delivery is ambiguous. Idle, an unrelated Run,
or its completion alone cannot prove that a queued prompt will never run, so they do not release
the fence automatically.

If inspection cannot establish a matching Run, an operator can explicitly release the dispatch
fence. Inspect the provider's pending input and confirm its queue is empty before sending new work.
Read the current Operation revision and document the inspection reason:

```bash
vt agent operation get "$OPERATION_REF" --json > operation.json
REVISION="$(jq -r '.result.operation.revision' operation.json)"
vt agent operation abandon "$OPERATION_REF" --expected-revision "$REVISION" \
  --reason 'Inspected the completed Run and queued input' --json
```

`abandon` accepts only an unlinked `delivery_unknown` Operation. It checks the exact reference,
generation, and revision, persists `result_receipt.code=operator_abandoned` with
`source_attribution=operator:REASON`, and releases only that Operation's fence. Reasons must contain
1–247 UTF-8 bytes, must not be blank, and must contain no control characters. Repeating the original
revision and reason returns the same record; changed revisions or reasons are rejected.
If a Run was already saved with that `operation_id` but confirmation failed before updating the
Operation, abandonment is rejected. Retry the provider observation or restart the daemon to finish
that confirmation; the persisted Run link is authoritative.

Delivery remains `delivery_unknown`: abandonment neither confirms acceptance nor cancels queued
input. It permits a new dispatch but can lead to duplicate work if the old prompt is still queued.
If the old queued prompt A and new Operation B have identical digests, A's Run can confirm B:
the hook cannot distinguish the sender, and B's own execution may remain unlinked. If their digests
differ, A can consume B's expected run sequence; B's later Run then remains unlinked and B may also
require manual abandonment. An idle display alone does not establish that the provider queue is empty.
The abandoned Operation is never redispatched or automatically linked to a later Run, and its fence
stays released after daemon restart. `operation wait --follow-unknown` ends on abandonment with a
`delivery_unknown` error, `side_effect=possible`, and the `operator_abandoned` receipt.

Retained dispatched Operations provide digest/Binding evidence for prompt redaction and automatic
input classification even after abandonment or an interleaved Run. This evidence never links an
abandoned Operation or releases a newer fence. It contains no prompt body and is rebuilt on restart.
Because hooks do not identify input provenance, a later identical prompt from the same owner is
conservatively kept private; matching automatic-resume text remains non-authoritative for Question
acknowledgement and does not replace the task context. This classification lasts while the Operation
is retained and its Binding matches; process/pane replacement is excluded.
Operations have no automatic garbage collection; they persist until an explicit storage reset
and are bounded by the 65,536-record limit. Classification is therefore effective for the matching
owner's lifetime. If automatic-resume text is configured to a short phrase such as `continue`, a
human later typing the same phrase also receives this conservative classification: it cannot
acknowledge a Question, replace the task context, or appear in the public prompt preview.

## Guarded terminal input

`agent steer` accepts only an exact Codex or Claude occupant whose initial canonical status is
`working`. It uses the same guarded copy-mode exit, pane/process identity, and foreground input-owner
fences as `agent send`. It does not wait for provider hooks or prove active-turn attribution. Its
receipt therefore reports `dispatch=guarded_terminal_best_effort` and
`race_policy=may_start_next_turn`: a completion racing with input may make the text the next turn.
Success means tmux applied the input, not that the provider accepted it or interrupted the current
turn. `opencode` advertises `steer=disabled` until its behavior is verified.

Claude reads pasted image paths asynchronously and discards an Enter that arrives meanwhile. For a
Claude `agent send` or `agent steer` whose prompt has a line, or text after a space before an
absolute path, ending in `.png`, `.jpg`, `.jpeg`, `.gif`, or `.webp`, vt pastes first, waits up to
10 seconds for Claude's input field to change and leave its `Pasting…` state, and then sends Enter
under the same guards. If the input does not settle or the Enter guard fails, the prompt remains
pasted without Enter and the command fails with `delivery_unknown`; inspect the pane instead of
resending.

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
- Prompt mutation: `agent_busy`, `agent_blocked`, `agent_limited`, `agent_not_ready`,
  `prompt_confirmation_unavailable`, `agent_not_input_owner`, `prompt_dispatch_busy`,
  `dispatch_rejected`, `delivery_unknown`.

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

## Question notices

Stock Codex's successful `request_user_input_async` PostToolUse issues a notification.
`PaneSummary` and `AgentSummary` expose `question_notice` for live Codex panes, with
`unacknowledged`, `owner_ref`, `latest_order`, `acknowledged_order`, `last_issued_at`,
`tracking_health` (`healthy` / `degraded`), and `reason`. No question text, answers, tool identifiers,
or pending count is exposed. Absence of a notice is not proof that there are no unanswered questions.
Only Embedded mode is supported; shared/remote app-server ancestry cannot identify a pane owner.

```bash
PANE_JSON="$(vt pane get %456 --json)"
PANE_REF="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.summary.pane_ref')"
OWNER_REF="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.summary.question_notice.owner_ref')"
ORDER="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.summary.question_notice.latest_order')"
vt pane question-notice ack "$PANE_REF" --owner-ref "$OWNER_REF" --through-order "$ORDER" --json
```

Use one snapshot for all three arguments. The exact owner is the server incarnation, PaneInstance,
and Codex PID/start token; agent epoch/session changes do not invalidate it. The daemon rechecks
process ownership. Future orders, replaced owners, and unverified owners are rejected. Repeating
the same acknowledgement is harmless. Newer notices stay visible. Acknowledgement does not send
input, read an unread occurrence, or change lifecycle/Run completion. Skip, focus, and turn
completion alone never acknowledge a notice.

### Reply acknowledgement

A complete accepted `<send_user_message_question_reply>` envelope from any Codex client version
acknowledges its matching question IDs without waiting for Idle/Done or another turn. The parser
accepts one reply object or a nonempty array, optionally after the standard IDE context prefix.
Each canonical `questionItemId` is `["request_user_input_async", call_id, question_index]` serialized
as compact JSON; question and answer must be strings. Only hash evidence enters daemon metadata.
The reply must pass the current embedded parent process, exact pane/owner/session, and generation
checks. Reply parsing and acknowledgement do not require a known rendering profile
or a client-version lookup. Known issued IDs do not require a startup hook in the current daemon
or a complete home-wide history. Resume and daemon restart retain their ID bindings and partial
answers. Unknown IDs, wrong sessions, malformed or quoted envelopes, and automatic capacity
inputs never acknowledge a notice.

ID/order bindings are frozen at acceptance. A newly issued question cannot make an unknown ID
valid, and consumed IDs cannot be rebound. The bounded queue holds 16 waiting jobs and one
executing job. Each dequeued attempt has 1.5 seconds for one fresh exact owner check and transfer;
the serial mutation then checks the canonical process/session binding and deadline before saving.
There is no home-journal acquisition, turn-order read or screen capture in this reply path.
Transient owner/transfer failures can retry the same frozen evidence at most twice; prompts are
never resent. Owner/session replacement, unknown items and persistence failure retain notices.

Issued item counts come from structured `questions[]`, independently of body fingerprints.
The private sidecar retains only hashed session/item/call identities, orders and pending bits.
Partial and reverse-order answers are saved immediately and survive daemon restart. All items in
an issuance must be answered. A fully answered later issuance is resolved independently of an
older pending issuance; the older notice remains visible. `acknowledged_order` is the
contiguous confirmed prefix, and Q explicitly acknowledges through its fixed snapshot order.
Notices without issued-ID metadata stay unresolved and are never reconstructed from a screen or
ordinary prompt; they remain available for Q. Question and answer bodies are never persisted or
exposed by this metadata.

### Ordinary-prompt acknowledgement

A later accepted ordinary input in a different turn of the same trusted session can acknowledge a
contiguous notice prefix, after structural completion/start ordering, exact identity, the home loss
journal, Idle/Done, and two known normal-composer captures all pass. Direct and accepted queued
inputs share this policy; queue registration and unrecognized answer framing never resolve
notices. Capture only vetoes acknowledgement.

Question text matching is restricted to the exact 0.159.3/0.160.0 profiles. The older 0.155.1/0.156.1
profiles retain their generic marker and normal-composer guards; their stock hook schema uses the
same `questions[].title` and optional string-array `options`. Runtime text evidence is bounded to
2048 issuance entries globally and 512 per owner, with at most 512 distinct fingerprints per
capture. Fingerprint extraction accepts at most eight questions, sixteen choices each, and 16 KiB
of raw text. Larger or malformed text evidence becomes unavailable; an owned question notice is
still accepted and retained for manual acknowledgement.
Repeated fingerprints are deduplicated only after every unacknowledged order and session has been
checked. Missing entries, capacity exhaustion, clipped cards and restart never justify clearing.

### Persistence and limits

Bounds are 2048 tracked issuances globally, 512 per owner, 8 items per issuance, 64 reply items
per envelope and 64KiB prompt bytes. Used call-ID hashes remain until owner death (4096 per owner,
65536 globally). Reusing a known call ID invalidates its ambiguous bindings. Capacity exhaustion
retains notices for Q. A pre-rename failure rolls back pending bits and write-backlog state and
marks only the affected pane degraded. A post-rename directory-fsync failure is a logical commit
with a durability diagnostic.

Notifications and their deduplication keys persist in private `question-notices-v1.json` under the
server incarnation state directory, independently of Pane State schema 10 and durable Runs. Limits
are 4096 keys/owner, 65536 total keys, 512 owners, and a 16 MiB sidecar. Confirmed dead owners are
reclaimed; acknowledged keys are retained until then. Invalid/version-mismatched sidecars are not
reset or overwritten automatically. Disk failure retains new notices in memory with
`persistence_pending`, with at most one retry per five seconds; a crash before retry can lose them.
Acknowledgement commits at atomic rename. A pre-rename failure retains the notice; a subsequent
directory-fsync failure is a logical acknowledgement with `question_ack_directory_fsync_failed`,
without rollback or automatic rewrite. The private `.expected` marker distinguishes initial absence
from loss of a previously committed sidecar. Resolver state, transcript cursors, and ingress dedup
remain memory-only; the shared private home journal stores only digests and writer identities.

## Codex screen evidence

`badge` can be `unknown`. It represents current presentation; `status`,
`lifecycle`, and `agent wait` continue to describe canonical lifecycle. A Codex pane
without authoritative lifecycle hooks can therefore have `status: idle` and
`badge: working`, `blocked`, or `unknown`. `--status working` and
`agent wait --until working` do not match screen activity alone.

The existing capture batch supplies finite Codex evidence. A fresh live activity timer
(including dynamic labels, remapped interrupt keys, and queued inputs) gives Working;
a current approval or synchronous question gives Blocked and `needs_action: true`.
An asynchronous question can coexist with Working and does not alone imply Blocked.
Unknown UI, transcript viewers, capture failure, and evidence older than three seconds
produce Unknown when no canonical state takes priority. Existing unread completion
remains Done. Authoritative hooks and canonical active/waiting/error states take priority.
Unknown uses `?` and the neutral Idle color, and is visible even when Idle is hidden.

Evidence lives only in the runtime tracker and is invalidated on epoch/process replacement,
failed observation, or expiry. It does not issue a new Run, complete a Run, confirm a prompt,
acknowledge a Question notice, or generate an OS notification/triage event. The 300-second
stale-completion rule for open non-authoritative runs still applies. A screen modal alone never
creates a Codex run.

Agent summaries include a `presentation` object: `reason` is a finite
code, `observed_at` is the optional Unix timestamp of the screen observation, and `ttl_seconds`
is its optional lifetime (three seconds for Codex screen evidence). The timestamp remains
available after expiry for diagnosis, but positive evidence cannot be used after its deadline.
Clients can compute age from `meta.emitted_at`; no screen text or activity title is included.
Reasons distinguish canonical/hook authority, current screen Working/approval/question,
directory trust/startup update, unread completion, unknown UI, transcript viewer, unavailable
or expired evidence, and an epoch mismatch. `badge` and its reason come from the same decision.
Directory-trust and startup-update screens affect presentation only and cannot change a Run or
Question notice.

Screen evidence does not change Question notices: capture only vetoes acknowledgement, and
unrecognized answers or unknown evidence never acknowledge a notice.

OS notifications remain tied to canonical Blocked transitions. Before starting the external
notification command, the daemon rechecks the original pane and Blocked occurrence. Resolved,
replaced, removed or superseded occurrences are skipped; unrelated metadata/read revisions
do not cancel an otherwise current notification. State changes after this check cannot retract
an accepted notification.

### Hook ownership and first prompt

Codex hooks require a process ancestor chain rooted in their claimed pane. Shared app-server
hooks, including renamed executables, cannot use inherited `TMUX_PANE` to update that pane
or another pane. Rejection logs contain only finite reason codes. Embedded hooks remain
usable with an unrelated MCP server child. This prevents misattribution; it does not restore
lifecycle events from a shared server. Start Codex with `--no-daemon` to select the embedded hook
lifecycle; `features.daemon_auto_start=false` does not prevent attaching to an already running
server. Hook authority is not inferred to have expired merely because events stop arriving.

Working/Blocked dispatch is rejected. Durable Codex dispatch normally requires authoritative hooks
in the current daemon epoch. The first prompt is also accepted before any session/hook is
registered when all of the following hold: the exact process is scan-verified and owns foreground
input, canonical state is Idle with zero Runs/completions and no prompt/session, argv proves an
explicit `--no-daemon` interactive invocation without a queued initial prompt, and a fresh stable
viewport/cursor identifies the empty input field. These checks run during preparation and again
before dispatch; the pane revision fence is rechecked after inspection. Presentation remains
Unknown until a hook is accepted. Only the matching provider prompt digest confirms the Operation
and binds its session/Run. Bare invocations, `daemon_auto_start=false` alone, dialogs, drafts, and
changed processes do not qualify. Current empty-input detection supports Codex's
`Ask Codex to do anything` composer; an unrecognized layout is rejected without sending.
The first `SessionStart` may advance the agent epoch. Follow the confirmed Operation's
returned `run_ref` for wait/response, rather than continuing to use the pre-start agent reference.

In embedded mode, a subsequent accepted lifecycle hook restores authority after daemon restart.
Saved exact Question IDs can be acknowledged without restoring ordinary-resolver trust.

### Restart readiness

For an existing canonical Idle Codex session, the observation worker identifies the exact process's
single writable rollout descriptor (macOS lsof or Linux procfs). It verifies PID/start token,
explicit independent argv, current session, file identity and complete bounded structural history.
Only a latest normal completion with no open turn supplies transient `provider_resynchronized`
presentation evidence. Old completed files, ambiguous descriptors, partial history and a different
session do not. This changes neither hook authority nor canonical Run completion.
Every durable prompt checks a fresh empty composer, cursor, foreground owner and current session;
resynchronized sessions also reread structural state immediately before dispatch. Drafts, current
queue headers, blocked/working screens and unresolved dispatches prevent sending.
A readiness rejection is `agent_not_ready` (before dispatch, wait then retry), not `stale_reference`.
Shared invocations require moving to an independently owned Codex session. No process is restarted
and no saved Run or operation is abandoned to recover readiness.

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

## Rollout checklists

These checklists record rollout completion and are not part of the API contract.

### API v4

The functional, test, and operational checklist is maintained in
[AGENT_API_V4.md](AGENT_API_V4.md#definition-of-done). The API is not rollout-complete while any
item in that checklist remains unchecked.

### Question notices (introduced in API v5)

#### Functional completion

- [x] Existing stock Codex PostToolUse detects issuance and a fenced acknowledgement clears only displayed notices.
- [x] Sidebar/API agree on current needs-action membership; lifecycle, unread, Runs, statusline, and OS notifications retain their contracts.

#### Test completion

- [x] Parser/owner guards, restart/dedup, persistence failure, capacity, stale/future fences, and multi-client rendering pass.
- [x] Isolated runtime smoke, UI preflight, and real stock Codex Embedded hook acceptance pass; fixture and real-CLI evidence are recorded separately.

Verified on 2026-09-20. The stock CLI 0.155.1 Embedded acceptance covered real issuance,
answer/skip retention, and physical/remote `Q` across two sidebars. The separate
`scripts/test-question-notice-isolated.py --extended` fixture measures API and two-sidebar
frames with 58 agent panes, two attached clients, and 100 issue/ack cycles at `poll_ms=1000`.
Timing begins before launching the hook process or sending the Q control request, so it
includes more work than the daemon-ingress bound; it excludes Codex's pre-hook delay.

#### Operational rollout

- [ ] CLI/daemon/sidebar are deployed together with API 6 / protocol 31 while retaining existing Pane State schema 10.
- [ ] Stock Codex version, Embedded mode, hook matcher, and post-restart notice behavior are verified in the deployment environment.
