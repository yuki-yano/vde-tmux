use crate::daemon::session_badge::BadgeState;

use super::model::{LifecycleState, PaneState, UnreadReason};

pub const SCREEN_EVIDENCE_TTL_SECONDS: i64 = 3;

/// Current presentation only. Canonical transitions, wait events, notices and
/// OS notifications must continue using resolve_badge / the canonical state.
pub fn resolve_presentation(
    state: &PaneState,
    tracker: &super::CaptureTrackerSnapshot,
    now: i64,
) -> BadgeState {
    let canonical = resolve_badge(state);
    if state.agent.as_str() != "codex"
        || tracker.hook_authoritative
        || !state.agent_present
        || !matches!(state.lifecycle, LifecycleState::Idle)
    {
        return canonical;
    }
    if let Some((evidence, observed_at)) = tracker.codex_screen
        && tracker.epoch.as_ref() == Some(&(state.state_id.clone(), state.agent_epoch))
        && (0..=SCREEN_EVIDENCE_TTL_SECONDS).contains(&now.saturating_sub(observed_at))
        && !evidence.transcript_viewer
    {
        if evidence.modal.is_some() {
            return BadgeState::Blocked;
        }
        if evidence.working {
            return BadgeState::Working;
        }
    }
    if canonical == BadgeState::Done {
        BadgeState::Done
    } else {
        BadgeState::Unknown
    }
}

pub fn resolve_badge(state: &PaneState) -> BadgeState {
    match state.lifecycle {
        LifecycleState::Waiting { ref reason } if reason.is_usage_limit() => BadgeState::Limited,
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
            agent_process: None,
            agent_epoch: 1,
            agent_present: true,
            scan_verified: false,
            synthetic_completion_armed: false,
            lifecycle,
            run_seq: run,
            current_run: None,
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
                }
            },
            started_at: (run > 0).then_some(1),
            completed_at: (completed > 0).then_some(2),
            prompt: None,
            latest_response: None,
            task_context: crate::pane_state::TaskContextState::default(),
            tasks: TaskState::default(),
            subagents: Vec::new(),
            worktree_activity: None,
            background_process: None,
            listening_ports: Vec::new(),
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
            resolve_badge(&state(LifecycleState::Running, 2, 1, false)),
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
        assert_eq!(
            resolve_badge(&state(
                LifecycleState::Waiting {
                    reason: crate::pane_state::model::WaitReason::usage_limit(),
                },
                1,
                0,
                false,
            )),
            BadgeState::Limited
        );
    }

    #[test]
    fn screen_evidence_changes_presentation_without_changing_canonical_state() {
        let state = state(LifecycleState::Idle, 0, 0, false);
        let original = state.clone();
        let mut tracker = super::super::CaptureTrackerSnapshot {
            epoch: Some((state.state_id.clone(), state.agent_epoch)),
            ..Default::default()
        };
        for (screen, badge) in [
            (
                "• Reviewing (3s)\n  ? 1 question · 3s\n› Ask Codex\n",
                BadgeState::Working,
            ),
            (
                "Would you like to run the following command?\n  › 1. Yes, proceed (y)\n",
                BadgeState::Blocked,
            ),
            ("Question 1/1 (1 unanswered)\n", BadgeState::Blocked),
            ("  ? 1 question · 3s\n› Ask Codex\n", BadgeState::Unknown),
            ("unrecognized screen", BadgeState::Unknown),
        ] {
            tracker.codex_screen = Some((crate::detect::codex::classify(screen), 100));
            assert_eq!(resolve_presentation(&state, &tracker, 100), badge);
            assert_eq!(
                resolve_presentation(&state, &tracker, 104),
                BadgeState::Unknown
            );
            tracker.hook_authoritative = true;
            assert_eq!(
                resolve_presentation(&state, &tracker, 100),
                BadgeState::Idle
            );
            tracker.hook_authoritative = false;
        }
        tracker.codex_screen = Some((crate::detect::codex::classify("• Working (1s)\n› "), 100));
        tracker.epoch = None;
        assert_eq!(
            resolve_presentation(&state, &tracker, 100),
            BadgeState::Unknown
        );
        assert_eq!(state, original);
        assert_eq!(resolve_badge(&state), BadgeState::Idle);
    }
}
