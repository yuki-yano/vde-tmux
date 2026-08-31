use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::Engine as _;
use sha2::{Digest, Sha256};

use super::super::common::{
    WAIT_POLL_INITIAL_INTERVAL, epoch_now, success_agent_json, success_json,
};
use super::super::connection::{ApiConnection, daemon_api_error};
use super::super::contract::{
    AgentKeySendReceipt, AgentStartReceipt, AgentStatus, AgentSteerReceipt,
    AgentTerminalSendReceipt, ApiAgentStartCapability, ApiAgentSteerCapability, ApiError,
    ApiErrorCode, ApiErrorStage, ApiPromptDispatchCapability, ApiResult, ApiRetryAction,
    ApiSideEffect, ApiSteerRacePolicy, MAX_PROMPT_CONFIRM_TIMEOUT, MAX_WAIT_TIMEOUT,
};
use super::super::mutation::{
    after_dispatch_error, apply_terminal_mutation, refresh_canonical_topology_after_dispatch,
};
use super::super::pane::{
    pane_ref, require_live_pane_instance, require_same_pane, resolve_pane, verify_live_pane,
};
use super::super::terminal_mutation;
use super::durable::{
    elapsed_millis, linked_run_ref, operation_is_terminal, operation_terminal_error,
    prompt_before_dispatch_timeout, prompt_wait_timeout, sleep_until_next_poll, wait_for_operation,
};
use super::guards::{
    AgentIdentity, agent_ref, agent_start_readiness_matches, provider_capabilities,
    provider_program, render_shell_command, require_same_agent, resolve_agent, supported_shell,
    validate_prompt, validate_start_args, validate_terminal_keys, verify_agent_input_target,
    verify_live_agent_process, wait_target,
};
use super::projection::{agent_detail, agent_status};
use crate::agent_state::{DispatchState, OperationId, Sha256Digest};
use crate::daemon::protocol::v2::{
    CLIENT_REQUEST_TIMEOUT, ClientMessage, PROTOCOL_VERSION, ServerMessage, V2RequestFailureStage,
};
use crate::pane_state::{EventId, PaneInstance};
use crate::tmux::TmuxRunner;

pub fn agent_send(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
    prompt: &str,
) -> Result<String> {
    validate_prompt(prompt)?;
    if !target.starts_with("vta1:") {
        return Err(api_error!(
            "invalid_arguments",
            "agent send requires an exact agent_ref target",
        )
        .into());
    }
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let before = connection.query_snapshot()?;
    let pane = resolve_agent(&before, target, &connection.server_identity)?;
    let identity = AgentIdentity::from_pane(pane)?;
    let state = &pane
        .resolved
        .as_ref()
        .expect("resolve_agent requires resolved state")
        .canonical;
    match agent_status(state) {
        AgentStatus::Working => {
            return Err(api_error!(
                "agent_busy",
                format!(
                    "agent in pane {} is already working",
                    pane.pane_instance.pane_id
                ),
            )
            .into());
        }
        AgentStatus::Blocked => {
            return Err(api_error!(
                "agent_blocked",
                format!("agent in pane {} is blocked", pane.pane_instance.pane_id),
            )
            .into());
        }
        AgentStatus::Limited => {
            return Err(api_error!(
                "agent_limited",
                format!(
                    "agent in pane {} is usage limited",
                    pane.pane_instance.pane_id
                ),
            )
            .into());
        }
        AgentStatus::Done | AgentStatus::Idle => {}
    }
    let capability = provider_capabilities(state.agent.as_str()).ok_or_else(|| {
        api_error!(
            "unsupported_provider",
            format!(
                "agent {} has no public transport contract",
                state.agent.as_str()
            ),
        )
    })?;
    if capability.prompt_dispatch != ApiPromptDispatchCapability::GuardedTerminal {
        return Err(api_error!(
            "unsupported_provider",
            format!(
                "agent {} requires {:?} prompt dispatch instead of guarded terminal send",
                state.agent.as_str(),
                capability.prompt_dispatch
            ),
        )
        .into());
    }
    verify_agent_input_target(runner, env, &connection, pane, &identity)?;
    let prompt_digest = Sha256Digest::parse(crate::pane_state::PromptState::digest_decoded_prompt(
        prompt,
    ))
    .expect("PromptState emits a valid SHA-256 digest");
    let nonce = EventId::generate()
        .map_err(|error| api_error!("internal_error", format!("send request ID: {error}")))?;
    apply_terminal_mutation(terminal_mutation::submit_text_guarded(
        runner,
        &connection.incarnation,
        &pane.pane_instance,
        &pane.current_command,
        prompt.as_bytes(),
        nonce.as_str(),
    ))?;
    let mut after_connection = connection.reconnect()?;
    let after = after_connection.query_snapshot()?;
    let after_pane = require_same_agent(&after, &identity).map_err(after_dispatch_error)?;
    verify_live_agent_process(runner, &identity, after_pane).map_err(after_dispatch_error)?;
    let target_receipt = wait_target(pane, &connection.server_identity, &identity, Some(target));
    success_json(
        &after_connection,
        &after,
        observed_at,
        ApiResult::AgentSend {
            send: AgentTerminalSendReceipt {
                target: target_receipt,
                prompt_digest: prompt_digest.as_str().to_string(),
                baseline_state_revision: state.revision,
                baseline_run_seq: state.run_seq,
                baseline_completed_seq: state.completed_seq,
                dispatch: ApiPromptDispatchCapability::GuardedTerminal,
            },
        },
    )
}

pub fn agent_steer(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
    prompt: &str,
) -> Result<String> {
    validate_prompt(prompt)?;
    if !target.starts_with("vta1:") {
        return Err(api_error!(
            "invalid_arguments",
            "agent steer requires an exact agent_ref target",
        )
        .into());
    }
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let before = connection.query_snapshot()?;
    let pane = resolve_agent(&before, target, &connection.server_identity)?;
    let identity = AgentIdentity::from_pane(pane)?;
    let state = &pane
        .resolved
        .as_ref()
        .expect("resolve_agent requires resolved state")
        .canonical;
    match agent_status(state) {
        AgentStatus::Working => {}
        AgentStatus::Blocked => {
            return Err(api_error!(
                "agent_blocked",
                format!("agent in pane {} is blocked", pane.pane_instance.pane_id),
            )
            .into());
        }
        AgentStatus::Limited => {
            return Err(api_error!(
                "agent_limited",
                format!(
                    "agent in pane {} is usage limited",
                    pane.pane_instance.pane_id
                ),
            )
            .into());
        }
        status @ (AgentStatus::Done | AgentStatus::Idle) => {
            return Err(api_error!(
                "invalid_target",
                format!(
                    "agent steer requires a working agent; pane {} is {}",
                    pane.pane_instance.pane_id,
                    status.as_str()
                ),
            )
            .into());
        }
    }
    let capability = provider_capabilities(state.agent.as_str()).ok_or_else(|| {
        api_error!(
            "unsupported_provider",
            format!(
                "agent {} has no public transport contract",
                state.agent.as_str()
            ),
        )
    })?;
    if capability.steer != ApiAgentSteerCapability::GuardedTerminalBestEffort {
        return Err(api_error!(
            "unsupported_provider",
            format!(
                "agent {} does not support guarded terminal steering",
                state.agent.as_str()
            ),
        )
        .into());
    }
    verify_agent_input_target(runner, env, &connection, pane, &identity)?;
    let prompt_digest = Sha256Digest::parse(crate::pane_state::PromptState::digest_decoded_prompt(
        prompt,
    ))
    .expect("PromptState emits a valid SHA-256 digest");
    let nonce = EventId::generate()
        .map_err(|error| api_error!("internal_error", format!("steer request ID: {error}")))?;
    apply_terminal_mutation(terminal_mutation::submit_text_guarded(
        runner,
        &connection.incarnation,
        &pane.pane_instance,
        &pane.current_command,
        prompt.as_bytes(),
        nonce.as_str(),
    ))?;
    let mut after_connection = connection.reconnect()?;
    let after = after_connection.query_snapshot()?;
    let after_pane = require_same_agent(&after, &identity).map_err(after_dispatch_error)?;
    verify_live_agent_process(runner, &identity, after_pane).map_err(after_dispatch_error)?;
    let target_receipt = wait_target(pane, &connection.server_identity, &identity, Some(target));
    success_json(
        &after_connection,
        &after,
        observed_at,
        ApiResult::AgentSteer {
            steer: AgentSteerReceipt {
                target: target_receipt,
                prompt_digest: prompt_digest.as_str().to_string(),
                baseline_state_revision: state.revision,
                baseline_run_seq: state.run_seq,
                baseline_completed_seq: state.completed_seq,
                dispatch: ApiAgentSteerCapability::GuardedTerminalBestEffort,
                race_policy: ApiSteerRacePolicy::MayStartNextTurn,
            },
        },
    )
}

pub fn agent_send_keys(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
    keys: &[String],
) -> Result<String> {
    validate_terminal_keys(keys)?;
    if !target.starts_with("vta1:") {
        return Err(api_error!(
            "invalid_arguments",
            "agent send-keys requires an exact agent_ref target",
        )
        .into());
    }
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let before = connection.query_snapshot()?;
    let pane = resolve_agent(&before, target, &connection.server_identity)?;
    let identity = AgentIdentity::from_pane(pane)?;
    let state = &pane
        .resolved
        .as_ref()
        .expect("resolve_agent requires resolved state")
        .canonical;
    if agent_status(state) != AgentStatus::Blocked {
        return Err(api_error!(
            "invalid_target",
            format!(
                "agent send-keys requires a blocked agent; pane {} is {}",
                pane.pane_instance.pane_id,
                agent_status(state).as_str()
            ),
        )
        .into());
    }
    let capability = provider_capabilities(state.agent.as_str()).ok_or_else(|| {
        api_error!(
            "unsupported_provider",
            format!(
                "agent {} has no public transport contract",
                state.agent.as_str()
            ),
        )
    })?;
    if !capability.interactive_keys {
        return Err(api_error!(
            "unsupported_provider",
            format!(
                "agent {} does not support interactive keys",
                state.agent.as_str()
            ),
        )
        .into());
    }
    verify_agent_input_target(runner, env, &connection, pane, &identity)?;
    let nonce = EventId::generate()
        .map_err(|error| api_error!("internal_error", format!("key request ID: {error}")))?;
    apply_terminal_mutation(terminal_mutation::send_keys_guarded(
        runner,
        &connection.incarnation,
        &pane.pane_instance,
        &pane.current_command,
        keys,
        nonce.as_str(),
    ))?;
    let target_receipt = wait_target(pane, &connection.server_identity, &identity, Some(target));
    success_json(
        &connection,
        &before,
        observed_at,
        ApiResult::AgentSendKeys {
            send: AgentKeySendReceipt {
                target: target_receipt,
                keys: keys.to_vec(),
                baseline_state_revision: state.revision,
            },
        },
    )
}

pub fn agent_start(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
    agent: &str,
    args: &[String],
    timeout: Duration,
) -> Result<String> {
    if !target.starts_with("vtp1:") {
        return Err(api_error!(
            "invalid_arguments",
            "agent start requires an exact pane_ref target",
        )
        .into());
    }
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
    let normalized_agent = crate::pane_state::AgentKind::parse(agent)
        .map_err(|error| api_error!("invalid_arguments", error.to_string()))?;
    let capabilities = provider_capabilities(normalized_agent.as_str()).ok_or_else(|| {
        api_error!(
            "unsupported_provider",
            format!(
                "agent {} has no public start contract",
                normalized_agent.as_str()
            ),
        )
    })?;
    if capabilities.start == ApiAgentStartCapability::Disabled {
        return Err(api_error!(
            "unsupported_provider",
            format!(
                "agent {} cannot be started by this API",
                normalized_agent.as_str()
            ),
        )
        .into());
    }
    let program = provider_program(normalized_agent.as_str()).ok_or_else(|| {
        api_error!(
            "unsupported_provider",
            format!(
                "agent {} has no executable mapping",
                normalized_agent.as_str()
            ),
        )
    })?;
    validate_start_args(args)?;
    let command = render_shell_command(program, args);
    let command_digest = format!("{:x}", Sha256::digest(command.as_bytes()));
    let started = Instant::now();
    let deadline = started + timeout;
    let mut connection = ApiConnection::connect(runner, env, Some(deadline))?;
    let before = connection.subscribe()?;
    let pane = resolve_pane(&before, target, &connection.server_identity)?;
    if pane
        .resolved
        .as_ref()
        .is_some_and(|resolved| resolved.canonical.agent_present)
    {
        return Err(api_error!(
            "agent_busy",
            format!(
                "pane {} already contains an agent",
                pane.pane_instance.pane_id
            ),
        )
        .into());
    }
    if !supported_shell(&pane.current_command) {
        return Err(api_error!(
            "invalid_target",
            format!(
                "pane {} foreground command {} is not a supported interactive shell",
                pane.pane_instance.pane_id, pane.current_command
            ),
        )
        .into());
    }
    let expected_pane = pane.pane_instance.clone();
    verify_live_pane(runner, env, &connection, &expected_pane)?;
    let nonce = EventId::generate()
        .map_err(|error| api_error!("internal_error", format!("start request ID: {error}")))?;
    apply_terminal_mutation(terminal_mutation::submit_text_guarded(
        runner,
        &connection.incarnation,
        &expected_pane,
        &pane.current_command,
        command.as_bytes(),
        nonce.as_str(),
    ))?;
    let mut refresh_revision = refresh_canonical_topology_after_dispatch(
        &connection,
        format!(
            "agent {} was started in pane {}",
            normalized_agent.as_str(),
            expected_pane.pane_id
        ),
    )?;

    loop {
        if Instant::now() >= deadline {
            return Err(
                agent_start_timeout(&expected_pane, normalized_agent.as_str(), timeout).into(),
            );
        }
        let snapshot = match connection.next_snapshot() {
            Ok(snapshot) => snapshot,
            Err(_error) if Instant::now() >= deadline => {
                return Err(agent_start_timeout(
                    &expected_pane,
                    normalized_agent.as_str(),
                    timeout,
                )
                .into());
            }
            Err(error) => return Err(after_dispatch_error(error).into()),
        };
        if snapshot.snapshot_revision < refresh_revision {
            continue;
        }
        let current = match require_same_pane(&snapshot, &expected_pane) {
            Ok(current) => current,
            Err(error) => {
                require_live_pane_instance(runner, &expected_pane)
                    .map_err(|_| after_dispatch_error(error))?;
                refresh_revision = refresh_canonical_topology_after_dispatch(
                    &connection,
                    format!(
                        "agent {} was started in live pane {}",
                        normalized_agent.as_str(),
                        expected_pane.pane_id
                    ),
                )?;
                continue;
            }
        };
        let Some(resolved) = current.resolved.as_ref() else {
            continue;
        };
        if !resolved.canonical.agent_present {
            continue;
        }
        if resolved.canonical.agent != normalized_agent {
            return Err(api_error!(
                "target_replaced",
                format!(
                    "pane {} started {} instead of {}",
                    expected_pane.pane_id,
                    resolved.canonical.agent.as_str(),
                    normalized_agent.as_str()
                ),
            )
            .with_dispatch_context(
                ApiErrorStage::AfterDispatch,
                ApiSideEffect::Confirmed,
                ApiRetryAction::InspectManually,
                None,
            )
            .into());
        }
        if !agent_start_readiness_matches(capabilities.start, &resolved.canonical) {
            continue;
        }
        let identity = match AgentIdentity::from_pane(current) {
            Ok(identity) => identity,
            Err(_) => continue,
        };
        verify_live_agent_process(runner, &identity, current).map_err(after_dispatch_error)?;
        runner
            .verify_agent_input_owner(expected_pane.pane_pid, identity.agent_process.pid)
            .map_err(|error| {
                api_error!(
                    "agent_not_input_owner",
                    format!(
                        "started agent {} is not the foreground input owner: {error:#}",
                        expected_pane.pane_id
                    ),
                )
                .with_dispatch_context(
                    ApiErrorStage::AfterDispatch,
                    ApiSideEffect::Confirmed,
                    ApiRetryAction::InspectManually,
                    None,
                )
            })?;
        if Instant::now() + WAIT_POLL_INITIAL_INTERVAL >= deadline {
            return Err(
                agent_start_timeout(&expected_pane, normalized_agent.as_str(), timeout).into(),
            );
        }
        std::thread::sleep(WAIT_POLL_INITIAL_INTERVAL);
        let mut confirmation_connection = connection.reconnect().map_err(after_dispatch_error)?;
        let confirmation = confirmation_connection
            .query_snapshot()
            .map_err(after_dispatch_error)?;
        let confirmed =
            require_same_pane(&confirmation, &expected_pane).map_err(after_dispatch_error)?;
        let Some(confirmed_resolved) = confirmed.resolved.as_ref() else {
            continue;
        };
        if confirmed_resolved.canonical.agent != normalized_agent {
            return Err(api_error!(
                "target_replaced",
                format!(
                    "pane {} changed to {} while confirming {} readiness",
                    expected_pane.pane_id,
                    confirmed_resolved.canonical.agent.as_str(),
                    normalized_agent.as_str()
                ),
            )
            .with_dispatch_context(
                ApiErrorStage::AfterDispatch,
                ApiSideEffect::Confirmed,
                ApiRetryAction::InspectManually,
                None,
            )
            .into());
        }
        if !agent_start_readiness_matches(capabilities.start, &confirmed_resolved.canonical) {
            continue;
        }
        let Ok(confirmed_identity) = AgentIdentity::from_pane(confirmed) else {
            continue;
        };
        if confirmed_identity.state_id != identity.state_id
            || confirmed_identity.agent_epoch != identity.agent_epoch
            || confirmed_identity.agent_process != identity.agent_process
        {
            continue;
        }
        verify_live_agent_process(runner, &confirmed_identity, confirmed)
            .map_err(after_dispatch_error)?;
        runner
            .verify_agent_input_owner(expected_pane.pane_pid, confirmed_identity.agent_process.pid)
            .map_err(|error| {
                api_error!(
                    "agent_not_input_owner",
                    format!(
                        "started agent {} lost foreground input ownership: {error:#}",
                        expected_pane.pane_id
                    ),
                )
                .with_dispatch_context(
                    ApiErrorStage::AfterDispatch,
                    ApiSideEffect::Confirmed,
                    ApiRetryAction::InspectManually,
                    None,
                )
            })?;
        let agent_ref = agent_ref(&confirmation_connection.server_identity, confirmed);
        let detail = agent_detail(
            confirmed,
            &confirmation,
            &confirmation_connection.server_identity,
        )
        .expect("started exact agent has public detail");
        return success_json(
            &confirmation_connection,
            &confirmation,
            observed_at,
            ApiResult::AgentStart {
                start: AgentStartReceipt {
                    target_pane_ref: target.to_string(),
                    pane_ref: pane_ref(&confirmation_connection.server_identity, &expected_pane),
                    agent_ref,
                    agent: normalized_agent.as_str().to_string(),
                    readiness: capabilities.start,
                    command_digest,
                },
                agent: detail,
                waited_ms: elapsed_millis(started),
            },
        );
    }
}

pub fn agent_prompt(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    target: &str,
    operation_id: &str,
    prompt: &str,
    confirm_timeout: Duration,
) -> Result<String> {
    validate_prompt(prompt)?;
    if !target.starts_with("vta1:") {
        return Err(api_error!(
            "invalid_arguments",
            "agent prompt requires an exact agent_ref target",
        )
        .into());
    }
    let operation_id = OperationId::parse(operation_id.to_string()).map_err(|error| {
        api_error!(
            "invalid_arguments",
            format!("invalid --operation-id: {error}")
        )
    })?;
    if confirm_timeout.is_zero() || confirm_timeout > MAX_PROMPT_CONFIRM_TIMEOUT {
        return Err(api_error!(
            "invalid_arguments",
            format!(
                "--confirm-timeout-ms must be between 1 and {}",
                MAX_PROMPT_CONFIRM_TIMEOUT.as_millis()
            ),
        )
        .into());
    }

    let started_at = epoch_now();
    let started = Instant::now();
    let deadline = Instant::now() + confirm_timeout;
    let prompt_digest = Sha256Digest::parse(crate::pane_state::PromptState::digest_decoded_prompt(
        prompt,
    ))
    .expect("PromptState emits a valid SHA-256 digest");
    let prompt_base64 = base64::engine::general_purpose::STANDARD.encode(prompt.as_bytes());
    let observed_at = started_at;
    let mut poll_interval = WAIT_POLL_INITIAL_INTERVAL;
    let mut request_may_have_reached_daemon = false;

    let (start_connection, operation_ref, operation) = loop {
        if Instant::now() >= deadline {
            let message = "daemon did not accept or recover the idempotent prompt operation before the deadline";
            return Err(if request_may_have_reached_daemon {
                prompt_wait_timeout(message)
            } else {
                prompt_before_dispatch_timeout(message)
            }
            .into());
        }
        let mut connection = match ApiConnection::connect(runner, env, Some(deadline)) {
            Ok(connection) => connection,
            Err(_) => {
                sleep_until_next_poll(deadline, &mut poll_interval);
                continue;
            }
        };
        let event_id = EventId::generate()
            .map_err(|error| api_error!("internal_error", format!("event ID: {error}")))?;
        let request = ClientMessage::StartAgentPrompt {
            proto: PROTOCOL_VERSION,
            daemon_instance_id: connection.client.daemon_instance_id().clone(),
            event_id,
            target_agent_ref: target.to_string(),
            operation_id: operation_id.clone(),
            prompt_base64: prompt_base64.clone(),
            prompt_digest: prompt_digest.clone(),
            dispatch_option: "paste_enter".to_string(),
            observed_at,
        };
        connection
            .client
            .set_deadline(deadline.min(Instant::now() + CLIENT_REQUEST_TIMEOUT));
        match connection.client.request_with_stage(&request) {
            Ok(ServerMessage::AgentPromptResult {
                proto,
                operation_ref,
                operation,
            }) if proto == PROTOCOL_VERSION => break (connection, operation_ref, operation),
            Ok(ServerMessage::Error { code, message, .. }) => {
                return Err(daemon_api_error(code, message).into());
            }
            Ok(other) => {
                return Err(api_error!(
                    "invalid_daemon_response",
                    format!("unexpected prompt response: {other:?}"),
                )
                .into());
            }
            Err(error) => {
                request_may_have_reached_daemon |=
                    error.stage == V2RequestFailureStage::AfterFullWrite;
                // The caller-supplied operation ID makes this exact request replay-safe.
                sleep_until_next_poll(deadline, &mut poll_interval);
            }
        }
    };

    let (connection, operation) = if operation_is_terminal(&operation) {
        (start_connection, operation)
    } else {
        let (connection, returned_ref, operation) = wait_for_operation(
            runner,
            env,
            &operation_ref,
            deadline,
            false,
            Some(operation),
        )?;
        if returned_ref != operation_ref {
            return Err(api_error!(
                "invalid_daemon_response",
                "operation query returned a different operation_ref",
            )
            .into());
        }
        (connection, operation)
    };
    if operation.dispatch_state != DispatchState::PromptConfirmed {
        return Err(operation_terminal_error(&operation_ref, operation).into());
    }
    let run_ref = linked_run_ref(&operation_ref, &operation)?;
    success_agent_json(
        &connection,
        started_at,
        ApiResult::AgentPrompt {
            operation_ref,
            run_ref,
            operation,
            waited_ms: elapsed_millis(started),
        },
    )
}

pub(in crate::api) fn agent_start_timeout(
    pane: &PaneInstance,
    agent: &str,
    timeout: Duration,
) -> ApiError {
    ApiError::new(
        ApiErrorCode::Timeout,
        format!(
            "agent {agent} was submitted to pane {} but did not become exactly ready within {} ms",
            pane.pane_id,
            timeout.as_millis()
        ),
    )
    .with_dispatch_context(
        ApiErrorStage::AfterDispatch,
        ApiSideEffect::Confirmed,
        ApiRetryAction::InspectManually,
        None,
    )
}
