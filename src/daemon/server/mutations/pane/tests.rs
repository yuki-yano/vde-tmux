use super::super::super::*;
use super::*;

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
fn repeated_permission_wait_observation_skips_fresh_visibility_query() {
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
