use std::thread;
use std::time::Duration;

use anyhow::Result;

use crate::daemon::protocol::v2::ServerMessage;
use crate::pane_state::{EventId, PaneEvent, PaneEventEnvelope, PaneInstance};

use super::super::state_helpers::{pane_snapshot_store, production_store_error_response};
use super::super::{ProductionV2Coordinator, epoch_seconds};
use super::agent::agent_state_query_error_with_event;
use super::pane::{
    apply_pane_event_mutation, finish_pane_event_projection, pane_belongs_to_run_epoch,
    pane_needs_durable_run_projection,
};

#[cfg(test)]
mod tests;

pub(in crate::daemon::server) fn apply_external_pane_event(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    envelope: PaneEventEnvelope,
) -> ServerMessage {
    apply_pane_event_mutation(coordinator, accepted_seq, envelope, false, None)
}

pub(in crate::daemon::server) fn apply_external_provider_event(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    envelope: PaneEventEnvelope,
    observation: crate::hook::provider::ProviderObservation,
) -> ServerMessage {
    let runner = coordinator.status_push_runner(Duration::from_secs(1));
    apply_external_provider_event_with_runner(
        coordinator,
        accepted_seq,
        envelope,
        observation,
        &runner,
    )
}

pub(in crate::daemon::server) fn apply_external_provider_event_with_runner(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    mut envelope: PaneEventEnvelope,
    mut observation: crate::hook::provider::ProviderObservation,
    process_runner: &dyn crate::tmux::TmuxRunner,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
    use crate::hook::provider::ProviderHookKind;

    if observation.provider.as_str() != "codex" {
        return ServerMessage::error(
            ErrorCode::UnsupportedProvider,
            "durable provider observations are enabled only for the authenticated Codex adapter",
            Some(envelope.event_id),
        );
    }
    if envelope.agent.as_ref() != Some(&observation.provider)
        || envelope.agent_session_id.as_ref() != Some(&observation.session_id)
    {
        return ServerMessage::error(
            ErrorCode::InvalidRequest,
            "provider observation identity does not match the pane event envelope",
            Some(envelope.event_id),
        );
    }
    if !provider_event_matches_pane_event(&observation, &envelope.event) {
        return ServerMessage::error(
            ErrorCode::InvalidRequest,
            "provider hook kind does not match the pane event transition",
            Some(envelope.event_id),
        );
    }

    let received_at = epoch_seconds();
    observation.observed_at = received_at;
    normalize_provider_pane_event(&mut envelope.event, received_at);

    if observation.hook_kind == ProviderHookKind::SessionStart {
        // A resumed prompt cannot be attributed to a durable Run, so keep it
        // outside the public Pane snapshot to preserve guarded-dispatch privacy.
        redact_private_provider_prompt(&mut envelope.event, true);
        return apply_external_pane_event(coordinator, accepted_seq, envelope);
    }

    let duplicate = {
        let runtime_guard = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let Some(runtime) = runtime_guard.as_ref() else {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "agent runtime is hydrating",
                Some(envelope.event_id),
            );
        };
        match runtime.provider_event_run(&observation) {
            Ok(value) => value,
            Err(error) => {
                return ServerMessage::error(
                    ErrorCode::PersistFailed,
                    error.to_string(),
                    Some(envelope.event_id),
                );
            }
        }
    };

    let (binding, run_seq) = if let Some(run) = duplicate {
        (run.binding, run.run_seq)
    } else {
        let record = match provider_binding_record(coordinator, &envelope, &observation) {
            Ok(record) => record,
            Err(response) => return response,
        };
        let record = if record.agent_process.is_none() {
            match refresh_provider_process_identity(
                coordinator,
                accepted_seq,
                &envelope,
                &observation,
                process_runner,
            ) {
                Ok(record) => record,
                Err(response) => return response,
            }
        } else {
            record
        };
        let Some(process) = record.agent_process.clone() else {
            return ServerMessage::error(
                ErrorCode::StaleAgentEvent,
                "provider event has no exact agent process identity after a fresh process scan",
                Some(envelope.event_id),
            );
        };
        let run_seq = if observation.hook_kind == ProviderHookKind::UserPromptSubmit {
            match record.run_seq.checked_add(1) {
                Some(value) => value,
                None => {
                    return ServerMessage::error(
                        ErrorCode::StateInvariantViolation,
                        "agent run sequence overflow",
                        Some(envelope.event_id),
                    );
                }
            }
        } else {
            record.run_seq
        };
        (
            crate::agent_state::AgentBinding {
                server_identity: coordinator.incarnation.identity.clone(),
                pane_instance: record.pane_instance,
                pane_state_id: record.state_id,
                agent_epoch: record.agent_epoch,
                agent_kind: record.agent,
                provider_session_id: observation.session_id.clone(),
                process,
            },
            run_seq,
        )
    };

    let apply_result = coordinator
        .agent_runtime
        .lock()
        .expect("agent runtime lock poisoned")
        .as_mut()
        .expect("agent runtime checked above")
        .apply_provider_observation(binding, run_seq, &observation);
    let apply_result = match apply_result {
        Ok(result) => result,
        Err(error) => {
            if matches!(error, crate::agent_state::StoreError::NotFound(_)) {
                let message = format!("provider_attribution_unresolved: {error}");
                let snapshot_revision = {
                    let mut state = coordinator
                        .state
                        .lock()
                        .expect("canonical state lock poisoned");
                    let state = state
                        .as_mut()
                        .expect("state initialized before provider event");
                    match state.add_global_diagnostic(ErrorCode::StaleAgentEvent, message) {
                        Ok(revision) => revision,
                        Err(store_error) => {
                            return production_store_error_response(
                                coordinator,
                                store_error,
                                Some(envelope.event_id),
                            );
                        }
                    }
                };
                return ServerMessage::SnapshotAck {
                    event_id: envelope.event_id,
                    accepted_seq,
                    snapshot_revision,
                };
            }
            return agent_state_query_error_with_event(error, Some(envelope.event_id));
        }
    };

    // Provider adapters already reduce human-entered prompts and responses to
    // bounded, single-line UI previews. Keep those previews for the sidebar,
    // but never project the prompt of a guarded dispatch into PaneState.
    let private_prompt = apply_result.run.as_ref().is_some_and(|run| {
        run.operation_id.is_some()
            && apply_result.operation.as_ref().is_none_or(|operation| {
                observation.prompt_digest.as_deref() == Some(operation.prompt_digest.as_str())
            })
    });
    redact_private_provider_prompt(&mut envelope.event, private_prompt);

    if apply_result.disposition == crate::agent_state::reducer::ApplyDisposition::Duplicate
        && let Some(run) = apply_result.run.as_ref()
    {
        let projection_check = {
            let state = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            let Some(state) = state.as_ref() else {
                return ServerMessage::error(
                    ErrorCode::NotReady,
                    "daemon is hydrating",
                    Some(envelope.event_id),
                );
            };
            let pane = state.leased.runtime.record(&envelope.pane_instance);
            pane.map_or(Ok(false), |pane| {
                pane_needs_durable_run_projection(pane, run)
            })
            .map(|needed| {
                (
                    needed,
                    pane.map(crate::pane_state::PaneState::version),
                    state.leased.runtime.snapshot_revision(),
                )
            })
        };
        let (projection_is_current, state_version, snapshot_revision) = match projection_check {
            Ok(result) => result,
            Err(message) => {
                coordinator.fail_stop(message.clone());
                return ServerMessage::error(
                    ErrorCode::StateInvariantViolation,
                    message,
                    Some(envelope.event_id),
                );
            }
        };
        if !projection_is_current {
            return ServerMessage::PaneEventResult {
                event_id: envelope.event_id,
                accepted_seq,
                state_version,
                snapshot_revision,
                outcome: crate::daemon::protocol::v2::PaneApplyOutcome::Noop,
            };
        }
    }

    if apply_result.disposition == crate::agent_state::reducer::ApplyDisposition::EvidenceOnly
        && let Some(run) = apply_result.run.as_ref()
    {
        return project_provider_run_evidence_only(
            coordinator,
            accepted_seq,
            envelope.event_id,
            &envelope.pane_instance,
            run,
        );
    }

    apply_pane_event_mutation(coordinator, accepted_seq, envelope, false, apply_result.run)
}

pub(in crate::daemon::server) fn project_provider_run_evidence_only(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    pane_instance: &PaneInstance,
    run: &crate::agent_state::RunRecord,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{PaneApplyOutcome, ServerMessage};
    use crate::pane_state::reducer::ReductionOutcome;

    let result = (|| -> Result<
        crate::pane_state::store::ApplyResult,
        crate::pane_state::store::StoreError,
    > {
        let mut state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let state = state_guard.as_mut().ok_or_else(|| {
            crate::pane_state::store::StoreError::PersistFailed("daemon is hydrating".to_string())
        })?;
        let revision_before = state.leased.runtime.snapshot_revision();
        let mut io = pane_snapshot_store(coordinator);
        let changed = if state
            .leased
            .runtime
            .record(pane_instance)
            .is_some_and(|pane| pane_belongs_to_run_epoch(pane, run))
        {
            state.leased.runtime.project_current_run(
                &mut io,
                pane_instance,
                crate::pane_state::CurrentDurableRunProjection {
                    run_id: run.run_id.as_str().to_string(),
                    run_seq: run.run_seq,
                    run_revision: run.revision,
                },
                run.execution_active(),
                run.updated_at,
            )?
        } else {
            false
        };
        let result = crate::pane_state::store::ApplyResult {
            outcome: if changed {
                ReductionOutcome::CanonicalChanged
            } else {
                ReductionOutcome::Noop
            },
            state_version: state
                .leased
                .runtime
                .record(pane_instance)
                .map(crate::pane_state::PaneState::version),
            snapshot_revision: state.leased.runtime.snapshot_revision(),
        };
        finish_pane_event_projection(
            coordinator,
            state,
            pane_instance,
            None,
            revision_before,
            result,
            false,
        )
    })();

    match result {
        Ok(result) => ServerMessage::PaneEventResult {
            event_id,
            accepted_seq,
            state_version: result.state_version,
            snapshot_revision: result.snapshot_revision,
            outcome: if result.outcome == ReductionOutcome::CanonicalChanged {
                PaneApplyOutcome::Committed
            } else {
                PaneApplyOutcome::Noop
            },
        },
        Err(error) => production_store_error_response(coordinator, error, Some(event_id)),
    }
}

#[allow(clippy::result_large_err)]
pub(in crate::daemon::server) fn provider_binding_record(
    coordinator: &ProductionV2Coordinator,
    envelope: &PaneEventEnvelope,
    observation: &crate::hook::provider::ProviderObservation,
) -> std::result::Result<crate::pane_state::PaneState, ServerMessage> {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    let record = {
        let state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let Some(state) = state_guard.as_ref() else {
            return Err(ServerMessage::error(
                ErrorCode::NotReady,
                "daemon is hydrating",
                Some(envelope.event_id.clone()),
            ));
        };
        state
            .leased
            .runtime
            .record(&envelope.pane_instance)
            .cloned()
    };
    let Some(record) = record else {
        return Err(ServerMessage::error(
            ErrorCode::PaneNotFound,
            "provider event has no canonical pane state",
            Some(envelope.event_id.clone()),
        ));
    };
    let provider_session_matches = match record.agent_session_id.as_ref() {
        Some(session) => session == &observation.session_id,
        None => observation.hook_kind == crate::hook::provider::ProviderHookKind::UserPromptSubmit,
    };
    if record.agent != observation.provider || !provider_session_matches || !record.agent_present {
        return Err(ServerMessage::error(
            ErrorCode::StaleAgentEvent,
            "provider event no longer matches the live Agent Binding",
            Some(envelope.event_id.clone()),
        ));
    }
    Ok(record)
}

#[allow(clippy::result_large_err)]
pub(in crate::daemon::server) fn refresh_provider_process_identity(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    envelope: &PaneEventEnvelope,
    observation: &crate::hook::provider::ProviderObservation,
    runner: &dyn crate::tmux::TmuxRunner,
) -> std::result::Result<crate::pane_state::PaneState, ServerMessage> {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    const RESOLVE_ATTEMPTS: usize = 4;
    const RETRY_DELAY: Duration = Duration::from_millis(20);

    let mut last_error = None;
    let mut process = None;
    for attempt in 0..RESOLVE_ATTEMPTS {
        match runner.resolve_agent_process(envelope.pane_instance.pane_pid, &observation.provider) {
            Ok(Some(resolved)) => {
                process = Some(resolved);
                break;
            }
            Ok(None) => last_error = None,
            Err(error) => last_error = Some(error.to_string()),
        }
        if attempt + 1 < RESOLVE_ATTEMPTS {
            thread::sleep(RETRY_DELAY);
        }
    }
    let Some(process) = process else {
        let message = last_error.map_or_else(
            || {
                format!(
                    "fresh pane process scans found no exact provider process identity after {RESOLVE_ATTEMPTS} attempts"
                )
            },
            |error| {
                format!(
                    "fresh pane process scans could not verify provider identity after {RESOLVE_ATTEMPTS} attempts: {error}"
                )
            },
        );
        return Err(ServerMessage::error(
            ErrorCode::StaleAgentEvent,
            message,
            Some(envelope.event_id.clone()),
        ));
    };
    let dispatch = {
        let state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let Some(state) = state_guard.as_ref() else {
            return Err(ServerMessage::error(
                ErrorCode::NotReady,
                "daemon is hydrating",
                Some(envelope.event_id.clone()),
            ));
        };
        state
            .leased
            .runtime
            .freeze_observation_dispatch([envelope.pane_instance.clone()])
            .into_iter()
            .next()
            .expect("one requested pane produces one observation dispatch snapshot")
    };
    let daemon_instance_id = coordinator
        .router
        .lock()
        .expect("v2 router lock poisoned")
        .daemon_instance_id()
        .clone();
    let process_envelope = crate::daemon::workers::observation_envelope(
        daemon_instance_id,
        envelope.pane_instance.clone(),
        dispatch.base,
        &dispatch.tracker,
        crate::daemon::workers::ObservationSample {
            observed_at: epoch_seconds(),
            presence: crate::pane_state::AgentPresenceObservation::Present(
                observation.provider.clone(),
            ),
            capture: None,
            process: Some(crate::pane_state::ProcessObservation {
                agent_process_checked: true,
                agent_process: Some(process.clone()),
                background_process_alive: None,
                listening_ports: None,
            }),
        },
    )
    .map_err(|error| {
        ServerMessage::error(
            ErrorCode::InternalError,
            format!("could not build provider process observation: {error:#}"),
            Some(envelope.event_id.clone()),
        )
    })?;
    match apply_pane_event_mutation(coordinator, accepted_seq, process_envelope, false, None) {
        ServerMessage::PaneEventResult { .. } => {}
        ServerMessage::Error { code, message, .. } => {
            return Err(ServerMessage::error(
                code,
                message,
                Some(envelope.event_id.clone()),
            ));
        }
        response => {
            return Err(ServerMessage::error(
                ErrorCode::InternalError,
                format!("unexpected provider process refresh response: {response:?}"),
                Some(envelope.event_id.clone()),
            ));
        }
    }
    let record = provider_binding_record(coordinator, envelope, observation)?;
    if record.agent_process.as_ref() != Some(&process) || !record.scan_verified {
        return Err(ServerMessage::error(
            ErrorCode::StaleAgentEvent,
            "fresh pane process identity did not become the live Agent Binding",
            Some(envelope.event_id.clone()),
        ));
    }
    Ok(record)
}

pub(in crate::daemon::server) fn normalize_provider_pane_event(
    event: &mut PaneEvent,
    observed_at: i64,
) {
    match event {
        PaneEvent::AgentSessionStarted {
            observed_at: event_at,
            ..
        }
        | PaneEvent::ActivityObserved {
            observed_at: event_at,
        }
        | PaneEvent::ActivityAndProgressObserved {
            observed_at: event_at,
            ..
        }
        | PaneEvent::WaitRequested {
            observed_at: event_at,
            ..
        }
        | PaneEvent::FailRun {
            observed_at: event_at,
            ..
        }
        | PaneEvent::ProgressUpdated {
            observed_at: event_at,
            ..
        } => *event_at = observed_at,
        PaneEvent::BeginRun { started_at, .. } => *started_at = observed_at,
        PaneEvent::CompleteRun { completed_at }
        | PaneEvent::ResponseAndCompleteRun { completed_at, .. }
        | PaneEvent::MarkDone { completed_at, .. } => *completed_at = observed_at,
        PaneEvent::ExplicitStateReported { report } => report.observed_at = observed_at,
        PaneEvent::ObservationBatch { .. }
        | PaneEvent::MarkPaneRead { .. }
        | PaneEvent::TaskSummaryGenerated { .. }
        | PaneEvent::PaneRemoved { .. } => {}
    }
}

pub(in crate::daemon::server) fn redact_private_provider_prompt(
    event: &mut PaneEvent,
    private_prompt: bool,
) {
    if !private_prompt {
        return;
    }
    match event {
        PaneEvent::AgentSessionStarted { resumed_prompt, .. } => *resumed_prompt = None,
        PaneEvent::BeginRun { prompt, .. } => *prompt = None,
        PaneEvent::ActivityAndProgressObserved { operations, .. }
        | PaneEvent::ProgressUpdated { operations, .. } => {
            operations.retain(|operation| {
                !matches!(
                    operation,
                    crate::pane_state::ProgressOperation::SetPrompt(_)
                )
            });
        }
        PaneEvent::ExplicitStateReported { report } => report.prompt = None,
        _ => {}
    }
}

pub(in crate::daemon::server) fn provider_event_matches_pane_event(
    observation: &crate::hook::provider::ProviderObservation,
    event: &PaneEvent,
) -> bool {
    use crate::hook::provider::ProviderHookKind;

    match (observation.hook_kind, event) {
        (ProviderHookKind::SessionStart, PaneEvent::AgentSessionStarted { .. }) => true,
        (
            ProviderHookKind::UserPromptSubmit,
            PaneEvent::BeginRun {
                prompt:
                    Some(crate::pane_state::PromptState {
                        digest: Some(digest),
                        ..
                    }),
                ..
            },
        ) => observation.prompt_digest.as_deref() == Some(digest),
        (
            ProviderHookKind::Activity,
            PaneEvent::ActivityObserved { .. } | PaneEvent::ActivityAndProgressObserved { .. },
        )
        | (ProviderHookKind::Waiting, PaneEvent::WaitRequested { .. }) => true,
        (ProviderHookKind::Stop, PaneEvent::ResponseAndCompleteRun { .. }) => {
            observation.response.is_some()
        }
        (ProviderHookKind::Stop, PaneEvent::CompleteRun { .. }) => observation.response.is_none(),
        _ => false,
    }
}
