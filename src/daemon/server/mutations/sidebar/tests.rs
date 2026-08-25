use super::super::super::state_helpers::persist_pruned_sidebar_pins;
use super::super::super::*;
use super::*;

#[test]
fn sidebar_preference_intents_commit_serially_and_dedupe_event_ids() {
    let root = test_root("sidebar-intents");
    let env = BTreeMap::from([(
        "XDG_STATE_HOME".to_string(),
        root.to_string_lossy().into_owned(),
    )]);
    let socket_path = root.join("tmux.sock");
    let server_identity = crate::daemon::topology::ServerIdentity {
        pid: 1,
        start_time: 2,
    };
    let coordinator = ProductionV2Coordinator::new(
        test_incarnation(&root, "sidebar-intents"),
        env.clone(),
        None,
    )
    .unwrap();
    let leased =
        crate::daemon::runtime::LeasedCanonicalPaneStateRuntime::acquire(&root.join("writer"))
            .unwrap();
    *coordinator.state.lock().unwrap() =
        Some(crate::daemon::runtime::CanonicalCoordinatorState::new(
            leased,
            crate::daemon::topology::TopologySnapshot {
                server_identity,
                panes: Vec::new(),
            },
            crate::daemon::view_hooks::CurrentClientViews::default(),
            crate::sidebar::state::SidebarPreferences::default(),
        ));
    let first_event = EventId::generate().unwrap();
    let second_event = EventId::generate().unwrap();

    let first = apply_sidebar_preference_intent(
        &coordinator,
        1,
        first_event.clone(),
        crate::sidebar::state::SidebarPreferenceIntent::SetDefaultFilter {
            filter: crate::sidebar::state::StatusFilter::DoneOnly,
        },
    );
    let second = apply_sidebar_preference_intent(
        &coordinator,
        2,
        second_event,
        crate::sidebar::state::SidebarPreferenceIntent::SetDefaultPresentationMode {
            presentation_mode: crate::sidebar::state::PresentationMode::Flat,
        },
    );
    let duplicate = apply_sidebar_preference_intent(
        &coordinator,
        3,
        first_event,
        crate::sidebar::state::SidebarPreferenceIntent::SetDefaultFilter {
            filter: crate::sidebar::state::StatusFilter::All,
        },
    );

    assert!(matches!(
        first,
        ServerMessage::SnapshotAck {
            snapshot_revision: 1,
            ..
        }
    ));
    assert!(matches!(
        second,
        ServerMessage::SnapshotAck {
            snapshot_revision: 2,
            ..
        }
    ));
    assert!(matches!(
        duplicate,
        ServerMessage::SnapshotAck {
            snapshot_revision: 2,
            ..
        }
    ));
    let persisted =
        crate::sidebar::store::load_state(&crate::sidebar::store::state_path(&env, &socket_path))
            .unwrap();
    assert_eq!(
        persisted.filter,
        crate::sidebar::state::StatusFilter::DoneOnly
    );
    assert_eq!(
        persisted.presentation_mode,
        crate::sidebar::state::PresentationMode::Flat
    );

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn category_intents_return_persisted_repo_effects_and_stable_noop_revisions() {
    let root = test_root("category-receipts");
    let env = BTreeMap::from([(
        "XDG_STATE_HOME".to_string(),
        root.to_string_lossy().into_owned(),
    )]);
    let coordinator = ProductionV2Coordinator::new(
        test_incarnation(&root, "category-receipts"),
        env.clone(),
        None,
    )
    .unwrap();
    install_test_state(
        &coordinator,
        &root,
        crate::daemon::view_hooks::CurrentClientViews::default(),
    );
    let repo = crate::category::RepoKey::path("/tmp/category-receipt-repo");
    let work = crate::category::CategoryName::parse("work").unwrap();
    {
        let mut guard = coordinator.state.lock().unwrap();
        guard
            .as_mut()
            .unwrap()
            .projection_config
            .categories
            .display_names
            .insert("work".to_string(), "Work".to_string());
    }

    let assign = apply_category_intent(
        &coordinator,
        1,
        EventId::generate().unwrap(),
        crate::category::CategoryIntent::AssignRepo {
            repo: repo.clone(),
            category: work.clone(),
        },
    );
    assert!(matches!(
        assign,
        ServerMessage::CategoryMutationResult {
            accepted_seq: 1,
            snapshot_revision: 1,
            category_state_revision: 1,
            changed: true,
            repo_effect: Some(
                crate::daemon::protocol::v2::CategoryRepoMutationEffect {
                    ref repo,
                    before_override: None,
                    after_override: Some(ref after),
                }
            ),
            ..
        } if repo == &crate::category::RepoKey::path("/tmp/category-receipt-repo")
            && after == &work
    ));

    let noop_assign = apply_category_intent(
        &coordinator,
        2,
        EventId::generate().unwrap(),
        crate::category::CategoryIntent::AssignRepo {
            repo: repo.clone(),
            category: work.clone(),
        },
    );
    assert!(matches!(
        noop_assign,
        ServerMessage::CategoryMutationResult {
            snapshot_revision: 1,
            category_state_revision: 1,
            changed: false,
            ..
        }
    ));

    let automatic = apply_category_intent(
        &coordinator,
        3,
        EventId::generate().unwrap(),
        crate::category::CategoryIntent::SetRepoAutomatic { repo: repo.clone() },
    );
    assert!(matches!(
        automatic,
        ServerMessage::CategoryMutationResult {
            snapshot_revision: 2,
            category_state_revision: 2,
            changed: true,
            repo_effect: Some(
                crate::daemon::protocol::v2::CategoryRepoMutationEffect {
                    before_override: Some(ref before),
                    after_override: None,
                    ..
                }
            ),
            ..
        } if before == &work
    ));

    let noop_automatic = apply_category_intent(
        &coordinator,
        4,
        EventId::generate().unwrap(),
        crate::category::CategoryIntent::SetRepoAutomatic { repo: repo.clone() },
    );
    assert!(matches!(
        noop_automatic,
        ServerMessage::CategoryMutationResult {
            snapshot_revision: 2,
            category_state_revision: 2,
            changed: false,
            ..
        }
    ));
    let persisted = crate::category::store::load_state(&crate::category::store::state_path(
        &env,
        &coordinator.incarnation.socket_path,
    ))
    .unwrap();
    assert_eq!(persisted.revision, 2);
    assert!(!persisted.repo_overrides.contains_key(&repo));

    let unknown = apply_category_intent(
        &coordinator,
        5,
        EventId::generate().unwrap(),
        crate::category::CategoryIntent::AssignRepo {
            repo,
            category: crate::category::CategoryName::parse("missing").unwrap(),
        },
    );
    assert!(matches!(
        unknown,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    assert_eq!(
        coordinator
            .state
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .category_state
            .revision,
        2
    );

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn pane_pin_persists_outside_canonical_state_and_prunes_with_topology() {
    let root = test_root("sidebar-pane-pin");
    let coordinator = initialized_test_coordinator(
        &root,
        "pane-pin",
        crate::daemon::view_hooks::CurrentClientViews::default(),
    );
    let target = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 101,
    };
    {
        let mut guard = coordinator.state.lock().unwrap();
        let state = guard.as_mut().unwrap();
        state
            .replace_topology(crate::daemon::topology::TopologySnapshot {
                server_identity: coordinator.incarnation.identity.clone(),
                panes: vec![crate::daemon::topology::TopologyPane {
                    pane_instance: target.clone(),
                    session_links: Vec::new(),
                    window_id: "@1".to_string(),
                    window_name: "main".to_string(),
                    current_path: "/tmp/app".to_string(),
                    current_command: "codex".to_string(),
                    pane_width: 80,
                    active: false,
                    editprompt_is_editor: false,
                    editprompt_target_panes: Vec::new(),
                    editprompt_editor_pane: None,
                }],
            })
            .unwrap();
        let daemon_instance_id = coordinator
            .router
            .lock()
            .unwrap()
            .daemon_instance_id()
            .clone();
        state
            .leased
            .runtime
            .apply_event(
                &mut pane_snapshot_store(&coordinator),
                &PaneEventEnvelope {
                    daemon_instance_id,
                    event_id: EventId::generate().unwrap(),
                    pane_instance: target.clone(),
                    agent: Some(crate::pane_state::AgentKind::parse("codex").unwrap()),
                    agent_session_id: Some(
                        crate::pane_state::AgentSessionId::parse("pin-session").unwrap(),
                    ),
                    event: PaneEvent::BeginRun {
                        started_at: 1,
                        prompt: None,
                    },
                },
                &crate::pane_state::VisibilitySnapshot::default(),
            )
            .unwrap();
    }

    let response = apply_sidebar_preference_intent(
        &coordinator,
        1,
        EventId::generate().unwrap(),
        crate::sidebar::state::SidebarPreferenceIntent::SetPanePinned {
            pane_instance: target.clone(),
            pinned: true,
        },
    );
    assert!(matches!(response, ServerMessage::SnapshotAck { .. }));
    let state_path =
        crate::sidebar::store::state_path(&coordinator.env, &coordinator.incarnation.socket_path);
    assert!(
        crate::sidebar::store::load_state(&state_path)
            .unwrap()
            .pinned_panes
            .contains(&target)
    );
    {
        let mut guard = coordinator.state.lock().unwrap();
        let state = guard.as_mut().unwrap();
        assert!(state.sidebar_preferences.pinned_panes.contains(&target));
        assert!(
            serde_json::to_value(state.leased.runtime.record(&target).unwrap()).unwrap()["unread"]
                .get("pinned")
                .is_none()
        );
        state.topology.panes.clear();
        assert!(persist_pruned_sidebar_pins(&coordinator, state).unwrap());
        assert!(state.sidebar_preferences.pinned_panes.is_empty());
    }
    assert!(
        crate::sidebar::store::load_state(&state_path)
            .unwrap()
            .pinned_panes
            .is_empty()
    );

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn sidebar_navigation_is_shared_in_snapshots_without_persisting_preferences() {
    let root = test_root("sidebar-navigation");
    let coordinator = initialized_test_coordinator(
        &root,
        "sidebar-navigation",
        crate::daemon::view_hooks::CurrentClientViews::default(),
    );
    let selection = Some("chat::%1::101".to_string());

    let first = apply_sidebar_navigation(
        &coordinator,
        1,
        EventId::generate().unwrap(),
        selection.clone(),
        12,
        true,
    );
    let first_revision = match first {
        ServerMessage::SnapshotAck {
            snapshot_revision, ..
        } => snapshot_revision,
        other => panic!("unexpected navigation response: {other:?}"),
    };
    let snapshot = coordinator
        .state
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .resolved_snapshot();
    assert_eq!(snapshot.sidebar_model.navigation.revision, 1);
    assert_eq!(snapshot.sidebar_model.navigation.selection, selection);
    assert_eq!(snapshot.sidebar_model.navigation.scroll, 12);
    assert!(snapshot.sidebar_model.navigation.manual_scroll);

    let duplicate = apply_sidebar_navigation(
        &coordinator,
        2,
        EventId::generate().unwrap(),
        snapshot.sidebar_model.navigation.selection.clone(),
        12,
        true,
    );
    assert!(matches!(
        duplicate,
        ServerMessage::SnapshotAck {
            snapshot_revision,
            ..
        } if snapshot_revision == first_revision
    ));

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn read_peek_commit_fences_the_old_occurrence_and_protects_the_source_during_advance() {
    let root = test_root("read-peek-fence");
    let (mut state, target, candidate) = read_peek_test_state(&root);
    assert!(state.begin_peek(20, target.clone(), [target.clone()], 3));
    state.activate_peek(20, 3, target.clone(), 0);

    let result = commit_read_peek_state(
        &mut state,
        &mut ReadPeekStoreIo { fail: false },
        &v2_daemon_id(),
        &v2_event_id(),
        &target,
        10,
        vec![candidate.clone()],
        2,
    )
    .unwrap();

    assert_eq!(
        result.read_outcome,
        crate::daemon::protocol::v2::PaneApplyOutcome::Committed
    );
    assert_eq!(result.candidates, vec![candidate.clone()]);
    assert!(
        !state
            .leased
            .runtime
            .record(&target)
            .unwrap()
            .unread
            .is_unread()
    );
    assert!(matches!(
        state.peek_leases.get(&10),
        Some(crate::daemon::runtime::PeekLease::Pending {
            operation_seq: 2,
            previous_target: Some(previous),
            candidates,
            ..
        }) if previous == &target && candidates == &BTreeSet::from([candidate])
    ));
    assert!(state.active_peek_target(20).is_none());

    let owner = read_peek_test_witness(10, &target);
    assert!(
        !state
            .read_authorized_panes(std::slice::from_ref(&owner))
            .contains(&target)
    );
    emit_read_peek_waiting_occurrence(&mut state, &target);
    let unread = &state.leased.runtime.record(&target).unwrap().unread;
    assert!(unread.is_unread());
    assert!(unread.latest_unread().unwrap().order > 1);

    drop(state);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn read_peek_without_an_advance_candidate_keeps_a_new_active_source_lease() {
    let root = test_root("read-peek-stayed");
    let (mut state, target, _) = read_peek_test_state(&root);
    let result = commit_read_peek_state(
        &mut state,
        &mut ReadPeekStoreIo { fail: false },
        &v2_daemon_id(),
        &v2_event_id(),
        &target,
        10,
        Vec::new(),
        2,
    )
    .unwrap();

    assert!(result.candidates.is_empty());
    assert_eq!(state.active_peek_target(10), Some(&target));
    let owner = read_peek_test_witness(10, &target);
    assert!(
        !state
            .read_authorized_panes(std::slice::from_ref(&owner))
            .contains(&target)
    );
    emit_read_peek_waiting_occurrence(&mut state, &target);
    assert!(
        state
            .leased
            .runtime
            .record(&target)
            .unwrap()
            .unread
            .is_unread()
    );

    drop(state);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn read_peek_advance_failure_restores_the_source_without_rolling_back_read() {
    let root = test_root("read-peek-advance-failure");
    let (mut state, target, candidate) = read_peek_test_state(&root);
    commit_read_peek_state(
        &mut state,
        &mut ReadPeekStoreIo { fail: false },
        &v2_daemon_id(),
        &v2_event_id(),
        &target,
        10,
        vec![candidate],
        2,
    )
    .unwrap();
    state.restore_peek_after_failure(10, 2, &[read_peek_test_witness(10, &target)], 3);

    assert_eq!(state.active_peek_target(10), Some(&target));
    assert!(
        !state
            .leased
            .runtime
            .record(&target)
            .unwrap()
            .unread
            .is_unread()
    );

    drop(state);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn read_peek_persist_failure_preserves_the_unread_occurrence_and_active_lease() {
    let root = test_root("read-peek-persist-failure");
    let (mut state, target, candidate) = read_peek_test_state(&root);
    let result = commit_read_peek_state(
        &mut state,
        &mut ReadPeekStoreIo { fail: true },
        &v2_daemon_id(),
        &v2_event_id(),
        &target,
        10,
        vec![candidate],
        2,
    );

    assert!(matches!(
        result,
        Err(crate::pane_state::store::StoreError::PersistFailed(_))
    ));
    assert_eq!(state.active_peek_target(10), Some(&target));
    assert!(
        state
            .leased
            .runtime
            .record(&target)
            .unwrap()
            .unread
            .is_unread()
    );

    drop(state);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn read_peek_terminal_occurrences_use_client_scoped_born_read_authority() {
    let scenarios = [
        (
            "waiting",
            PaneEvent::WaitRequested {
                observed_at: 3,
                reason: crate::pane_state::WaitReason::PermissionPrompt,
            },
            crate::pane_state::UnreadReason::Waiting,
        ),
        (
            "error",
            PaneEvent::FailRun {
                observed_at: 3,
                reason: Some("failed".to_string()),
            },
            crate::pane_state::UnreadReason::Error,
        ),
        (
            "completed",
            PaneEvent::CompleteRun { completed_at: 3 },
            crate::pane_state::UnreadReason::Completed,
        ),
    ];

    for (label, terminal_event, expected_reason) in scenarios {
        for observer_visible in [false, true] {
            let root = test_root(&format!(
                "read-peek-born-{label}-{}",
                if observer_visible {
                    "observer"
                } else {
                    "owner"
                }
            ));
            let (mut state, target, _) = read_peek_test_state(&root);
            commit_read_peek_state(
                &mut state,
                &mut ReadPeekStoreIo { fail: false },
                &v2_daemon_id(),
                &v2_event_id(),
                &target,
                10,
                Vec::new(),
                2,
            )
            .unwrap();
            apply_read_peek_event(
                &mut state,
                &target,
                PaneEvent::BeginRun {
                    started_at: 2,
                    prompt: None,
                },
                &crate::pane_state::VisibilitySnapshot::default(),
            );

            let owner = read_peek_test_witness(10, &target);
            let mut witnesses = vec![owner];
            if observer_visible {
                witnesses.push(read_peek_test_witness(20, &target));
            }
            let panes = BTreeSet::from([target.clone()]);
            let authorized = state.has_read_authority_for(&witnesses, &panes);
            assert_eq!(authorized, observer_visible, "{label}");
            if authorized {
                state.clear_peeks_for_read_panes(&panes);
            }
            apply_read_peek_event(
                &mut state,
                &target,
                terminal_event.clone(),
                &crate::pane_state::VisibilitySnapshot {
                    pane_visible_to_eligible_client: authorized,
                },
            );

            let unread = &state.leased.runtime.record(&target).unwrap().unread;
            if observer_visible {
                assert!(!unread.is_unread(), "{label}");
                assert!(state.active_peek_target(10).is_none(), "{label}");
            } else {
                assert_eq!(
                    unread.latest_unread().map(|occurrence| occurrence.reason),
                    Some(expected_reason),
                    "{label}"
                );
                assert_eq!(state.active_peek_target(10), Some(&target), "{label}");
            }

            drop(state);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

#[test]
fn peek_observations_are_causal_across_effect_completion_orderings() {
    for observation_before_completion in [false, true] {
        let root = test_root(if observation_before_completion {
            "peek-observation-before-completion"
        } else {
            "peek-completion-before-observation"
        });
        let (mut state, source, target) = read_peek_test_state(&root);
        assert!(state.begin_peek(10, source.clone(), [target.clone()], 2));
        let source_witness = read_peek_test_witness(10, &source);
        let target_witness = read_peek_test_witness(10, &target);

        if observation_before_completion {
            state.reconcile_peek_leases(std::slice::from_ref(&target_witness), 4);
            assert!(matches!(
                state.peek_leases.get(&10),
                Some(crate::daemon::runtime::PeekLease::Pending {
                    operation_seq: 2,
                    ..
                })
            ));
        }
        let coordinator = test_coordinator(&root, "causal-completion");
        *coordinator.state.lock().unwrap() = Some(state);
        let response = apply_production_mutation(
            &coordinator,
            V2SequencedMutation {
                accepted_seq: 3,
                mutation: V2AcceptedMutation::Internal(V2InternalMutation::SidebarEffectCompleted(
                    SidebarEffectCompletion {
                        original_accepted_seq: 2,
                        event_id: v2_event_id(),
                        snapshot_revision: 7,
                        witness_observation_floor: 5,
                        result: SidebarEffectResult::Succeeded(target.clone()),
                        effect: crate::daemon::runtime::CanonicalSidebarEffect::PeekPane {
                            pane_instance: target.clone(),
                            client_pid: 10,
                            source_pane: source.clone(),
                        },
                    },
                )),
            },
        );
        assert!(matches!(
            response,
            ServerMessage::SnapshotAck {
                accepted_seq: 3,
                ..
            }
        ));

        {
            let mut guard = coordinator.state.lock().unwrap();
            let state = guard.as_mut().unwrap();
            state.reconcile_peek_leases(std::slice::from_ref(&source_witness), 4);
            assert_eq!(state.active_peek_target(10), Some(&target));
            state.reconcile_peek_leases(std::slice::from_ref(&target_witness), 6);
            assert_eq!(state.active_peek_target(10), Some(&target));
            state.reconcile_peek_leases(std::slice::from_ref(&source_witness), 4);
            assert_eq!(state.active_peek_target(10), Some(&target));

            state.reconcile_peek_leases(std::slice::from_ref(&source_witness), 7);
            assert!(state.active_peek_target(10).is_none());
        }
        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn read_peek_stale_advance_is_stayed_and_other_failures_remain_failed() {
    let pane = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 101,
    };
    assert_eq!(
        read_peek_advance_outcome(&SidebarEffectResult::Succeeded(pane.clone())),
        crate::daemon::protocol::v2::PeekAdvanceOutcome::Jumped {
            pane_instance: pane,
        }
    );
    assert_eq!(
        read_peek_advance_outcome(&SidebarEffectResult::NoAvailablePane),
        crate::daemon::protocol::v2::PeekAdvanceOutcome::Stayed
    );
    assert_eq!(
        read_peek_advance_outcome(&SidebarEffectResult::SourceClientMismatch),
        crate::daemon::protocol::v2::PeekAdvanceOutcome::Failed
    );
}

#[test]
fn sidebar_jump_requires_one_eligible_client_for_source_pane() {
    let source = PaneInstance {
        pane_id: "%9".to_string(),
        pane_pid: 909,
    };
    let mut views = crate::daemon::view_hooks::CurrentClientViews::default();
    assert_eq!(unique_eligible_client_pid(&views, &source), Err(0));

    let witness = |client_pid| crate::pane_state::ClientWitness {
        client_pid,
        session_id: format!("${client_pid}"),
        window_id: "@1".to_string(),
        active_pane: source.clone(),
        control_mode: false,
        active_pane_flag: false,
    };
    views
        .reconcile(
            &[witness(10)],
            &BTreeMap::from([("@1".to_string(), vec![source.clone()])]),
        )
        .unwrap();
    assert_eq!(unique_eligible_client_pid(&views, &source), Ok(10));

    views
        .reconcile(
            &[witness(10), witness(20)],
            &BTreeMap::from([("@1".to_string(), vec![source.clone()])]),
        )
        .unwrap();
    assert_eq!(unique_eligible_client_pid(&views, &source), Err(2));
}
