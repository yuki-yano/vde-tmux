use super::*;
use crate::daemon::protocol::v2::{PanePresentation, SessionLinkPresentation};
use crate::hook::RollupLevel;
use crate::pane_state::StoredStateDescriptor;
use crate::sidebar::tree::SidebarRowKind;

pub(super) fn structural_row(id: &str, kind: SidebarRowKind) -> SidebarRow {
    SidebarRow {
        id: id.to_string(),
        kind,
        depth: 0,
        label: id.to_string(),
        chat_count: 0,
        rollup: RollupLevel::Idle,
        badge_state: None,
        expanded: true,
        pane_id: None,
        git: None,
        active: false,
        meta: None,
    }
}

pub(super) fn pane(pane_pid: u32) -> PanePresentation {
    PanePresentation {
        pane_instance: PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid,
        },
        session_links: vec![SessionLinkPresentation {
            session_id: "$1".to_string(),
            session_name: "main".to_string(),
            window_index: 0,
            window_active: true,
            window_last: false,
        }],
        window_id: "@1".to_string(),
        window_name: "main".to_string(),
        current_path: "/tmp".to_string(),
        current_command: "zsh".to_string(),
        pane_width: 80,
        active: true,
        focused: true,
        agent_process: None,
        stored: None,
        resolved: None,
        retained_state: None,
    }
}

pub(super) fn snapshot(pane_pid: u32) -> ResolvedSnapshot {
    ResolvedSnapshot {
        snapshot_revision: 9,
        panes: vec![pane(pane_pid)],
        sidebar_model: crate::daemon::SidebarModel::default(),
        attention: Vec::new(),
        events: Vec::new(),
        diagnostics: Vec::new(),
    }
}

pub(super) fn resolved_pane(pane_id: &str, pane_pid: u32, session_id: &str) -> PanePresentation {
    use crate::pane_state::{
        AgentKind, LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneState, StateId, TaskState,
    };
    let pane_instance = PaneInstance {
        pane_id: pane_id.to_string(),
        pane_pid,
    };
    let canonical = PaneState {
        schema_version: PANE_STATE_SCHEMA_VERSION,
        state_id: StateId::parse(format!("{pane_pid:032x}")).unwrap(),
        revision: 1,
        pane_instance: pane_instance.clone(),
        agent: AgentKind::parse("codex").unwrap(),
        agent_session_id: None,
        agent_process: None,
        agent_epoch: 1,
        agent_present: true,
        scan_verified: true,
        synthetic_completion_armed: false,
        lifecycle: LifecycleState::Running,
        run_seq: 1,
        current_run: None,
        completed_seq: 0,
        unread: crate::pane_state::UnreadState::default(),
        started_at: Some(1),
        completed_at: None,
        prompt: None,
        latest_response: None,
        task_context: crate::pane_state::TaskContextState::default(),
        tasks: TaskState::default(),
        subagents: Vec::new(),
        worktree_activity: None,
        background_process: None,
        listening_ports: Vec::new(),
    };
    PanePresentation {
        pane_instance: pane_instance.clone(),
        session_links: vec![SessionLinkPresentation {
            session_id: session_id.to_string(),
            session_name: "main".to_string(),
            window_index: 0,
            window_active: true,
            window_last: false,
        }],
        window_id: "@1".to_string(),
        window_name: "main".to_string(),
        current_path: "/tmp/app".to_string(),
        current_command: "codex".to_string(),
        pane_width: 80,
        active: true,
        focused: true,
        agent_process: None,
        stored: Some(StoredStateDescriptor::Canonical {
            version: canonical.version(),
        }),
        resolved: Some(crate::pane_state::ResolvedPaneState {
            canonical,
            window_id: "@1".to_string(),
            pane_id: pane_id.to_string(),
            current_path: "/tmp/app".to_string(),
            badge: BadgeState::Working,
        }),
        retained_state: None,
    }
}
