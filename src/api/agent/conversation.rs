use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::super::common::{epoch_now, success_json};
use super::super::connection::{ApiConnection, daemon_api_error};
use super::super::contract::{
    AgentStatus, AgentWaitTarget, ApiError, ApiErrorCode, ApiResult, LifecycleSummary,
    MAX_WAIT_TIMEOUT,
};
use super::guards::{
    AgentIdentity, require_same_agent, resolve_agent, verify_live_agent_process, wait_target,
};
use super::projection::{agent_summary, canonical_state};
use crate::conversation::{self, BoundConversation, MessageKind, MessageRole, ParsedConversation};
use crate::daemon::protocol::v2::{
    ClientMessage, PROTOCOL_VERSION, ResolvedSnapshot, ServerMessage,
};
use crate::tmux::TmuxRunner;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConversationUpdate {
    Snapshot,
    Unchanged,
    Replace,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ConversationSnapshot {
    pub target: AgentWaitTarget,
    pub session_id: String,
    pub run_seq: u64,
    pub completed_seq: u64,
    pub provider_turn_id: Option<String>,
    pub status: AgentStatus,
    pub lifecycle: LifecycleSummary,
    pub context_revision: String,
    pub cursor: String,
    pub update: ConversationUpdate,
    pub read_bytes: u64,
    pub stop_boundary: bool,
    pub history: ParsedConversation,
}

fn unavailable(message: impl Into<String>) -> anyhow::Error {
    ApiError::new(ApiErrorCode::ConversationUnavailable, message).into()
}
fn changed() -> anyhow::Error {
    ApiError::new(
        ApiErrorCode::ContextChanged,
        "conversation identity, turn, or transcript changed during the read",
    )
    .into()
}

fn binding(connection: &ApiConnection, identity: &AgentIdentity) -> Result<BoundConversation> {
    let mut query = connection.reconnect()?;
    match query
        .client
        .request(&ClientMessage::QueryConversationBinding {
            proto: PROTOCOL_VERSION,
            pane_instance: identity.pane_instance.clone(),
        })? {
        ServerMessage::ConversationBindingResult {
            binding: Some(binding),
        } => Ok(binding),
        ServerMessage::ConversationBindingResult { binding: None } => Err(unavailable(
            "no accepted provider hook has bound this conversation",
        )),
        ServerMessage::Error { code, message, .. } => Err(daemon_api_error(code, message).into()),
        _ => Err(ApiError::new(
            ApiErrorCode::InvalidDaemonResponse,
            "unexpected conversation binding response",
        )
        .into()),
    }
}

fn matches_binding(
    pane: &crate::daemon::protocol::v2::PanePresentation,
    bound: &BoundConversation,
) -> bool {
    pane.resolved.as_ref().is_some_and(|r| {
        let state = &r.canonical;
        state.state_id == bound.state_id
            && state.agent_epoch == bound.agent_epoch
            && state.run_seq == bound.run_seq
            && state
                .agent_session_id
                .as_ref()
                .is_some_and(|id| id.as_str() == bound.session_id)
    })
}

fn verify_bound_writer(
    runner: &dyn TmuxRunner,
    pane: &crate::daemon::protocol::v2::PanePresentation,
    bound: &BoundConversation,
) -> Result<()> {
    if pane
        .resolved
        .as_ref()
        .is_some_and(|r| r.canonical.agent.as_str() == "codex")
    {
        let process = pane.agent_process.as_ref().ok_or_else(changed)?;
        let locator = runner
            .codex_active_transcript(process, &bound.session_id)
            .map_err(|_| unavailable("exact live Codex root transcript writer is unavailable"))?;
        if bound.hook.path.canonicalize().ok().as_ref() != Some(&locator.transcript)
            || locator.dev != bound.hook.device
            || locator.ino != bound.hook.inode
        {
            return Err(changed());
        }
    }
    Ok(())
}

fn verify_turn(
    parsed: &ParsedConversation,
    bound: &BoundConversation,
    done: bool,
    provider: &str,
) -> Result<()> {
    let turn = parsed
        .turns
        .last()
        .ok_or_else(|| unavailable("latest provider turn is unavailable"))?;
    if bound.run_seq > 0 && bound.hook.provider_turn_id.is_none() {
        return Err(unavailable("accepted hook has no provider turn identity"));
    }
    if bound.hook.provider_turn_id.is_some() && turn.provider_turn_id != bound.hook.provider_turn_id
    {
        return Err(unavailable(
            "provider turn does not match the accepted hook",
        ));
    }
    if let Some(expected) = &bound.hook.prompt_digest {
        let latest = parsed
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User && m.kind == MessageKind::Instruction)
            .ok_or_else(|| unavailable("latest user instruction is unavailable"))?;
        if !latest.text_complete
            || latest.turn_index != turn.turn_index
            || !turn.user_source_refs.contains(&latest.source_ref)
            || &conversation::digest(latest.text.as_bytes()) != expected
        {
            return Err(unavailable(
                "latest user instruction does not match the accepted hook",
            ));
        }
    } else if bound.run_seq > 0 {
        return Err(unavailable(
            "latest user instruction has no accepted boundary",
        ));
    }
    if done {
        if bound.stop_bytes.is_none() {
            return Err(unavailable(
                "normal completion has no matching provider Stop boundary",
            ));
        }
        let final_ref = turn
            .final_source_ref
            .as_ref()
            .ok_or_else(|| unavailable("latest final response is unavailable"))?;
        let final_message = parsed
            .messages
            .iter()
            .find(|m| &m.source_ref == final_ref)
            .ok_or_else(|| unavailable("latest final response is unavailable"))?;
        let expected = bound
            .hook
            .response_digest
            .as_ref()
            .ok_or_else(|| unavailable("Stop has no complete response evidence"))?;
        // Claude 2.1.287 builds Stop text by joining text blocks with LF then trimming.
        // Keep the full transcript text in the response; normalize only the hook proof.
        let stop_text = if provider == "claude" {
            final_message.text.trim()
        } else {
            &final_message.text
        };
        if !final_message.text_complete
            || final_message.turn_index != turn.turn_index
            || final_message.kind != MessageKind::Final
            || &conversation::digest(stop_text.as_bytes()) != expected
        {
            return Err(unavailable("final response does not match Stop evidence"));
        }
    }
    Ok(())
}

fn context_revision(session: &str, history: &ParsedConversation) -> Result<String> {
    let semantic_messages: Vec<_> = history
        .messages
        .iter()
        .filter(|m| m.kind != MessageKind::ToolCall)
        .collect();
    let semantic_omissions: Vec<_> = history
        .omissions
        .iter()
        .filter(|o| {
            o.reason != "incomplete_jsonl_tail"
                && !o.reason.ends_with("_excluded")
                && !history.messages.iter().any(|m| {
                    m.kind == MessageKind::ToolCall && o.source_ref.as_ref() == Some(&m.source_ref)
                })
        })
        .collect();
    Ok(format!(
        "vtc1:{}",
        conversation::digest(&serde_json::to_vec(&(
            session,
            semantic_messages,
            &history.turns,
            history.compacted,
            semantic_omissions
        ))?)
    ))
}

fn observation_key(
    connection: &ApiConnection,
    snapshot: &ResolvedSnapshot,
    identity: &AgentIdentity,
) -> Result<String> {
    let pane = require_same_agent(snapshot, identity)?;
    let state = canonical_state(pane).unwrap();
    let bound = binding(connection, identity)?;
    let metadata = conversation::file_stamp(&bound.hook);
    Ok(conversation::digest(&serde_json::to_vec(&(
        state.run_seq,
        state.completed_seq,
        &state.lifecycle,
        bound,
        metadata,
    ))?))
}

fn read_snapshot(
    runner: &dyn TmuxRunner,
    connection: &ApiConnection,
    before: &ResolvedSnapshot,
    identity: &AgentIdentity,
    include_tools: bool,
    since: Option<&str>,
) -> Result<(ApiConnection, ResolvedSnapshot, ConversationSnapshot)> {
    let pane = require_same_agent(before, identity)?;
    verify_live_agent_process(runner, identity, pane)?;
    let summary = agent_summary(pane, before, &connection.server_identity).unwrap();
    if !matches!(summary.agent.as_str(), "codex" | "claude") {
        return Err(ApiError::new(
            ApiErrorCode::UnsupportedProvider,
            "conversation supports only Codex and Claude",
        )
        .into());
    }
    let mut query = connection.reconnect()?;
    let bound = binding(&query, identity)?;
    if !matches_binding(pane, &bound) {
        return Err(changed());
    }
    verify_bound_writer(runner, pane, &bound)?;
    let state = canonical_state(pane).unwrap();
    let done = summary.status == AgentStatus::Done
        && state.run_seq > 0
        && state.completed_seq == state.run_seq;
    let stop = done.then_some(bound.stop_bytes).flatten();
    // Stop bytes are the observed lower boundary, not a flush guarantee.
    // Completion is pinned by provider turn + exact final digest below.
    let bytes = bound.hook.read().map_err(|e| {
        if e.0.ends_with("size_limit") {
            ApiError::new(ApiErrorCode::ResourceLimit, e.0).into()
        } else {
            unavailable(e.0)
        }
    })?;
    let mut history =
        conversation::parse(&summary.agent, &bound.session_id, &bytes).map_err(|e| {
            if e.0.ends_with("size_limit") {
                ApiError::new(ApiErrorCode::ResourceLimit, e.0).into()
            } else {
                unavailable(e.0)
            }
        })?;
    if done && summary.agent == "codex" && !history.turns.last().is_some_and(|turn| turn.completed)
    {
        return Err(unavailable("Codex Stop has no matching task completion"));
    }
    verify_turn(&history, &bound, done, &summary.agent)?;
    if done && let Some(turn) = history.turns.last_mut() {
        turn.completed = true;
    }
    let after = query.query_snapshot()?;
    let after_pane = require_same_agent(&after, identity)?;
    verify_live_agent_process(runner, identity, after_pane)?;
    verify_bound_writer(runner, after_pane, &bound)?;
    if !matches_binding(after_pane, &bound) || binding(&query, identity)? != bound {
        return Err(changed());
    }
    let after_state = canonical_state(after_pane).unwrap();
    if state.completed_seq != after_state.completed_seq || state.lifecycle != after_state.lifecycle
    {
        return Err(changed());
    }
    // Make the second text observation after all process/canonical fences.
    if bound.hook.read().map_err(|_| changed())? != bytes {
        return Err(changed());
    }
    // An unfinished JSONL tail is acquisition metadata, not a new semantic revision.
    let context_revision = context_revision(&bound.session_id, &history)?;
    let cursor = format!(
        "vtcc1:{}",
        conversation::digest(&serde_json::to_vec(&(
            &identity.state_id,
            identity.agent_epoch,
            &bound.session_id,
            state.run_seq,
            state.completed_seq,
            &summary.lifecycle,
            &context_revision
        ))?)
    );
    if !include_tools {
        for message in history
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool || m.kind == MessageKind::ToolCall)
        {
            history.omissions.push(conversation::HistoryOmission {
                reason: "tool_evidence_not_requested".into(),
                source_ref: Some(message.source_ref.clone()),
                omitted_text_digest: Some(message.text_digest.clone()),
            });
        }
        history
            .messages
            .retain(|m| m.role != MessageRole::Tool && m.kind != MessageKind::ToolCall);
    }
    let update = match since {
        None => ConversationUpdate::Snapshot,
        Some(revision) if revision == context_revision => ConversationUpdate::Unchanged,
        Some(_) => ConversationUpdate::Replace,
    };
    if update == ConversationUpdate::Unchanged {
        history.messages.clear();
    }
    let snapshot = ConversationSnapshot {
        target: wait_target(pane, &connection.server_identity, identity, None),
        session_id: bound.session_id,
        run_seq: state.run_seq,
        completed_seq: state.completed_seq,
        provider_turn_id: history
            .turns
            .last()
            .and_then(|t| t.provider_turn_id.clone()),
        status: summary.status,
        lifecycle: summary.lifecycle,
        context_revision,
        cursor,
        update,
        read_bytes: bytes.len() as u64,
        stop_boundary: stop.is_some(),
        history,
    };
    Ok((query, after, snapshot))
}

pub fn agent_conversation(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    target: &str,
    include_tools: bool,
    since: Option<&str>,
    after_cursor: Option<&str>,
    timeout: Duration,
) -> Result<String> {
    if !target.starts_with("vta1:") {
        return Err(ApiError::new(
            ApiErrorCode::InvalidTarget,
            "conversation requires an exact agent_ref",
        )
        .into());
    }
    if timeout.is_zero() || timeout > MAX_WAIT_TIMEOUT {
        return Err(ApiError::new(
            ApiErrorCode::InvalidArguments,
            "timeout must be between 1 and 86400000 ms",
        )
        .into());
    }
    for (value, prefix) in [(since, "vtc1:"), (after_cursor, "vtcc1:")] {
        if value.is_some_and(|v| {
            !v.starts_with(prefix)
                || v.len() != prefix.len() + 64
                || !v[prefix.len()..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            return Err(ApiError::new(
                ApiErrorCode::InvalidArguments,
                "invalid conversation revision or cursor",
            )
            .into());
        }
    }
    let started = epoch_now();
    let deadline = Instant::now() + timeout;
    let mut connection = ApiConnection::connect(runner, env, after_cursor.map(|_| deadline))?;
    let mut before = if after_cursor.is_some() {
        connection.subscribe()?
    } else {
        connection.query_snapshot()?
    };
    let identity =
        AgentIdentity::from_pane(resolve_agent(&before, target, &connection.server_identity)?)?;
    let mut last_observation = None;
    loop {
        let key = observation_key(&connection, &before, &identity)?;
        if last_observation.as_ref() == Some(&key) {
            before = connection.next_snapshot().map_err(|e| {
                if Instant::now() >= deadline {
                    ApiError::new(
                        ApiErrorCode::Timeout,
                        "conversation did not change before the deadline",
                    )
                    .into()
                } else {
                    e
                }
            })?;
            continue;
        }
        last_observation = Some(key);
        let (query, after, conversation) = read_snapshot(
            runner,
            &connection,
            &before,
            &identity,
            include_tools,
            since,
        )?;
        if after_cursor != Some(conversation.cursor.as_str()) {
            return success_json(
                &query,
                &after,
                started,
                ApiResult::AgentConversation { conversation },
            );
        }
        if Instant::now() >= deadline {
            return Err(ApiError::new(
                ApiErrorCode::Timeout,
                "conversation did not change before the deadline",
            )
            .into());
        }
        before = connection.next_snapshot().map_err(|e| {
            if Instant::now() >= deadline {
                ApiError::new(
                    ApiErrorCode::Timeout,
                    "conversation did not change before the deadline",
                )
                .into()
            } else {
                e
            }
        })?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_support::test_agent_pane;
    use crate::conversation::{
        ConversationMessage, ConversationTurn, HistoryOmission, HookBinding,
    };
    use crate::pane_state::{DaemonInstanceId, EventId, PaneEvent, PaneEventEnvelope};

    fn history() -> ParsedConversation {
        ParsedConversation {
            messages: vec![
                ConversationMessage {
                    source_ref: "user-ref".into(),
                    order: 0,
                    role: MessageRole::User,
                    kind: MessageKind::Instruction,
                    original_role: "user".into(),
                    text: "Do the scoped work\nwithout publishing.".into(),
                    text_complete: true,
                    text_digest: conversation::digest(b"Do the scoped work\nwithout publishing."),
                    provider_message_id: None,
                    provider_turn_id: Some("turn-1".into()),
                    turn_index: 1,
                    tool_call_id: None,
                },
                ConversationMessage {
                    source_ref: "final-ref".into(),
                    order: 1,
                    role: MessageRole::Assistant,
                    kind: MessageKind::Final,
                    original_role: "assistant".into(),
                    text: "Scoped work completed.".into(),
                    text_complete: true,
                    text_digest: conversation::digest(b"Scoped work completed."),
                    provider_message_id: None,
                    provider_turn_id: Some("turn-1".into()),
                    turn_index: 1,
                    tool_call_id: None,
                },
            ],
            turns: vec![ConversationTurn {
                turn_index: 1,
                provider_turn_id: Some("turn-1".into()),
                user_source_refs: vec!["user-ref".into()],
                final_source_ref: Some("final-ref".into()),
                completed: true,
            }],
            compacted: false,
            partial_record: false,
            omissions: vec![],
        }
    }
    fn bound() -> BoundConversation {
        let pane = test_agent_pane();
        let state = &pane.resolved.as_ref().unwrap().canonical;
        let history = history();
        BoundConversation {
            state_id: state.state_id.clone(),
            agent_epoch: state.agent_epoch,
            session_id: "session-1".into(),
            run_seq: 1,
            stop_bytes: Some(100),
            hook: HookBinding {
                path: "/tmp/synthetic.jsonl".into(),
                device: 1,
                inode: 1,
                bytes: 100,
                provider_turn_id: Some("turn-1".into()),
                prompt_digest: Some(conversation::digest(history.messages[0].text.as_bytes())),
                response_digest: Some(conversation::digest(history.messages[1].text.as_bytes())),
            },
        }
    }

    #[test]
    fn rejects_unattributed_instruction_final_and_turn_boundaries() {
        let valid = history();
        let binding = bound();
        verify_turn(&valid, &binding, true, "codex").unwrap();
        let mut changed = valid.clone();
        changed.messages[0].text.push_str(" new instruction");
        assert_eq!(
            verify_turn(&changed, &binding, true, "codex")
                .unwrap_err()
                .downcast_ref::<ApiError>()
                .unwrap()
                .code(),
            "conversation_unavailable"
        );
        let mut changed = valid.clone();
        changed.messages[1].text.clear();
        assert!(verify_turn(&changed, &binding, true, "codex").is_err());
        let mut changed = valid.clone();
        changed.turns[0].provider_turn_id = Some("turn-2".into());
        assert!(verify_turn(&changed, &binding, false, "codex").is_err());
        let mut missing = binding.clone();
        missing.stop_bytes = None;
        assert!(verify_turn(&valid, &missing, true, "codex").is_err());
        let mut missing = binding;
        missing.hook.provider_turn_id = None;
        assert!(verify_turn(&valid, &missing, false, "codex").is_err());
        let mut incomplete = valid.clone();
        incomplete.messages[0].text_complete = false;
        assert!(verify_turn(&incomplete, &bound(), false, "codex").is_err());
        incomplete = valid.clone();
        incomplete.messages[1].text_complete = false;
        assert!(verify_turn(&incomplete, &bound(), true, "codex").is_err());
        // Equal text from an earlier turn cannot establish the latest instruction.
        let mut wrong_turn = valid;
        wrong_turn.messages[0].turn_index = 0;
        assert!(verify_turn(&wrong_turn, &bound(), false, "codex").is_err());
    }

    #[test]
    fn claude_stop_proof_preserves_full_transcript_whitespace() {
        let mut parsed = history();
        parsed.messages[1].text = format!("\n{}\n", parsed.messages[1].text);
        verify_turn(&parsed, &bound(), true, "claude").unwrap();
        assert!(verify_turn(&parsed, &bound(), true, "codex").is_err());
        assert!(
            parsed.messages[1].text.starts_with('\n') && parsed.messages[1].text.ends_with('\n')
        );
    }

    #[test]
    fn revision_ignores_activity_and_partial_tail_but_tracks_instruction_and_evidence() {
        let original = history();
        let revision = context_revision("session", &original).unwrap();
        let mut activity = original.clone();
        activity.partial_record = true;
        activity.omissions.push(HistoryOmission {
            reason: "incomplete_jsonl_tail".into(),
            source_ref: None,
            omitted_text_digest: None,
        });
        let mut tool = original.messages[1].clone();
        tool.role = MessageRole::Tool;
        tool.kind = MessageKind::ToolCall;
        tool.source_ref = "tool-ref".into();
        tool.text = "running a tool".into();
        tool.text_digest = conversation::digest(tool.text.as_bytes());
        activity.omissions.push(HistoryOmission {
            reason: "tool_text_truncated".into(),
            source_ref: Some(tool.source_ref.clone()),
            omitted_text_digest: Some(tool.text_digest.clone()),
        });
        activity.messages.push(tool.clone());
        assert_eq!(context_revision("session", &activity).unwrap(), revision);
        activity.messages.last_mut().unwrap().kind = MessageKind::ToolResult;
        let evidence_revision = context_revision("session", &activity).unwrap();
        assert_ne!(evidence_revision, revision);
        // A change beyond the visible tool prefix still invalidates the context.
        activity.messages.last_mut().unwrap().text_digest =
            conversation::digest(b"same prefix, changed omitted tail");
        assert_ne!(
            context_revision("session", &activity).unwrap(),
            evidence_revision
        );
        let mut edit = original;
        edit.messages[0].text.push_str(" correction");
        assert_ne!(context_revision("session", &edit).unwrap(), revision);
    }

    #[test]
    fn binding_rejects_new_run_and_epoch_and_stop_retry_keeps_original_offset() {
        let mut pane = test_agent_pane();
        let binding = bound();
        assert!(matches_binding(&pane, &binding));
        pane.resolved.as_mut().unwrap().canonical.run_seq += 1;
        assert!(!matches_binding(&pane, &binding));
        pane.resolved.as_mut().unwrap().canonical.run_seq = 1;
        pane.resolved.as_mut().unwrap().canonical.agent_epoch += 1;
        assert!(!matches_binding(&pane, &binding));
        let mut pane = test_agent_pane();
        pane.resolved.as_mut().unwrap().canonical.completed_seq = 1;
        pane.resolved.as_mut().unwrap().canonical.lifecycle =
            crate::pane_state::LifecycleState::Idle;
        let state = &pane.resolved.as_ref().unwrap().canonical;
        let mut bindings = [(pane.pane_instance.clone(), binding.clone())]
            .into_iter()
            .collect();
        let envelope = PaneEventEnvelope {
            daemon_instance_id: DaemonInstanceId::parse("00112233445566778899aabbccddeeff")
                .unwrap(),
            event_id: EventId::generate().unwrap(),
            pane_instance: pane.pane_instance.clone(),
            agent: Some(state.agent.clone()),
            agent_session_id: state.agent_session_id.clone(),
            event: PaneEvent::CompleteRun { completed_at: 2 },
        };
        let mut retry = binding.hook.clone();
        retry.bytes = 999;
        conversation::observe_binding(&mut bindings, Some(state), &envelope, Some(retry));
        assert_eq!(
            bindings.get(&pane.pane_instance).unwrap().stop_bytes,
            Some(100)
        );
    }
    #[test]
    fn deferred_claude_stop_does_not_seal_the_background_turn() {
        let pane = test_agent_pane();
        let mut state = pane.resolved.unwrap().canonical;
        state.agent = crate::pane_state::AgentKind::parse("claude").unwrap();
        let mut original = bound();
        original.stop_bytes = None;
        original.hook.provider_turn_id = None;
        let mut bindings = [(pane.pane_instance.clone(), original.clone())]
            .into_iter()
            .collect();
        let envelope = PaneEventEnvelope {
            daemon_instance_id: DaemonInstanceId::parse("00112233445566778899aabbccddeeff")
                .unwrap(),
            event_id: EventId::generate().unwrap(),
            pane_instance: pane.pane_instance.clone(),
            agent: Some(state.agent.clone()),
            agent_session_id: state.agent_session_id.clone(),
            event: PaneEvent::ClaudeStopped {
                observed_at: 2,
                response: None,
                registry: None,
                crons: None,
            },
        };
        let mut hook = original.hook.clone();
        hook.bytes = 120;
        conversation::observe_binding(&mut bindings, Some(&state), &envelope, Some(hook.clone()));
        assert_eq!(bindings.get(&pane.pane_instance).unwrap().stop_bytes, None);
        state.lifecycle = crate::pane_state::LifecycleState::Idle;
        state.completed_seq = state.run_seq;
        hook.bytes = 240;
        conversation::observe_binding(&mut bindings, Some(&state), &envelope, Some(hook));
        assert_eq!(
            bindings.get(&pane.pane_instance).unwrap().stop_bytes,
            Some(240)
        );
    }

    #[test]
    fn transcript_reader_rejects_replacement_symlinks_truncation_and_oversized_files() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!(
            "vde-conversation-read-{}",
            EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("transcript.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        let hook = HookBinding::capture(&serde_json::json!({"transcript_path": path}).to_string())
            .unwrap();
        assert_eq!(hook.read().unwrap(), b"{}\n");
        std::fs::write(&path, "").unwrap();
        assert!(hook.read().is_err());
        std::fs::rename(&path, root.join("old.jsonl")).unwrap();
        std::fs::write(&path, "{}\n").unwrap();
        assert!(hook.read().is_err());
        std::fs::remove_file(&path).unwrap();
        symlink(root.join("missing.jsonl"), &path).unwrap();
        assert!(hook.read().is_err());
        assert!(
            HookBinding::capture(&serde_json::json!({"transcript_path": path}).to_string())
                .is_none()
        );
        std::fs::remove_file(&path).unwrap();
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(conversation::MAX_TRANSCRIPT_BYTES + 1)
            .unwrap();
        let oversized =
            HookBinding::capture(&serde_json::json!({"transcript_path": path}).to_string())
                .unwrap();
        assert_eq!(oversized.read().unwrap_err().0, "transcript_size_limit");
        std::fs::remove_dir_all(root).unwrap();
    }
}
