use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::super::common::success_agent_json;
use super::super::contract::ApiResult;
use super::durable::{
    elapsed_millis, linked_run_ref, operation_terminal_error, query_agent_operation,
    validate_agent_wait_timeout, wait_for_operation,
};
use crate::agent_state::DispatchState;
use crate::tmux::TmuxRunner;

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
        wait_for_operation(runner, env, operation_ref, deadline, follow_unknown)?;
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
