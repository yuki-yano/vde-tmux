use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::mpsc::TrySendError;
use std::time::Duration;

use anyhow::Result;

use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
use crate::pane_state::{EventId, PaneEvent, PaneEventEnvelope, PaneInstance};

use super::super::effects::{NotificationWorkerJob, log_notification_failure};
use super::super::observation::{
    apply_observation_poll_projection, observation_poll_error_response, query_full_topology,
};
use super::super::router::ObservationBatchPayload;
use super::super::state_helpers::{
    pane_snapshot_store, persist_pruned_sidebar_pins, production_store_error_response,
};
use super::super::{ProductionV2Coordinator, epoch_seconds};

#[cfg(test)]
mod tests;

pub(in crate::daemon::server) fn apply_diagnostic_projection(
    coordinator: &ProductionV2Coordinator,
    pane_instance: Option<PaneInstance>,
    message: String,
) -> Result<u64, crate::pane_state::store::StoreError> {
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before diagnostic");
    if let Some(pane) = pane_instance {
        state.leased.runtime.add_diagnostic(pane, message)?;
    } else {
        state.add_global_diagnostic(ErrorCode::InternalError, message)?;
    }
    Ok(state.leased.runtime.snapshot_revision())
}

pub(in crate::daemon::server) fn apply_triage_projection(
    coordinator: &ProductionV2Coordinator,
) -> Result<u64, crate::pane_state::store::StoreError> {
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before triage projection");
    state.leased.runtime.advance_poll_projection()?;
    Ok(state.leased.runtime.snapshot_revision())
}

/// Applies one observation poll as a single sequenced mutation. Every stage
/// reuses the standalone-mutation helpers, so reducer, persist, and read-back
/// contracts are identical to the previous one-mutation-per-event queue; only
/// the snapshot publish moves to the end of the batch.
pub(in crate::daemon::server) fn apply_observation_batch(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    payload: ObservationBatchPayload,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    let ObservationBatchPayload {
        projection,
        observations,
        removals,
        diagnostics,
    } = payload;
    if let Err(error) = apply_observation_poll_projection(coordinator, *projection) {
        return observation_poll_error_response(coordinator, error);
    }
    for envelope in observations.into_iter().chain(removals) {
        // Nonfatal per-pane failures keep processing the remaining panes, same
        // as the standalone mutation queue did; fail-stop conditions raise the
        // shutdown flag inside the helper and abort the rest of the batch.
        let _ = apply_pane_event_mutation(coordinator, accepted_seq, envelope, true, None);
        if coordinator.shutdown.load(Ordering::SeqCst) {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "daemon failed stop during observation batch",
                None,
            );
        }
    }
    for (pane_instance, message) in diagnostics {
        if let Err(error) = apply_diagnostic_projection(coordinator, pane_instance, message) {
            return production_store_error_response(coordinator, error, None);
        }
    }
    match apply_triage_projection(coordinator) {
        Ok(revision) => ServerMessage::SnapshotAck {
            event_id: EventId::generate().expect("OS random source failed after daemon startup"),
            accepted_seq,
            snapshot_revision: revision,
        },
        Err(error) => production_store_error_response(coordinator, error, None),
    }
}

pub(in crate::daemon::server) fn apply_pane_event_mutation(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    envelope: PaneEventEnvelope,
    defer_full_preflight: bool,
    durable_run: Option<crate::agent_state::RunRecord>,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{PaneApplyOutcome, ServerMessage};

    let event_id = envelope.event_id.clone();
    let durable_run = match durable_run {
        Some(run) => Some(run),
        None => match reconcile_run_for_pane_event(coordinator, &envelope) {
            Ok(run) => run,
            Err(error) => {
                return ServerMessage::error(
                    ErrorCode::PersistFailed,
                    error.to_string(),
                    Some(event_id),
                );
            }
        },
    };
    if let PaneEvent::PaneRemoved { expected } = &envelope.event {
        return apply_pane_removal(
            coordinator,
            accepted_seq,
            event_id,
            envelope.pane_instance,
            expected.clone(),
        );
    }
    let (visibility, visibility_diagnostic) =
        match unread_visibility_for_event(coordinator, &envelope) {
            Ok(value) => value,
            Err(error) => {
                coordinator.fail_stop(error.to_string());
                return production_store_error_response(coordinator, error, Some(event_id));
            }
        };
    let result = {
        let mut state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let Some(state) = state_guard.as_mut() else {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "daemon is hydrating",
                Some(event_id),
            );
        };
        let mut io = pane_snapshot_store(coordinator);
        let revision_before = state.leased.runtime.snapshot_revision();
        state
            .leased
            .runtime
            .apply_event(&mut io, &envelope, &visibility)
            .and_then(|mut result| {
                if let Some(run) = durable_run.as_ref()
                    && state
                        .leased
                        .runtime
                        .record(&envelope.pane_instance)
                        .is_some_and(|pane| pane_belongs_to_run_epoch(pane, run))
                {
                    let projection = crate::pane_state::CurrentDurableRunProjection {
                        run_id: run.run_id.as_str().to_string(),
                        run_seq: run.run_seq,
                        run_revision: run.revision,
                    };
                    if state.leased.runtime.project_current_run(
                        &mut io,
                        &envelope.pane_instance,
                        projection,
                        run.execution_active(),
                        run.updated_at,
                    )? {
                        result.state_version = state
                            .leased
                            .runtime
                            .record(&envelope.pane_instance)
                            .map(crate::pane_state::PaneState::version);
                        result.snapshot_revision = state.leased.runtime.snapshot_revision();
                    }
                }
                let result = finish_pane_event_projection(
                    coordinator,
                    state,
                    &envelope.pane_instance,
                    visibility_diagnostic.as_deref(),
                    revision_before,
                    result,
                    defer_full_preflight,
                )?;
                if matches!(envelope.event, PaneEvent::BeginRun { .. })
                    && let Some(record) = state.leased.runtime.record(&envelope.pane_instance)
                {
                    coordinator.schedule_task_summary(record);
                }
                Ok(result)
            })
    };
    match result {
        Ok(result) => ServerMessage::PaneEventResult {
            event_id,
            accepted_seq,
            state_version: result.state_version,
            snapshot_revision: result.snapshot_revision,
            outcome: if result.outcome
                == crate::pane_state::reducer::ReductionOutcome::CanonicalChanged
            {
                PaneApplyOutcome::Committed
            } else {
                PaneApplyOutcome::Noop
            },
        },
        Err(error) => {
            if error.requires_daemon_exit() {
                coordinator.fail_stop(error.to_string());
            }
            production_store_error_response(coordinator, error, Some(event_id))
        }
    }
}

pub(in crate::daemon::server) fn reconcile_run_for_pane_event(
    coordinator: &ProductionV2Coordinator,
    envelope: &PaneEventEnvelope,
) -> Result<Option<crate::agent_state::RunRecord>, crate::agent_state::StoreError> {
    let (checked, process, observed_at) = match &envelope.event {
        PaneEvent::ObservationBatch {
            process: Some(process),
            observed_at,
            ..
        } => (
            process.agent_process_checked,
            process.agent_process.as_ref(),
            *observed_at,
        ),
        PaneEvent::PaneRemoved { .. } => (true, None, epoch_seconds()),
        _ => return Ok(None),
    };
    coordinator
        .agent_runtime
        .lock()
        .expect("agent runtime lock poisoned")
        .as_mut()
        .map_or(Ok(None), |runtime| {
            runtime.reconcile_process_for_pane(
                &envelope.pane_instance,
                checked,
                process,
                observed_at,
            )
        })
}

pub(in crate::daemon::server) fn pane_belongs_to_run_epoch(
    pane: &crate::pane_state::PaneState,
    run: &crate::agent_state::RunRecord,
) -> bool {
    pane.pane_instance == run.binding.pane_instance
        && pane.state_id == run.binding.pane_state_id
        && pane.agent_epoch == run.binding.agent_epoch
        && pane.agent == run.binding.agent_kind
        && pane.agent_session_id.as_ref() == Some(&run.binding.provider_session_id)
}

pub(in crate::daemon::server) fn pane_needs_durable_run_projection(
    pane: &crate::pane_state::PaneState,
    run: &crate::agent_state::RunRecord,
) -> std::result::Result<bool, String> {
    if !pane_belongs_to_run_epoch(pane, run) {
        return Ok(false);
    }
    match pane.current_run.as_ref() {
        Some(current) if current.run_id == run.run_id.as_str() => Ok(true),
        Some(_) if pane.run_seq == run.run_seq => Err(
            "Pane current durable run identity conflicts with a duplicate provider Run".to_string(),
        ),
        Some(_) => Ok(pane.run_seq < run.run_seq),
        None => Ok(pane.run_seq <= run.run_seq),
    }
}

/// `defer_full_preflight` is set on the observation-batch path: the per-pane
/// persist/read-back contract still runs here, while the full resolved-snapshot
/// preflight happens once when the batch publishes.
pub(in crate::daemon::server) fn finish_pane_event_projection(
    coordinator: &ProductionV2Coordinator,
    state: &mut crate::daemon::runtime::CanonicalCoordinatorState,
    pane: &PaneInstance,
    visibility_diagnostic: Option<&str>,
    revision_before: u64,
    mut result: crate::pane_state::store::ApplyResult,
    defer_full_preflight: bool,
) -> Result<crate::pane_state::store::ApplyResult, crate::pane_state::store::StoreError> {
    let mut messages = visibility_diagnostic
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for notification in state.leased.runtime.drain_notification_jobs() {
        let agent = match state.leased.runtime.record(&notification.pane_instance) {
            Some(active) if active.version() == notification.state_version => {
                active.agent.as_str().to_string()
            }
            _ => {
                messages.push(format!(
                    "notification_target_missing: pane={} state={:?}",
                    notification.pane_instance.pane_id, notification.state_version
                ));
                continue;
            }
        };
        let Some(sender) = coordinator.notification_tx.as_ref() else {
            continue;
        };
        let job = NotificationWorkerJob {
            pane_id: notification.pane_instance.pane_id.clone(),
            agent,
        };
        if let Err(error) = sender.try_send(job) {
            let reason = match error {
                TrySendError::Full(_) => "queue_full",
                TrySendError::Disconnected(_) => "worker_disconnected",
            };
            messages.push(format!(
                "notification_dispatch_failed: pane={} reason={reason}",
                notification.pane_instance.pane_id
            ));
            log_notification_failure(
                Some(&(
                    coordinator.env.clone(),
                    coordinator.incarnation.hash.clone(),
                )),
                &format!(
                    "notification dispatch failed for pane {}: {reason}",
                    notification.pane_instance.pane_id
                ),
            );
        }
    }
    result.snapshot_revision = state.leased.runtime.finish_sequenced_projection(
        Some(pane),
        messages,
        false,
        revision_before,
    )?;
    if !defer_full_preflight {
        let _ = state.checked_resolved_snapshot()?;
    }
    Ok(result)
}

pub(in crate::daemon::server) fn apply_pane_removal(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    pane: PaneInstance,
    expected: Option<crate::pane_state::StoredStateDescriptor>,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{PaneApplyOutcome, ServerMessage};
    let topology = match query_full_topology(coordinator, Duration::from_millis(100)) {
        Ok(topology) => topology,
        Err(error) => {
            if error.requires_daemon_exit() {
                coordinator.fail_stop(error.to_string());
            }
            return ServerMessage::error(
                ErrorCode::InternalError,
                error.to_string(),
                Some(event_id),
            );
        }
    };
    let still_present = topology
        .panes
        .iter()
        .any(|current| current.pane_instance == pane);
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before pane removal");
    let topology_changed = state.topology != topology;
    state.topology = topology;
    if let Err(error) = persist_pruned_sidebar_pins(coordinator, state) {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    if still_present {
        if topology_changed && let Err(error) = state.leased.runtime.mark_projection_changed() {
            return production_store_error_response(coordinator, error, Some(event_id));
        }
        return ServerMessage::PaneEventResult {
            event_id,
            accepted_seq,
            state_version: state
                .leased
                .runtime
                .record(&pane)
                .map(|state| state.version()),
            snapshot_revision: state.leased.runtime.snapshot_revision(),
            outcome: PaneApplyOutcome::Noop,
        };
    }
    let mut io = pane_snapshot_store(coordinator);
    let removed = match state
        .leased
        .runtime
        .remove_absent_pane(&mut io, &pane, expected.as_ref())
    {
        Ok(removed) => removed,
        Err(error) => {
            return production_store_error_response(coordinator, error, Some(event_id));
        }
    };
    if topology_changed
        && !removed
        && let Err(error) = state.leased.runtime.mark_projection_changed()
    {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    ServerMessage::PaneEventResult {
        event_id,
        accepted_seq,
        state_version: None,
        snapshot_revision: state.leased.runtime.snapshot_revision(),
        outcome: if removed {
            PaneApplyOutcome::Committed
        } else {
            PaneApplyOutcome::Noop
        },
    }
}

pub(in crate::daemon::server) fn unread_visibility_for_event(
    coordinator: &ProductionV2Coordinator,
    envelope: &PaneEventEnvelope,
) -> Result<
    (crate::pane_state::VisibilitySnapshot, Option<String>),
    crate::pane_state::store::StoreError,
> {
    use crate::pane_state::{PaneEvent, ReportedLifecycle};

    let (current, tracker, focus_equivalent_panes) = {
        let state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let state = state_guard.as_ref();
        let current = state
            .and_then(|state| state.leased.runtime.record(&envelope.pane_instance))
            .cloned();
        let tracker = state
            .map(|state| state.leased.runtime.tracker(&envelope.pane_instance))
            .unwrap_or_default();
        let focus_equivalent_panes = state.map_or_else(
            || BTreeSet::from([envelope.pane_instance.clone()]),
            |state| state.focus_equivalent_panes(&envelope.pane_instance),
        );
        (current, tracker, focus_equivalent_panes)
    };
    let may_create_unread = match &envelope.event {
        PaneEvent::WaitRequested { .. } | PaneEvent::FailRun { .. } => true,
        PaneEvent::CompleteRun { .. } | PaneEvent::ResponseAndCompleteRun { .. } => {
            current.as_ref().is_none_or(|state| {
                state.run_seq > state.completed_seq || state.synthetic_completion_armed
            })
        }
        PaneEvent::ExplicitStateReported { report }
            if matches!(
                report.lifecycle,
                Some(ReportedLifecycle::Waiting { .. } | ReportedLifecycle::Error { .. })
            ) =>
        {
            true
        }
        PaneEvent::ExplicitStateReported { report }
            if matches!(report.lifecycle, Some(ReportedLifecycle::Idle)) =>
        {
            current
                .as_ref()
                .map_or(report.completed_at.is_some() || report.attention, |state| {
                    state.run_seq > state.completed_seq
                        || (state.synthetic_completion_armed
                            && (report.completed_at.is_some() || report.attention))
                })
        }
        PaneEvent::ObservationBatch {
            presence, capture, ..
        } => current.as_ref().is_some_and(|state| {
            observation_may_create_unread(state, &tracker, presence, capture.as_ref())
        }),
        _ => false,
    };
    if !may_create_unread {
        return Ok((crate::pane_state::VisibilitySnapshot::default(), None));
    }
    let io = crate::daemon::view_hooks::SystemFreshVisibilityIo::new(
        coordinator
            .env
            .get("VDE_TMUX_SOCKET_NAME")
            .cloned()
            .filter(|value| !value.trim().is_empty()),
        coordinator.incarnation.identity.clone(),
    );
    use crate::daemon::view_hooks::FreshVisibilityIo as _;
    let observation_seq = coordinator.begin_witness_observation();
    match io.query_witnesses(crate::daemon::view_hooks::FRESH_VISIBILITY_TIMEOUT) {
        Ok(witnesses) => {
            let pane_visible_to_eligible_client = {
                let mut guard = coordinator
                    .state
                    .lock()
                    .expect("canonical state lock poisoned");
                guard.as_mut().is_some_and(|state| {
                    state.reconcile_peek_leases(&witnesses, observation_seq);
                    let authorized =
                        state.has_read_authority_for(&witnesses, &focus_equivalent_panes);
                    if authorized {
                        state.clear_peeks_for_read_panes(&focus_equivalent_panes);
                    }
                    authorized
                })
            };
            Ok((
                crate::pane_state::VisibilitySnapshot {
                    pane_visible_to_eligible_client,
                },
                None,
            ))
        }
        Err(error) if error.requires_daemon_exit() => Err(
            crate::pane_state::store::StoreError::FailStop(error.to_string()),
        ),
        Err(error) => Ok((
            crate::pane_state::VisibilitySnapshot::default(),
            Some(format!("fresh_visibility_unavailable: {error}")),
        )),
    }
}

pub(in crate::daemon::server) fn observation_may_create_unread(
    state: &crate::pane_state::PaneState,
    tracker: &crate::pane_state::CaptureTrackerSnapshot,
    presence: &crate::pane_state::AgentPresenceObservation,
    capture: Option<&crate::pane_state::CaptureObservation>,
) -> bool {
    use crate::pane_state::{AgentPresenceObservation, CaptureInference, LifecycleState};

    let absence_evidence = match presence {
        AgentPresenceObservation::Absent => true,
        AgentPresenceObservation::Present(kind) => kind != &state.agent,
        AgentPresenceObservation::Unknown => false,
    };
    let confirmed_absence_can_complete = absence_evidence
        && tracker.absence_count >= 1
        && state.scan_verified
        && !matches!(state.lifecycle, LifecycleState::Idle);
    let capture_is_applied = state.agent_present
        && match presence {
            AgentPresenceObservation::Present(kind) => kind == &state.agent,
            AgentPresenceObservation::Absent => true,
            AgentPresenceObservation::Unknown => false,
        };
    let stale_capture_can_complete = capture_is_applied
        && matches!(
            capture,
            Some(crate::pane_state::CaptureObservation {
                inference: CaptureInference::StaleRunCompleted,
                ..
            })
        )
        && matches!(state.lifecycle, LifecycleState::Running);
    let permission_wait_can_create = capture_is_applied
        && matches!(
            capture,
            Some(crate::pane_state::CaptureObservation {
                inference: CaptureInference::PermissionWait { .. },
                ..
            })
        )
        && !matches!(state.lifecycle, LifecycleState::Waiting { .. });
    let provider_error_can_create = capture_is_applied
        && matches!(
            capture,
            Some(crate::pane_state::CaptureObservation {
                inference: CaptureInference::ProviderError { .. },
                ..
            })
        )
        && !matches!(state.lifecycle, LifecycleState::Error { .. });
    let usage_limit_can_create = capture_is_applied
        && matches!(
            capture,
            Some(crate::pane_state::CaptureObservation {
                inference: CaptureInference::UsageLimit,
                ..
            })
        )
        && !state.lifecycle.is_usage_limited();
    confirmed_absence_can_complete
        || stale_capture_can_complete
        || permission_wait_can_create
        || provider_error_can_create
        || usage_limit_can_create
}

pub(in crate::daemon::server) fn apply_external_view_event(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event: crate::pane_state::ViewEvent,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    let event_id = event.event_id.clone();
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let Some(state) = state_guard.as_mut() else {
        return ServerMessage::error(ErrorCode::NotReady, "daemon is hydrating", Some(event_id));
    };
    let revision_before = state.leased.runtime.snapshot_revision();
    if let Err(error) = event.validate() {
        return ServerMessage::error(ErrorCode::InvalidRequest, error.to_string(), Some(event_id));
    }

    let diagnostic_pane = event.active_pane.as_ref().cloned().or_else(|| {
        state
            .topology
            .panes
            .first()
            .map(|pane| pane.pane_instance.clone())
    });

    if let Err(error) = state.leased.runtime.finish_sequenced_projection(
        diagnostic_pane.as_ref(),
        std::iter::empty(),
        false,
        revision_before,
    ) {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    ServerMessage::ViewQueued {
        event_id,
        accepted_seq,
    }
}
