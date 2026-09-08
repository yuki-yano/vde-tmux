use super::super::super::*;
use super::*;

#[test]
fn task_summary_loading_tracks_dispatch_completion_and_superseded_requests() {
    use crate::daemon::task_summary::{TaskSummaryCompletion, TaskSummaryJob};
    use crate::pane_state::{TaskSummaryOutcome, TaskSummaryState};

    let root = test_root("summary-loading");
    let mut coordinator = initialized_test_coordinator(&root, "a".repeat(64), Default::default());
    coordinator.env.insert(
        "XDG_STATE_HOME".to_string(),
        root.join("state").display().to_string(),
    );
    let (sender, receiver) = mpsc::sync_channel(4);
    coordinator.task_summary_tx = Some(sender);
    let pane = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 101,
    };
    coordinator
        .state
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .topology
        .panes = vec![read_peek_test_topology_pane(pane.clone(), false)];
    let begin = |prompt: &str, at| {
        let envelope = PaneEventEnvelope {
            daemon_instance_id: coordinator
                .router
                .lock()
                .unwrap()
                .daemon_instance_id()
                .clone(),
            event_id: EventId::generate().unwrap(),
            pane_instance: pane.clone(),
            agent: Some(crate::pane_state::AgentKind::parse("claude").unwrap()),
            agent_session_id: Some(
                crate::pane_state::AgentSessionId::parse("summary-session").unwrap(),
            ),
            event: PaneEvent::BeginRun {
                started_at: at,
                prompt: Some(crate::pane_state::PromptState {
                    text: prompt.to_string(),
                    source: "user".to_string(),
                    digest: None,
                }),
            },
        };
        let result =
            apply_pane_event_mutation(&coordinator, at as u64, envelope, false, None, None);
        assert!(
            matches!(result, ServerMessage::PaneEventResult { .. }),
            "{result:?}"
        );
    };
    let snapshot = || {
        coordinator
            .state
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .resolved_snapshot()
    };
    let complete = |job: TaskSummaryJob, text: Result<Option<&str>, &str>| {
        let fingerprint = job.task_context.context_fingerprint().unwrap();
        let completion = TaskSummaryCompletion {
            pane_instance: job.pane_instance,
            state_id: job.state_id,
            agent_epoch: job.agent_epoch,
            context_fingerprint: fingerprint.clone(),
            result: text
                .map(|text| TaskSummaryState {
                    text: text.map(str::to_string),
                    context_fingerprint: fingerprint,
                    generated_at: 10,
                    outcome: TaskSummaryOutcome::Generated,
                    failure_code: None,
                })
                .map_err(str::to_string),
        };
        let result = apply_production_mutation(
            &coordinator,
            V2SequencedMutation {
                accepted_seq: 10,
                mutation: V2AcceptedMutation::Internal(V2InternalMutation::TaskSummaryCompleted(
                    completion,
                )),
            },
        );
        assert!(
            matches!(result, ServerMessage::PaneEventResult { .. }),
            "{result:?}"
        );
    };

    begin("最初のタスク", 1);
    let first = receiver.try_recv().unwrap();
    assert!(
        snapshot()
            .sidebar_model
            .task_summary_loading
            .contains(&pane)
    );
    let first_snapshot = snapshot();
    let sidebar = crate::sidebar::tree::project_sidebar(
        &crate::config::Config::default(),
        &first_snapshot.panes,
        &first_snapshot.sidebar_model,
        &[],
        &crate::sidebar::state::SidebarState {
            presentation_mode: crate::sidebar::state::PresentationMode::Flat,
            category_scope: crate::sidebar::state::CategoryScope::All,
            ..Default::default()
        },
        1,
    );
    assert!(sidebar.rows.iter().any(|row| {
        row.meta
            .as_ref()
            .is_some_and(|meta| meta.task_summary_loading)
    }));

    begin("次のタスク", 2);
    let second = receiver.try_recv().unwrap();
    complete(first, Ok(Some("古い要約")));
    assert!(
        snapshot()
            .sidebar_model
            .task_summary_loading
            .contains(&pane)
    );
    complete(second, Ok(Some("次の要約")));
    let done = snapshot();
    assert!(done.sidebar_model.task_summary_loading.is_empty());
    assert_eq!(
        done.panes[0]
            .resolved
            .as_ref()
            .unwrap()
            .canonical
            .task_context
            .current_summary()
            .unwrap()
            .text
            .as_deref(),
        Some("次の要約")
    );

    begin("失敗するタスク", 3);
    complete(receiver.try_recv().unwrap(), Err("process timed out"));
    assert!(snapshot().sidebar_model.task_summary_loading.is_empty());
    begin("要約のないタスク", 4);
    complete(receiver.try_recv().unwrap(), Ok(None));
    assert!(snapshot().sidebar_model.task_summary_loading.is_empty());

    begin("再起動前のタスク", 5);
    assert!(
        snapshot()
            .sidebar_model
            .task_summary_loading
            .contains(&pane)
    );
    let records = coordinator
        .state
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .leased
        .runtime
        .records_snapshot();
    drop(coordinator);
    let coordinator = initialized_test_coordinator(&root, "a".repeat(64), Default::default());
    let mut guard = coordinator.state.lock().unwrap();
    let state = guard.as_mut().unwrap();
    state.leased.hydrate(records).unwrap();
    assert!(
        state
            .resolved_snapshot()
            .sidebar_model
            .task_summary_loading
            .is_empty()
    );
    drop(guard);
    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn task_summary_dispatch_does_not_mark_unqueued_or_duplicate_work_as_loading() {
    let root = test_root("summary-dispatch");
    let mut coordinator = test_coordinator(&root, "b".repeat(64));
    coordinator.env.insert(
        "XDG_STATE_HOME".to_string(),
        root.join("state").display().to_string(),
    );
    let mut state = guarded_prompt_test_pane_state(&guarded_prompt_test_binding());
    state.task_context.observe_prompt("要約を生成して");
    assert!(coordinator.schedule_task_summary(&state, None).is_none());
    let (sender, receiver) = mpsc::sync_channel(1);
    coordinator.task_summary_tx = Some(sender);
    let pending = coordinator.schedule_task_summary(&state, None).unwrap();
    assert!(
        coordinator
            .schedule_task_summary(&state, Some(&pending))
            .is_none()
    );
    state.task_context.observe_prompt("新しいタスク");
    assert!(
        coordinator
            .schedule_task_summary(&state, Some(&pending))
            .is_none()
    );
    receiver.try_recv().unwrap();
    drop(receiver);
    assert!(coordinator.schedule_task_summary(&state, None).is_none());
    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn observation_batch_applies_all_stages_and_publishes_one_snapshot_build() {
    for pane_count in [0usize, 1, 62] {
        let root = test_root(&format!("batch-apply-{pane_count}"));
        let server_identity = crate::daemon::topology::ServerIdentity {
            pid: 1,
            start_time: 2,
        };
        let coordinator = test_coordinator(&root, format!("batch-apply-{pane_count:0>52}"));
        coordinator
            .router
            .lock()
            .unwrap()
            .set_phase(DaemonPhase::Serving);
        let daemon_instance_id = coordinator
            .router
            .lock()
            .unwrap()
            .daemon_instance_id()
            .clone();
        let leased =
            crate::daemon::runtime::LeasedCanonicalPaneStateRuntime::acquire(&root.join("writer"))
                .unwrap();
        *coordinator.state.lock().unwrap() =
            Some(crate::daemon::runtime::CanonicalCoordinatorState::new(
                leased,
                crate::daemon::topology::TopologySnapshot {
                    server_identity: server_identity.clone(),
                    panes: Vec::new(),
                },
                crate::daemon::view_hooks::CurrentClientViews::default(),
                crate::sidebar::state::SidebarPreferences::default(),
            ));

        let observations = (0..pane_count)
            .map(|index| PaneEventEnvelope {
                daemon_instance_id: daemon_instance_id.clone(),
                event_id: EventId::generate().unwrap(),
                pane_instance: PaneInstance {
                    pane_id: format!("%{index}"),
                    pane_pid: 10_000 + index as u32,
                },
                agent: None,
                agent_session_id: None,
                event: PaneEvent::ObservationBatch {
                    base: None,
                    tracker_generation: 0,
                    observed_at: 1,
                    presence: crate::pane_state::AgentPresenceObservation::Unknown,
                    capture: None,
                    process: None,
                },
            })
            .collect::<Vec<_>>();
        let response = apply_production_mutation(
            &coordinator,
            V2SequencedMutation {
                accepted_seq: 1,
                mutation: V2AcceptedMutation::Internal(V2InternalMutation::ObservationBatch(
                    Box::new(ObservationBatchPayload {
                        projection: Box::new(ObservationPollProjection {
                            observation_seq: 1,
                            topology: crate::daemon::topology::TopologySnapshot {
                                server_identity: server_identity.clone(),
                                panes: Vec::new(),
                            },
                            status_metadata:
                                crate::daemon::runtime::StatusProjectionMetadata::default(),
                            witnesses: Vec::new(),
                            observation_bases: BTreeMap::new(),
                            view_base: crate::daemon::view_hooks::CurrentClientViews::default(),
                            through_unread_order: 0,
                        }),
                        observations,
                        removals: Vec::new(),
                        diagnostics: vec![(None, "poll diagnostic".to_string())],
                    }),
                )),
            },
        );
        let ServerMessage::SnapshotAck {
            snapshot_revision, ..
        } = response
        else {
            panic!("batch response for {pane_count} panes: {response:?}");
        };
        assert!(!coordinator.shutdown.load(Ordering::SeqCst));

        let published = coordinator.publish_resolved_snapshot().unwrap();
        assert_eq!(published.revision, snapshot_revision);

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn observation_unread_preflight_matches_state_creating_inferences() {
    use crate::pane_state::{
        AgentKind, AgentPresenceObservation, CaptureInference, CaptureObservation,
        CaptureTrackerSnapshot, LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneInstance, PaneState,
        StateId, TaskState, UnreadState, WaitReason,
    };

    let agent = AgentKind::parse("codex").unwrap();
    let mut state = PaneState {
        schema_version: PANE_STATE_SCHEMA_VERSION,
        state_id: StateId::parse("00112233445566778899aabbccddeeff").unwrap(),
        revision: 1,
        pane_instance: PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        },
        agent: agent.clone(),
        agent_session_id: None,
        agent_process: None,
        agent_epoch: 1,
        agent_present: true,
        scan_verified: true,
        synthetic_completion_armed: false,
        lifecycle: LifecycleState::Waiting {
            reason: WaitReason::PermissionPrompt,
        },
        run_seq: 1,
        current_run: None,
        completed_seq: 0,
        unread: UnreadState::default(),
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
    let permission_wait = CaptureObservation {
        inference: CaptureInference::PermissionWait {
            reason: WaitReason::PermissionPrompt,
        },
        observed_fingerprint: Some([1; 32]),
    };
    let tracker = CaptureTrackerSnapshot::default();
    let present = AgentPresenceObservation::Present(agent);
    let provider_error = CaptureObservation {
        inference: CaptureInference::ProviderError {
            reason: crate::detect::PROVIDER_OVERLOADED_REASON.to_string(),
        },
        observed_fingerprint: Some([2; 32]),
    };
    let usage_limit = CaptureObservation {
        inference: CaptureInference::UsageLimit,
        observed_fingerprint: Some([3; 32]),
    };

    assert!(!observation_may_create_unread(
        &state,
        &tracker,
        &present,
        Some(&permission_wait),
    ));

    state.lifecycle = LifecycleState::Error { reason: None };
    assert!(observation_may_create_unread(
        &state,
        &tracker,
        &present,
        Some(&permission_wait),
    ));

    state.lifecycle = LifecycleState::Running;
    assert!(observation_may_create_unread(
        &state,
        &tracker,
        &present,
        Some(&provider_error),
    ));

    state.lifecycle = LifecycleState::Error {
        reason: Some(crate::detect::PROVIDER_OVERLOADED_REASON.to_string()),
    };
    assert!(!observation_may_create_unread(
        &state,
        &tracker,
        &present,
        Some(&provider_error),
    ));

    state.lifecycle = LifecycleState::Running;
    assert!(observation_may_create_unread(
        &state,
        &tracker,
        &present,
        Some(&usage_limit),
    ));
    assert!(observation_may_create_unread(
        &state,
        &tracker,
        &AgentPresenceObservation::Absent,
        Some(&usage_limit),
    ));
    assert!(observation_may_create_unread(
        &state,
        &tracker,
        &AgentPresenceObservation::Absent,
        Some(&provider_error),
    ));

    state.lifecycle = LifecycleState::Waiting {
        reason: WaitReason::Other("usage_limit".to_string()),
    };
    assert!(!observation_may_create_unread(
        &state,
        &tracker,
        &present,
        Some(&usage_limit),
    ));

    state.lifecycle = LifecycleState::Waiting {
        reason: WaitReason::PermissionPrompt,
    };
    let absence_tracker = CaptureTrackerSnapshot {
        absence_count: 1,
        ..CaptureTrackerSnapshot::default()
    };
    assert!(observation_may_create_unread(
        &state,
        &absence_tracker,
        &AgentPresenceObservation::Absent,
        Some(&permission_wait),
    ));
}
