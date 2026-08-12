use crate::daemon::session_badge::BadgeState;

use super::model::{LifecycleState, PaneState, UnreadReason};

pub fn resolve_badge(state: &PaneState) -> BadgeState {
    match state.lifecycle {
        LifecycleState::Waiting { .. } | LifecycleState::Error { .. } => BadgeState::Blocked,
        LifecycleState::Running => BadgeState::Working,
        LifecycleState::Idle
            if state
                .unread
                .latest_unread()
                .is_some_and(|latest| latest.reason == UnreadReason::Completed) =>
        {
            BadgeState::Done
        }
        LifecycleState::Idle => BadgeState::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane_state::model::{
        AgentKind, PANE_STATE_SCHEMA_VERSION, PaneInstance, StateId, TaskState, UnreadOccurrence,
        UnreadState,
    };

    fn state(lifecycle: LifecycleState, run: u64, completed: u64, read: bool) -> PaneState {
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
            unread: if completed == 0 {
                UnreadState::default()
            } else {
                UnreadState {
                    occurrence_seq: 1,
                    read_seq: u64::from(read),
                    latest: Some(UnreadOccurrence {
                        seq: 1,
                        order: 1,
                        reason: UnreadReason::Completed,
                        occurred_at: 2,
                    }),
                    pinned: false,
                }
            },
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
            resolve_badge(&state(LifecycleState::Idle, 0, 0, false)),
            BadgeState::Idle
        );
        assert_eq!(
            resolve_badge(&state(LifecycleState::Running, 1, 0, false)),
            BadgeState::Working
        );
        assert_eq!(
            resolve_badge(&state(LifecycleState::Idle, 1, 1, false)),
            BadgeState::Done
        );
        assert_eq!(
            resolve_badge(&state(LifecycleState::Idle, 1, 1, true)),
            BadgeState::Idle
        );
        assert_eq!(
            resolve_badge(&state(
                LifecycleState::Waiting {
                    reason: crate::pane_state::model::WaitReason::PermissionPrompt,
                },
                1,
                0,
                false,
            )),
            BadgeState::Blocked
        );
    }
}
