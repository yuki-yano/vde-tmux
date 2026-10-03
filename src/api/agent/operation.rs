use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::super::common::success_agent_json;
use super::super::contract::ApiResult;
use super::durable::{
    elapsed_millis, linked_run_ref, operation_terminal_error, query_agent_operation,
    validate_agent_wait_timeout, wait_for_operation,
};
use crate::agent_state::{DispatchState, OperationId, OperationRecord, Sha256Digest};
use crate::tmux::TmuxRunner;

pub fn agent_operation_abandon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    operation_ref: &str,
    expected_revision: u64,
    reason: &str,
) -> Result<String> {
    use crate::api::connection::{ApiConnection, daemon_api_error};
    use crate::daemon::protocol::v2::{
        CLIENT_REQUEST_TIMEOUT, ClientMessage, PROTOCOL_VERSION, ServerMessage,
    };

    crate::agent_state::OperationRef::decode(operation_ref)
        .map_err(|error| api_error!("invalid_reference", error.to_string()))?;
    if expected_revision == 0 {
        return Err(api_error!("invalid_arguments", "--expected-revision must be positive").into());
    }
    crate::agent_state::OperationResultReceipt::operator_abandon(reason, observed_at)
        .map_err(|error| api_error!("invalid_arguments", error.to_string()))?;
    let mut connection = ApiConnection::connect(runner, env, None)?;
    connection
        .client
        .set_deadline(Instant::now() + CLIENT_REQUEST_TIMEOUT);
    let event_id = crate::pane_state::EventId::generate()
        .map_err(|error| api_error!("internal_error", error.to_string()))?;
    let response = connection
        .client
        .request(&ClientMessage::AbandonAgentOperation {
            proto: PROTOCOL_VERSION,
            daemon_instance_id: connection.client.daemon_instance_id().clone(),
            event_id,
            operation_ref: operation_ref.to_string(),
            expected_revision,
            reason: reason.to_string(),
        })
        .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
    match response {
        ServerMessage::AgentOperationResult {
            proto,
            operation_ref: returned_ref,
            operation,
        } if proto == PROTOCOL_VERSION
            && returned_ref == operation_ref
            && operation.operator_abandoned() =>
        {
            success_agent_json(
                &connection,
                observed_at,
                ApiResult::AgentOperation {
                    operation_ref: returned_ref,
                    run_ref: None,
                    operation,
                    waited_ms: 0,
                },
            )
        }
        ServerMessage::Error { code, message, .. } => Err(daemon_api_error(code, message).into()),
        other => Err(api_error!(
            "invalid_daemon_response",
            format!("unexpected operation abandon result: {other:?}")
        )
        .into()),
    }
}

pub(crate) struct PromptRequestIdentity<'a> {
    pub operation_id: &'a OperationId,
    pub target: &'a str,
    pub prompt_digest: &'a Sha256Digest,
}

pub fn agent_operation_get(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    operation_ref: &str,
) -> Result<String> {
    let (connection, returned_ref, operation) =
        query_agent_operation(runner, env, operation_ref, None)?;
    let run_ref = linked_run_ref(&returned_ref, &operation)?;
    success_agent_json(
        &connection,
        observed_at,
        ApiResult::AgentOperation {
            operation_ref: returned_ref,
            run_ref,
            operation,
            waited_ms: 0,
        },
    )
}

pub fn agent_operation_wait(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    operation_ref: &str,
    timeout: Duration,
    _until_prompt_confirmed: bool,
    follow_unknown: bool,
) -> Result<String> {
    validate_agent_wait_timeout(timeout)?;
    let started = Instant::now();
    let deadline = started + timeout;
    let (connection, returned_ref, operation) =
        wait_for_operation(runner, env, operation_ref, deadline, follow_unknown, None)?;
    if operation.dispatch_state != DispatchState::PromptConfirmed {
        return Err(operation_terminal_error(&returned_ref, operation).into());
    }
    let run_ref = linked_run_ref(&returned_ref, &operation)?;
    success_agent_json(
        &connection,
        observed_at,
        ApiResult::AgentOperation {
            operation_ref: returned_ref,
            run_ref,
            operation,
            waited_ms: elapsed_millis(started),
        },
    )
}

pub(crate) fn agent_prompt_resume(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    operation_ref: &str,
    expected: PromptRequestIdentity<'_>,
    timeout: Duration,
) -> Result<String> {
    validate_agent_wait_timeout(timeout)?;
    let started = Instant::now();
    let deadline = started + timeout;
    let (initial_connection, returned_ref, initial_operation) =
        query_agent_operation(runner, env, operation_ref, Some(deadline))?;
    validate_resumed_prompt_operation(
        &returned_ref,
        &initial_operation,
        expected.operation_id,
        expected.target,
        expected.prompt_digest,
    )?;
    let (connection, returned_ref, operation) =
        if super::durable::operation_is_terminal(&initial_operation) {
            (initial_connection, returned_ref, initial_operation)
        } else {
            wait_for_operation(
                runner,
                env,
                operation_ref,
                deadline,
                false,
                Some(initial_operation),
            )?
        };
    validate_resumed_prompt_operation(
        &returned_ref,
        &operation,
        expected.operation_id,
        expected.target,
        expected.prompt_digest,
    )?;
    if returned_ref != operation_ref {
        return Err(crate::api::ApiError::new(
            crate::api::ApiErrorCode::InvalidDaemonResponse,
            "operation query returned a different operation_ref",
        )
        .into());
    }
    if operation.dispatch_state != DispatchState::PromptConfirmed {
        return Err(operation_terminal_error(&returned_ref, operation).into());
    }
    let run_ref = linked_run_ref(&returned_ref, &operation)?;
    success_agent_json(
        &connection,
        observed_at,
        ApiResult::AgentPrompt {
            operation_ref: returned_ref,
            run_ref,
            operation,
            waited_ms: elapsed_millis(started),
        },
    )
}

fn validate_resumed_prompt_operation(
    operation_ref: &str,
    operation: &OperationRecord,
    expected_operation_id: &OperationId,
    expected_target: &str,
    expected_prompt_digest: &Sha256Digest,
) -> Result<()> {
    let reference = crate::agent_state::OperationRef::decode(operation_ref).map_err(|error| {
        crate::api::ApiError::new(
            crate::api::ApiErrorCode::InvalidDaemonResponse,
            format!("daemon returned an invalid operation_ref during request resume: {error}"),
        )
    })?;
    if &reference.operation_id != expected_operation_id
        || &operation.operation_id != expected_operation_id
        || operation.target_agent_ref != expected_target
        || &operation.prompt_digest != expected_prompt_digest
    {
        return Err(crate::api::ApiError::new(
            crate::api::ApiErrorCode::InvalidDaemonResponse,
            "resumed operation does not match the persisted request-state intent",
        )
        .into());
    }
    Ok(())
}
