use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::super::common::WAIT_POLL_INITIAL_INTERVAL;
use super::super::connection::{ApiConnection, daemon_api_error};
use super::super::contract::{
    ApiError, ApiErrorCode, ApiErrorStage, ApiRetryAction, ApiSideEffect, CurrentRunSummary,
    DurableRunStatus, MAX_WAIT_TIMEOUT, OperationErrorReceipt,
};
use crate::agent_state::{
    AgentBinding, DispatchState, ExecutionPhase, OperationRecord, OperationRef, RunRecord, RunRef,
    SemanticOutcome,
};
use crate::daemon::protocol::v2::{
    CLIENT_REQUEST_TIMEOUT, ClientMessage, CurrentAgentRun, PROTOCOL_VERSION, PanePresentation,
    ResolvedSnapshot, ServerMessage,
};
use crate::tmux::TmuxRunner;

pub(in crate::api) const WAIT_POLL_MAX_INTERVAL: Duration = Duration::from_secs(1);

pub(in crate::api) fn query_agent_run(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    requested_run_ref: &str,
    deadline: Option<Instant>,
) -> Result<(ApiConnection, String, RunRecord)> {
    let mut connection = ApiConnection::connect(runner, env, deadline)?;
    let request_deadline = deadline
        .map(|deadline| deadline.min(Instant::now() + CLIENT_REQUEST_TIMEOUT))
        .unwrap_or_else(|| Instant::now() + CLIENT_REQUEST_TIMEOUT);
    connection.client.set_deadline(request_deadline);
    let response = connection
        .client
        .request(&ClientMessage::QueryAgentRun {
            proto: PROTOCOL_VERSION,
            run_ref: requested_run_ref.to_string(),
        })
        .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
    match response {
        ServerMessage::AgentRunResult {
            proto,
            run_ref,
            run,
        } if proto == PROTOCOL_VERSION && run_ref == requested_run_ref => {
            Ok((connection, run_ref, run))
        }
        ServerMessage::AgentRunResult { .. } => Err(api_error!(
            "invalid_daemon_response",
            "run query returned a mismatched protocol version or run_ref",
        )
        .into()),
        ServerMessage::Error { code, message, .. } => Err(daemon_api_error(code, message).into()),
        other => Err(api_error!(
            "invalid_daemon_response",
            format!("unexpected agent run result: {other:?}"),
        )
        .into()),
    }
}

pub(in crate::api) fn exact_binding_for_pane(
    pane: &PanePresentation,
    server_identity: &crate::daemon::topology::ServerIdentity,
) -> Option<AgentBinding> {
    let state = &pane.resolved.as_ref()?.canonical;
    if !state.agent_present {
        return None;
    }
    let process = state.agent_process.clone()?;
    if pane.agent_process.as_ref() != Some(&process) {
        return None;
    }
    let binding = AgentBinding {
        server_identity: server_identity.clone(),
        pane_instance: pane.pane_instance.clone(),
        pane_state_id: state.state_id.clone(),
        agent_epoch: state.agent_epoch,
        agent_kind: state.agent.clone(),
        provider_session_id: state.agent_session_id.clone()?,
        process,
    };
    binding.validate().ok()?;
    Some(binding)
}

pub(in crate::api) fn query_current_agent_runs(
    connection: &mut ApiConnection,
    snapshot: &ResolvedSnapshot,
) -> Result<Vec<CurrentAgentRun>> {
    let bindings = snapshot
        .panes
        .iter()
        .filter_map(|pane| exact_binding_for_pane(pane, &connection.incarnation.identity))
        .collect::<Vec<_>>();
    // Runtime connections accept exactly one request after Hello. Keep the snapshot and
    // current-run observations tied to the same daemon instance, but use a fresh connection for
    // the second request instead of writing to the peer-closed snapshot connection.
    let mut current_run_connection = connection.reconnect()?;
    current_run_connection
        .client
        .set_deadline(Instant::now() + CLIENT_REQUEST_TIMEOUT);
    let response = current_run_connection
        .client
        .request(&ClientMessage::QueryCurrentAgentRuns {
            proto: PROTOCOL_VERSION,
            bindings: bindings.clone(),
        })
        .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
    let runs = match response {
        ServerMessage::CurrentAgentRunsResult { proto, runs } if proto == PROTOCOL_VERSION => runs,
        ServerMessage::Error { code, message, .. } => {
            return Err(daemon_api_error(code, message).into());
        }
        other => {
            return Err(api_error!(
                "invalid_daemon_response",
                format!("unexpected current agent runs result: {other:?}"),
            )
            .into());
        }
    };
    let mut seen = Vec::<AgentBinding>::new();
    for run in &runs {
        if !bindings.contains(&run.binding) || seen.contains(&run.binding) {
            return Err(api_error!(
                "invalid_daemon_response",
                "current run batch returned an unrequested or duplicate Agent Binding",
            )
            .into());
        }
        let reference = RunRef::decode(&run.run_ref).map_err(|error| {
            api_error!(
                "invalid_daemon_response",
                format!("current run batch returned an invalid run_ref: {error}")
            )
        })?;
        if reference.server_identity != connection.server_identity {
            return Err(api_error!(
                "invalid_daemon_response",
                "current run batch returned a run_ref for another tmux server",
            )
            .into());
        }
        seen.push(run.binding.clone());
    }
    Ok(runs)
}

pub(in crate::api) fn current_run_for_pane(
    pane: &PanePresentation,
    runs: &[CurrentAgentRun],
) -> Option<CurrentRunSummary> {
    let state = &pane.resolved.as_ref()?.canonical;
    let process = state.agent_process.as_ref()?;
    let run = runs.iter().find(|run| {
        run.binding.pane_instance == pane.pane_instance
            && run.binding.pane_state_id == state.state_id
            && run.binding.agent_epoch == state.agent_epoch
            && &run.binding.process == process
    })?;
    Some(CurrentRunSummary {
        run_ref: run.run_ref.clone(),
        execution_phase: run.execution_phase,
        semantic_outcome: run.semantic_outcome,
        status: durable_run_status(run.execution_phase, run.semantic_outcome),
    })
}

pub(in crate::api) fn durable_run_status(
    execution_phase: ExecutionPhase,
    semantic_outcome: SemanticOutcome,
) -> DurableRunStatus {
    if semantic_outcome == SemanticOutcome::Completed {
        return DurableRunStatus::Done;
    }
    match execution_phase {
        ExecutionPhase::Running => DurableRunStatus::Working,
        ExecutionPhase::Waiting | ExecutionPhase::Error => DurableRunStatus::Blocked,
        ExecutionPhase::Ended => DurableRunStatus::EndedUnconfirmed,
    }
}

pub(in crate::api) fn query_agent_operation(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    requested_operation_ref: &str,
    deadline: Option<Instant>,
) -> Result<(ApiConnection, String, OperationRecord)> {
    let mut connection = ApiConnection::connect(runner, env, deadline)?;
    let request_deadline = deadline
        .map(|deadline| deadline.min(Instant::now() + CLIENT_REQUEST_TIMEOUT))
        .unwrap_or_else(|| Instant::now() + CLIENT_REQUEST_TIMEOUT);
    connection.client.set_deadline(request_deadline);
    let response = connection
        .client
        .request(&ClientMessage::QueryAgentOperation {
            proto: PROTOCOL_VERSION,
            operation_ref: requested_operation_ref.to_string(),
        })
        .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
    match response {
        ServerMessage::AgentOperationResult {
            proto,
            operation_ref,
            operation,
        } if proto == PROTOCOL_VERSION && operation_ref == requested_operation_ref => {
            Ok((connection, operation_ref, operation))
        }
        ServerMessage::AgentOperationResult { .. } => Err(api_error!(
            "invalid_daemon_response",
            "operation query returned a mismatched protocol version or operation_ref",
        )
        .into()),
        ServerMessage::Error { code, message, .. } => Err(daemon_api_error(code, message).into()),
        other => Err(api_error!(
            "invalid_daemon_response",
            format!("unexpected agent operation result: {other:?}"),
        )
        .into()),
    }
}

pub(in crate::api) fn wait_for_run(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    run_ref: &str,
    deadline: Instant,
    until_completed: bool,
) -> Result<(ApiConnection, String, RunRecord)> {
    let mut poll_interval = WAIT_POLL_INITIAL_INTERVAL;
    loop {
        match query_agent_run(runner, env, run_ref, Some(deadline)) {
            Ok((connection, returned_ref, run)) if run_wait_matches(&run, until_completed) => {
                return Ok((connection, returned_ref, run));
            }
            Ok(_) => {}
            Err(error) if retryable_poll_error(&error) => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            let expected = if until_completed {
                "semantic_outcome=completed"
            } else {
                "completed, waiting, error, or ended_unconfirmed"
            };
            return Err(api_error!(
                "timeout",
                format!("run {run_ref} did not reach {expected} before the deadline"),
            )
            .into());
        }
        sleep_until_next_poll(deadline, &mut poll_interval);
    }
}

pub(in crate::api) fn wait_for_operation(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    operation_ref: &str,
    deadline: Instant,
    follow_unknown: bool,
    initial_operation: Option<OperationRecord>,
) -> Result<(ApiConnection, String, OperationRecord)> {
    let mut poll_interval = WAIT_POLL_INITIAL_INTERVAL;
    let mut last_operation = initial_operation;
    loop {
        match query_agent_operation(runner, env, operation_ref, Some(deadline)) {
            Ok((connection, returned_ref, operation))
                if operation_wait_matches(&operation, follow_unknown) =>
            {
                return Ok((connection, returned_ref, operation));
            }
            Ok((_, _, operation)) => last_operation = Some(operation),
            Err(error) if retryable_poll_error(&error) => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            if let Some(operation) = last_operation {
                return Err(match operation.dispatch_state {
                    DispatchState::Prepared => {
                        operation_pre_dispatch_timeout(operation_ref, operation)
                    }
                    DispatchState::DispatchStarted => {
                        operation_dispatch_started_timeout(operation_ref, operation)
                    }
                    DispatchState::DeliveryUnknown => {
                        operation_terminal_error(operation_ref, operation)
                    }
                    DispatchState::PromptConfirmed | DispatchState::Rejected => ApiError::new(
                        ApiErrorCode::InvalidDaemonResponse,
                        "operation wait ignored a terminal operation",
                    ),
                }
                .into());
            }
            let expected = if follow_unknown {
                "prompt_confirmed or rejected after delivery_unknown"
            } else {
                "a terminal dispatch state"
            };
            return Err(prompt_wait_timeout(format!(
                "operation {operation_ref} did not reach {expected} before the deadline"
            ))
            .into());
        }
        sleep_until_next_poll(deadline, &mut poll_interval);
    }
}

pub(in crate::api) fn run_wait_matches(run: &RunRecord, until_completed: bool) -> bool {
    if until_completed {
        return run.semantic_outcome == SemanticOutcome::Completed;
    }
    durable_run_status(run.execution_phase, run.semantic_outcome) != DurableRunStatus::Working
}

pub(in crate::api) fn operation_wait_matches(
    operation: &OperationRecord,
    follow_unknown: bool,
) -> bool {
    match operation.dispatch_state {
        DispatchState::PromptConfirmed | DispatchState::Rejected => true,
        DispatchState::DeliveryUnknown => !follow_unknown,
        DispatchState::Prepared | DispatchState::DispatchStarted => false,
    }
}

pub(in crate::api) fn operation_is_terminal(operation: &OperationRecord) -> bool {
    matches!(
        operation.dispatch_state,
        DispatchState::PromptConfirmed | DispatchState::DeliveryUnknown | DispatchState::Rejected
    )
}

pub(in crate::api) fn linked_run_ref(
    operation_ref: &str,
    operation: &OperationRecord,
) -> Result<Option<String>> {
    let reference = OperationRef::decode(operation_ref).map_err(|error| {
        api_error!(
            "invalid_daemon_response",
            format!("daemon returned an invalid operation_ref: {error}")
        )
    })?;
    if reference.operation_id != operation.operation_id {
        return Err(api_error!(
            "invalid_daemon_response",
            "operation_ref does not identify the returned operation record",
        )
        .into());
    }
    operation
        .run_id
        .as_ref()
        .map(|run_id| {
            RunRef {
                server_identity: reference.server_identity,
                generation: reference.generation,
                run_id: run_id.clone(),
            }
            .encode()
            .map_err(|error| {
                api_error!(
                    "invalid_daemon_response",
                    format!("failed to derive linked run_ref: {error}")
                )
                .into()
            })
        })
        .transpose()
}

pub(in crate::api) fn validate_agent_wait_timeout(timeout: Duration) -> Result<()> {
    if timeout.is_zero() || timeout > MAX_WAIT_TIMEOUT {
        return Err(api_error!(
            "invalid_arguments",
            format!(
                "--timeout-ms must be between 1 and {}",
                MAX_WAIT_TIMEOUT.as_millis()
            ),
        )
        .into());
    }
    Ok(())
}

pub(in crate::api) fn retryable_poll_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|source| source.downcast_ref::<ApiError>())
        .is_some_and(|error| {
            matches!(
                error.code,
                ApiErrorCode::DaemonUnavailable
                    | ApiErrorCode::DaemonNotReady
                    | ApiErrorCode::DaemonQueryFailed
                    | ApiErrorCode::StaleDaemon
            )
        })
}

pub(in crate::api) fn sleep_until_next_poll(deadline: Instant, interval: &mut Duration) {
    if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        std::thread::sleep(remaining.min(*interval));
        *interval = next_wait_poll_interval(*interval);
    }
}

pub(in crate::api) fn next_wait_poll_interval(interval: Duration) -> Duration {
    interval
        .checked_mul(2)
        .unwrap_or(WAIT_POLL_MAX_INTERVAL)
        .min(WAIT_POLL_MAX_INTERVAL)
}

pub(in crate::api) fn elapsed_millis(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

pub(in crate::api) fn prompt_wait_timeout(message: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorCode::DeliveryUnknown, message).with_dispatch_context(
        ApiErrorStage::AfterDispatch,
        ApiSideEffect::Possible,
        ApiRetryAction::InspectManually,
        None,
    )
}

pub(in crate::api) fn prompt_before_dispatch_timeout(message: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorCode::Timeout, message).with_dispatch_context(
        ApiErrorStage::BeforeDispatch,
        ApiSideEffect::None,
        ApiRetryAction::RetrySameRequest,
        None,
    )
}

pub(in crate::api) fn operation_pre_dispatch_timeout(
    operation_ref: &str,
    operation: OperationRecord,
) -> ApiError {
    ApiError::new(
        ApiErrorCode::Timeout,
        "prompt operation remained prepared before the wait deadline",
    )
    .with_dispatch_context(
        ApiErrorStage::BeforeDispatch,
        ApiSideEffect::None,
        ApiRetryAction::RetrySameRequest,
        Some(OperationErrorReceipt {
            operation_ref: operation_ref.to_string(),
            operation,
        }),
    )
}

pub(in crate::api) fn operation_dispatch_started_timeout(
    operation_ref: &str,
    operation: OperationRecord,
) -> ApiError {
    ApiError::new(
        ApiErrorCode::DeliveryUnknown,
        "prompt dispatch started but was not confirmed before the wait deadline",
    )
    .with_dispatch_context(
        ApiErrorStage::AfterDispatch,
        ApiSideEffect::Possible,
        ApiRetryAction::InspectManually,
        Some(OperationErrorReceipt {
            operation_ref: operation_ref.to_string(),
            operation,
        }),
    )
}

pub(in crate::api) fn operation_terminal_error(
    operation_ref: &str,
    operation: OperationRecord,
) -> ApiError {
    let receipt_code = operation
        .result_receipt
        .as_ref()
        .map(|receipt| receipt.code.as_str())
        .unwrap_or("missing_receipt")
        .to_string();
    let dispatch_state = operation.dispatch_state;
    let receipt = Some(OperationErrorReceipt {
        operation_ref: operation_ref.to_string(),
        operation,
    });
    match dispatch_state {
        DispatchState::DeliveryUnknown => ApiError::new(
            ApiErrorCode::DeliveryUnknown,
            format!("prompt delivery is ambiguous: {receipt_code}"),
        )
        .with_dispatch_context(
            ApiErrorStage::AfterDispatch,
            ApiSideEffect::Possible,
            ApiRetryAction::InspectManually,
            receipt,
        ),
        DispatchState::Rejected => ApiError::new(
            ApiErrorCode::DispatchRejected,
            format!("prompt dispatch was rejected: {receipt_code}"),
        )
        .with_dispatch_context(
            ApiErrorStage::Dispatch,
            ApiSideEffect::None,
            ApiRetryAction::RefreshTarget,
            receipt,
        ),
        _ => ApiError::new(
            ApiErrorCode::InvalidDaemonResponse,
            "operation terminal error requested for a non-error state",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_state::{OperationId, Sha256Digest};
    use crate::api::test_support::*;

    #[test]
    fn operation_result_derives_the_linked_run_reference() {
        let generation = crate::agent_state::StateGeneration::generate().unwrap();
        let operation_id = OperationId::parse("operation_123456").unwrap();
        let run_id = crate::agent_state::StableRunId::generate().unwrap();
        let operation_ref = OperationRef {
            server_identity: "server_identity".to_string(),
            generation: generation.clone(),
            operation_id: operation_id.clone(),
        }
        .encode()
        .unwrap();
        let pane = test_agent_pane();
        let state = &pane.resolved.as_ref().unwrap().canonical;
        let mut operation = OperationRecord {
            state_format_version: crate::agent_state::PRIVATE_STATE_FORMAT_VERSION,
            generation,
            operation_id,
            revision: 3,
            request_fingerprint: Sha256Digest::of(b"request"),
            target_agent_ref: "vta1:test".to_string(),
            prompt_digest: Sha256Digest::of(b"prompt"),
            dispatch_option: "paste_enter".to_string(),
            binding: crate::agent_state::AgentBinding {
                server_identity: crate::daemon::topology::ServerIdentity {
                    pid: 77,
                    start_time: 88,
                },
                pane_instance: pane.pane_instance,
                pane_state_id: state.state_id.clone(),
                agent_epoch: state.agent_epoch,
                agent_kind: state.agent.clone(),
                provider_session_id: state.agent_session_id.clone().unwrap(),
                process: state.agent_process.clone().unwrap(),
            }
            .into(),
            expected_pane_version: state.version(),
            expected_current_run: state.current_run.clone(),
            expected_run_seq: 2,
            confirmation_deadline_at: 11,
            dispatch_state: DispatchState::PromptConfirmed,
            run_id: Some(run_id.clone()),
            result_receipt: Some(crate::agent_state::OperationResultReceipt {
                code: "prompt_confirmed".to_string(),
                observed_at: 3,
                confirmation_basis: Some("provider_prompt_digest".to_string()),
                source_attribution: Some("user_prompt_submit".to_string()),
            }),
            created_at: 1,
            updated_at: 3,
        };
        operation.validate().unwrap();

        let linked = linked_run_ref(&operation_ref, &operation).unwrap().unwrap();
        let linked = RunRef::decode(&linked).unwrap();
        assert_eq!(linked.server_identity, "server_identity");
        assert_eq!(linked.generation, operation.generation);
        assert_eq!(linked.run_id, run_id);

        operation.dispatch_state = DispatchState::Prepared;
        operation.run_id = None;
        operation.result_receipt = None;
        assert_eq!(linked_run_ref(&operation_ref, &operation).unwrap(), None);
    }

    #[test]
    fn durable_status_and_unknown_follow_match_the_public_wait_contract() {
        assert_eq!(
            durable_run_status(ExecutionPhase::Running, SemanticOutcome::Unresolved),
            DurableRunStatus::Working
        );
        assert_eq!(
            durable_run_status(ExecutionPhase::Waiting, SemanticOutcome::Unresolved),
            DurableRunStatus::Blocked
        );
        assert_eq!(
            durable_run_status(ExecutionPhase::Error, SemanticOutcome::Unresolved),
            DurableRunStatus::Blocked
        );
        assert_eq!(
            durable_run_status(ExecutionPhase::Ended, SemanticOutcome::Unresolved),
            DurableRunStatus::EndedUnconfirmed
        );
        assert_eq!(
            durable_run_status(ExecutionPhase::Ended, SemanticOutcome::Completed),
            DurableRunStatus::Done
        );

        let mut operation = {
            let generation = crate::agent_state::StateGeneration::generate().unwrap();
            let pane = test_agent_pane();
            let state = &pane.resolved.as_ref().unwrap().canonical;
            OperationRecord {
                state_format_version: crate::agent_state::PRIVATE_STATE_FORMAT_VERSION,
                generation,
                operation_id: OperationId::parse("operation_wait_1234").unwrap(),
                revision: 2,
                request_fingerprint: Sha256Digest::of(b"request"),
                target_agent_ref: "vta1:test".to_string(),
                prompt_digest: Sha256Digest::of(b"prompt"),
                dispatch_option: "paste_enter".to_string(),
                binding: AgentBinding {
                    server_identity: crate::daemon::topology::ServerIdentity {
                        pid: 77,
                        start_time: 88,
                    },
                    pane_instance: pane.pane_instance.clone(),
                    pane_state_id: state.state_id.clone(),
                    agent_epoch: state.agent_epoch,
                    agent_kind: state.agent.clone(),
                    provider_session_id: state.agent_session_id.clone().unwrap(),
                    process: state.agent_process.clone().unwrap(),
                }
                .into(),
                expected_pane_version: state.version(),
                expected_current_run: state.current_run.clone(),
                expected_run_seq: 2,
                confirmation_deadline_at: 11,
                dispatch_state: DispatchState::DeliveryUnknown,
                run_id: None,
                result_receipt: None,
                created_at: 1,
                updated_at: 2,
            }
        };
        assert!(operation_wait_matches(&operation, false));
        assert!(!operation_wait_matches(&operation, true));
        operation.result_receipt = Some(crate::agent_state::OperationResultReceipt {
            code: "prompt_confirmation_timeout".to_string(),
            observed_at: 2,
            confirmation_basis: None,
            source_attribution: None,
        });
        let unknown = operation_terminal_error("vto3:test", operation.clone());
        assert_eq!(unknown.code, ApiErrorCode::DeliveryUnknown);
        assert_eq!(unknown.side_effect, ApiSideEffect::Possible);
        assert_eq!(unknown.retry_action, ApiRetryAction::InspectManually);
        assert_eq!(
            unknown
                .receipt
                .as_ref()
                .map(|receipt| receipt.operation_ref.as_str()),
            Some("vto3:test")
        );
        operation.dispatch_state = DispatchState::PromptConfirmed;
        assert!(operation_wait_matches(&operation, true));
        operation.dispatch_state = DispatchState::Rejected;
        assert!(operation_wait_matches(&operation, true));
        operation.result_receipt.as_mut().unwrap().code = "pane_precondition_changed".to_string();
        let rejected = operation_terminal_error("vto3:test", operation);
        assert_eq!(rejected.code, ApiErrorCode::DispatchRejected);
        assert_eq!(rejected.side_effect, ApiSideEffect::None);
        assert_eq!(rejected.retry_action, ApiRetryAction::RefreshTarget);
    }

    #[test]
    fn durable_wait_polling_backs_off_to_one_second() {
        let mut interval = WAIT_POLL_INITIAL_INTERVAL;
        let mut observed = Vec::new();
        for _ in 0..6 {
            observed.push(interval);
            interval = next_wait_poll_interval(interval);
        }
        assert_eq!(
            observed,
            [50, 100, 200, 400, 800, 1_000].map(Duration::from_millis)
        );
        assert_eq!(next_wait_poll_interval(interval), WAIT_POLL_MAX_INTERVAL);
    }
}
