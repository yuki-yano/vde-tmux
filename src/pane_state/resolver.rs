use crate::daemon::session_badge::BadgeState;

use super::model::{
    LifecycleState, PaneState, PresentationExplanation, PresentationReason, UnreadReason,
};

pub const SCREEN_EVIDENCE_TTL_SECONDS: i64 = 3;

/// Current presentation only. Canonical transitions, wait events, notices and
/// OS notifications must continue using resolve_badge / the canonical state.
pub fn resolve_presentation(
    state: &PaneState,
    tracker: &super::CaptureTrackerSnapshot,
    now: i64,
) -> BadgeState {
    resolve_presentation_with_explanation(state, tracker, now).0
}

pub fn resolve_presentation_with_explanation(
    state: &PaneState,
    tracker: &super::CaptureTrackerSnapshot,
    now: i64,
) -> (BadgeState, PresentationExplanation) {
    use PresentationReason as Reason;
    let canonical = resolve_badge(state);
    let canonical_reason = if state.agent.as_str() != "codex" {
        Some(Reason::Canonical)
    } else if tracker.hook_authoritative {
        Some(Reason::HookAuthoritative)
    } else if !state.agent_present {
        Some(Reason::AgentAbsent)
    } else if !matches!(state.lifecycle, LifecycleState::Idle) {
        Some(Reason::CanonicalActive)
    } else {
        None
    };
    if let Some(reason) = canonical_reason {
        return (
            canonical,
            PresentationExplanation {
                reason,
                ..Default::default()
            },
        );
    }
    let same_epoch = tracker.epoch.as_ref() == Some(&(state.state_id.clone(), state.agent_epoch));
    let observed_at = same_epoch
        .then(|| {
            tracker
                .codex_screen
                .map(|(_, at)| at)
                .or(tracker.codex_screen_expired_at)
        })
        .flatten();
    let explanation = |reason| PresentationExplanation {
        reason,
        observed_at,
        ttl_seconds: Some(SCREEN_EVIDENCE_TTL_SECONDS),
    };
    let unavailable_reason = if !same_epoch {
        Reason::EpochMismatch
    } else if let Some((evidence, at)) = tracker.codex_screen {
        if now < at {
            Reason::ObservationTimeInvalid
        } else if now.saturating_sub(at) > SCREEN_EVIDENCE_TTL_SECONDS {
            Reason::EvidenceExpired
        } else if evidence.transcript_viewer {
            Reason::TranscriptViewer
        } else if let Some(modal) = evidence.modal {
            let reason = match modal {
                crate::detect::codex::Modal::Approval => Reason::ScreenApproval,
                crate::detect::codex::Modal::SynchronousQuestion => Reason::ScreenQuestion,
                crate::detect::codex::Modal::TrustDirectory => Reason::ScreenTrust,
                crate::detect::codex::Modal::StartupUpdate => Reason::ScreenUpdate,
            };
            return (BadgeState::Blocked, explanation(reason));
        } else if evidence.working {
            return (BadgeState::Working, explanation(Reason::ScreenWorking));
        } else {
            Reason::UnknownScreen
        }
    } else if let Some(at) = tracker.codex_screen_expired_at {
        if now < at {
            Reason::ObservationTimeInvalid
        } else {
            Reason::EvidenceExpired
        }
    } else {
        Reason::EvidenceUnavailable
    };
    if canonical == BadgeState::Done {
        (
            BadgeState::Done,
            PresentationExplanation {
                reason: Reason::UnreadCompletion,
                ..Default::default()
            },
        )
    } else {
        (BadgeState::Unknown, explanation(unavailable_reason))
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

    #[test]
    fn explanation_matches_authority_freshness_and_modal_precedence_without_bodies() {
        use PresentationReason as Reason;
        let idle = state(LifecycleState::Idle, 0, 0, false);
        let mut tracker = super::super::CaptureTrackerSnapshot {
            epoch: Some((idle.state_id.clone(), idle.agent_epoch)),
            ..Default::default()
        };
        assert_eq!(
            resolve_presentation_with_explanation(&idle, &tracker, 100)
                .1
                .reason,
            Reason::EvidenceUnavailable
        );
        for (screen, badge, reason) in [
            (
                "• Private activity title (3s)\n› Private input\n",
                BadgeState::Working,
                Reason::ScreenWorking,
            ),
            (
                "Allow command to run?\n  y) yes\n",
                BadgeState::Blocked,
                Reason::ScreenApproval,
            ),
            (
                "Question 1/1 (1 unanswered)\nPrivate question body\n",
                BadgeState::Blocked,
                Reason::ScreenQuestion,
            ),
            (
                "> You are in /private\nDo you trust the contents of this directory?\n› 1. Yes, continue\n",
                BadgeState::Blocked,
                Reason::ScreenTrust,
            ),
            (
                "Update available!\nUpdate now\nSkip until next version\nPress enter to continue\n",
                BadgeState::Blocked,
                Reason::ScreenUpdate,
            ),
            (
                "private unknown UI",
                BadgeState::Unknown,
                Reason::UnknownScreen,
            ),
            (
                "↑/↓ to scroll",
                BadgeState::Unknown,
                Reason::TranscriptViewer,
            ),
        ] {
            tracker.codex_screen = Some((crate::detect::codex::classify(screen), 100));
            let (actual, explanation) = resolve_presentation_with_explanation(&idle, &tracker, 103);
            assert_eq!((actual, explanation.reason), (badge, reason));
            assert_eq!(explanation.observed_at, Some(100));
            assert_eq!(explanation.ttl_seconds, Some(3));
            let json = serde_json::to_string(&explanation).unwrap();
            assert!(!json.to_ascii_lowercase().contains("private"));
            assert_eq!(
                resolve_presentation_with_explanation(&idle, &tracker, 104)
                    .1
                    .reason,
                Reason::EvidenceExpired
            );
            assert_eq!(
                resolve_presentation_with_explanation(&idle, &tracker, 99)
                    .1
                    .reason,
                Reason::ObservationTimeInvalid
            );
        }
        tracker.codex_screen = None;
        tracker.codex_screen_expired_at = Some(100);
        assert_eq!(
            resolve_presentation_with_explanation(&idle, &tracker, 104)
                .1
                .reason,
            Reason::EvidenceExpired
        );
        tracker.epoch = None;
        let explanation = resolve_presentation_with_explanation(&idle, &tracker, 104).1;
        assert_eq!(explanation.reason, Reason::EpochMismatch);
        assert_eq!(explanation.observed_at, None);
        let done = state(LifecycleState::Idle, 1, 1, false);
        assert_eq!(
            resolve_presentation_with_explanation(&done, &tracker, 104)
                .1
                .reason,
            Reason::UnreadCompletion
        );
        let running = state(LifecycleState::Running, 1, 0, false);
        assert_eq!(
            resolve_presentation_with_explanation(&running, &tracker, 104)
                .1
                .reason,
            Reason::CanonicalActive
        );
        let mut absent = idle.clone();
        absent.agent_present = false;
        assert_eq!(
            resolve_presentation_with_explanation(&absent, &tracker, 104)
                .1
                .reason,
            Reason::AgentAbsent
        );
        tracker.hook_authoritative = true;
        assert_eq!(
            resolve_presentation_with_explanation(&idle, &tracker, 104)
                .1
                .reason,
            Reason::HookAuthoritative
        );
        let mut other = idle;
        other.agent = AgentKind::parse("claude").unwrap();
        assert_eq!(
            resolve_presentation_with_explanation(&other, &tracker, 104)
                .1
                .reason,
            Reason::Canonical
        );
    }

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
