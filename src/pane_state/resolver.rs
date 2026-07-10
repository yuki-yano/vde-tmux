use crate::daemon::session_badge::BadgeState;

use super::model::{LifecycleState, PaneState};

pub fn resolve_badge(state: &PaneState) -> BadgeState {
    match state.lifecycle {
        LifecycleState::Waiting { .. } | LifecycleState::Error { .. } => BadgeState::Blocked,
        LifecycleState::Running => BadgeState::Working,
        LifecycleState::Idle if state.completed_seq > state.acknowledged_seq => BadgeState::Done,
        LifecycleState::Idle => BadgeState::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane_state::model::{
        AgentKind, PANE_STATE_SCHEMA_VERSION, PaneInstance, StateId, TaskState,
    };

    fn state(lifecycle: LifecycleState, run: u64, completed: u64, acknowledged: u64) -> PaneState {
        PaneState {
            schema_version: PANE_STATE_SCHEMA_VERSION,
            state_id: StateId::parse("00112233445566778899aabbccddeeff").unwrap(),
            revision: 1,
            pane_instance: PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 10,
            },
            agent: AgentKind::parse("codex").unwrap(),
            agent_session_id: None,
            agent_epoch: 1,
            agent_present: true,
            scan_verified: false,
            synthetic_completion_armed: false,
            lifecycle,
            run_seq: run,
            completed_seq: completed,
            acknowledged_seq: acknowledged,
            started_at: (run > 0).then_some(1),
            completed_at: (completed > 0).then_some(2),
            prompt: None,
            tasks: TaskState::default(),
            subagents: Vec::new(),
            worktree_activity: None,
        }
    }

    #[test]
    fn badge_is_derived_only_from_canonical_state() {
        assert_eq!(
            resolve_badge(&state(LifecycleState::Idle, 0, 0, 0)),
            BadgeState::Idle
        );
        assert_eq!(
            resolve_badge(&state(LifecycleState::Running, 1, 0, 0)),
            BadgeState::Working
        );
        assert_eq!(
            resolve_badge(&state(LifecycleState::Idle, 1, 1, 0)),
            BadgeState::Done
        );
        assert_eq!(
            resolve_badge(&state(LifecycleState::Idle, 1, 1, 1)),
            BadgeState::Idle
        );
        assert_eq!(
            resolve_badge(&state(
                LifecycleState::Waiting {
                    reason: crate::pane_state::model::WaitReason::PermissionPrompt,
                },
                1,
                0,
                0,
            )),
            BadgeState::Blocked
        );
    }
}
