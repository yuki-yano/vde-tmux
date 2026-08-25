use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::super::common::{epoch_now, format_statuses, success_json};
use super::super::connection::ApiConnection;
use super::super::contract::{
    AgentStatus, AgentWaitMatchSource, ApiResult, MAX_WAIT_TIMEOUT, ReadOptions,
};
use super::super::pane::{capture_pane_guarded, validate_read_options, verify_live_pane};
use super::guards::{
    AgentIdentity, current_agent_after_event_match, reject_live_agent_process_replacement,
    reject_replaced_agent_process, require_same_agent, require_same_agent_state, resolve_agent,
    resolve_wait_resume_agent, verify_live_agent_process, wait_target,
};
use super::projection::{AgentStateView, agent_summary, canonical_state};
use crate::daemon::protocol::v2::{PanePresentation, ResolvedSnapshot};
use crate::daemon::session_badge::BadgeState;
use crate::pane_state::{LifecycleState, PaneInstance};
use crate::tmux::TmuxRunner;

pub fn agent_read(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
    options: ReadOptions,
) -> Result<String> {
    validate_read_options(options)?;
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let before = connection.query_snapshot()?;
    let pane = resolve_agent(&before, target, &connection.server_identity)?;
    let identity = AgentIdentity::from_pane(pane)?;
    verify_live_agent_process(runner, &identity, pane)?;
    let read = capture_pane_guarded(runner, env, &connection, &identity.pane_instance, options)?;
    let mut after_connection = connection.reconnect()?;
    let after = after_connection.query_snapshot()?;
    let pane = require_same_agent(&after, &identity)?;
    verify_live_agent_process(runner, &identity, pane)?;
    let summary = agent_summary(pane, &after, &after_connection.server_identity)
        .expect("same agent has resolved state");
    success_json(
        &after_connection,
        &after,
        observed_at,
        ApiResult::AgentRead {
            agent: summary,
            read,
        },
    )
}

pub fn agent_wait(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    target: &str,
    until: &BTreeSet<AgentStatus>,
    timeout: Duration,
    after_completed_seq: Option<u64>,
) -> Result<String> {
    if until.is_empty() {
        return Err(api_error!("invalid_arguments", "--until must not be empty").into());
    }
    if timeout.is_zero() {
        return Err(api_error!("invalid_arguments", "--timeout-ms must be positive").into());
    }
    if timeout > MAX_WAIT_TIMEOUT {
        return Err(api_error!(
            "invalid_arguments",
            format!(
                "--timeout-ms must not exceed {}",
                MAX_WAIT_TIMEOUT.as_millis()
            ),
        )
        .into());
    }
    if after_completed_seq.is_some() && !target.starts_with("vta1:") {
        return Err(api_error!(
            "invalid_arguments",
            "--after-completed-seq requires an exact agent_ref target",
        )
        .into());
    }
    let started_at = epoch_now();
    let started = Instant::now();
    let deadline = started + timeout;
    let mut connection = ApiConnection::connect(runner, env, Some(deadline))?;
    let first = connection.subscribe()?;
    let (pane, identity) = if after_completed_seq.is_some() {
        resolve_wait_resume_agent(&first, target, &connection.server_identity)?
    } else {
        let pane = resolve_agent(&first, target, &connection.server_identity)?;
        (pane, AgentIdentity::from_pane(pane)?)
    };
    let baseline = WaitBaseline::from_pane(pane, after_completed_seq)?;
    let completion_already_recorded = canonical_state(pane).is_some_and(|state| {
        until.contains(&AgentStatus::Done)
            && state.completed_seq >= baseline.expected_completion_seq
    });
    if canonical_state(pane).is_some_and(|state| state.agent_present)
        && !completion_already_recorded
    {
        verify_live_pane(runner, env, &connection, &identity.pane_instance)?;
        reject_live_agent_process_replacement(runner, &identity, pane)?;
    }
    let target = wait_target(
        pane,
        &connection.server_identity,
        &identity,
        target.starts_with("vta1:").then_some(target),
    );
    let mut history_revision = baseline.state_revision;
    let mut current = first;
    let mut initial = true;
    loop {
        if !initial
            && let Some(matched) = match_wait_event(&current, &baseline, history_revision, until)
        {
            let current_agent =
                current_agent_after_event_match(runner, env, &connection, &current, &identity);
            return success_json(
                &connection,
                &current,
                started_at,
                ApiResult::AgentWait {
                    target,
                    matched_status: matched.status,
                    match_source: AgentWaitMatchSource::TransitionEvent,
                    baseline_completed_seq: baseline.completed_seq,
                    matched_completed_seq: matched.completed_seq,
                    matched_state_revision: matched.state_revision,
                    matched_at: Some(matched.at_epoch),
                    current_agent,
                    waited_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                },
            );
        }

        let pane = require_same_agent_state(&current, &identity)?;
        let state = canonical_state(pane).expect("same agent state requires a retained record");
        if let Some(status) = match_current_wait_status(
            state,
            &baseline,
            until,
            initial,
            after_completed_seq.is_some(),
        ) {
            let current_agent = if state.agent_present {
                current_agent_after_event_match(runner, env, &connection, &current, &identity)
            } else {
                None
            };
            let matched_at = match status {
                AgentStatus::Done => state.completed_at,
                AgentStatus::Working
                | AgentStatus::Blocked
                | AgentStatus::Limited
                | AgentStatus::Idle => None,
            };
            return success_json(
                &connection,
                &current,
                started_at,
                ApiResult::AgentWait {
                    target,
                    matched_status: status,
                    match_source: AgentWaitMatchSource::CurrentState,
                    baseline_completed_seq: baseline.completed_seq,
                    matched_completed_seq: state.completed_seq,
                    matched_state_revision: state.revision,
                    matched_at,
                    current_agent,
                    waited_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                },
            );
        }
        verify_wait_history_coverage(&current, &baseline, history_revision, state.revision, until)?;
        if !state.agent_present {
            return Err(api_error!(
                "stale_reference",
                format!(
                    "agent in pane {} exited before reaching {}",
                    identity.pane_instance.pane_id,
                    format_statuses(until)
                ),
            )
            .into());
        }
        reject_replaced_agent_process(pane, &identity)?;
        history_revision = state.revision;
        initial = false;
        if Instant::now() >= deadline {
            return Err(api_error!(
                "timeout",
                format!(
                    "agent {} did not reach {} within {} ms",
                    identity.pane_instance.pane_id,
                    format_statuses(until),
                    timeout.as_millis()
                ),
            )
            .into());
        }
        current = match connection.next_snapshot() {
            Ok(snapshot) => snapshot,
            Err(_) if Instant::now() >= deadline => {
                return Err(api_error!(
                    "timeout",
                    format!(
                        "agent {} did not reach {} within {} ms",
                        identity.pane_instance.pane_id,
                        format_statuses(until),
                        timeout.as_millis()
                    ),
                )
                .into());
            }
            Err(error) => return Err(error),
        };
    }
}

#[derive(Debug, Clone)]
pub(in crate::api) struct WaitBaseline {
    pub(in crate::api) pane_instance: PaneInstance,
    pub(in crate::api) state_id: String,
    pub(in crate::api) agent_epoch: u64,
    pub(in crate::api) agent: String,
    pub(in crate::api) state_revision: u64,
    pub(in crate::api) completed_seq: u64,
    pub(in crate::api) expected_completion_seq: u64,
}

impl WaitBaseline {
    pub(in crate::api) fn from_pane(
        pane: &PanePresentation,
        after_completed_seq: Option<u64>,
    ) -> Result<Self> {
        let state = canonical_state(pane)
            .ok_or_else(|| api_error!("agent_not_found", "agent state is unavailable"))?;
        if after_completed_seq.is_some_and(|completed| completed > state.completed_seq) {
            return Err(api_error!(
                "invalid_arguments",
                format!(
                    "--after-completed-seq exceeds the current completed sequence {}",
                    state.completed_seq
                ),
            )
            .into());
        }
        let expected_completion_seq = match after_completed_seq {
            Some(completed) => completed
                .checked_add(1)
                .ok_or_else(|| api_error!("resource_limit", "run sequence overflow"))?,
            None if state.run_seq > state.completed_seq => state.run_seq,
            None => state
                .completed_seq
                .checked_add(1)
                .ok_or_else(|| api_error!("resource_limit", "run sequence overflow"))?,
        };
        Ok(Self {
            pane_instance: pane.pane_instance.clone(),
            state_id: state.state_id.as_str().to_string(),
            agent_epoch: state.agent_epoch,
            agent: state.agent.as_str().to_string(),
            state_revision: state.revision,
            completed_seq: after_completed_seq.unwrap_or(state.completed_seq),
            expected_completion_seq,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::api) struct WaitMatch {
    pub(in crate::api) status: AgentStatus,
    pub(in crate::api) state_revision: u64,
    pub(in crate::api) completed_seq: u64,
    pub(in crate::api) at_epoch: i64,
}

pub(in crate::api) fn match_wait_event(
    snapshot: &ResolvedSnapshot,
    baseline: &WaitBaseline,
    history_revision: u64,
    until: &BTreeSet<AgentStatus>,
) -> Option<WaitMatch> {
    for event in &snapshot.events {
        let Some(version) = &event.state_version else {
            continue;
        };
        if event.pane_instance != baseline.pane_instance
            || event.agent != baseline.agent
            || version.state_id.as_str() != baseline.state_id
            || version.agent_epoch != baseline.agent_epoch
            || version.revision <= history_revision
        {
            continue;
        }
        let badge_status = match event.to {
            BadgeState::Blocked => AgentStatus::Blocked,
            BadgeState::Limited => AgentStatus::Limited,
            BadgeState::Working => AgentStatus::Working,
            BadgeState::Done => AgentStatus::Done,
            BadgeState::Idle if event.completed_seq == 0 => AgentStatus::Idle,
            BadgeState::Idle => AgentStatus::Done,
        };
        let completion_is_new = event.completed_seq >= baseline.expected_completion_seq;
        let status = if until.contains(&badge_status)
            && (badge_status != AgentStatus::Done || completion_is_new)
        {
            Some(badge_status)
        } else if completion_is_new && until.contains(&AgentStatus::Done) {
            Some(AgentStatus::Done)
        } else {
            None
        };
        if let Some(status) = status {
            return Some(WaitMatch {
                status,
                state_revision: version.revision,
                completed_seq: event.completed_seq,
                at_epoch: event.at_epoch,
            });
        }
    }
    None
}

pub(in crate::api) fn match_current_wait_status(
    state: AgentStateView<'_>,
    baseline: &WaitBaseline,
    until: &BTreeSet<AgentStatus>,
    initial: bool,
    explicit_completion_baseline: bool,
) -> Option<AgentStatus> {
    if until.contains(&AgentStatus::Done) && state.completed_seq >= baseline.expected_completion_seq
    {
        return Some(AgentStatus::Done);
    }
    let current = match state.lifecycle {
        LifecycleState::Waiting { reason } if reason.is_usage_limit() => AgentStatus::Limited,
        LifecycleState::Waiting { .. } | LifecycleState::Error { .. } => AgentStatus::Blocked,
        LifecycleState::Running => AgentStatus::Working,
        LifecycleState::Idle if state.completed_seq > 0 => AgentStatus::Done,
        LifecycleState::Idle => AgentStatus::Idle,
    };
    if current == AgentStatus::Done && explicit_completion_baseline {
        return None;
    }
    (initial || state.revision > baseline.state_revision)
        .then_some(current)
        .filter(|status| until.contains(status))
}

pub(in crate::api) fn verify_wait_history_coverage(
    snapshot: &ResolvedSnapshot,
    baseline: &WaitBaseline,
    history_revision: u64,
    current_revision: u64,
    until: &BTreeSet<AgentStatus>,
) -> Result<()> {
    let transient_requested = until.iter().any(|status| *status != AgentStatus::Done);
    if !transient_requested {
        return Ok(());
    }
    let revisions = snapshot
        .events
        .iter()
        .filter_map(|event| {
            let version = event.state_version.as_ref()?;
            (event.pane_instance == baseline.pane_instance
                && event.agent == baseline.agent
                && version.state_id.as_str() == baseline.state_id
                && version.agent_epoch == baseline.agent_epoch
                && version.revision > history_revision
                && version.revision <= current_revision)
                .then_some(version.revision)
        })
        .collect::<BTreeSet<_>>();
    let revision_delta = current_revision.saturating_sub(history_revision);
    if (revisions.len() as u64) < revision_delta {
        return Err(api_error!(
            "event_history_lost",
            format!(
                "agent transition history no longer covers revisions {}..={}",
                history_revision.saturating_add(1),
                current_revision
            ),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::agent::guards::agent_ref;
    use crate::api::agent::projection::agent_status;
    use crate::api::contract::ApiError;
    use crate::api::test_support::*;

    #[test]
    fn acknowledged_completion_is_done_but_never_matches_idle() {
        let mut pane = test_agent_pane();
        let resolved = pane.resolved.as_mut().unwrap();
        resolved.canonical.lifecycle = LifecycleState::Idle;
        resolved.canonical.completed_seq = 1;
        resolved.canonical.completed_at = Some(2);
        resolved.badge = BadgeState::Idle;
        let baseline = WaitBaseline::from_pane(&pane, None).unwrap();
        let idle = [AgentStatus::Idle].into_iter().collect();
        let done = [AgentStatus::Done].into_iter().collect();
        let state = &pane.resolved.as_ref().unwrap().canonical;

        assert_eq!(agent_status(state), AgentStatus::Done);
        assert_eq!(
            match_current_wait_status(AgentStateView::from(state), &baseline, &idle, true, false,),
            None
        );
        assert_eq!(
            match_current_wait_status(AgentStateView::from(state), &baseline, &done, true, false,),
            Some(AgentStatus::Done)
        );
    }

    #[test]
    fn explicit_completion_baseline_can_match_an_existing_newer_completion() {
        let mut pane = test_agent_pane();
        let state = &mut pane.resolved.as_mut().unwrap().canonical;
        state.lifecycle = LifecycleState::Idle;
        state.completed_seq = 1;
        state.completed_at = Some(2);
        let baseline = WaitBaseline::from_pane(&pane, Some(0)).unwrap();
        let until = [AgentStatus::Done].into_iter().collect();
        let state = &pane.resolved.as_ref().unwrap().canonical;

        assert_eq!(
            match_current_wait_status(AgentStateView::from(state), &baseline, &until, true, true,),
            Some(AgentStatus::Done)
        );
    }

    #[test]
    fn wait_recovers_a_completion_coalesced_into_the_next_working_snapshot() {
        let first = test_agent_pane();
        let baseline = WaitBaseline::from_pane(&first, None).unwrap();
        let mut current = first.clone();
        let state = &mut current.resolved.as_mut().unwrap().canonical;
        state.revision = 3;
        state.run_seq = 2;
        state.completed_seq = 1;
        state.completed_at = Some(2);
        let mut completion_version = state.version();
        completion_version.revision = 2;
        let mut snapshot = test_snapshot(current.clone());
        snapshot.snapshot_revision = 3;
        snapshot.events.push(crate::daemon::TransitionEvent {
            pane_instance: current.pane_instance.clone(),
            agent: "codex".to_string(),
            state_version: Some(completion_version),
            run_seq: 1,
            completed_seq: 1,
            prompt_digest: None,
            prompt_submitted: false,
            from: Some(BadgeState::Working),
            to: BadgeState::Idle,
            at_epoch: 2,
        });
        snapshot.panes.clear();
        let until = [AgentStatus::Done].into_iter().collect();

        assert_eq!(
            match_wait_event(&snapshot, &baseline, baseline.state_revision, &until),
            Some(WaitMatch {
                status: AgentStatus::Done,
                state_revision: 2,
                completed_seq: 1,
                at_epoch: 2,
            })
        );
    }

    #[test]
    fn wait_recovers_a_transient_blocked_transition() {
        let first = test_agent_pane();
        let baseline = WaitBaseline::from_pane(&first, None).unwrap();
        let mut current = first.clone();
        let state = &mut current.resolved.as_mut().unwrap().canonical;
        state.revision = 3;
        let mut blocked_version = state.version();
        blocked_version.revision = 2;
        let mut snapshot = test_snapshot(current.clone());
        snapshot.snapshot_revision = 3;
        snapshot.events.push(crate::daemon::TransitionEvent {
            pane_instance: current.pane_instance.clone(),
            agent: "codex".to_string(),
            state_version: Some(blocked_version),
            run_seq: 1,
            completed_seq: 0,
            prompt_digest: None,
            prompt_submitted: false,
            from: Some(BadgeState::Working),
            to: BadgeState::Blocked,
            at_epoch: 2,
        });
        let until = [AgentStatus::Blocked].into_iter().collect();

        assert_eq!(
            match_wait_event(&snapshot, &baseline, baseline.state_revision, &until),
            Some(WaitMatch {
                status: AgentStatus::Blocked,
                state_revision: 2,
                completed_seq: 0,
                at_epoch: 2,
            })
        );
    }

    #[test]
    fn a_prior_completion_does_not_hide_a_later_blocked_transition() {
        let first = test_agent_pane();
        let baseline = WaitBaseline::from_pane(&first, None).unwrap();
        let mut current = first.clone();
        let state = &mut current.resolved.as_mut().unwrap().canonical;
        state.revision = 3;
        state.run_seq = 2;
        state.completed_seq = 1;
        let mut blocked_version = state.version();
        blocked_version.revision = 3;
        let mut snapshot = test_snapshot(current.clone());
        snapshot.events.push(crate::daemon::TransitionEvent {
            pane_instance: current.pane_instance.clone(),
            agent: "codex".to_string(),
            state_version: Some(blocked_version),
            run_seq: 2,
            completed_seq: 1,
            prompt_digest: None,
            prompt_submitted: false,
            from: Some(BadgeState::Working),
            to: BadgeState::Blocked,
            at_epoch: 3,
        });
        let until = [AgentStatus::Blocked].into_iter().collect();

        assert_eq!(
            match_wait_event(&snapshot, &baseline, baseline.state_revision, &until),
            Some(WaitMatch {
                status: AgentStatus::Blocked,
                state_revision: 3,
                completed_seq: 1,
                at_epoch: 3,
            })
        );
    }

    #[test]
    fn wait_history_coverage_advances_with_each_verified_snapshot() {
        let pane = test_agent_pane();
        let baseline = WaitBaseline::from_pane(&pane, None).unwrap();
        let until = [AgentStatus::Blocked].into_iter().collect();
        let snapshot_with_revisions = |start: u64, end: u64| {
            let mut snapshot = test_snapshot(pane.clone());
            snapshot.events = (start..=end)
                .map(|revision| crate::daemon::TransitionEvent {
                    pane_instance: pane.pane_instance.clone(),
                    agent: "codex".to_string(),
                    state_version: Some(crate::pane_state::StateVersion {
                        state_id: crate::pane_state::StateId::parse(
                            "00112233445566778899aabbccddeeff",
                        )
                        .unwrap(),
                        agent_epoch: 1,
                        revision,
                    }),
                    run_seq: 1,
                    completed_seq: 0,
                    prompt_digest: None,
                    prompt_submitted: false,
                    from: Some(BadgeState::Working),
                    to: BadgeState::Working,
                    at_epoch: revision as i64,
                })
                .collect();
            snapshot
        };

        let first = snapshot_with_revisions(2, 201);
        verify_wait_history_coverage(&first, &baseline, 1, 201, &until).unwrap();
        let second = snapshot_with_revisions(202, 401);
        verify_wait_history_coverage(&second, &baseline, 201, 401, &until).unwrap();
    }

    #[test]
    fn completion_cursor_resumes_an_exact_agent_that_exited_before_wait_started() {
        let mut pane = test_agent_pane();
        let reference = agent_ref("server", &pane);
        pane.agent_process = None;
        let mut state = pane.resolved.take().unwrap().canonical;
        state.agent_present = false;
        state.lifecycle = LifecycleState::Idle;
        state.completed_seq = 1;
        state.completed_at = Some(2);
        pane.retained_state = Some(crate::daemon::protocol::v2::RetainedAgentState::from(
            &state,
        ));
        let snapshot = test_snapshot(pane);

        let (pane, identity) = resolve_wait_resume_agent(&snapshot, &reference, "server").unwrap();
        assert_eq!(identity.agent, "codex");
        let baseline = WaitBaseline::from_pane(pane, Some(0)).unwrap();
        assert_eq!(baseline.completed_seq, 0);
        assert_eq!(baseline.expected_completion_seq, 1);
        let until = [AgentStatus::Done].into_iter().collect();
        assert_eq!(
            match_current_wait_status(
                canonical_state(pane).unwrap(),
                &baseline,
                &until,
                true,
                true,
            ),
            Some(AgentStatus::Done)
        );
    }

    #[test]
    fn wait_timeout_is_bounded_before_resolving_tmux() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let until = [AgentStatus::Done].into_iter().collect();

        let error = agent_wait(
            &runner,
            &BTreeMap::new(),
            "%1",
            &until,
            MAX_WAIT_TIMEOUT + Duration::from_millis(1),
            None,
        )
        .unwrap_err();

        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "invalid_arguments"
        );
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn completion_cursor_requires_an_exact_agent_reference_before_resolving_tmux() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let until = [AgentStatus::Done].into_iter().collect();

        let error = agent_wait(
            &runner,
            &BTreeMap::new(),
            "%1",
            &until,
            Duration::from_secs(1),
            Some(0),
        )
        .unwrap_err();

        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "invalid_arguments"
        );
        assert!(runner.calls().is_empty());
    }
}
