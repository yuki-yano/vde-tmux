use std::collections::BTreeMap;

use anyhow::Result;

use super::super::common::success_json;
use super::super::connection::ApiConnection;
use super::super::contract::{
    AgentBadge, AgentDetail, AgentIdentityStrength, AgentListFilter, AgentStatus, AgentSummary,
    ApiResult, LifecycleSummary, PaneDetail, PaneSummary, SessionLink, TaskSummaryStatus,
};
use super::super::pane::pane_ref;
use super::durable::{current_run_for_pane, query_current_agent_runs};
use super::guards::{agent_ref, resolve_agent};
use crate::daemon::protocol::v2::{PanePresentation, ResolvedSnapshot};
use crate::pane_state::{LifecycleState, WaitReason};
use crate::tmux::TmuxRunner;

pub fn agent_list(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    filter: &AgentListFilter,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let snapshot = connection.query_snapshot()?;
    let current_runs = query_current_agent_runs(&mut connection, &snapshot)?;
    let agents = snapshot
        .panes
        .iter()
        .filter_map(|pane| {
            let mut summary = agent_summary(pane, &snapshot, &connection.server_identity)?;
            summary.current_run = current_run_for_pane(pane, &current_runs);
            Some(summary)
        })
        .filter(|agent| matches_agent_filter(agent, filter))
        .collect();
    success_json(
        &connection,
        &snapshot,
        observed_at,
        ApiResult::AgentList { agents },
    )
}

pub fn agent_get(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let snapshot = connection.query_snapshot()?;
    let pane = resolve_agent(&snapshot, target, &connection.server_identity)?;
    let current_runs = query_current_agent_runs(&mut connection, &snapshot)?;
    let mut agent = agent_detail(pane, &snapshot, &connection.server_identity)
        .expect("resolve_agent only returns resolved agents");
    agent.summary.current_run = current_run_for_pane(pane, &current_runs);
    success_json(
        &connection,
        &snapshot,
        observed_at,
        ApiResult::AgentGet { agent },
    )
}

pub(in crate::api) fn pane_summary(pane: &PanePresentation, server_identity: &str) -> PaneSummary {
    PaneSummary {
        pane_ref: pane_ref(server_identity, &pane.pane_instance),
        pane_id: pane.pane_instance.pane_id.clone(),
        pane_pid: pane.pane_instance.pane_pid,
        sessions: session_links(pane),
        window_id: pane.window_id.clone(),
        window_name: pane.window_name.clone(),
        current_path: pane.current_path.clone(),
        current_command: pane.current_command.clone(),
        pane_width: pane.pane_width,
        active: pane.active,
        agent_ref: pane
            .resolved
            .as_ref()
            .filter(|resolved| resolved.canonical.agent_present && pane.agent_process.is_some())
            .map(|_| agent_ref(server_identity, pane)),
    }
}

#[derive(Clone, Copy)]
pub(in crate::api) struct AgentStateView<'a> {
    pub(in crate::api) state_id: &'a crate::pane_state::StateId,
    pub(in crate::api) revision: u64,
    pub(in crate::api) agent: &'a crate::pane_state::AgentKind,
    pub(in crate::api) agent_process: Option<&'a crate::pane_state::AgentProcessIdentity>,
    pub(in crate::api) agent_epoch: u64,
    pub(in crate::api) agent_present: bool,
    pub(in crate::api) lifecycle: &'a LifecycleState,
    pub(in crate::api) run_seq: u64,
    pub(in crate::api) completed_seq: u64,
    pub(in crate::api) completed_at: Option<i64>,
}

impl<'a> From<&'a crate::pane_state::PaneState> for AgentStateView<'a> {
    fn from(state: &'a crate::pane_state::PaneState) -> Self {
        Self {
            state_id: &state.state_id,
            revision: state.revision,
            agent: &state.agent,
            agent_process: state.agent_process.as_ref(),
            agent_epoch: state.agent_epoch,
            agent_present: state.agent_present,
            lifecycle: &state.lifecycle,
            run_seq: state.run_seq,
            completed_seq: state.completed_seq,
            completed_at: state.completed_at,
        }
    }
}

impl<'a> From<&'a crate::daemon::protocol::v2::RetainedAgentState> for AgentStateView<'a> {
    fn from(state: &'a crate::daemon::protocol::v2::RetainedAgentState) -> Self {
        Self {
            state_id: &state.state_id,
            revision: state.revision,
            agent: &state.agent,
            agent_process: state.agent_process.as_ref(),
            agent_epoch: state.agent_epoch,
            agent_present: state.agent_present,
            lifecycle: &state.lifecycle,
            run_seq: state.run_seq,
            completed_seq: state.completed_seq,
            completed_at: state.completed_at,
        }
    }
}

pub(in crate::api) fn canonical_state(pane: &PanePresentation) -> Option<AgentStateView<'_>> {
    if let Some(resolved) = &pane.resolved {
        Some(AgentStateView::from(&resolved.canonical))
    } else {
        pane.retained_state.as_ref().map(AgentStateView::from)
    }
}

pub(in crate::api) fn pane_detail(
    pane: &PanePresentation,
    snapshot: &ResolvedSnapshot,
    server_identity: &str,
) -> PaneDetail {
    PaneDetail {
        summary: pane_summary(pane, server_identity),
        agent: agent_detail(pane, snapshot, server_identity),
    }
}

pub(in crate::api) fn session_links(pane: &PanePresentation) -> Vec<SessionLink> {
    pane.session_links
        .iter()
        .map(|link| SessionLink {
            session_id: link.session_id.clone(),
            session_name: link.session_name.clone(),
            window_index: link.window_index,
            window_active: link.window_active,
            window_last: link.window_last,
        })
        .collect()
}

pub(in crate::api) fn lifecycle_summary(lifecycle: &LifecycleState) -> LifecycleSummary {
    match lifecycle {
        LifecycleState::Idle => LifecycleSummary {
            state: "idle".to_string(),
            reason: None,
        },
        LifecycleState::Running => LifecycleSummary {
            state: "running".to_string(),
            reason: None,
        },
        LifecycleState::Waiting { reason } => LifecycleSummary {
            state: "waiting".to_string(),
            reason: Some(match reason {
                WaitReason::PermissionPrompt => "permission_prompt".to_string(),
                WaitReason::Other(reason) => reason.clone(),
            }),
        },
        LifecycleState::Error { reason } => LifecycleSummary {
            state: "error".to_string(),
            reason: reason.clone(),
        },
    }
}

pub(in crate::api) fn agent_summary(
    pane: &PanePresentation,
    snapshot: &ResolvedSnapshot,
    server_identity: &str,
) -> Option<AgentSummary> {
    let resolved = pane.resolved.as_ref()?;
    let state = &resolved.canonical;
    if !state.agent_present {
        return None;
    }
    let exact_identity = pane.agent_process.is_some();
    let task_summary = state.task_context.current_summary();
    let task_summary_status = match task_summary.map(|summary| summary.outcome) {
        Some(crate::pane_state::TaskSummaryOutcome::Generated) => Some(TaskSummaryStatus::Current),
        Some(crate::pane_state::TaskSummaryOutcome::Failed) => Some(TaskSummaryStatus::Failed),
        None => None,
    };
    Some(AgentSummary {
        agent_ref: exact_identity.then(|| agent_ref(server_identity, pane)),
        identity: if exact_identity {
            AgentIdentityStrength::Exact
        } else {
            AgentIdentityStrength::Inferred
        },
        pane_ref: pane_ref(server_identity, &pane.pane_instance),
        pane_id: pane.pane_instance.pane_id.clone(),
        pane_pid: pane.pane_instance.pane_pid,
        agent: state.agent.as_str().to_string(),
        status: agent_status(state),
        badge: AgentBadge::from(resolved.badge),
        lifecycle: lifecycle_summary(&state.lifecycle),
        current_run: None,
        sessions: session_links(pane),
        window_id: pane.window_id.clone(),
        window_name: pane.window_name.clone(),
        current_path: pane.current_path.clone(),
        active: pane.active,
        present: state.agent_present,
        unread: state.unread.is_unread(),
        needs_action: snapshot
            .sidebar_model
            .needs_action
            .contains(&pane.pane_instance),
        task_summary: task_summary
            .filter(|summary| summary.outcome == crate::pane_state::TaskSummaryOutcome::Generated)
            .and_then(|summary| summary.text.clone()),
        task_summary_status,
        task_summary_error: task_summary
            .filter(|summary| summary.outcome == crate::pane_state::TaskSummaryOutcome::Failed)
            .and_then(|summary| summary.failure_code.clone()),
        latest_response: state
            .latest_response
            .as_ref()
            .map(|response| response.text.clone()),
        completed_at: state.completed_at,
    })
}

pub(in crate::api) fn agent_status(state: &crate::pane_state::PaneState) -> AgentStatus {
    match state.lifecycle {
        LifecycleState::Waiting { ref reason } if reason.is_usage_limit() => AgentStatus::Limited,
        LifecycleState::Waiting { .. } | LifecycleState::Error { .. } => AgentStatus::Blocked,
        LifecycleState::Running => AgentStatus::Working,
        LifecycleState::Idle if state.completed_seq > 0 => AgentStatus::Done,
        LifecycleState::Idle => AgentStatus::Idle,
    }
}

pub(in crate::api) fn agent_detail(
    pane: &PanePresentation,
    snapshot: &ResolvedSnapshot,
    server_identity: &str,
) -> Option<AgentDetail> {
    let resolved = pane.resolved.as_ref()?;
    let state = &resolved.canonical;
    Some(AgentDetail {
        summary: agent_summary(pane, snapshot, server_identity)?,
        state_id: state.state_id.as_str().to_string(),
        state_revision: state.revision,
        agent_epoch: state.agent_epoch,
        agent_session_id: state
            .agent_session_id
            .as_ref()
            .map(|session| session.as_str().to_string()),
        run_seq: state.run_seq,
        completed_seq: state.completed_seq,
        started_at: state.started_at,
        prompt: state.prompt.as_ref().map(|prompt| prompt.text.clone()),
        prompt_digest: state
            .prompt
            .as_ref()
            .and_then(|prompt| prompt.digest.clone()),
        task_progress_done: state.tasks.progress.done,
        task_progress_total: state.tasks.progress.total,
        subagent_count: state.subagents.len(),
        listening_ports: state.listening_ports.clone(),
    })
}

pub(in crate::api) fn matches_agent_filter(agent: &AgentSummary, filter: &AgentListFilter) -> bool {
    filter.session.as_ref().is_none_or(|session| {
        agent
            .sessions
            .iter()
            .any(|link| link.session_id == *session || link.session_name == *session)
    }) && filter
        .agent
        .as_ref()
        .is_none_or(|kind| agent.agent == *kind)
        && filter.status.is_none_or(|status| agent.status == status)
        && filter.cwd_prefix.as_ref().is_none_or(|prefix| {
            std::path::Path::new(&agent.current_path).starts_with(std::path::Path::new(prefix))
        })
        && (!filter.unread_only || agent.unread)
        && (!filter.needs_action_only || agent.needs_action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::contract::ApiError;
    use crate::api::test_support::*;
    use crate::daemon::session_badge::BadgeState;

    #[test]
    fn absent_agent_records_are_not_exposed_as_current_occupants() {
        let mut pane = test_agent_pane();
        let state = &mut pane.resolved.as_mut().unwrap().canonical;
        state.agent_present = false;
        state.lifecycle = LifecycleState::Idle;
        state.completed_seq = state.run_seq;
        state.completed_at = Some(2);
        let snapshot = test_snapshot(pane.clone());

        assert!(agent_summary(&pane, &snapshot, "server").is_none());
        assert!(pane_summary(&pane, "server").agent_ref.is_none());
        assert_eq!(
            resolve_agent(&snapshot, "%1", "server")
                .unwrap_err()
                .downcast_ref::<ApiError>()
                .unwrap()
                .code(),
            "agent_not_found"
        );
    }

    #[test]
    fn absent_usage_limited_agent_is_not_exposed_as_a_current_occupant() {
        let mut pane = test_agent_pane();
        pane.agent_process = None;
        let state = &mut pane.resolved.as_mut().unwrap().canonical;
        state.agent_process = None;
        state.agent_present = false;
        state.lifecycle = LifecycleState::Waiting {
            reason: WaitReason::usage_limit(),
        };
        pane.resolved.as_mut().unwrap().badge = BadgeState::Limited;
        let snapshot = test_snapshot(pane.clone());

        assert!(agent_summary(&pane, &snapshot, "server").is_none());
        assert_eq!(
            resolve_agent(&snapshot, "%1", "server")
                .unwrap_err()
                .downcast_ref::<ApiError>()
                .unwrap()
                .code(),
            "agent_not_found"
        );
    }

    #[test]
    fn agent_summary_never_exposes_a_stale_task_summary() {
        let mut pane = test_agent_pane();
        let state = &mut pane.resolved.as_mut().unwrap().canonical;
        state.task_context.observe_prompt("最初の古い依頼");
        state.task_context.summary = Some(crate::pane_state::TaskSummaryState {
            text: Some("古い要約".to_string()),
            context_fingerprint: state.task_context.context_fingerprint().unwrap(),
            generated_at: 1,
            outcome: crate::pane_state::TaskSummaryOutcome::Generated,
            failure_code: None,
        });
        state.task_context.observe_prompt("現在の新しい依頼");
        let snapshot = test_snapshot(pane.clone());

        let summary = agent_summary(&pane, &snapshot, "server").unwrap();
        assert_eq!(summary.task_summary, None);
        assert_eq!(summary.task_summary_status, None);
        assert_eq!(summary.task_summary_error, None);
    }
}
