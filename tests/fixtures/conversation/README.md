# Conversation fixtures

All bodies and identifiers are synthetic. Their structure was checked against locally installed
Codex 0.162.1 and Claude Code 2.1.287; private transcripts are not copied into these fixtures.
The accepted hook payload supplies Codex `turn_id` or Claude `prompt_id`. See
[AGENT_API.md](../../../AGENT_API.md#conversation-context-api-v9) for the public contract.

| Fixture | Purpose |
| --- | --- |
| `codex-basic.jsonl` | User instructions versus injected context, duplicate items, images, reasoning and tool results |
| `codex-turns.jsonl` | Steer within one turn, interruption, mismatched final text, error and pending final |
| `codex-compaction.jsonl` | Effective replacement history, retained source references and unreadable summaries |
| `codex-rollback.jsonl` | Removal of the latest instruction and its response |
| `claude-basic.jsonl` | Tool results with original user role, parallel results, slash commands, notifications, queued input and errors |
| `claude-branch.jsonl` | Rewind selects the effective parent chain |
| `claude-compaction.jsonl` | Latest compaction summary and history boundary |
| `claude-preserved-anchor.jsonl`, `claude-preserved-tail.jsonl` | Both observed preserved-segment chain structures |
| `claude-pending-final.jsonl` | Final text candidates without a transcript completion marker |

`src/conversation/tests.rs` also builds malformed, missing, conflicting and unsupported records,
partial JSONL tails and stale turn attribution. Rollback follows upstream Codex
[`history.rs` at `rust-v0.162.1`](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/context_manager/history.rs#L756):
zero is a no-op; excess counts remove all effective instruction turns. A rollback that includes
excluded, unpaired or untraced boundaries is unavailable, because this endpoint cannot prove their effective
position. The source was inspected read-only through the opensrc cache. Unknown formats are unavailable rather than
silently treated as complete history. Codex fork/history-base and realtime sessions are unavailable
when their effective text cannot be reconstructed.

`../agent-conversation-response.json` is a real API envelope from a synthetic isolated session.
Its references are examples; consumers must resolve current exact references before use.
Run `python3 scripts/test-agent-conversation-isolated.py` from the repository root to test the
read-only command, hooks, daemon revisions, delayed transcript flush, repeated text in a later
turn, transcript-only subscription changes, timeout, cancellation and process replacement.
