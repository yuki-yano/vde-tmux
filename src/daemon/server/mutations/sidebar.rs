use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::Result;

use crate::daemon::protocol::v2::ServerMessage;
use crate::pane_state::{DaemonInstanceId, EventId, PaneEvent, PaneEventEnvelope, PaneInstance};

use super::super::ProductionV2Coordinator;
use super::super::contracts::SidebarEffectResult;
use super::super::state_helpers::production_store_error_response;

#[cfg(test)]
mod tests;

pub(in crate::daemon::server) fn task_summary_failure_code(error: &str) -> &'static str {
    if error.contains("timed out") || error.contains("timeout") {
        "timeout"
    } else if error.contains("failed to start") || error.contains("No such file") {
        "process_start"
    } else if error.contains("queue") {
        "queue_full"
    } else if error.contains("invalid") || error.contains("exceeded") || error.contains("empty") {
        "invalid_output"
    } else {
        "process_failed"
    }
}

pub(in crate::daemon::server) fn unique_eligible_client_pid(
    views: &crate::daemon::view_hooks::CurrentClientViews,
    source_pane: &PaneInstance,
) -> std::result::Result<u32, usize> {
    let clients = views
        .clients()
        .values()
        .filter(|witness| witness.is_eligible() && &witness.active_pane == source_pane)
        .map(|witness| witness.client_pid)
        .collect::<BTreeSet<_>>();
    if clients.len() == 1 {
        Ok(*clients.iter().next().expect("one client was verified"))
    } else {
        Err(clients.len())
    }
}

pub(in crate::daemon::server) fn eligible_witness_matches(
    witnesses: &[crate::pane_state::ClientWitness],
    client_pid: u32,
    source_pane: &PaneInstance,
) -> bool {
    witnesses.iter().any(|witness| {
        witness.client_pid == client_pid
            && witness.is_eligible()
            && &witness.active_pane == source_pane
    })
}

pub(in crate::daemon::server) fn read_peek_advance_outcome(
    result: &SidebarEffectResult,
) -> crate::daemon::protocol::v2::PeekAdvanceOutcome {
    match result {
        SidebarEffectResult::Succeeded(pane_instance) => {
            crate::daemon::protocol::v2::PeekAdvanceOutcome::Jumped {
                pane_instance: pane_instance.clone(),
            }
        }
        SidebarEffectResult::NoAvailablePane => {
            crate::daemon::protocol::v2::PeekAdvanceOutcome::Stayed
        }
        _ => crate::daemon::protocol::v2::PeekAdvanceOutcome::Failed,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::daemon::server) struct ReadPeekCommitResult {
    pub(in crate::daemon::server) revision: u64,
    pub(in crate::daemon::server) read_outcome: crate::daemon::protocol::v2::PaneApplyOutcome,
    pub(in crate::daemon::server) candidates: Vec<PaneInstance>,
}

#[allow(clippy::too_many_arguments)]
pub(in crate::daemon::server) fn commit_read_peek_state(
    state: &mut crate::daemon::runtime::CanonicalCoordinatorState,
    io: &mut dyn crate::pane_state::snapshot::PaneSnapshotStoreIo,
    daemon_instance_id: &DaemonInstanceId,
    event_id: &EventId,
    target: &PaneInstance,
    client_pid: u32,
    advance_candidates: Vec<PaneInstance>,
    accepted_seq: u64,
) -> Result<ReadPeekCommitResult, crate::pane_state::store::StoreError> {
    let through_order = state
        .leased
        .runtime
        .record(target)
        .and_then(|pane| pane.unread.latest_unread())
        .map(|occurrence| occurrence.order);
    let read_outcome = if let Some(through_order) = through_order {
        let envelope = PaneEventEnvelope {
            daemon_instance_id: daemon_instance_id.clone(),
            event_id: event_id.clone(),
            pane_instance: target.clone(),
            agent: None,
            agent_session_id: None,
            event: PaneEvent::MarkPaneRead { through_order },
        };
        let result = state.leased.runtime.apply_pane_reads(io, &[envelope])?;
        if result.committed > 0 {
            crate::daemon::protocol::v2::PaneApplyOutcome::Committed
        } else {
            crate::daemon::protocol::v2::PaneApplyOutcome::Noop
        }
    } else {
        crate::daemon::protocol::v2::PaneApplyOutcome::Noop
    };

    state.clear_peeks_for_read_panes_except(&BTreeSet::from([target.clone()]), Some(client_pid));
    let mut seen = BTreeSet::new();
    let candidates = advance_candidates
        .into_iter()
        .filter(|candidate| seen.insert(candidate.clone()))
        .filter(|candidate| {
            state.contains_pane(candidate)
                && state
                    .leased
                    .runtime
                    .record(candidate)
                    .is_some_and(|pane| pane.unread.is_unread())
        })
        .collect::<Vec<_>>();
    let revision = state.leased.runtime.snapshot_revision();
    if !candidates.is_empty() {
        let began = state.begin_peek(
            client_pid,
            target.clone(),
            candidates.iter().cloned(),
            accepted_seq,
        );
        debug_assert!(began, "read-current starts from an active lease");
    }
    Ok(ReadPeekCommitResult {
        revision,
        read_outcome,
        candidates,
    })
}

pub(in crate::daemon::server) fn apply_sidebar_preference_intent(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    intent: crate::sidebar::state::SidebarPreferenceIntent,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before sidebar preference intent");
    if !state.sidebar_intent_dedupe.accept(event_id.clone()) {
        return ServerMessage::SnapshotAck {
            event_id,
            accepted_seq,
            snapshot_revision: state.leased.runtime.snapshot_revision(),
        };
    }
    let snapshot = state.resolved_snapshot();
    let projection = crate::sidebar::tree::project_sidebar(
        &state.projection_config,
        &snapshot.panes,
        &snapshot.sidebar_model,
        &snapshot.events,
        &crate::sidebar::state::SidebarState {
            category_scope: crate::sidebar::state::CategoryScope::All,
            ..crate::sidebar::state::SidebarState::default()
        },
        crate::sidebar::tree::now_epoch_secs(),
    );
    let known_rows = projection
        .rows
        .into_iter()
        .map(|row| row.id)
        .collect::<BTreeSet<_>>();
    let mut candidate = state.sidebar_preferences.clone();
    if !candidate.apply_intent(&intent, &known_rows) {
        return ServerMessage::SnapshotAck {
            event_id,
            accepted_seq,
            snapshot_revision: state.leased.runtime.snapshot_revision(),
        };
    }
    let path =
        crate::sidebar::store::state_path(&coordinator.env, &coordinator.incarnation.socket_path);
    if let Err(error) = crate::sidebar::store::save_state(&path, &candidate) {
        let message = format!("sidebar preference persistence failed: {error:#}");
        coordinator.log_daemon_error(&message);
        let _ = state.add_global_diagnostic(ErrorCode::PersistFailed, message.clone());
        return ServerMessage::error(ErrorCode::PersistFailed, message, Some(event_id));
    }
    if let Err(error) = state.replace_sidebar_preferences(candidate) {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    ServerMessage::SnapshotAck {
        event_id,
        accepted_seq,
        snapshot_revision: state.leased.runtime.snapshot_revision(),
    }
}

pub(in crate::daemon::server) fn apply_category_intent(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    intent: crate::category::CategoryIntent,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before category intent");
    if !state.sidebar_intent_dedupe.accept(event_id.clone()) {
        return ServerMessage::CategoryMutationResult {
            event_id,
            accepted_seq,
            snapshot_revision: state.leased.runtime.snapshot_revision(),
            category_state_revision: state.category_state.revision,
            changed: false,
            repo_effect: category_repo_mutation_effect(
                &intent,
                &state.category_state,
                &state.category_state,
            ),
        };
    }
    let model = state.effective_category_model();
    let mut candidate = state.category_state.clone();
    let changed = match candidate.apply_intent(&state.projection_config, &intent, &model) {
        Ok(changed) => changed,
        Err(message) => {
            return ServerMessage::error(ErrorCode::InvalidRequest, message, Some(event_id));
        }
    };
    if !changed {
        return ServerMessage::CategoryMutationResult {
            event_id,
            accepted_seq,
            snapshot_revision: state.leased.runtime.snapshot_revision(),
            category_state_revision: candidate.revision,
            changed: false,
            repo_effect: category_repo_mutation_effect(&intent, &state.category_state, &candidate),
        };
    }
    let repo_effect = category_repo_mutation_effect(&intent, &state.category_state, &candidate);
    let category_state_revision = candidate.revision;
    let path =
        crate::category::store::state_path(&coordinator.env, &coordinator.incarnation.socket_path);
    if let Err(error) = crate::category::store::save_state(&path, &candidate) {
        let message = format!("category state persistence failed: {error:#}");
        coordinator.log_daemon_error(&message);
        let _ = state.add_global_diagnostic(ErrorCode::PersistFailed, message.clone());
        return ServerMessage::error(ErrorCode::PersistFailed, message, Some(event_id));
    }
    if let Err(error) = state.replace_category_state(candidate) {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    let model = state.effective_category_model();
    let mirrors = state
        .status_metadata
        .sessions
        .values()
        .map(|session| {
            let category = state
                .repo_identities
                .get(&session.project_path)
                .and_then(|identity| model.placements.get(&identity.key))
                .map(|placement| placement.category.to_string())
                .unwrap_or_else(|| crate::category::UNCATEGORIZED.to_string());
            (session.session_name.clone(), category)
        })
        .collect::<Vec<_>>();
    let snapshot_revision = state.leased.runtime.snapshot_revision();
    drop(state_guard);
    let runner = coordinator.status_push_runner(Duration::from_secs(1));
    for (session_name, category) in mirrors {
        if let Err(error) = crate::options::set_session_option(
            &runner,
            &session_name,
            crate::options::KEY_CATEGORY,
            &category,
        ) {
            coordinator.log_daemon_error(&format!(
                "failed to update category mirror for {session_name}: {error:#}"
            ));
        }
    }
    ServerMessage::CategoryMutationResult {
        event_id,
        accepted_seq,
        snapshot_revision,
        category_state_revision,
        changed: true,
        repo_effect,
    }
}

pub(in crate::daemon::server) fn category_repo_mutation_effect(
    intent: &crate::category::CategoryIntent,
    before: &crate::category::CategoryState,
    after: &crate::category::CategoryState,
) -> Option<crate::daemon::protocol::v2::CategoryRepoMutationEffect> {
    let repo = match intent {
        crate::category::CategoryIntent::AssignRepo { repo, .. }
        | crate::category::CategoryIntent::SetRepoAutomatic { repo } => repo,
        _ => return None,
    };
    Some(crate::daemon::protocol::v2::CategoryRepoMutationEffect {
        repo: repo.clone(),
        before_override: before.repo_overrides.get(repo).cloned(),
        after_override: after.repo_overrides.get(repo).cloned(),
    })
}

pub(in crate::daemon::server) fn apply_sidebar_navigation(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    selection: Option<String>,
    scroll: usize,
    manual_scroll: bool,
) -> ServerMessage {
    use crate::daemon::protocol::v2::ServerMessage;
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before sidebar navigation");
    if !state.sidebar_intent_dedupe.accept(event_id.clone()) {
        return ServerMessage::SnapshotAck {
            event_id,
            accepted_seq,
            snapshot_revision: state.leased.runtime.snapshot_revision(),
        };
    }
    if let Err(error) = state.replace_sidebar_navigation(selection, scroll, manual_scroll) {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    ServerMessage::SnapshotAck {
        event_id,
        accepted_seq,
        snapshot_revision: state.leased.runtime.snapshot_revision(),
    }
}
