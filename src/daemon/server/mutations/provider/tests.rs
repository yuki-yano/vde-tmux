use super::super::super::*;
use super::*;

#[test]
fn dispatched_prompt_generates_summary_without_publishing_or_persisting_its_input() {
    use crate::agent_state::{AgentBinding, OperationId, Sha256Digest};
    use crate::daemon::task_summary::{TaskSummaryCompletion, TaskSummaryJob};
    use crate::pane_state::{AgentProcessIdentity, TaskSummaryOutcome, TaskSummaryState};

    let root = test_root("private-task-summary");
    let hash = "private-task-summary";
    let env = BTreeMap::from([
        ("XDG_STATE_HOME".to_string(), root.display().to_string()),
        (
            "VDE_TMUX_SOCKET_NAME".to_string(),
            format!("vde-private-summary-{}", std::process::id()),
        ),
    ]);
    let mut coordinator =
        ProductionV2Coordinator::new(test_incarnation(&root, hash), env, None).unwrap();
    install_test_state(&coordinator, &root, Default::default());
    *coordinator.agent_runtime.lock().unwrap() = Some(
        crate::agent_state::runtime::AgentRuntime::open(root.join("agent-state"), hash.to_string())
            .unwrap(),
    );
    let (sender, receiver) = mpsc::sync_channel(4);
    coordinator.task_summary_tx = Some(sender);
    let pane = PaneInstance {
        pane_id: "%540".to_string(),
        pane_pid: 54_000,
    };
    coordinator
        .state
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .topology
        .panes = vec![read_peek_test_topology_pane(pane.clone(), false)];
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
            pid: 54_062,
            start_token: "private-summary-process".to_string(),
        }),
    );
    let event =
        |kind, prompt: &str, at| {
            codex_provider_test_event(
        daemon_id.clone(), pane.clone(), kind,
        &serde_json::json!({
            "session_id": "private-summary-session", "turn_id": "private-summary-turn",
            "source": "startup", "prompt": prompt, "last_assistant_message": "response preview",
        }).to_string(), at,
    )
        };
    let apply = |envelope, observation, at| {
        let response = apply_external_provider_event_with_runner(
            &coordinator,
            at,
            envelope,
            observation,
            &runner,
        );
        assert!(
            matches!(response, ServerMessage::PaneEventResult { .. }),
            "{response:?}"
        );
    };
    let (session, observation) = event("SessionStart", "", 1);
    apply(session, observation, 1);

    let private_prompt = "private dispatch body: investigate the missing sidebar summary";
    let (begin, observation) = event("UserPromptSubmit", private_prompt, 2);
    let record =
        refresh_provider_process_identity(&coordinator, 2, &begin, &observation, &runner).unwrap();
    let operation_id = OperationId::parse("private_summary_operation_0001").unwrap();
    let binding = AgentBinding {
        server_identity: coordinator.incarnation.identity.clone(),
        pane_instance: pane.clone(),
        pane_state_id: record.state_id.clone(),
        agent_epoch: record.agent_epoch,
        agent_kind: record.agent.clone(),
        provider_session_id: record.agent_session_id.clone().unwrap(),
        process: record.agent_process.clone().unwrap(),
    };
    {
        let mut guard = coordinator.agent_runtime.lock().unwrap();
        let runtime = guard.as_mut().unwrap();
        runtime
            .prepare_operation(
                operation_id.clone(),
                "vta1:private-summary-target".to_string(),
                private_prompt.as_bytes(),
                Sha256Digest::parse(crate::pane_state::PromptState::digest_decoded_prompt(
                    private_prompt,
                ))
                .unwrap(),
                "paste_enter".to_string(),
                binding,
                record.version(),
                record.current_run.clone(),
                record.run_seq + 1,
                epoch_seconds(),
            )
            .unwrap();
        runtime
            .mark_dispatch_started(&operation_id, epoch_seconds())
            .unwrap();
    }
    apply(begin.clone(), observation.clone(), 3);
    let first = receiver.try_recv().unwrap();
    assert_eq!(first.task_context.recent_prompts, [private_prompt]);
    apply(begin, observation, 4);
    assert!(
        receiver.try_recv().is_err(),
        "duplicate hook must not enqueue a second job"
    );
    let snapshot = || {
        coordinator
            .state
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .resolved_snapshot()
    };
    let current_record = || {
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
    assert!(
        snapshot()
            .sidebar_model
            .task_summary_loading
            .contains(&pane)
    );
    assert!(current_record().prompt.is_none());
    assert!(current_record().task_context.recent_prompts.is_empty());
    assert!(
        !serde_json::to_string(&snapshot())
            .unwrap()
            .contains(private_prompt)
    );
    let snapshot_path = crate::pane_state::snapshot::snapshot_path(&coordinator.env, hash);
    assert!(
        !std::fs::read_to_string(&snapshot_path)
            .unwrap()
            .contains(private_prompt)
    );

    let complete = |job: TaskSummaryJob, result: Result<&str, &str>| {
        let fingerprint = job.task_context.context_fingerprint().unwrap();
        let completion = TaskSummaryCompletion {
            pane_instance: job.pane_instance,
            state_id: job.state_id,
            agent_epoch: job.agent_epoch,
            context_fingerprint: fingerprint.clone(),
            result: result
                .map(|text| TaskSummaryState {
                    text: Some(text.to_string()),
                    context_fingerprint: fingerprint,
                    generated_at: epoch_seconds(),
                    outcome: TaskSummaryOutcome::Generated,
                    failure_code: None,
                })
                .map_err(str::to_string),
        };
        let response = apply_production_mutation(
            &coordinator,
            V2SequencedMutation {
                accepted_seq: 10,
                mutation: V2AcceptedMutation::Internal(V2InternalMutation::TaskSummaryCompleted(
                    completion,
                )),
            },
        );
        assert!(
            matches!(response, ServerMessage::PaneEventResult { .. }),
            "{response:?}"
        );
    };
    // A same-turn human follow-up must keep private evidence out of public
    // context while superseding the queued summary.
    let (follow_up, observation) = event("UserPromptSubmit", "also test the loading indicator", 5);
    apply(follow_up, observation, 5);
    let second = receiver.try_recv().unwrap();
    assert_eq!(
        second.task_context.recent_prompts,
        [private_prompt, "also test the loading indicator"]
    );
    complete(first, Ok("outdated summary"));
    assert!(
        snapshot()
            .sidebar_model
            .task_summary_loading
            .contains(&pane)
    );
    assert!(current_record().task_context.current_summary().is_none());
    complete(second, Ok("Sidebar summary repair"));
    assert!(snapshot().sidebar_model.task_summary_loading.is_empty());
    assert_eq!(
        current_record()
            .task_context
            .current_summary()
            .unwrap()
            .text
            .as_deref(),
        Some("Sidebar summary repair")
    );

    let restored = crate::pane_state::snapshot::load_snapshot(
        &snapshot_path,
        &coordinator.incarnation.identity,
    )
    .unwrap();
    let restored = restored.get(&pane).unwrap();
    assert!(restored.task_context.summary_input().is_none());
    assert_eq!(
        restored.task_context.current_summary(),
        current_record().task_context.current_summary()
    );
    assert!(coordinator.schedule_task_summary(restored, None).is_none());

    let (follow_up, observation) = event("UserPromptSubmit", "check a newer task", 6);
    apply(follow_up, observation, 6);
    complete(receiver.try_recv().unwrap(), Err("process timed out"));
    assert!(snapshot().sidebar_model.task_summary_loading.is_empty());
    assert_eq!(
        current_record()
            .task_context
            .current_summary()
            .unwrap()
            .failure_code
            .as_deref(),
        Some("timeout")
    );

    let (follow_up, observation) = event("UserPromptSubmit", "task before session replacement", 7);
    apply(follow_up, observation, 7);
    let obsolete = receiver.try_recv().unwrap();
    let (session, observation) = event("SessionStart", "", 8);
    apply(session, observation, 8);
    complete(obsolete, Ok("obsolete epoch summary"));
    assert_eq!(
        current_record().task_context,
        crate::pane_state::TaskContextState::default()
    );
    assert!(snapshot().sidebar_model.task_summary_loading.is_empty());
    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

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
