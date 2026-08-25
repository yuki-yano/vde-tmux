use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;

use crate::daemon::protocol::v2::{ErrorCode, HookHealth, ServerMessage};
use crate::pane_state::EventId;

use super::super::state_helpers::{pane_snapshot_store, production_store_error_response};
use super::super::{ProductionV2Coordinator, epoch_seconds};
use super::pane::pane_belongs_to_run_epoch;

#[cfg(test)]
mod tests;

#[allow(clippy::too_many_arguments)]
pub(in crate::daemon::server) fn apply_start_agent_prompt(
    coordinator: &ProductionV2Coordinator,
    event_id: EventId,
    target_agent_ref: String,
    operation_id: crate::agent_state::OperationId,
    prompt_base64: String,
    prompt_digest: crate::agent_state::Sha256Digest,
    dispatch_option: String,
    observed_at: i64,
) -> ServerMessage {
    let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3));
    apply_start_agent_prompt_with_runner(
        coordinator,
        &runner,
        event_id,
        target_agent_ref,
        operation_id,
        prompt_base64,
        prompt_digest,
        dispatch_option,
        observed_at,
    )
}

#[allow(clippy::too_many_arguments)]
pub(in crate::daemon::server) fn apply_start_agent_prompt_with_runner(
    coordinator: &ProductionV2Coordinator,
    runner: &dyn crate::tmux::TmuxRunner,
    event_id: EventId,
    target_agent_ref: String,
    operation_id: crate::agent_state::OperationId,
    prompt_base64: String,
    prompt_digest: crate::agent_state::Sha256Digest,
    dispatch_option: String,
    observed_at: i64,
) -> ServerMessage {
    use crate::agent_state::runtime::PrepareOperationResult;
    use crate::daemon::agent_dispatch::DispatchOutcome;
    use crate::daemon::protocol::v2::{ErrorCode, PROTOCOL_VERSION, ServerMessage};

    let operation_result = |runtime: &crate::agent_state::runtime::AgentRuntime,
                            operation: crate::agent_state::OperationRecord|
     -> ServerMessage {
        let reference = runtime.operation_ref(operation.operation_id.clone());
        match reference.encode() {
            Ok(operation_ref) => ServerMessage::AgentPromptResult {
                proto: PROTOCOL_VERSION,
                operation_ref,
                operation,
            },
            Err(error) => ServerMessage::error(
                ErrorCode::InternalError,
                error.to_string(),
                Some(event_id.clone()),
            ),
        }
    };

    if observed_at < 0 || dispatch_option != "paste_enter" {
        return ServerMessage::error(
            ErrorCode::InvalidRequest,
            "agent prompt requires a non-negative timestamp and dispatch_option=paste_enter",
            Some(event_id),
        );
    }
    let observed_at = epoch_seconds();
    let prompt = match base64::engine::general_purpose::STANDARD.decode(&prompt_base64) {
        Ok(prompt)
            if !prompt.is_empty()
                && prompt.len() <= crate::agent_state::PROMPT_BODY_MAX_BYTES
                && !prompt.contains(&0)
                && std::str::from_utf8(&prompt).is_ok() =>
        {
            prompt
        }
        _ => {
            return ServerMessage::error(
                ErrorCode::InvalidRequest,
                "agent prompt must be non-empty UTF-8 without NUL and at most 65,536 bytes",
                Some(event_id),
            );
        }
    };
    let decoded_prompt = std::str::from_utf8(&prompt).expect("prompt UTF-8 checked above");
    let observed_digest = crate::agent_state::Sha256Digest::parse(
        crate::pane_state::PromptState::digest_decoded_prompt(decoded_prompt),
    )
    .expect("PromptState emits a valid SHA-256 digest");
    if observed_digest != prompt_digest {
        return ServerMessage::error(
            ErrorCode::InvalidRequest,
            "agent prompt body does not match prompt_digest",
            Some(event_id),
        );
    }

    let request_fingerprint = crate::agent_state::runtime::AgentRuntime::request_fingerprint(
        &target_agent_ref,
        &prompt_digest,
        &dispatch_option,
    );
    let existing_prepared = {
        let runtime = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let Some(runtime) = runtime.as_ref() else {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "agent runtime is hydrating",
                Some(event_id),
            );
        };
        match runtime.lookup_operation_request(&operation_id, &request_fingerprint) {
            Ok(Some(existing))
                if existing.dispatch_state == crate::agent_state::DispatchState::Prepared =>
            {
                Some(existing)
            }
            Ok(Some(existing)) => return operation_result(runtime, existing),
            Ok(None) => None,
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        }
    };

    if let Some(existing) = existing_prepared.as_ref() {
        let expired = {
            let mut runtime = coordinator
                .agent_runtime
                .lock()
                .expect("agent runtime lock poisoned");
            let runtime = runtime.as_mut().expect("agent runtime checked above");
            runtime.reject_prepared_retry_if_expired(&existing.operation_id, observed_at)
        };
        match expired {
            Ok(Some(operation)) => {
                let runtime = coordinator
                    .agent_runtime
                    .lock()
                    .expect("agent runtime lock poisoned");
                let runtime = runtime.as_ref().expect("agent runtime checked above");
                return operation_result(runtime, operation);
            }
            Ok(None) => {}
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        }
    }

    if coordinator
        .router
        .lock()
        .expect("v2 router lock poisoned")
        .hook_health()
        != HookHealth::Healthy
    {
        return ServerMessage::error(
            ErrorCode::HookCollision,
            "hook health is degraded; prompt was not staged or sent",
            Some(event_id),
        );
    }

    let (binding, expected_run_seq, pane, expected_pane_version, expected_current_run) =
        match resolve_agent_prompt_target(coordinator, &target_agent_ref) {
            Ok(value) => value,
            Err(message) => {
                if let Some(rejection_code) =
                    prepared_target_rejection_code(existing_prepared.is_some(), None)
                {
                    let mut runtime = coordinator
                        .agent_runtime
                        .lock()
                        .expect("agent runtime lock poisoned");
                    let runtime = runtime.as_mut().expect("agent runtime checked above");
                    return match runtime.settle_dispatch(
                        &operation_id,
                        crate::agent_state::DispatchState::Rejected,
                        rejection_code,
                        observed_at,
                    ) {
                        Ok(operation) => operation_result(runtime, operation),
                        Err(error) => agent_state_query_error_with_event(error, Some(event_id)),
                    };
                }
                let code = if message.starts_with("unsupported provider:") {
                    ErrorCode::UnsupportedProvider
                } else {
                    ErrorCode::StaleAgentEvent
                };
                return ServerMessage::error(code, message, Some(event_id));
            }
        };
    let operation = if let Some(existing) = existing_prepared {
        let target_matches = prepared_operation_matches_target(
            &existing,
            &binding,
            expected_pane_version,
            expected_current_run.as_ref(),
            expected_run_seq,
        );
        if let Some(rejection_code) = prepared_target_rejection_code(true, Some(target_matches)) {
            let mut runtime = coordinator
                .agent_runtime
                .lock()
                .expect("agent runtime lock poisoned");
            let runtime = runtime.as_mut().expect("agent runtime checked above");
            return match runtime.settle_dispatch(
                &operation_id,
                crate::agent_state::DispatchState::Rejected,
                rejection_code,
                observed_at,
            ) {
                Ok(operation) => operation_result(runtime, operation),
                Err(error) => agent_state_query_error_with_event(error, Some(event_id)),
            };
        }
        existing
    } else {
        let mut runtime = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let runtime = runtime.as_mut().expect("agent runtime checked above");
        match runtime.prepare_operation(
            operation_id.clone(),
            target_agent_ref,
            &prompt,
            prompt_digest,
            dispatch_option,
            binding.clone(),
            expected_pane_version,
            expected_current_run,
            expected_run_seq,
            observed_at,
        ) {
            Ok(PrepareOperationResult::Existing(existing)) => {
                return operation_result(runtime, existing);
            }
            Ok(PrepareOperationResult::Created(created)) => created,
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        }
    };
    maybe_crash_agent_operation("after_prepared", &operation.operation_id);

    let reject_pre_dispatch = |code: &str, message: String| -> ServerMessage {
        let mut runtime = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let runtime = runtime.as_mut().expect("agent runtime initialized");
        match runtime.settle_dispatch(
            &operation.operation_id,
            crate::agent_state::DispatchState::Rejected,
            code,
            observed_at,
        ) {
            Ok(operation) => operation_result(runtime, operation),
            Err(error) => ServerMessage::error(
                ErrorCode::PersistFailed,
                format!("{message}; failed to persist rejection: {error}"),
                Some(event_id.clone()),
            ),
        }
    };

    let dispatch_lock =
        match acquire_agent_prompt_dispatch_lock(&coordinator.incarnation.identity, &pane) {
            Ok(lock) => lock,
            Err(rejection) => return reject_pre_dispatch(rejection.code, rejection.message),
        };

    if let Err(rejection) = verify_agent_prompt_process_and_owner(runner, &pane, &binding) {
        return reject_pre_dispatch(rejection.code, rejection.message);
    }
    if let Err(message) = verify_agent_prompt_precondition(coordinator, &operation) {
        return reject_pre_dispatch("pane_precondition_changed", message);
    }
    let staged = {
        let mut runtime = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let runtime = runtime.as_mut().expect("agent runtime initialized");
        if let Err(error) = runtime.mark_dispatch_started(&operation_id, observed_at) {
            return agent_state_query_error_with_event(error, Some(event_id));
        }
        match runtime.store().read_prompt(&operation_id) {
            Ok(prompt) => prompt,
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        }
    };
    let dispatch = crate::daemon::agent_dispatch::dispatch_prompt_guarded(
        runner,
        &coordinator.incarnation,
        &pane,
        &staged,
        operation_id.as_str(),
    );
    if matches!(dispatch, DispatchOutcome::Submitted) {
        maybe_crash_agent_operation("after_dispatch_submitted", &operation_id);
    }
    drop(dispatch_lock);

    let mut runtime = coordinator
        .agent_runtime
        .lock()
        .expect("agent runtime lock poisoned");
    let runtime = runtime.as_mut().expect("agent runtime initialized");
    let operation = match dispatch {
        DispatchOutcome::Submitted => match runtime.store().load_operation(&operation_id) {
            Ok(Some(operation)) => operation,
            Ok(None) => {
                return ServerMessage::error(
                    ErrorCode::InternalError,
                    "submitted operation disappeared",
                    Some(event_id),
                );
            }
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        },
        DispatchOutcome::Rejected(message) => match runtime.settle_dispatch(
            &operation_id,
            crate::agent_state::DispatchState::Rejected,
            "guarded_dispatch_rejected",
            observed_at,
        ) {
            Ok(operation) => operation,
            Err(error) => {
                return ServerMessage::error(
                    ErrorCode::PersistFailed,
                    format!("{message}; failed to persist rejection: {error}"),
                    Some(event_id),
                );
            }
        },
        DispatchOutcome::DeliveryUnknown(message) => match runtime.settle_dispatch(
            &operation_id,
            crate::agent_state::DispatchState::DeliveryUnknown,
            "guarded_dispatch_ambiguous",
            observed_at,
        ) {
            Ok(operation) => operation,
            Err(error) => {
                return ServerMessage::error(
                    ErrorCode::PersistFailed,
                    format!("{message}; failed to persist ambiguity: {error}"),
                    Some(event_id),
                );
            }
        },
    };
    operation_result(runtime, operation)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::daemon::server) struct AgentPromptPreDispatchRejection {
    pub(in crate::daemon::server) code: &'static str,
    pub(in crate::daemon::server) message: String,
}

pub(in crate::daemon::server) fn acquire_agent_prompt_dispatch_lock(
    server_identity: &crate::daemon::topology::ServerIdentity,
    pane: &crate::pane_state::PaneInstance,
) -> std::result::Result<crate::runtime_dir::PaneDispatchLock, AgentPromptPreDispatchRejection> {
    match crate::runtime_dir::try_acquire_pane_dispatch_lock(
        server_identity,
        &pane.pane_id,
        pane.pane_pid,
    ) {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => Err(AgentPromptPreDispatchRejection {
            code: "dispatch_lock_busy",
            message: "another guarded dispatch owns this pane".to_string(),
        }),
        Err(error) => Err(AgentPromptPreDispatchRejection {
            code: "dispatch_lock_error",
            message: format!("could not acquire guarded dispatch lock: {error:#}"),
        }),
    }
}

pub(in crate::daemon::server) fn verify_agent_prompt_process_and_owner(
    runner: &dyn crate::tmux::TmuxRunner,
    pane: &crate::pane_state::PaneInstance,
    binding: &crate::agent_state::OperationBinding,
) -> std::result::Result<(), AgentPromptPreDispatchRejection> {
    match runner.resolve_agent_process(pane.pane_pid, &binding.agent_kind) {
        Ok(Some(process)) if process == binding.process => {}
        Ok(Some(_)) => {
            return Err(AgentPromptPreDispatchRejection {
                code: "target_process_replaced",
                message: "exact agent process identity changed before guarded dispatch".to_string(),
            });
        }
        Ok(None) => {
            return Err(AgentPromptPreDispatchRejection {
                code: "target_process_absent",
                message: "exact agent process disappeared before guarded dispatch".to_string(),
            });
        }
        Err(error) => {
            return Err(AgentPromptPreDispatchRejection {
                code: "target_process_unverifiable",
                message: format!(
                    "could not re-resolve exact agent process before dispatch: {error:#}"
                ),
            });
        }
    }
    runner
        .verify_agent_input_owner(pane.pane_pid, binding.process.pid)
        .map_err(|error| AgentPromptPreDispatchRejection {
            code: "agent_not_input_owner",
            message: format!("exact agent process is not the foreground input owner: {error:#}"),
        })
}

pub(in crate::daemon::server) fn prepared_operation_matches_target(
    existing: &crate::agent_state::OperationRecord,
    binding: &crate::agent_state::OperationBinding,
    expected_pane_version: crate::pane_state::StateVersion,
    expected_current_run: Option<&crate::pane_state::CurrentDurableRunProjection>,
    expected_run_seq: u64,
) -> bool {
    existing.binding == *binding
        && existing.expected_pane_version == expected_pane_version
        && existing.expected_current_run.as_ref() == expected_current_run
        && existing.expected_run_seq == expected_run_seq
}

pub(in crate::daemon::server) fn prepared_target_rejection_code(
    has_existing_prepared: bool,
    resolved_target_matches: Option<bool>,
) -> Option<&'static str> {
    match (has_existing_prepared, resolved_target_matches) {
        (true, None) => Some("target_no_longer_current"),
        (true, Some(false)) => Some("binding_changed_before_dispatch"),
        (true, Some(true)) | (false, _) => None,
    }
}

#[cfg(debug_assertions)]
pub(in crate::daemon::server) fn maybe_crash_agent_operation(
    point: &str,
    operation_id: &crate::agent_state::OperationId,
) {
    let Some(root) = std::env::var_os("VDE_TMUX_TEST_AGENT_OPERATION_FAULT_DIR") else {
        return;
    };
    let marker = PathBuf::from(root).join(format!("{}.{}", operation_id.as_str(), point));
    if fs::remove_file(marker).is_ok() {
        std::process::abort();
    }
}

#[cfg(not(debug_assertions))]
pub(in crate::daemon::server) fn maybe_crash_agent_operation(
    _point: &str,
    _operation_id: &crate::agent_state::OperationId,
) {
}

pub(in crate::daemon::server) fn resolve_agent_prompt_target(
    coordinator: &ProductionV2Coordinator,
    target_agent_ref: &str,
) -> std::result::Result<
    (
        crate::agent_state::OperationBinding,
        u64,
        crate::pane_state::PaneInstance,
        crate::pane_state::StateVersion,
        Option<crate::pane_state::CurrentDurableRunProjection>,
    ),
    String,
> {
    use sha2::{Digest as _, Sha256};

    let parts = target_agent_ref.split(':').collect::<Vec<_>>();
    if parts.len() != 8 || parts[0] != "vta1" || parts[1] != coordinator.incarnation.hash {
        return Err("invalid or stale exact agent_ref".to_string());
    }
    let pane = crate::pane_state::PaneInstance {
        pane_id: format!("%{}", parts[2]),
        pane_pid: parts[3]
            .parse::<u32>()
            .map_err(|_| "invalid agent_ref pane PID".to_string())?,
    };
    pane.validate().map_err(|error| error.to_string())?;
    let expected_epoch = parts[5]
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "invalid agent_ref epoch".to_string())?;
    let expected_process_pid = parts[6]
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "invalid agent_ref process PID".to_string())?;
    if parts[7].len() != 64
        || !parts[7]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("invalid agent_ref process start token digest".to_string());
    }
    let record = {
        let state = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let state = state
            .as_ref()
            .ok_or_else(|| "daemon is hydrating".to_string())?;
        state
            .leased
            .runtime
            .record(&pane)
            .cloned()
            .ok_or_else(|| "agent pane is not retained".to_string())?
    };
    let process = record
        .agent_process
        .clone()
        .ok_or_else(|| "agent process identity is unavailable".to_string())?;
    let process_digest = format!("{:x}", Sha256::digest(process.start_token.as_bytes()));
    if record.state_id.as_str() != parts[4]
        || record.agent_epoch != expected_epoch
        || process.pid != expected_process_pid
        || process_digest != parts[7]
        || !record.agent_present
    {
        return Err("agent_ref was replaced before dispatch".to_string());
    }
    if record.agent.as_str() != "codex" {
        return Err(format!(
            "unsupported provider: durable guarded prompt dispatch is enabled only for Codex, not {}",
            record.agent.as_str()
        ));
    }
    if !matches!(record.lifecycle, crate::pane_state::LifecycleState::Idle) {
        return Err("agent is busy or blocked".to_string());
    }
    let provider_session_id = record.agent_session_id.clone();
    let expected_run_seq = record
        .run_seq
        .checked_add(1)
        .ok_or_else(|| "agent run sequence overflow".to_string())?;
    let expected_pane_version = record.version();
    let expected_current_run = record.current_run.clone();
    Ok((
        crate::agent_state::OperationBinding {
            server_identity: coordinator.incarnation.identity.clone(),
            pane_instance: pane.clone(),
            pane_state_id: record.state_id,
            agent_epoch: record.agent_epoch,
            agent_kind: record.agent,
            provider_session_id,
            process,
        },
        expected_run_seq,
        pane,
        expected_pane_version,
        expected_current_run,
    ))
}

pub(in crate::daemon::server) fn verify_agent_prompt_precondition(
    coordinator: &ProductionV2Coordinator,
    operation: &crate::agent_state::OperationRecord,
) -> std::result::Result<(), String> {
    let state = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state
        .as_ref()
        .ok_or_else(|| "daemon is hydrating".to_string())?;
    let record = state
        .leased
        .runtime
        .record(&operation.binding.pane_instance)
        .ok_or_else(|| "agent pane is no longer retained".to_string())?;
    let exact_binding_matches = agent_prompt_precondition_matches(record, operation);
    if exact_binding_matches {
        Ok(())
    } else {
        Err(
            "pane revision, lifecycle, current run, session, or process changed before dispatch"
                .to_string(),
        )
    }
}

pub(in crate::daemon::server) fn agent_prompt_precondition_matches(
    record: &crate::pane_state::PaneState,
    operation: &crate::agent_state::OperationRecord,
) -> bool {
    record.agent_present
        && record.version() == operation.expected_pane_version
        && record.current_run == operation.expected_current_run
        && record.agent == operation.binding.agent_kind
        && record.agent_session_id == operation.binding.provider_session_id
        && record.agent_process.as_ref() == Some(&operation.binding.process)
        && record.run_seq.checked_add(1) == Some(operation.expected_run_seq)
        && matches!(record.lifecycle, crate::pane_state::LifecycleState::Idle)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::daemon::server) fn apply_resolve_agent_run(
    coordinator: &ProductionV2Coordinator,
    event_id: EventId,
    run_ref: String,
    outcome: String,
    precondition: crate::agent_state::RecoveryPrecondition,
    resolution_id: crate::agent_state::ResolutionId,
    reason: String,
    actor_pid: u32,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, PROTOCOL_VERSION, ServerMessage};
    use crate::tmux::TmuxRunner as _;

    if outcome != "completed" || actor_pid == 0 {
        return ServerMessage::error(
            ErrorCode::InvalidRequest,
            "agent run resolve requires outcome=completed and a positive actor PID",
            Some(event_id),
        );
    }
    let observed_at = epoch_seconds();
    let reference = match crate::agent_state::RunRef::decode(&run_ref) {
        Ok(reference) => reference,
        Err(error) => {
            return ServerMessage::error(
                ErrorCode::InvalidRequest,
                error.to_string(),
                Some(event_id),
            );
        }
    };
    let (run, already_resolved) = {
        let runtime = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let Some(runtime) = runtime.as_ref() else {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "agent runtime is hydrating",
                Some(event_id),
            );
        };
        let existing = match runtime.lookup_operator_completion(&reference, &resolution_id, &reason)
        {
            Ok(existing) => existing,
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        };
        if let Some(run) = existing {
            (run, true)
        } else {
            match runtime.get_run(&reference) {
                Ok(run) => (run, false),
                Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
            }
        }
    };
    if already_resolved {
        if let Err(error) = project_operator_completed_run(coordinator, &run) {
            return production_store_error_response(coordinator, error, Some(event_id));
        }
        return ServerMessage::AgentRunResolved {
            proto: PROTOCOL_VERSION,
            run_ref,
            run,
        };
    }
    let runner = coordinator.status_push_runner(Duration::from_secs(3));
    let first_process = match runner
        .resolve_agent_process(run.binding.pane_instance.pane_pid, &run.binding.agent_kind)
    {
        Ok(process) => process,
        Err(error) => {
            return ServerMessage::error(
                ErrorCode::StalePrecondition,
                format!("fresh process observation failed: {error}"),
                Some(event_id),
            );
        }
    };
    let fresh_viewport_fingerprint =
        if let crate::agent_state::RecoveryProcessExpectation::ExactPresentStable {
            process: expected,
        } = &precondition.process_expectation
        {
            if first_process.as_ref() != Some(expected) || expected != &run.binding.process {
                return ServerMessage::error(
                    ErrorCode::StalePrecondition,
                    "the exact bound process is no longer present",
                    Some(event_id),
                );
            }
            if let Err(error) = crate::api::verify_recovery_foreground_owner(
                &runner,
                &run.binding.pane_instance,
                &run.binding.process,
            ) {
                return ServerMessage::error(
                    ErrorCode::StalePrecondition,
                    format!("the exact bound process is no longer the foreground owner: {error}"),
                    Some(event_id),
                );
            }
            let fingerprint = match crate::api::capture_visible_viewport_fingerprint(
                &runner,
                &run.binding.pane_instance,
            ) {
                Ok(fingerprint) => fingerprint,
                Err(error) => {
                    return ServerMessage::error(
                        ErrorCode::StalePrecondition,
                        format!("fresh viewport capture failed: {error:#}"),
                        Some(event_id),
                    );
                }
            };
            let second_process = match runner
                .resolve_agent_process(run.binding.pane_instance.pane_pid, &run.binding.agent_kind)
            {
                Ok(process) => process,
                Err(error) => {
                    return ServerMessage::error(
                        ErrorCode::StalePrecondition,
                        format!("second fresh process observation failed: {error}"),
                        Some(event_id),
                    );
                }
            };
            if second_process != first_process
                || crate::api::verify_recovery_foreground_owner(
                    &runner,
                    &run.binding.pane_instance,
                    &run.binding.process,
                )
                .is_err()
            {
                return ServerMessage::error(
                    ErrorCode::StalePrecondition,
                    "process identity or foreground ownership changed during viewport capture",
                    Some(event_id),
                );
            }
            Some(fingerprint)
        } else {
            None
        };
    let fresh_pane = match recovery_pane_fence_for_run(coordinator, &run) {
        Ok(pane) => pane,
        Err(message) => {
            return ServerMessage::error(ErrorCode::StalePrecondition, message, Some(event_id));
        }
    };
    let resolved = {
        let mut runtime = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let Some(runtime) = runtime.as_mut() else {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "agent runtime is hydrating",
                Some(event_id),
            );
        };
        match runtime.resolve_operator_completed(
            &reference,
            &precondition,
            resolution_id,
            reason,
            // The daemon socket is private to this user's runtime directory. Record the
            // effective daemon UID and the caller-reported PID in the durable audit.
            unsafe { libc::geteuid() },
            actor_pid,
            observed_at,
            &fresh_pane,
            first_process,
            fresh_viewport_fingerprint.as_ref(),
        ) {
            Ok(run) => run,
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        }
    };
    if let Err(error) = project_operator_completed_run(coordinator, &resolved) {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    ServerMessage::AgentRunResolved {
        proto: PROTOCOL_VERSION,
        run_ref,
        run: resolved,
    }
}

pub(in crate::daemon::server) fn project_operator_completed_run(
    coordinator: &ProductionV2Coordinator,
    run: &crate::agent_state::RunRecord,
) -> std::result::Result<(), crate::pane_state::store::StoreError> {
    let mut state = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let Some(state) = state.as_mut() else {
        return Err(crate::pane_state::store::StoreError::PersistFailed(
            "canonical pane state is hydrating".to_string(),
        ));
    };
    if !state
        .leased
        .runtime
        .record(&run.binding.pane_instance)
        .is_some_and(|pane| pane_belongs_to_run_epoch(pane, run))
    {
        // A replacement pane must never receive a historical run projection. The
        // durable Run is already complete, so there is nothing to repair here.
        return Ok(());
    }
    let projection = crate::pane_state::CurrentDurableRunProjection {
        run_id: run.run_id.as_str().to_string(),
        run_seq: run.run_seq,
        run_revision: run.revision,
    };
    let mut io = pane_snapshot_store(coordinator);
    if state.leased.runtime.project_current_run(
        &mut io,
        &run.binding.pane_instance,
        projection,
        false,
        run.updated_at,
    )? {
        let _ = state.checked_resolved_snapshot()?;
    }
    Ok(())
}

pub(in crate::daemon::server) fn recovery_pane_fence_for_run(
    coordinator: &ProductionV2Coordinator,
    run: &crate::agent_state::RunRecord,
) -> std::result::Result<crate::agent_state::RecoveryPaneFence, String> {
    let state = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let canonical = state
        .as_ref()
        .and_then(|state| state.leased.runtime.record(&run.binding.pane_instance))
        .ok_or_else(|| "the pane bound to the run has no canonical state".to_string())?;
    if canonical.state_id != run.binding.pane_state_id
        || canonical.agent_epoch != run.binding.agent_epoch
        || canonical.agent != run.binding.agent_kind
        || canonical.agent_session_id.as_ref() != Some(&run.binding.provider_session_id)
        || canonical.pane_instance != run.binding.pane_instance
    {
        return Err("the pane no longer has the complete binding recorded by the run".to_string());
    }
    let current_run = canonical
        .current_run
        .clone()
        .ok_or_else(|| "the pane no longer points at a durable run".to_string())?;
    let subagent_count = u32::try_from(canonical.subagents.len())
        .map_err(|_| "pane subagent count overflow".to_string())?;
    Ok(crate::agent_state::RecoveryPaneFence {
        state_id: canonical.state_id.clone(),
        revision: canonical.revision,
        current_run,
        lifecycle: canonical.lifecycle.clone(),
        subagent_count,
    })
}

pub(in crate::daemon::server) fn agent_state_query_error(
    error: crate::agent_state::StoreError,
) -> ServerMessage {
    agent_state_query_error_with_event(error, None)
}

pub(in crate::daemon::server) fn agent_state_query_error_with_event(
    error: crate::agent_state::StoreError,
    event_id: Option<EventId>,
) -> ServerMessage {
    use crate::agent_state::StoreError;
    let code = match error {
        StoreError::StalePrecondition(_) => ErrorCode::StalePrecondition,
        StoreError::RecoveryNotAllowed(_) => ErrorCode::RecoveryNotAllowed,
        StoreError::ResolutionConflict(_) => ErrorCode::ResolutionConflict,
        StoreError::RunAlreadyResolved(_) => ErrorCode::RunAlreadyResolved,
        StoreError::PromptDispatchBusy(_) => ErrorCode::PromptDispatchBusy,
        StoreError::OperationConflict(_) => ErrorCode::OperationConflict,
        StoreError::OperationStoreFull(_) => ErrorCode::OperationStoreFull,
        StoreError::OperationNotFound(_) => ErrorCode::OperationNotFound,
        StoreError::OperationGenerationReplaced(_) => ErrorCode::OperationGenerationReplaced,
        StoreError::RunNotFound(_) => ErrorCode::RunNotFound,
        StoreError::RunGenerationReplaced(_) => ErrorCode::RunGenerationReplaced,
        StoreError::ProviderEventConflict(_) => ErrorCode::ProviderEventConflict,
        StoreError::ArtifactUnavailable => ErrorCode::ArtifactUnavailable,
        StoreError::ArtifactExpired => ErrorCode::ArtifactExpired,
        StoreError::StateUninitialized => ErrorCode::StateUninitialized,
        StoreError::Capacity(_) => ErrorCode::StorageCapacityExceeded,
        StoreError::NotFound(_) => ErrorCode::RunNotFound,
        StoreError::Invalid(_) | StoreError::Conflict(_) => ErrorCode::InvalidRequest,
        StoreError::Io(_) | StoreError::Corrupt(_) => ErrorCode::PersistFailed,
    };
    ServerMessage::error(code, error.to_string(), event_id)
}
