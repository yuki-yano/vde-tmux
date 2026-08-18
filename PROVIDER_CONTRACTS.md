# Provider contract probes

## Codex 0.147.0

- Observed at: 2026-08-15 (Asia/Tokyo)
- Provider: `codex-cli 0.147.0`
- Source revision: `a4fb816a52fc4178ef3a01d285f0c6cc0191d7c0`
- Probe: `scripts/p0-provider-contract.sh`
- Current result: pass (20 hashed observations, no validation errors)
- Queue control: queue A ran `sleep 5` in a provider shell; queue B was added with
  the provider's explicit queue control while queue A was visibly running

Frozen observations:

- A single-line prompt and a three-line pasted prompt reached
  `UserPromptSubmit` with the exact expected UTF-8 byte length and SHA-256
  digest.
- A second prompt queued while the preceding turn was visibly running was
  observed after the preceding prompt, without reordering.
- `session_id` is stable across the session. All four Prompt and Stop observations carried a
  `turn_id`; the four Prompt turn IDs were distinct, and each Stop matched its Prompt turn in
  order. Gate 3 fails closed if any of those identity checks is absent.
- Each `Stop` payload contained a response equal to the response reconstructed
  from the provider transcript. The durable adapter therefore uses the completion
  payload as the Response Artifact candidate and records provider completeness.
- A failing sibling hook did not prevent the collector hook from observing
  `SessionStart`, four `UserPromptSubmit` events, four `Stop` events, and
  `SessionEnd`.

The probe stores only allow-listed hashes, byte lengths, versions, and event ordering.
Provider-native runtime data was removed after verification. The retained hashed result satisfies
the strengthened LF-count and no-callback-replay-through-`SessionEnd` gates.

## Claude Code

The isolated probe has not passed yet. Claude Code 2.1.227 did not reuse the
macOS login when `CLAUDE_CONFIG_DIR` pointed at the private scratch profile.
Do not run the probe against the persistent profile merely to bypass this gate;
the remaining work is an authenticated, disposable profile path.
The API v4 durable adapter therefore remains disabled for Claude Code. API v4 separately exposes
guarded terminal dispatch with lifecycle-cursor confirmation and bounded terminal read; this does
not claim durable provider attribution or Response Artifact support.
