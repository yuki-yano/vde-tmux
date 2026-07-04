//! daemon の snapshot 集約と statusline badge。

pub mod protocol;
pub mod server;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::hook::{AgentStatus, RollupLevel, pane_rollup_level};
use crate::options::snapshot::{PaneSnapshot, read_all_panes};
use crate::tmux::TmuxRunner;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPaneSummary {
    pub pane_id: String,
    pub agent: String,
    pub status: Option<AgentStatus>,
    pub wait_reason: Option<String>,
    pub rollup: RollupLevel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonSnapshot {
    pub agent_count: usize,
    pub rollup: RollupLevel,
    pub panes: Vec<AgentPaneSummary>,
}

pub fn build_snapshot(panes: &[PaneSnapshot]) -> DaemonSnapshot {
    let panes = panes
        .iter()
        .filter(|pane| !pane.agent.is_empty())
        .map(|pane| {
            let status = parse_agent_status(&pane.status);
            let wait_reason = (!pane.wait_reason.is_empty()).then(|| pane.wait_reason.clone());
            let rollup = pane_rollup_level(status, wait_reason.as_deref());
            AgentPaneSummary {
                pane_id: pane.pane_id.clone(),
                agent: pane.agent.clone(),
                status,
                wait_reason,
                rollup,
            }
        })
        .collect::<Vec<_>>();
    let rollup = panes
        .iter()
        .map(|pane| pane.rollup)
        .min()
        .unwrap_or(RollupLevel::Idle);
    DaemonSnapshot {
        agent_count: panes.len(),
        rollup,
        panes,
    }
}

pub fn render_agent_badge(snapshot: &DaemonSnapshot) -> String {
    if snapshot.agent_count == 0 {
        return String::new();
    }
    format!("{}:{}", rollup_label(snapshot.rollup), snapshot.agent_count)
}

pub fn statusline_agent_badge_fallback(runner: &dyn TmuxRunner) -> Result<String> {
    let panes = read_all_panes(runner)?;
    Ok(render_agent_badge(&build_snapshot(&panes)))
}

fn parse_agent_status(raw: &str) -> Option<AgentStatus> {
    match raw {
        "running" => Some(AgentStatus::Running),
        "waiting" => Some(AgentStatus::Waiting),
        "idle" => Some(AgentStatus::Idle),
        "error" => Some(AgentStatus::Error),
        _ => None,
    }
}

fn rollup_label(level: RollupLevel) -> &'static str {
    match level {
        RollupLevel::Error => "error",
        RollupLevel::Running => "running",
        RollupLevel::Permission => "permission",
        RollupLevel::Background => "background",
        RollupLevel::Waiting => "waiting",
        RollupLevel::Idle => "idle",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::snapshot::PaneSnapshot;

    fn pane(agent: &str, status: &str, wait_reason: &str) -> PaneSnapshot {
        PaneSnapshot {
            pane_id: "%1".to_string(),
            agent: agent.to_string(),
            status: status.to_string(),
            wait_reason: wait_reason.to_string(),
            ..PaneSnapshot::default()
        }
    }

    #[test]
    fn build_snapshot_ignores_non_agent_panes() {
        let snapshot = build_snapshot(&[
            pane("codex", "running", ""),
            pane("", "", ""),
            pane("claude", "idle", ""),
        ]);
        assert_eq!(snapshot.agent_count, 2);
        assert_eq!(snapshot.rollup, crate::hook::RollupLevel::Running);
    }

    #[test]
    fn permission_waiting_wins_over_idle() {
        let snapshot = build_snapshot(&[
            pane("claude", "idle", ""),
            pane("codex", "waiting", "permission_prompt"),
        ]);
        assert_eq!(snapshot.rollup, crate::hook::RollupLevel::Permission);
    }

    #[test]
    fn render_agent_badge_is_empty_without_agents() {
        let snapshot = build_snapshot(&[pane("", "", "")]);
        assert_eq!(render_agent_badge(&snapshot), "");
    }

    #[test]
    fn render_agent_badge_includes_rollup_and_count() {
        let snapshot = build_snapshot(&[pane("codex", "running", "")]);
        assert_eq!(render_agent_badge(&snapshot), "running:1");
    }
}
