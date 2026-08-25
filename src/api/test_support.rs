use crate::daemon::protocol::v2::{PanePresentation, ResolvedSnapshot};
use crate::daemon::session_badge::BadgeState;
use crate::pane_state::{LifecycleState, PaneInstance};

pub(super) fn test_agent_pane() -> PanePresentation {
    let pane_instance = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 101,
    };
    PanePresentation {
        pane_instance: pane_instance.clone(),
        session_links: Vec::new(),
        window_id: "@1".to_string(),
        window_name: "main".to_string(),
        current_path: "/tmp".to_string(),
        current_command: "codex".to_string(),
        pane_width: 80,
        active: true,
        focused: true,
        agent_process: Some(crate::pane_state::AgentProcessIdentity {
            pid: 9001,
            start_token: "test-process-start".to_string(),
        }),
        stored: None,
        resolved: Some(crate::pane_state::ResolvedPaneState {
            canonical: crate::pane_state::PaneState {
                schema_version: crate::pane_state::PANE_STATE_SCHEMA_VERSION,
                state_id: crate::pane_state::StateId::parse("00112233445566778899aabbccddeeff")
                    .unwrap(),
                revision: 1,
                pane_instance,
                agent: crate::pane_state::AgentKind::parse("codex").unwrap(),
                agent_session_id: Some(
                    crate::pane_state::AgentSessionId::parse("session-1").unwrap(),
                ),
                agent_process: Some(crate::pane_state::AgentProcessIdentity {
                    pid: 9001,
                    start_token: "test-process-start".to_string(),
                }),
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
                tasks: crate::pane_state::TaskState::default(),
                subagents: Vec::new(),
                worktree_activity: None,
                background_process: None,
                listening_ports: Vec::new(),
            },
            window_id: "@1".to_string(),
            pane_id: "%1".to_string(),
            current_path: "/tmp".to_string(),
            badge: BadgeState::Working,
        }),
        retained_state: None,
    }
}

pub(super) fn test_snapshot(pane: PanePresentation) -> ResolvedSnapshot {
    ResolvedSnapshot {
        snapshot_revision: 1,
        panes: vec![pane],
        sidebar_model: crate::daemon::SidebarModel::default(),
        attention: Vec::new(),
        events: Vec::new(),
        diagnostics: Vec::new(),
    }
}
