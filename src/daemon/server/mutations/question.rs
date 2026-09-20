use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
use crate::pane_state::{AgentKind, EventId, PaneEventEnvelope, PaneInstance};
use crate::question_notice::{MAX_ANCESTORS, NoticeReason, NoticeResult, QuestionNoticeInput};
use crate::tmux::TmuxRunner;

use super::super::ProductionV2Coordinator;

pub(in crate::daemon::server) fn apply(
    coordinator: &ProductionV2Coordinator,
    envelope: &PaneEventEnvelope,
    observation: &crate::hook::provider::ProviderObservation,
    input: QuestionNoticeInput,
    runner: &dyn TmuxRunner,
) -> NoticeResult {
    let verified = (|| {
        let QuestionNoticeInput::Issued {
            session_id,
            turn_id,
            tool_use_id,
            ancestors,
        } = input
        else {
            let QuestionNoticeInput::Rejected { reason } = input else {
                unreachable!()
            };
            return Err(reason);
        };
        if observation.provider.as_str() != "codex"
            || observation.hook_kind != crate::hook::provider::ProviderHookKind::Activity
            || session_id != observation.session_id.as_str()
            || observation.provider_turn_key.as_deref() != Some(&turn_id)
            || ![session_id.as_str(), turn_id.as_str(), tool_use_id.as_str()]
                .into_iter()
                .all(crate::question_notice::ingress::valid_identifier)
            || ancestors.is_empty()
            || ancestors.len() > MAX_ANCESTORS
            || ancestors
                .iter()
                .any(|ancestor| ancestor.validate().is_err())
        {
            return Err(NoticeReason::InvalidPayload);
        }
        {
            let state = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            let state = state.as_ref().ok_or(NoticeReason::OwnerUnverified)?;
            if !state
                .topology
                .panes
                .iter()
                .any(|pane| pane.pane_instance == envelope.pane_instance)
                || !state
                    .leased
                    .runtime
                    .record(&envelope.pane_instance)
                    .is_some_and(|record| record.agent.as_str() == "codex" && record.agent_present)
            {
                return Err(NoticeReason::OwnerUnverified);
            }
        }
        // Do not reuse provider_binding_record: sessions/epochs are deliberately not notice owners.
        let process = runner
            .resolve_agent_process(
                envelope.pane_instance.pane_pid,
                &AgentKind::parse("codex").expect("constant kind"),
            )
            .map_err(|_| NoticeReason::OwnerUnverified)?
            .ok_or(NoticeReason::OwnerUnverified)?;
        if !ancestors.contains(&process) {
            return Err(NoticeReason::AncestorNotInPane);
        }
        Ok((process, session_id, turn_id, tool_use_id))
    })();
    let mut guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let Some(state) = guard.as_mut() else {
        return NoticeResult::rejected(NoticeReason::OwnerUnverified);
    };
    let before = state
        .question_notices
        .summary(&envelope.pane_instance, None);
    let result = match verified {
        Ok((process, session, turn, tool)) => state.question_notices.issue(
            envelope.pane_instance.clone(),
            process,
            (&session, &turn, &tool),
            super::super::epoch_seconds(),
        ),
        Err(reason) => state
            .question_notices
            .reject(&envelope.pane_instance, reason),
    };
    if before
        != state
            .question_notices
            .summary(&envelope.pane_instance, None)
        && let Err(error) = state.leased.runtime.mark_projection_changed()
    {
        coordinator.fail_stop(error.to_string());
    }
    result
}

pub(in crate::daemon::server) fn acknowledge(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    pane: PaneInstance,
    owner_ref: String,
    through_order: u64,
) -> ServerMessage {
    let runner = coordinator.status_push_runner(std::time::Duration::from_secs(1));
    acknowledge_with_runner(
        coordinator,
        accepted_seq,
        event_id,
        pane,
        owner_ref,
        through_order,
        &runner,
    )
}

pub(in crate::daemon::server) fn acknowledge_with_runner(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    pane: PaneInstance,
    owner_ref: String,
    through_order: u64,
    runner: &dyn TmuxRunner,
) -> ServerMessage {
    let process = runner
        .resolve_agent_process(
            pane.pane_pid,
            &AgentKind::parse("codex").expect("constant kind"),
        )
        .ok()
        .flatten();
    let mut guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let Some(state) = guard.as_mut() else {
        return ServerMessage::error(ErrorCode::NotReady, "daemon is hydrating", Some(event_id));
    };
    if !state
        .topology
        .panes
        .iter()
        .any(|entry| entry.pane_instance == pane)
        || process.is_none()
        || state.question_notices.owner_process(&pane, &owner_ref) != process.as_ref()
    {
        return ServerMessage::error(
            ErrorCode::StaleAgentEvent,
            "owner_unverified: question notice owner is no longer exact",
            Some(event_id),
        );
    }
    let before = state.question_notices.summary(&pane, None);
    state
        .question_notices
        .bind(pane.clone(), process.expect("verified process"));
    let result = state
        .question_notices
        .acknowledge(&pane, &owner_ref, through_order);
    // Include write failures: the health indicator is part of the projection.
    if before != state.question_notices.summary(&pane, None)
        && let Err(error) = state.leased.runtime.mark_projection_changed()
    {
        return ServerMessage::error(
            ErrorCode::StateInvariantViolation,
            error.to_string(),
            Some(event_id),
        );
    }
    match result {
        Ok(_) => ServerMessage::SnapshotAck {
            event_id,
            accepted_seq,
            snapshot_revision: state.leased.runtime.snapshot_revision(),
        },
        Err(reason) => ServerMessage::error(
            if reason == "persistence_pending" {
                ErrorCode::PersistFailed
            } else {
                ErrorCode::InvalidRequest
            },
            reason,
            Some(event_id),
        ),
    }
}

pub(in crate::daemon::server) fn maintain(coordinator: &ProductionV2Coordinator) {
    let mut guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let Some(state) = guard.as_mut() else {
        return;
    };
    let current: std::collections::BTreeMap<_, _> = state
        .topology
        .panes
        .iter()
        .map(|pane| {
            let process = state
                .leased
                .runtime
                .tracker(&pane.pane_instance)
                .agent_process
                .filter(|process| {
                    crate::daemon::lifecycle::agent_process_start_token(process.pid)
                        .is_ok_and(|token| token == process.start_token)
                });
            (pane.pane_instance.clone(), process)
        })
        .collect();
    let before: Vec<_> = current
        .keys()
        .map(|pane| state.question_notices.summary(pane, None))
        .collect();
    state
        .question_notices
        .retain_panes(&current.keys().cloned().collect());
    for (pane, process) in &current {
        if let Some(process) = process {
            state.question_notices.bind(pane.clone(), process.clone());
        }
    }
    let changed = state.question_notices.reconcile(|pane, process| {
        let Some(current_process) = current.get(pane) else {
            return false;
        };
        if current_process
            .as_ref()
            .is_some_and(|current| current != process)
        {
            return false;
        }
        match crate::daemon::lifecycle::agent_process_start_token(process.pid) {
            Ok(token) => token == process.start_token,
            // An observation failure is not proof of death.
            Err(_) => {
                // SAFETY: signal 0 checks existence without sending a signal.
                unsafe {
                    libc::kill(process.pid as i32, 0) == 0
                        || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                }
            }
        }
    });
    let after: Vec<_> = current
        .keys()
        .map(|pane| state.question_notices.summary(pane, None))
        .collect();
    if (changed || before != after)
        && let Err(error) = state.leased.runtime.mark_projection_changed()
    {
        coordinator.fail_stop(error.to_string());
    }
}
