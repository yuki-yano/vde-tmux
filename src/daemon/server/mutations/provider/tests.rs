use super::super::super::*;
use super::*;

#[test]
fn codex_goal_continuations_project_working_waiting_and_completion() {
    use crate::agent_state::{ExecutionPhase, SemanticOutcome};
    use crate::pane_state::{AgentProcessIdentity, LifecycleState};

    for first_hook in ["PreToolUse", "PermissionRequest"] {
        let root = test_root(&format!("codex-goal-{first_hook}"));
        let hash = "codex-goal-continuation";
        let env = BTreeMap::from([
            (
                "XDG_STATE_HOME".to_string(),
                root.to_string_lossy().into_owned(),
            ),
            // No real tmux server participates in this coordinator test.
            (
                "VDE_TMUX_SOCKET_NAME".to_string(),
                format!("vde-goal-test-{}-{first_hook}", std::process::id()),
            ),
        ]);
        let coordinator =
            ProductionV2Coordinator::new(test_incarnation(&root, hash), env, None).unwrap();
        install_test_state(
            &coordinator,
            &root,
            crate::daemon::view_hooks::CurrentClientViews::default(),
        );
        *coordinator.agent_runtime.lock().unwrap() = Some(
            crate::agent_state::runtime::AgentRuntime::open(
                root.join("agent-state"),
                hash.to_string(),
            )
            .unwrap(),
        );
        let pane = PaneInstance {
            pane_id: "%539".to_string(),
            pane_pid: 53_900,
        };
        let daemon_id = coordinator
            .router
            .lock()
            .unwrap()
            .daemon_instance_id()
            .clone();
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub_agent_process(
            pane.pane_pid,
            "codex",
            Some(AgentProcessIdentity {
                pid: 53_962,
                start_token: "goal-process-start".to_string(),
            }),
        );
        let send = |event, turn, at| {
            let payload = serde_json::json!({
                "session_id": "session-goal", "turn_id": turn,
                "source": "startup", "prompt": "original user prompt",
                "last_assistant_message": "turn completed",
            })
            .to_string();
            let (envelope, observation) =
                codex_provider_test_event(daemon_id.clone(), pane.clone(), event, &payload, at);
            let response = apply_external_provider_event_with_runner(
                &coordinator,
                at as u64,
                envelope,
                observation,
                &runner,
            );
            assert!(
                matches!(response, ServerMessage::PaneEventResult { .. }),
                "{response:?}"
            );
        };
        let record = || {
            coordinator
                .state
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .leased
                .runtime
                .record(&pane)
                .unwrap()
                .clone()
        };

        send("SessionStart", "", 1);
        send("UserPromptSubmit", "manual-turn", 2);
        send("Stop", "manual-turn", 3);
        assert_eq!(record().completed_seq, 1);
        send(first_hook, "goal-turn", 4);
        let started = record();
        assert_eq!(started.run_seq, 2);
        assert_eq!(started.completed_seq, 1);
        assert!(started.prompt.is_none());
        if first_hook == "PermissionRequest" {
            assert!(matches!(started.lifecycle, LifecycleState::Waiting { .. }));
        } else {
            assert_eq!(started.lifecycle, LifecycleState::Running);
        }
        let run_id = started.current_run.unwrap().run_id;
        send("PostToolUse", "goal-turn", 5);
        assert_eq!(record().run_seq, 2);
        assert_eq!(record().lifecycle, LifecycleState::Running);
        assert_eq!(record().current_run.unwrap().run_id, run_id);
        send("PermissionRequest", "goal-turn", 6);
        assert!(matches!(record().lifecycle, LifecycleState::Waiting { .. }));
        send("PreToolUse", "goal-turn", 7);
        assert_eq!(record().lifecycle, LifecycleState::Running);
        send("Stop", "goal-turn", 8);
        assert_eq!(record().lifecycle, LifecycleState::Idle);
        assert_eq!(record().completed_seq, 2);
        send("PostToolUse", "goal-turn", 9);
        assert_eq!(record().lifecycle, LifecycleState::Idle);
        assert_eq!(record().run_seq, 2);
        assert_eq!(record().completed_seq, 2);
        let runtime_guard = coordinator.agent_runtime.lock().unwrap();
        let runtime = runtime_guard.as_ref().unwrap();
        let completed = runtime
            .get_run(&runtime.run_ref(crate::agent_state::StableRunId::parse(run_id).unwrap()))
            .unwrap();
        assert_eq!(completed.execution_phase, ExecutionPhase::Ended);
        assert_eq!(completed.semantic_outcome, SemanticOutcome::Completed);
        assert!(completed.artifact.is_some());
        drop(runtime_guard);
        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn provider_hook_kind_must_match_the_pane_transition_before_run_mutation() {
    let observation = crate::hook::provider::observation_from_json(
        "codex",
        "UserPromptSubmit",
        r#"{"session_id":"session","turn_id":"turn","prompt":"hello"}"#,
        EventId::parse(V2_EVENT_ID).unwrap(),
        10,
    )
    .unwrap()
    .unwrap();
    let begin = PaneEvent::BeginRun {
        started_at: 10,
        prompt: Some(crate::pane_state::PromptState {
            text: "hello".to_string(),
            source: "user".to_string(),
            digest: observation.prompt_digest.clone(),
        }),
    };
    assert!(provider_event_matches_pane_event(&observation, &begin));
    assert!(!provider_event_matches_pane_event(
        &observation,
        &PaneEvent::CompleteRun { completed_at: 10 }
    ));

    let stop = crate::hook::provider::observation_from_json(
        "codex",
        "Stop",
        r#"{"session_id":"session","turn_id":"turn","last_assistant_message":"done"}"#,
        EventId::parse(V2_EVENT_ID).unwrap(),
        11,
    )
    .unwrap()
    .unwrap();
    assert!(provider_event_matches_pane_event(
        &stop,
        &PaneEvent::ResponseAndCompleteRun {
            completed_at: 11,
            response: crate::pane_state::ResponseState {
                text: "done".to_string(),
                observed_at: 11,
            },
        }
    ));
    assert!(!provider_event_matches_pane_event(
        &stop,
        &PaneEvent::CompleteRun { completed_at: 11 }
    ));
}

#[test]
fn claude_provider_observation_is_rejected_before_mutation() {
    let root = test_root("claude-provider-rejection");
    let coordinator = test_coordinator(&root, "c".repeat(64));
    let observation = crate::hook::provider::observation_from_json(
        "claude",
        "Stop",
        r#"{"session_id":"session","prompt_id":"prompt","last_assistant_message":"private"}"#,
        v2_event_id(),
        1,
    )
    .unwrap()
    .unwrap();
    let envelope = PaneEventEnvelope {
        daemon_instance_id: v2_daemon_id(),
        event_id: v2_event_id(),
        pane_instance: PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        },
        agent: Some(crate::pane_state::AgentKind::parse("claude").unwrap()),
        agent_session_id: Some(crate::pane_state::AgentSessionId::parse("session").unwrap()),
        event: PaneEvent::ResponseAndCompleteRun {
            completed_at: 1,
            response: crate::pane_state::ResponseState {
                text: "private".to_string(),
                observed_at: 1,
            },
        },
    };

    assert!(matches!(
        apply_external_provider_event(&coordinator, 1, envelope, observation),
        ServerMessage::Error {
            code: ErrorCode::UnsupportedProvider,
            ..
        }
    ));
    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn missing_provider_process_refresh_remains_fail_closed() {
    let root = test_root("codex-process-refresh-fail-closed");
    let hash = "codex-process-refresh-fail-closed";
    let env = BTreeMap::from([(
        "XDG_STATE_HOME".to_string(),
        root.to_string_lossy().into_owned(),
    )]);
    let coordinator =
        ProductionV2Coordinator::new(test_incarnation(&root, hash), env, None).unwrap();
    install_test_state(
        &coordinator,
        &root,
        crate::daemon::view_hooks::CurrentClientViews::default(),
    );
    *coordinator.agent_runtime.lock().unwrap() = Some(
        crate::agent_state::runtime::AgentRuntime::open(root.join("agent-state"), hash.to_string())
            .unwrap(),
    );
    let pane = PaneInstance {
        pane_id: "%538".to_string(),
        pane_pid: 53_800,
    };
    let daemon_instance_id = coordinator
        .router
        .lock()
        .unwrap()
        .daemon_instance_id()
        .clone();
    let make_event = |event: &str, payload: &str, observed_at: i64| {
        codex_provider_test_event(
            daemon_instance_id.clone(),
            pane.clone(),
            event,
            payload,
            observed_at,
        )
    };
    let runner = crate::tmux::mock::MockTmuxRunner::new();
    let (session_envelope, session_observation) = make_event(
        "SessionStart",
        r#"{"session_id":"session-538","source":"startup"}"#,
        1,
    );
    assert!(matches!(
        apply_external_provider_event_with_runner(
            &coordinator,
            1,
            session_envelope,
            session_observation,
            &runner,
        ),
        ServerMessage::PaneEventResult { .. }
    ));
    runner.stub_agent_process(pane.pane_pid, "codex", None);
    let (prompt_envelope, prompt_observation) = make_event(
        "UserPromptSubmit",
        r#"{"session_id":"session-538","turn_id":"turn-1","prompt":"hello"}"#,
        2,
    );
    assert!(matches!(
        apply_external_provider_event_with_runner(
            &coordinator,
            2,
            prompt_envelope,
            prompt_observation,
            &runner,
        ),
        ServerMessage::Error {
            code: ErrorCode::StaleAgentEvent,
            ..
        }
    ));
    let state = coordinator.state.lock().unwrap();
    let record = state
        .as_ref()
        .unwrap()
        .leased
        .runtime
        .record(&pane)
        .unwrap();
    assert!(record.agent_process.is_none());
    assert_eq!(record.run_seq, 0);
    assert!(record.current_run.is_none());

    drop(state);
    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn provider_projection_keeps_ui_previews_but_redacts_guarded_prompts() {
    let begin = PaneEvent::BeginRun {
        started_at: 1,
        prompt: Some(crate::pane_state::PromptState {
            text: "human prompt preview".to_string(),
            source: "user".to_string(),
            digest: Some("a".repeat(64)),
        }),
    };
    let mut public_begin = begin.clone();
    redact_private_provider_prompt(&mut public_begin, false);
    assert_eq!(public_begin, begin);

    let mut guarded_begin = begin;
    redact_private_provider_prompt(&mut guarded_begin, true);
    assert!(matches!(
        guarded_begin,
        PaneEvent::BeginRun { prompt: None, .. }
    ));

    let mut stop = PaneEvent::ResponseAndCompleteRun {
        completed_at: 2,
        response: crate::pane_state::ResponseState {
            text: "response preview".to_string(),
            observed_at: 2,
        },
    };
    let expected_stop = stop.clone();
    redact_private_provider_prompt(&mut stop, true);
    assert_eq!(stop, expected_stop);

    let mut progress = PaneEvent::ProgressUpdated {
        observed_at: 3,
        operations: vec![
            crate::pane_state::ProgressOperation::SetPrompt(crate::pane_state::PromptState {
                text: "private progress prompt".to_string(),
                source: "generic_hook".to_string(),
                digest: None,
            }),
            crate::pane_state::ProgressOperation::TaskCreated,
        ],
    };
    let PaneEvent::ProgressUpdated { operations, .. } = &progress else {
        unreachable!();
    };
    let mut goal = PaneEvent::ActivityAndProgressObserved {
        observed_at: 3,
        operations: operations.clone(),
    };
    redact_private_provider_prompt(&mut goal, true);
    assert!(
        matches!(goal, PaneEvent::ActivityAndProgressObserved { operations, .. }
        if operations == vec![crate::pane_state::ProgressOperation::TaskCreated])
    );
    redact_private_provider_prompt(&mut progress, true);
    assert!(matches!(
        progress,
        PaneEvent::ProgressUpdated { operations, .. }
            if operations == vec![crate::pane_state::ProgressOperation::TaskCreated]
    ));

    let mut report = PaneEvent::ExplicitStateReported {
        report: crate::pane_state::ExplicitStateReport {
            observed_at: 4,
            lifecycle: None,
            started_at: None,
            completed_at: None,
            prompt: Some(crate::pane_state::FieldUpdate::Set(
                crate::pane_state::PromptState {
                    text: "private report prompt".to_string(),
                    source: "generic_hook".to_string(),
                    digest: None,
                },
            )),
            tasks: None,
            subagents: None,
            attention: false,
        },
    };
    redact_private_provider_prompt(&mut report, true);
    assert!(matches!(
        report,
        PaneEvent::ExplicitStateReported { report } if report.prompt.is_none()
    ));
}
