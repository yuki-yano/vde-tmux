use super::super::super::*;
use super::super::pane::pane_needs_durable_run_projection;
use super::super::provider::apply_external_provider_event_with_runner;
use super::*;

#[test]
fn immediate_codex_prompt_retries_transient_process_scan_after_session_start() {
    use crate::agent_state::{ExecutionPhase, SemanticOutcome};
    use crate::pane_state::{AgentProcessIdentity, LifecycleState};

    let root = test_root("codex-session-start-process-race");
    let hash = "codex-session-start-process-race";
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
        pane_id: "%537".to_string(),
        pane_pid: 53_700,
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
        r#"{"session_id":"session-537","source":"startup"}"#,
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
    let started = coordinator
        .state
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .leased
        .runtime
        .record(&pane)
        .unwrap()
        .clone();
    assert!(started.agent_process.is_none());
    assert!(!started.scan_verified);

    let process = AgentProcessIdentity {
        pid: 53_762,
        start_token: "codex-process-start".to_string(),
    };
    runner.stub_agent_process_sequence(
        pane.pane_pid,
        "codex",
        [
            Ok(None),
            Err("transient process identity race".to_string()),
            Ok(Some(process.clone())),
        ],
    );
    let (prompt_envelope, prompt_observation) = make_event(
        "UserPromptSubmit",
        r#"{"session_id":"session-537","turn_id":"turn-1","prompt":"hello"}"#,
        2,
    );
    assert!(matches!(
        apply_external_provider_event_with_runner(
            &coordinator,
            2,
            prompt_envelope,
            prompt_observation.clone(),
            &runner,
        ),
        ServerMessage::PaneEventResult { .. }
    ));
    let running = coordinator
        .state
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .leased
        .runtime
        .record(&pane)
        .unwrap()
        .clone();
    assert_eq!(running.agent_process.as_ref(), Some(&process));
    assert!(running.scan_verified);
    assert_eq!(running.run_seq, 1);
    assert!(matches!(running.lifecycle, LifecycleState::Running));
    assert!(running.current_run.is_some());
    assert_eq!(
        running.prompt.as_ref().map(|prompt| prompt.text.as_str()),
        Some("hello")
    );
    assert_eq!(running.task_context.recent_prompts, ["hello"]);
    let durable_run = coordinator
        .agent_runtime
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .provider_event_run(&prompt_observation)
        .unwrap()
        .unwrap();
    assert_eq!(durable_run.execution_phase, ExecutionPhase::Running);
    assert_eq!(durable_run.semantic_outcome, SemanticOutcome::Unresolved);

    let (steer_envelope, steer_observation) = make_event(
        "UserPromptSubmit",
        r#"{"session_id":"session-537","turn_id":"turn-1","prompt":"review with pane five"}"#,
        3,
    );
    assert!(matches!(
        apply_external_provider_event_with_runner(
            &coordinator,
            3,
            steer_envelope,
            steer_observation.clone(),
            &runner,
        ),
        ServerMessage::PaneEventResult { .. }
    ));
    let steered = coordinator
        .state
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .leased
        .runtime
        .record(&pane)
        .unwrap()
        .clone();
    assert_eq!(steered.run_seq, 1);
    assert_eq!(
        steered.prompt.as_ref().map(|prompt| prompt.text.as_str()),
        Some("review with pane five")
    );
    assert_eq!(
        steered.task_context.recent_prompts,
        ["hello", "review with pane five"]
    );
    let steered_run = coordinator
        .agent_runtime
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .provider_event_run(&steer_observation)
        .unwrap()
        .unwrap();
    assert_eq!(steered_run.run_id, durable_run.run_id);
    assert_eq!(steered_run.run_seq, durable_run.run_seq);
    assert_eq!(
        steered_run
            .evidence
            .provider_events
            .last()
            .unwrap()
            .disposition,
        "prompt_updated"
    );

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn durable_prompt_confirms_across_an_in_process_codex_session_rollover() {
    use crate::agent_state::{DispatchState, OperationId, Sha256Digest};
    use crate::pane_state::{AgentProcessIdentity, LifecycleState};

    let root = test_root("codex-session-rollover-confirmation");
    let hash = "codex-session-rollover-confirmation";
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
        pane_id: "%539".to_string(),
        pane_pid: 53_900,
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
    let process = AgentProcessIdentity {
        pid: 53_962,
        start_token: "codex-session-rollover-process".to_string(),
    };

    let (session_envelope, session_observation) = make_event(
        "SessionStart",
        r#"{"session_id":"session-before-rollover","source":"startup"}"#,
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
    runner.stub_agent_process(pane.pane_pid, "codex", Some(process.clone()));

    let (previous_envelope, previous_observation) = make_event(
        "UserPromptSubmit",
        r#"{"session_id":"session-before-rollover","turn_id":"turn-before-rollover","prompt":"previous task"}"#,
        2,
    );
    assert!(matches!(
        apply_external_provider_event_with_runner(
            &coordinator,
            2,
            previous_envelope,
            previous_observation,
            &runner,
        ),
        ServerMessage::PaneEventResult { .. }
    ));
    let before_rollover = coordinator
        .state
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .leased
        .runtime
        .record(&pane)
        .unwrap()
        .clone();
    assert_eq!(before_rollover.agent_epoch, 1);
    assert_eq!(before_rollover.run_seq, 1);
    assert!(matches!(before_rollover.lifecycle, LifecycleState::Running));
    let operation_id = OperationId::parse("operation_session_rollover_server_0001").unwrap();
    let operation_prompt = "task after session rollover";
    let operation_digest = Sha256Digest::parse(
        crate::pane_state::PromptState::digest_decoded_prompt(operation_prompt),
    )
    .unwrap();
    let binding = crate::agent_state::AgentBinding {
        server_identity: coordinator.incarnation.identity.clone(),
        pane_instance: pane.clone(),
        pane_state_id: before_rollover.state_id.clone(),
        agent_epoch: before_rollover.agent_epoch,
        agent_kind: before_rollover.agent.clone(),
        provider_session_id: before_rollover.agent_session_id.clone().unwrap(),
        process: before_rollover.agent_process.clone().unwrap(),
    };
    let operation_observed_at = epoch_seconds();
    {
        let mut runtime = coordinator.agent_runtime.lock().unwrap();
        let runtime = runtime.as_mut().unwrap();
        runtime
            .prepare_operation(
                operation_id.clone(),
                "vta1:session-rollover-target".to_string(),
                operation_prompt.as_bytes(),
                operation_digest,
                "paste_enter".to_string(),
                binding,
                before_rollover.version(),
                before_rollover.current_run.clone(),
                2,
                operation_observed_at,
            )
            .unwrap();
        runtime
            .mark_dispatch_started(&operation_id, operation_observed_at)
            .unwrap();
    }

    let (rollover_envelope, rollover_observation) = make_event(
        "SessionStart",
        r#"{"session_id":"session-after-rollover","source":"startup"}"#,
        4,
    );
    assert!(matches!(
        apply_external_provider_event_with_runner(
            &coordinator,
            4,
            rollover_envelope,
            rollover_observation,
            &runner,
        ),
        ServerMessage::PaneEventResult { .. }
    ));
    runner.stub_agent_process(pane.pane_pid, "codex", Some(process));

    let (prompt_envelope, prompt_observation) = make_event(
        "UserPromptSubmit",
        r#"{"session_id":"session-after-rollover","turn_id":"turn-after-rollover","prompt":"task after session rollover"}"#,
        5,
    );
    assert!(matches!(
        apply_external_provider_event_with_runner(
            &coordinator,
            5,
            prompt_envelope,
            prompt_observation,
            &runner,
        ),
        ServerMessage::PaneEventResult { .. }
    ));

    {
        let runtime = coordinator.agent_runtime.lock().unwrap();
        let runtime = runtime.as_ref().unwrap();
        let operation = runtime
            .get_operation(&runtime.operation_ref(operation_id.clone()))
            .unwrap();
        assert_eq!(operation.dispatch_state, DispatchState::PromptConfirmed);
        assert_eq!(operation.binding.agent_epoch, 2);
        assert_eq!(
            operation
                .binding
                .provider_session_id
                .as_ref()
                .unwrap()
                .as_str(),
            "session-after-rollover"
        );
        assert_eq!(operation.expected_run_seq, 2);
        let run = runtime
            .get_run(&runtime.run_ref(operation.run_id.unwrap()))
            .unwrap();
        assert_eq!(run.run_seq, 1);
        assert_eq!(run.operation_id.as_ref(), Some(&operation_id));
    }

    let after_rollover = coordinator
        .state
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .leased
        .runtime
        .record(&pane)
        .unwrap()
        .clone();
    assert_eq!(after_rollover.agent_epoch, 2);
    assert_eq!(after_rollover.run_seq, 1);
    assert!(matches!(after_rollover.lifecycle, LifecycleState::Running));
    assert!(after_rollover.prompt.is_none());

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn guarded_prompt_process_owner_rejections_cover_every_fail_closed_branch() {
    let binding = guarded_prompt_test_binding();
    let operation_binding = crate::agent_state::OperationBinding::from(binding.clone());
    let pane = binding.pane_instance.clone();

    let replaced = crate::tmux::mock::MockTmuxRunner::new();
    let mut other_process = binding.process.clone();
    other_process.pid += 1;
    replaced.stub_agent_process(
        pane.pane_pid,
        binding.agent_kind.as_str(),
        Some(other_process),
    );
    assert_eq!(
        verify_agent_prompt_process_and_owner(&replaced, &pane, &operation_binding)
            .unwrap_err()
            .code,
        "target_process_replaced"
    );

    let absent = crate::tmux::mock::MockTmuxRunner::new();
    absent.stub_agent_process(pane.pane_pid, binding.agent_kind.as_str(), None);
    assert_eq!(
        verify_agent_prompt_process_and_owner(&absent, &pane, &operation_binding)
            .unwrap_err()
            .code,
        "target_process_absent"
    );

    let unverifiable = crate::tmux::mock::MockTmuxRunner::new();
    assert_eq!(
        verify_agent_prompt_process_and_owner(&unverifiable, &pane, &operation_binding)
            .unwrap_err()
            .code,
        "target_process_unverifiable"
    );

    let not_owner = crate::tmux::mock::MockTmuxRunner::new();
    not_owner.stub_agent_process(
        pane.pane_pid,
        binding.agent_kind.as_str(),
        Some(binding.process.clone()),
    );
    not_owner.stub_agent_input_owner(pane.pane_pid, binding.process.pid, false);
    assert_eq!(
        verify_agent_prompt_process_and_owner(&not_owner, &pane, &operation_binding)
            .unwrap_err()
            .code,
        "agent_not_input_owner"
    );

    let accepted = crate::tmux::mock::MockTmuxRunner::new();
    accepted.stub_agent_process(
        pane.pane_pid,
        binding.agent_kind.as_str(),
        Some(binding.process.clone()),
    );
    accepted.stub_agent_input_owner(pane.pane_pid, binding.process.pid, true);
    verify_agent_prompt_process_and_owner(&accepted, &pane, &operation_binding).unwrap();
}

#[test]
fn guarded_prompt_lock_binding_and_pane_preconditions_are_fail_closed() {
    let binding = guarded_prompt_test_binding();
    let pane = binding.pane_instance.clone();
    let first = acquire_agent_prompt_dispatch_lock(&binding.server_identity, &pane).unwrap();
    assert_eq!(
        acquire_agent_prompt_dispatch_lock(&binding.server_identity, &pane)
            .unwrap_err()
            .code,
        "dispatch_lock_busy"
    );
    drop(first);

    let operation = guarded_prompt_test_operation(binding.clone());
    let operation_binding = crate::agent_state::OperationBinding::from(binding.clone());
    assert!(prepared_operation_matches_target(
        &operation,
        &operation_binding,
        operation.expected_pane_version.clone(),
        None,
        operation.expected_run_seq,
    ));
    assert!(!prepared_operation_matches_target(
        &operation,
        &operation_binding,
        operation.expected_pane_version.clone(),
        None,
        operation.expected_run_seq + 1,
    ));
    assert_eq!(
        prepared_target_rejection_code(true, None),
        Some("target_no_longer_current")
    );
    assert_eq!(
        prepared_target_rejection_code(true, Some(false)),
        Some("binding_changed_before_dispatch")
    );
    assert_eq!(prepared_target_rejection_code(true, Some(true)), None);
    assert_eq!(prepared_target_rejection_code(false, None), None);

    let mut pane_state = guarded_prompt_test_pane_state(&binding);
    assert!(agent_prompt_precondition_matches(&pane_state, &operation));
    pane_state.revision += 1;
    assert!(!agent_prompt_precondition_matches(&pane_state, &operation));
}

#[test]
fn duplicate_operator_resolution_repairs_a_failed_completed_pane_projection() {
    use std::os::unix::fs::PermissionsExt as _;

    use crate::agent_state::{
        AgentBinding, ExecutionPhase, PRIVATE_STATE_FORMAT_VERSION, ResolutionId, ResolutionKind,
        RunEvidenceSummary, RunRecord, RunResolution, SemanticOutcome, StableRunId,
        StateGeneration,
    };
    use crate::pane_state::{
        AgentKind, AgentProcessIdentity, AgentSessionId, CurrentDurableRunProjection,
        LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneState, StateId, TaskContextState, TaskState,
        UnreadState,
    };

    let root = test_root("operator-projection-repair");
    let hash = "operator-projection-repair-hash";
    let env = BTreeMap::from([(
        "XDG_STATE_HOME".to_string(),
        root.to_string_lossy().into_owned(),
    )]);
    let coordinator =
        ProductionV2Coordinator::new(test_incarnation(&root, hash), env.clone(), None).unwrap();
    let pane = PaneInstance {
        pane_id: "%7".to_string(),
        pane_pid: 77,
    };
    let state_id = StateId::parse("1".repeat(32)).unwrap();
    let run_id = StableRunId::parse("2".repeat(32)).unwrap();
    let agent = AgentKind::parse("codex").unwrap();
    let session_id = AgentSessionId::parse("session-projection-repair").unwrap();
    let process = AgentProcessIdentity {
        pid: 88,
        start_token: "process-start-token".to_string(),
    };
    let pane_state = PaneState {
        schema_version: PANE_STATE_SCHEMA_VERSION,
        state_id: state_id.clone(),
        revision: 1,
        pane_instance: pane.clone(),
        agent: agent.clone(),
        agent_session_id: Some(session_id.clone()),
        agent_process: Some(process.clone()),
        agent_epoch: 1,
        agent_present: true,
        scan_verified: true,
        synthetic_completion_armed: false,
        lifecycle: LifecycleState::Running,
        run_seq: 1,
        current_run: Some(CurrentDurableRunProjection {
            run_id: run_id.as_str().to_string(),
            run_seq: 1,
            run_revision: 1,
        }),
        completed_seq: 0,
        unread: UnreadState::default(),
        started_at: Some(1),
        completed_at: None,
        prompt: None,
        latest_response: None,
        task_context: TaskContextState::default(),
        tasks: TaskState::default(),
        subagents: Vec::new(),
        worktree_activity: None,
        background_process: None,
        listening_ports: Vec::new(),
    };
    let mut leased =
        crate::daemon::runtime::LeasedCanonicalPaneStateRuntime::acquire(&root.join("writer"))
            .unwrap();
    leased
        .hydrate(BTreeMap::from([(pane.clone(), pane_state.clone())]))
        .unwrap();
    *coordinator.state.lock().unwrap() =
        Some(crate::daemon::runtime::CanonicalCoordinatorState::new(
            leased,
            crate::daemon::topology::TopologySnapshot {
                server_identity: coordinator.incarnation.identity.clone(),
                panes: Vec::new(),
            },
            crate::daemon::view_hooks::CurrentClientViews::default(),
            crate::sidebar::state::SidebarPreferences::default(),
        ));
    let run = RunRecord {
        state_format_version: PRIVATE_STATE_FORMAT_VERSION,
        generation: StateGeneration::parse("3".repeat(32)).unwrap(),
        run_id,
        run_seq: 1,
        revision: 2,
        binding: AgentBinding {
            server_identity: coordinator.incarnation.identity.clone(),
            pane_instance: pane.clone(),
            pane_state_id: state_id,
            agent_epoch: 1,
            agent_kind: agent,
            provider_session_id: session_id,
            process,
        },
        provider_turn_key: Some("turn-projection-repair".to_string()),
        operation_id: None,
        execution_phase: ExecutionPhase::Ended,
        semantic_outcome: SemanticOutcome::Completed,
        evidence: RunEvidenceSummary::default(),
        resolution: Some(RunResolution {
            resolution_id: ResolutionId::parse("resolution_projection_repair").unwrap(),
            kind: ResolutionKind::ProviderCompleted,
            resolved_at: 2,
            operator_audit: None,
        }),
        artifact: None,
        created_at: 1,
        updated_at: 2,
    };
    run.validate().unwrap();
    assert!(pane_needs_durable_run_projection(&pane_state, &run).unwrap());
    let mut newer_pane_run = pane_state.clone();
    newer_pane_run.run_seq = 2;
    newer_pane_run.current_run = Some(CurrentDurableRunProjection {
        run_id: "4".repeat(32),
        run_seq: 2,
        run_revision: 1,
    });
    assert!(!pane_needs_durable_run_projection(&newer_pane_run, &run).unwrap());
    let mut lagging_projection = pane_state.clone();
    lagging_projection.run_seq = 0;
    lagging_projection.current_run = None;
    assert!(pane_needs_durable_run_projection(&lagging_projection, &run).unwrap());
    let mut equal_sequence_without_pointer = pane_state.clone();
    equal_sequence_without_pointer.current_run = None;
    assert!(pane_needs_durable_run_projection(&equal_sequence_without_pointer, &run).unwrap());
    let mut conflicting_projection = pane_state.clone();
    conflicting_projection.current_run = Some(CurrentDurableRunProjection {
        run_id: "4".repeat(32),
        run_seq: 1,
        run_revision: 1,
    });
    assert!(pane_needs_durable_run_projection(&conflicting_projection, &run).is_err());

    let snapshot_dir = crate::daemon::lifecycle::incarnation_log_directory(&env, hash);
    std::fs::create_dir_all(snapshot_dir.parent().unwrap()).unwrap();
    std::fs::write(&snapshot_dir, b"block snapshot directory").unwrap();
    assert!(project_operator_completed_run(&coordinator, &run).is_err());
    {
        let state = coordinator.state.lock().unwrap();
        let unchanged = state
            .as_ref()
            .unwrap()
            .leased
            .runtime
            .record(&pane)
            .unwrap();
        assert!(matches!(unchanged.lifecycle, LifecycleState::Running));
        assert_eq!(unchanged.current_run.as_ref().unwrap().run_revision, 1);
    }

    std::fs::remove_file(&snapshot_dir).unwrap();
    std::fs::create_dir_all(&snapshot_dir).unwrap();
    std::fs::set_permissions(&snapshot_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    project_operator_completed_run(&coordinator, &run).unwrap();
    let repaired_revision = {
        let state = coordinator.state.lock().unwrap();
        let repaired = state
            .as_ref()
            .unwrap()
            .leased
            .runtime
            .record(&pane)
            .unwrap();
        assert!(matches!(repaired.lifecycle, LifecycleState::Idle));
        assert_eq!(repaired.current_run.as_ref().unwrap().run_revision, 2);
        repaired.revision
    };
    project_operator_completed_run(&coordinator, &run).unwrap();
    assert_eq!(
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
            .revision,
        repaired_revision
    );

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}
