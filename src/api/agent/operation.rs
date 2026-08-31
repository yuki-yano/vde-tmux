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
