use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::Engine as _;

use super::super::common::{epoch_now, success_agent_json};
use super::super::connection::{ApiConnection, daemon_api_error};
use super::super::contract::{ApiResult, MAX_READ_BYTES, RunRecoveryDiagnostic, RunRecoveryStatus};
use super::durable::{elapsed_millis, query_agent_run, validate_agent_wait_timeout, wait_for_run};
use crate::agent_state::{
    RecoveryPaneFence, RecoveryPrecondition, RecoveryProcessExpectation,
    RecoveryViewportFingerprint, ResolutionId, RunRecord, SemanticOutcome, Sha256Digest,
    VIEWPORT_FINGERPRINT_CONVENTION_VERSION,
};
use crate::daemon::protocol::v2::{
    CLIENT_REQUEST_TIMEOUT, ClientMessage, PROTOCOL_VERSION, ResolvedSnapshot, ServerMessage,
};
use crate::pane_state::{EventId, LifecycleState, PaneInstance};
use crate::tmux::TmuxRunner;

pub fn agent_run_get(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    run_ref: &str,
) -> Result<String> {
    let (connection, returned_ref, run) = query_agent_run(runner, env, run_ref, None)?;
    success_agent_json(
        &connection,
        observed_at,
        ApiResult::AgentRun {
            run_ref: returned_ref,
            run,
            waited_ms: 0,
        },
    )
}

pub fn agent_run_wait(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    run_ref: &str,
    timeout: Duration,
    until_completed: bool,
) -> Result<String> {
    validate_agent_wait_timeout(timeout)?;
    let started = Instant::now();
    let deadline = started + timeout;
    let (connection, returned_ref, run) =
        wait_for_run(runner, env, run_ref, deadline, until_completed)?;
    success_agent_json(
        &connection,
        observed_at,
        ApiResult::AgentRun {
            run_ref: returned_ref,
            run,
            waited_ms: elapsed_millis(started),
        },
    )
}

pub fn agent_run_response(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    run_ref: &str,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    connection
        .client
        .set_deadline(Instant::now() + CLIENT_REQUEST_TIMEOUT);
    let response = connection
        .client
        .request(&ClientMessage::QueryAgentResponse {
            proto: PROTOCOL_VERSION,
            run_ref: run_ref.to_string(),
        })
        .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
    let (returned_ref, metadata, body_base64) = match response {
        ServerMessage::AgentResponseResult {
            proto,
            run_ref,
            metadata,
            body_base64,
        } if proto == PROTOCOL_VERSION => (run_ref, metadata, body_base64),
        ServerMessage::Error { code, message, .. } => {
            return Err(daemon_api_error(code, message).into());
        }
        ServerMessage::AgentResponseResult { .. } => {
            return Err(api_error!(
                "invalid_daemon_response",
                "response query returned a mismatched protocol version",
            )
            .into());
        }
        other => {
            return Err(api_error!(
                "invalid_daemon_response",
                format!("unexpected agent response result: {other:?}"),
            )
            .into());
        }
    };
    if returned_ref != run_ref {
        return Err(api_error!(
            "invalid_daemon_response",
            "response query returned a different run_ref",
        )
        .into());
    }
    metadata.validate().map_err(|error| {
        api_error!(
            "invalid_daemon_response",
            format!("invalid response metadata: {error}")
        )
    })?;
    if metadata.encoding != "utf-8" {
        return Err(api_error!(
            "invalid_daemon_response",
            format!("unsupported response encoding: {}", metadata.encoding),
        )
        .into());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body_base64.as_bytes())
        .map_err(|error| {
            api_error!(
                "invalid_daemon_response",
                format!("response body is not valid base64: {error}")
            )
        })?;
    if bytes.len() as u64 != metadata.stored_byte_count
        || metadata.stored_digest.as_ref() != Some(&Sha256Digest::of(&bytes))
    {
        return Err(api_error!(
            "invalid_daemon_response",
            "response body does not match its stored byte count and digest",
        )
        .into());
    }
    let body = String::from_utf8(bytes).map_err(|error| {
        api_error!(
            "invalid_daemon_response",
            format!("response body is not valid UTF-8: {error}")
        )
    })?;
    success_agent_json(
        &connection,
        observed_at,
        ApiResult::AgentResponse {
            run_ref: returned_ref,
            metadata,
            encoding: "utf-8".to_string(),
            body,
        },
    )
}

pub fn agent_run_check(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    run_ref: &str,
) -> Result<String> {
    agent_run_check_with_interval(runner, env, observed_at, run_ref, Duration::from_secs(2))
}

fn agent_run_check_with_interval(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    run_ref: &str,
    interval: Duration,
) -> Result<String> {
    let first = observe_run_recovery_sample(runner, env, run_ref)?;
    std::thread::sleep(interval);
    let second = observe_run_recovery_sample(runner, env, run_ref)?;
    if first.run_ref != run_ref
        || second.run_ref != run_ref
        || first.run.run_id != second.run.run_id
    {
        return Err(api_error!(
            "invalid_daemon_response",
            "run check returned a mismatched stable run identity",
        )
        .into());
    }

    let (status, message, process_expectation) = classify_run_recovery(&first, &second);
    let recovery_precondition = if let Some(process_expectation) = process_expectation {
        let issued_at = epoch_now();
        Some(RecoveryPrecondition {
            run_ref: run_ref.to_string(),
            binding: second.run.binding.clone(),
            run_revision: second.run.revision,
            evidence_digest: crate::agent_state::runtime::AgentRuntime::evidence_digest(
                &second.run,
            )?,
            pane: second.pane.clone(),
            viewport_fingerprint: second.viewport_fingerprint.clone(),
            process_expectation,
            issued_at,
            expires_at: issued_at.saturating_add(60),
        })
    } else {
        None
    };
    success_agent_json(
        &second.connection,
        observed_at,
        ApiResult::AgentRunCheck {
            run_ref: second.run_ref.clone(),
            run: Box::new(second.run.clone()),
            diagnostic: RunRecoveryDiagnostic {
                status,
                first_revision: first.run.revision,
                second_revision: second.run.revision,
                message,
            },
            recovery_precondition,
        },
    )
}

struct RunRecoveryObservation {
    connection: ApiConnection,
    run_ref: String,
    run: RunRecord,
    pane: RecoveryPaneFence,
    process: Option<crate::pane_state::AgentProcessIdentity>,
    viewport_fingerprint: Option<RecoveryViewportFingerprint>,
}

fn observe_run_recovery_sample(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    run_ref: &str,
) -> Result<RunRecoveryObservation> {
    let (connection, returned_ref, run) = query_agent_run(runner, env, run_ref, None)?;
    let mut snapshot_connection = connection.reconnect()?;
    let snapshot = snapshot_connection.query_snapshot()?;
    let pane = recovery_pane_fence(&snapshot, &run)?;
    let process = observe_run_process(runner, &run)?;
    let viewport_fingerprint = if process.as_ref() == Some(&run.binding.process) {
        verify_recovery_foreground_owner(runner, &run.binding.pane_instance, &run.binding.process)
            .map_err(|error| {
                api_error!(
                    "identity_verification_failed",
                    format!("the exact bound process is not the foreground input owner: {error}"),
                )
            })?;
        Some(capture_visible_viewport_fingerprint(
            runner,
            &run.binding.pane_instance,
        )?)
    } else {
        None
    };
    Ok(RunRecoveryObservation {
        connection,
        run_ref: returned_ref,
        run,
        pane,
        process,
        viewport_fingerprint,
    })
}

pub(crate) fn verify_recovery_foreground_owner(
    runner: &dyn TmuxRunner,
    pane: &PaneInstance,
    process: &crate::pane_state::AgentProcessIdentity,
) -> std::result::Result<(), String> {
    runner
        .verify_agent_input_owner(pane.pane_pid, process.pid)
        .map_err(|error| format!("{error:#}"))
}

fn recovery_pane_fence(snapshot: &ResolvedSnapshot, run: &RunRecord) -> Result<RecoveryPaneFence> {
    let state = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_instance == run.binding.pane_instance)
        .and_then(|pane| pane.resolved.as_ref())
        .map(|resolved| &resolved.canonical)
        .ok_or_else(|| {
            api_error!(
                "stale_precondition",
                "the pane bound to the run has no canonical state",
            )
        })?;
    if state.state_id != run.binding.pane_state_id
        || state.agent_epoch != run.binding.agent_epoch
        || state.agent != run.binding.agent_kind
        || state.agent_session_id.as_ref() != Some(&run.binding.provider_session_id)
        || state.pane_instance != run.binding.pane_instance
    {
        return Err(api_error!(
            "stale_precondition",
            "the pane no longer has the complete binding recorded by the run",
        )
        .into());
    }
    let current_run = state.current_run.clone().ok_or_else(|| {
        api_error!(
            "stale_precondition",
            "the pane no longer points at the checked durable run",
        )
    })?;
    if current_run.run_id != run.run_id.as_str()
        || current_run.run_seq != run.run_seq
        || current_run.run_revision != run.revision
    {
        return Err(
            api_error!("stale_precondition", "the pane durable-run pointer changed",).into(),
        );
    }
    if matches!(state.lifecycle, LifecycleState::Waiting { .. }) {
        return Err(api_error!(
            "recovery_not_allowed",
            "a pane waiting for permission or user input cannot be operator-completed",
        )
        .into());
    }
    if !state.subagents.is_empty() {
        return Err(api_error!(
            "recovery_not_allowed",
            "a run with active subagents cannot be operator-completed",
        )
        .into());
    }
    Ok(RecoveryPaneFence {
        state_id: state.state_id.clone(),
        revision: state.revision,
        current_run,
        lifecycle: state.lifecycle.clone(),
        subagent_count: 0,
    })
}

pub(crate) fn capture_visible_viewport_fingerprint(
    runner: &dyn TmuxRunner,
    pane: &PaneInstance,
) -> Result<RecoveryViewportFingerprint> {
    let before = recovery_viewport_dimensions(runner, pane)?;
    let output = runner
        .run_bounded(
            &["capture-pane", "-pJ", "-t", &pane.pane_id],
            MAX_READ_BYTES,
        )
        .map_err(|error| {
            api_error!(
                "capture_failed",
                format!("failed to capture the visible recovery viewport: {error:#}"),
            )
        })?;
    if output.truncated {
        return Err(api_error!(
            "resource_limit",
            "visible recovery viewport exceeded the capture limit",
        )
        .into());
    }
    let after = recovery_viewport_dimensions(runner, pane)?;
    if before != after {
        return Err(api_error!(
            "stale_precondition",
            "pane identity or dimensions changed during viewport capture",
        )
        .into());
    }
    let (pane_width, pane_height) = before;
    let mut bytes = Vec::with_capacity(output.text.len() + 32);
    bytes.extend_from_slice(b"vde-tmux-recovery-viewport-v1\0");
    bytes.extend_from_slice(&pane_width.to_be_bytes());
    bytes.extend_from_slice(&pane_height.to_be_bytes());
    bytes.extend_from_slice(output.text.as_bytes());
    Ok(RecoveryViewportFingerprint {
        convention_version: VIEWPORT_FINGERPRINT_CONVENTION_VERSION,
        pane_width,
        pane_height,
        digest: Sha256Digest::of(&bytes),
    })
}

fn recovery_viewport_dimensions(
    runner: &dyn TmuxRunner,
    pane: &PaneInstance,
) -> Result<(u16, u16)> {
    let output = runner
        .run(&[
            "display-message",
            "-p",
            "-t",
            &pane.pane_id,
            "#{pane_id}\t#{pane_pid}\t#{pane_width}\t#{pane_height}",
        ])
        .map_err(|error| {
            api_error!(
                "stale_precondition",
                format!("failed to resolve the recovery pane viewport: {error:#}"),
            )
        })?;
    let fields = output.trim_end().split('\t').collect::<Vec<_>>();
    let width = fields.get(2).and_then(|value| value.parse().ok());
    let height = fields.get(3).and_then(|value| value.parse().ok());
    if fields.len() != 4
        || fields[0] != pane.pane_id
        || fields[1].parse::<u32>().ok() != Some(pane.pane_pid)
        || width.is_none_or(|value: u16| value == 0)
        || height.is_none_or(|value: u16| value == 0)
    {
        return Err(api_error!(
            "stale_precondition",
            "recovery pane identity or dimensions are invalid",
        )
        .into());
    }
    Ok((width.unwrap(), height.unwrap()))
}

fn classify_run_recovery(
    first: &RunRecoveryObservation,
    second: &RunRecoveryObservation,
) -> (
    RunRecoveryStatus,
    Option<String>,
    Option<RecoveryProcessExpectation>,
) {
    if second.run.semantic_outcome == SemanticOutcome::Completed {
        return (
            RunRecoveryStatus::Completed,
            Some("run is already completed".to_string()),
            None,
        );
    }
    if first.run != second.run
        || first.pane != second.pane
        || first.process != second.process
        || first.viewport_fingerprint != second.viewport_fingerprint
    {
        return (
            RunRecoveryStatus::Unstable,
            Some(
                "run, pane, process, foreground ownership, or viewport changed between samples"
                    .to_string(),
            ),
            None,
        );
    }
    match &second.process {
        Some(process) if process == &second.run.binding.process => (
            RunRecoveryStatus::ExactPresentStable,
            Some(
                "the exact foreground process and content-agnostic viewport were stable"
                    .to_string(),
            ),
            Some(RecoveryProcessExpectation::ExactPresentStable {
                process: process.clone(),
            }),
        ),
        None => (
            RunRecoveryStatus::ExactAbsent,
            None,
            Some(RecoveryProcessExpectation::ExactAbsent),
        ),
        Some(process) => (
            RunRecoveryStatus::Replaced,
            None,
            Some(RecoveryProcessExpectation::ReplacedBy {
                process: process.clone(),
            }),
        ),
    }
}

fn observe_run_process(
    runner: &dyn TmuxRunner,
    run: &RunRecord,
) -> Result<Option<crate::pane_state::AgentProcessIdentity>> {
    runner
        .resolve_agent_process(run.binding.pane_instance.pane_pid, &run.binding.agent_kind)
        .map_err(|error| {
            api_error!(
                "identity_verification_failed",
                format!(
                    "could not freshly resolve the process bound to pane {}: {error}",
                    run.binding.pane_instance.pane_id
                ),
            )
            .into()
        })
}

#[allow(clippy::too_many_arguments)]
pub fn agent_run_resolve(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    run_ref: &str,
    outcome: &str,
    precondition: RecoveryPrecondition,
    resolution_id: &str,
    reason: &str,
) -> Result<String> {
    if outcome != "completed" {
        return Err(api_error!(
            "invalid_arguments",
            "agent run resolve only supports --outcome completed",
        )
        .into());
    }
    if reason.is_empty() || reason.len() > 1024 {
        return Err(api_error!(
            "invalid_arguments",
            "operator completion reason must be 1 to 1,024 UTF-8 bytes",
        )
        .into());
    }
    let resolution_id = ResolutionId::parse(resolution_id.to_string())
        .map_err(|error| api_error!("invalid_arguments", error.to_string()))?;
    precondition
        .validate()
        .map_err(|error| api_error!("invalid_arguments", error.to_string()))?;
    let mut connection = ApiConnection::connect(runner, env, None)?;
    connection
        .client
        .set_deadline(Instant::now() + CLIENT_REQUEST_TIMEOUT);
    let event_id =
        EventId::generate().map_err(|error| api_error!("internal_error", error.to_string()))?;
    let response = connection
        .client
        .request(&ClientMessage::ResolveAgentRun {
            proto: PROTOCOL_VERSION,
            daemon_instance_id: connection.client.daemon_instance_id().clone(),
            event_id,
            run_ref: run_ref.to_string(),
            outcome: outcome.to_string(),
            precondition,
            resolution_id,
            reason: reason.to_string(),
            actor_pid: std::process::id(),
        })
        .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
    match response {
        ServerMessage::AgentRunResolved {
            proto,
            run_ref: returned_ref,
            run,
        } if proto == PROTOCOL_VERSION && returned_ref == run_ref => success_agent_json(
            &connection,
            observed_at,
            ApiResult::AgentRunResolved {
                run_ref: returned_ref,
                run,
            },
        ),
        ServerMessage::Error { code, message, .. } => Err(daemon_api_error(code, message).into()),
        other => Err(api_error!(
            "invalid_daemon_response",
            format!("unexpected agent run resolve result: {other:?}"),
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_viewport_fingerprint_is_content_agnostic_and_dimension_bound() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        runner.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "%1",
                "#{pane_id}\t#{pane_pid}\t#{pane_width}\t#{pane_height}",
            ],
            "%1\t101\t80\t24\n",
        );
        runner.stub(&["capture-pane", "-pJ", "-t", "%1"], "first screen\n");
        let first = capture_visible_viewport_fingerprint(&runner, &pane).unwrap();

        runner.stub(&["capture-pane", "-pJ", "-t", "%1"], "different screen\n");
        let second = capture_visible_viewport_fingerprint(&runner, &pane).unwrap();

        assert_eq!(
            first.convention_version,
            VIEWPORT_FINGERPRINT_CONVENTION_VERSION
        );
        assert_eq!((first.pane_width, first.pane_height), (80, 24));
        assert_ne!(first.digest, second.digest);
    }

    #[test]
    fn recovery_foreground_fence_rejects_a_non_owner() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let process = crate::pane_state::AgentProcessIdentity {
            pid: 9001,
            start_token: "process-start".to_string(),
        };
        runner.stub_agent_input_owner(pane.pane_pid, process.pid, false);

        assert!(verify_recovery_foreground_owner(&runner, &pane, &process).is_err());
    }
}
