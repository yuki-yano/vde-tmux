use super::super::*;
use super::*;

#[test]
fn peek_protocol_origin_rejects_invalid_identity_candidates_and_bounds() {
    let pane = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 100,
    };
    let invalid = PaneInstance {
        pane_id: "1".to_string(),
        pane_pid: 0,
    };
    let peek_with_zero_client =
        v2_sidebar_command(crate::daemon::protocol::v2::SidebarCommand::PeekPane {
            pane_instance: pane.clone(),
            source_pane: pane.clone(),
            client_pid: 0,
        });
    assert!(matches!(
        validate_v2_origin(&peek_with_zero_client),
        Err(ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        })
    ));

    let peek_with_invalid_pane =
        v2_sidebar_command(crate::daemon::protocol::v2::SidebarCommand::PeekPane {
            pane_instance: invalid.clone(),
            source_pane: pane.clone(),
            client_pid: 10,
        });
    assert!(matches!(
        validate_v2_origin(&peek_with_invalid_pane),
        Err(ServerMessage::Error {
            code: ErrorCode::InvalidPaneInstance,
            ..
        })
    ));

    let read_with_duplicate =
        v2_sidebar_command(crate::daemon::protocol::v2::SidebarCommand::ReadPeek {
            source_pane: pane.clone(),
            client_pid: 10,
            advance_candidates: vec![pane.clone(), pane.clone()],
        });
    assert!(matches!(
        validate_v2_origin(&read_with_duplicate),
        Err(ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        })
    ));

    let read_with_invalid =
        v2_sidebar_command(crate::daemon::protocol::v2::SidebarCommand::ReadPeek {
            source_pane: pane.clone(),
            client_pid: 10,
            advance_candidates: vec![invalid],
        });
    assert!(matches!(
        validate_v2_origin(&read_with_invalid),
        Err(ServerMessage::Error {
            code: ErrorCode::InvalidPaneInstance,
            ..
        })
    ));

    let read_above_bound =
        v2_sidebar_command(crate::daemon::protocol::v2::SidebarCommand::ReadPeek {
            source_pane: pane.clone(),
            client_pid: 10,
            advance_candidates: vec![pane; crate::pane_state::MAX_VIEW_PANES + 1],
        });
    assert!(matches!(
        validate_v2_origin(&read_above_bound),
        Err(ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        })
    ));
}

#[test]
fn v2_requires_hello_and_rejects_v1_before_side_effects() {
    let mut router = V2Router::new(v2_daemon_id(), "server");
    let mut connection = V2ConnectionState::default();
    assert!(matches!(
        router.route(&mut connection, v2_begin()),
        V2Route::Response(ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        })
    ));
    assert!(matches!(
        router.route(&mut connection, ClientMessage::Hello { proto: 1 },),
        V2Route::Response(ServerMessage::Error {
            code: ErrorCode::UnsupportedProtocol,
            ..
        })
    ));
    v2_handshake(&mut router, &mut connection);
    assert!(matches!(
        router.route(
            &mut connection,
            ClientMessage::QueryResolvedSnapshot { proto: 1 },
        ),
        V2Route::Response(ServerMessage::Error {
            code: ErrorCode::UnsupportedProtocol,
            ..
        })
    ));
}

#[test]
fn v2_read_only_query_does_not_consume_accepted_sequence() {
    let mut router = V2Router::new(v2_daemon_id(), "server");
    router.set_phase(DaemonPhase::Serving);
    let mut connection = V2ConnectionState::default();
    v2_handshake(&mut router, &mut connection);
    assert!(matches!(
        router.route(
            &mut connection,
            ClientMessage::QueryResolvedSnapshot {
                proto: PROTOCOL_VERSION,
            },
        ),
        V2Route::Query(_)
    ));
    assert!(matches!(
        router.route(
            &mut connection,
            ClientMessage::QueryRuntimeInfo {
                proto: PROTOCOL_VERSION,
            },
        ),
        V2Route::Query(_)
    ));
    let V2Route::Mutation(mutation) = router.route(&mut connection, v2_begin()) else {
        panic!("expected mutation");
    };
    assert_eq!(mutation.accepted_seq, 1);
}

#[test]
fn v2_internal_and_external_mutations_share_one_accepted_sequence() {
    let mut router = V2Router::new(v2_daemon_id(), "server");
    router.set_phase(DaemonPhase::Serving);
    let mut connection = V2ConnectionState::default();
    v2_handshake(&mut router, &mut connection);
    let V2Route::Mutation(external) = router.route(&mut connection, v2_begin()) else {
        panic!("expected external mutation");
    };
    let V2Route::Mutation(internal) = router.accept_internal(V2InternalMutation::RefreshTopology)
    else {
        panic!("expected internal mutation");
    };
    let V2Route::Mutation(next_external) = router.route(&mut connection, v2_begin()) else {
        panic!("expected external mutation");
    };
    assert_eq!(
        (
            external.accepted_seq,
            internal.accepted_seq,
            next_external.accepted_seq,
        ),
        (1, 2, 3)
    );
    assert!(matches!(
        internal.mutation,
        V2AcceptedMutation::Internal(V2InternalMutation::RefreshTopology)
    ));
}

#[test]
fn v2_serving_with_degraded_hooks_continues_queries_and_canonical_mutations() {
    let mut router = V2Router::new(v2_daemon_id(), "server");
    router.set_phase(DaemonPhase::Serving);
    router.set_hook_health(HookHealth::Degraded);
    let mut connection = V2ConnectionState::default();

    let hello = router.route(
        &mut connection,
        ClientMessage::Hello {
            proto: PROTOCOL_VERSION,
        },
    );
    assert!(matches!(
        hello,
        V2Route::Response(ServerMessage::HelloAck {
            phase: DaemonPhase::Serving,
            hook_health: HookHealth::Degraded,
            ..
        })
    ));
    assert!(matches!(
        router.route(
            &mut connection,
            ClientMessage::QueryResolvedSnapshot {
                proto: PROTOCOL_VERSION,
            },
        ),
        V2Route::Query(ClientMessage::QueryResolvedSnapshot { .. })
    ));
    assert!(matches!(
        router.route(&mut connection, v2_begin()),
        V2Route::Mutation(V2SequencedMutation {
            accepted_seq: 1,
            mutation: V2AcceptedMutation::External(ClientMessage::SubmitPaneEvent { .. }),
        })
    ));
}

#[test]
fn v2_bootstrap_fifo_preserves_order_and_rejects_overflow_without_consuming_seq() {
    let mut router = V2Router::new(v2_daemon_id(), "server");
    router.set_phase(DaemonPhase::Hydrating);
    let mut connection = V2ConnectionState::default();
    v2_handshake(&mut router, &mut connection);
    for expected in 1..=V2_BOOTSTRAP_FIFO_CAPACITY as u64 {
        assert_eq!(
            router.route(&mut connection, v2_begin()),
            V2Route::Queued {
                accepted_seq: expected
            }
        );
    }
    assert!(matches!(
        router.route(&mut connection, v2_begin()),
        V2Route::Response(ServerMessage::Error {
            code: ErrorCode::QueueFull,
            ..
        })
    ));
    assert_eq!(
        router.accept_internal(V2InternalMutation::ReconcileViews),
        V2Route::DroppedInternal
    );
    let mut queued = Vec::new();
    router
        .finish_bootstrap::<()>(|mutations| {
            queued = mutations;
            Ok(())
        })
        .unwrap();
    assert_eq!(queued.len(), V2_BOOTSTRAP_FIFO_CAPACITY);
    assert!(
        queued
            .windows(2)
            .all(|window| window[0].accepted_seq < window[1].accepted_seq)
    );
    let V2Route::Mutation(next) = router.route(&mut connection, v2_begin()) else {
        panic!("expected mutation");
    };
    assert_eq!(next.accepted_seq, 65);
}

#[test]
fn restart_owned_hook_view_event_keeps_fifo_order_during_bootstrap() {
    let mut router = V2Router::new(v2_daemon_id(), "server");
    router.set_phase(DaemonPhase::Hydrating);
    let mut connection = V2ConnectionState::default();
    v2_handshake(&mut router, &mut connection);
    let pane = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 100,
    };
    let owned_hook_event = ClientMessage::SubmitViewEvent {
        proto: PROTOCOL_VERSION,
        event: crate::pane_state::ViewEvent {
            daemon_instance_id: v2_daemon_id(),
            event_id: v2_event_id(),
            hook_kind: crate::pane_state::ViewHookKind::WindowPaneChanged,
            active_pane: Some(pane.clone()),
            window_panes: vec![pane],
            visibility: crate::pane_state::ViewVisibilityProof {
                pane_visible: false,
                window_visible: false,
            },
        },
    };
    assert!(matches!(
        router.route(&mut connection, owned_hook_event),
        V2Route::Response(ServerMessage::ViewQueued {
            accepted_seq: 1,
            ..
        })
    ));
    assert_eq!(
        router.route(&mut connection, v2_begin()),
        V2Route::Queued { accepted_seq: 2 }
    );

    let queued = router.take_bootstrap_fifo();
    assert_eq!(
        queued
            .iter()
            .map(|mutation| mutation.accepted_seq)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(matches!(
        queued[0].mutation,
        V2AcceptedMutation::External(ClientMessage::SubmitViewEvent { .. })
    ));
    assert!(matches!(
        queued[1].mutation,
        V2AcceptedMutation::External(ClientMessage::SubmitPaneEvent { .. })
    ));
}

#[test]
fn v2_bootstrap_failure_keeps_hydrating_and_never_serves_queries() {
    let mut router = V2Router::new(v2_daemon_id(), "server");
    router.begin_hydration().unwrap();
    let mut connection = V2ConnectionState::default();
    v2_handshake(&mut router, &mut connection);
    assert!(matches!(
        router.route(&mut connection, v2_begin()),
        V2Route::Queued { accepted_seq: 1 }
    ));
    let result = router.finish_bootstrap(|queued| {
        assert_eq!(queued.len(), 1);
        Err("initial reconciliation failed")
    });
    assert_eq!(result, Err("initial reconciliation failed"));
    assert_eq!(router.phase(), DaemonPhase::Hydrating);
    assert!(matches!(
        router.route(
            &mut connection,
            ClientMessage::QueryResolvedSnapshot {
                proto: PROTOCOL_VERSION,
            },
        ),
        V2Route::Response(ServerMessage::Error {
            code: ErrorCode::NotReady,
            ..
        })
    ));
}

#[test]
fn v2_rejects_stale_instance_and_internal_event_origins() {
    let mut router = V2Router::new(v2_daemon_id(), "server");
    router.set_phase(DaemonPhase::Serving);
    let mut connection = V2ConnectionState::default();
    v2_handshake(&mut router, &mut connection);
    let mut stale = v2_begin();
    let ClientMessage::SubmitPaneEvent { envelope, .. } = &mut stale else {
        unreachable!();
    };
    envelope.daemon_instance_id =
        DaemonInstanceId::parse("00112233445566778899aabbccddeeff").unwrap();
    assert!(matches!(
        router.route(&mut connection, stale),
        V2Route::Response(ServerMessage::Error {
            code: ErrorCode::StaleDaemonInstance,
            ..
        })
    ));
    let internal_events = [
        PaneEvent::MarkPaneRead { through_order: 1 },
        PaneEvent::ObservationBatch {
            base: None,
            tracker_generation: 0,
            observed_at: 1,
            presence: crate::pane_state::AgentPresenceObservation::Unknown,
            capture: None,
            process: None,
        },
        PaneEvent::PaneRemoved { expected: None },
    ];
    for internal_event in internal_events {
        assert!(matches!(
            router.route(&mut connection, v2_pane_event(internal_event)),
            V2Route::Response(ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));
    }
}

#[test]
fn v2_rejects_invalid_view_before_consuming_accepted_sequence() {
    let mut router = V2Router::new(v2_daemon_id(), "server");
    router.set_phase(DaemonPhase::Serving);
    let mut connection = V2ConnectionState::default();
    v2_handshake(&mut router, &mut connection);
    let pane = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 100,
    };
    let invalid = ClientMessage::SubmitViewEvent {
        proto: PROTOCOL_VERSION,
        event: crate::pane_state::ViewEvent {
            daemon_instance_id: v2_daemon_id(),
            event_id: v2_event_id(),
            hook_kind: crate::pane_state::ViewHookKind::WindowPaneChanged,
            active_pane: Some(pane.clone()),
            window_panes: vec![
                pane,
                PaneInstance {
                    pane_id: "invalid".to_string(),
                    pane_pid: 0,
                },
            ],
            visibility: crate::pane_state::ViewVisibilityProof {
                pane_visible: false,
                window_visible: false,
            },
        },
    };
    assert!(matches!(
        router.route(&mut connection, invalid),
        V2Route::Response(ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        })
    ));
    let detached_pane = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 100,
    };
    let detached_with_occurrence = ClientMessage::SubmitViewEvent {
        proto: PROTOCOL_VERSION,
        event: crate::pane_state::ViewEvent {
            daemon_instance_id: v2_daemon_id(),
            event_id: v2_event_id(),
            hook_kind: crate::pane_state::ViewHookKind::ClientDetached,
            active_pane: Some(detached_pane.clone()),
            window_panes: vec![detached_pane],
            visibility: crate::pane_state::ViewVisibilityProof {
                pane_visible: false,
                window_visible: false,
            },
        },
    };
    assert!(matches!(
        router.route(&mut connection, detached_with_occurrence),
        V2Route::Response(ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        })
    ));
    let V2Route::Mutation(mutation) = router.route(&mut connection, v2_begin()) else {
        panic!("expected mutation after rejected view");
    };
    assert_eq!(mutation.accepted_seq, 1);
}

#[test]
fn v2_accepted_sequence_overflow_is_internal_error() {
    let mut router = V2Router::new(v2_daemon_id(), "server");
    router.set_phase(DaemonPhase::Serving);
    router.set_next_accepted_seq(u64::MAX);
    let mut connection = V2ConnectionState::default();
    v2_handshake(&mut router, &mut connection);
    assert!(matches!(
        router.route(&mut connection, v2_begin()),
        V2Route::Fatal(ServerMessage::Error {
            code: ErrorCode::InternalError,
            ..
        })
    ));
    assert!(router.is_fatal());
}
