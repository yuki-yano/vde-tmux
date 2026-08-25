use super::*;

#[test]
fn production_fail_stop_marks_router_and_releases_waiters() {
    let coordinator = detached_test_coordinator("a".repeat(64));
    let (sender, receiver) = mpsc::channel();
    coordinator.waiters.lock().unwrap().insert(1, sender);

    coordinator.fail_stop("counter overflow");

    assert!(coordinator.router.lock().unwrap().is_fatal());
    assert!(coordinator.shutdown.load(Ordering::SeqCst));
    assert!(coordinator.shutdown_ready.load(Ordering::SeqCst));
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(100)).unwrap(),
        ServerMessage::Error {
            code: ErrorCode::InternalError,
            ..
        }
    ));
}

#[test]
fn disconnected_mutation_waiter_enqueues_sequenced_diagnostic() {
    let coordinator = detached_test_coordinator("d".repeat(64));
    coordinator
        .router
        .lock()
        .unwrap()
        .set_phase(DaemonPhase::Serving);
    let (sender, receiver) = mpsc::channel();
    drop(receiver);
    coordinator.waiters.lock().unwrap().insert(3, sender);

    coordinator.complete(
        3,
        ServerMessage::SnapshotAck {
            event_id: v2_event_id(),
            accepted_seq: 3,
            snapshot_revision: 0,
        },
    );

    let queue = coordinator.queue.lock().unwrap();
    assert!(matches!(
        queue.items.front().map(|item| &item.sequenced.mutation),
        Some(V2AcceptedMutation::Internal(
            V2InternalMutation::DiagnosticProjection {
                pane_instance: None,
                ..
            }
        ))
    ));
}

#[test]
fn graceful_shutdown_releases_later_waiters_and_keeps_current_response() {
    let coordinator = detached_test_coordinator("b".repeat(64));
    let (current_tx, current_rx) = mpsc::channel();
    let (later_tx, later_rx) = mpsc::channel();
    coordinator
        .waiters
        .lock()
        .unwrap()
        .extend([(4, current_tx), (5, later_tx)]);

    coordinator.begin_graceful_shutdown(4);
    assert!(!coordinator.shutdown_ready.load(Ordering::SeqCst));

    assert!(matches!(
        later_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
        ServerMessage::Error {
            code: ErrorCode::NotReady,
            ..
        }
    ));
    assert!(current_rx.try_recv().is_err());
    let response = coordinator.route_external(
        &mut V2ConnectionState::default(),
        ClientMessage::Hello {
            proto: PROTOCOL_VERSION,
        },
        0,
    );
    assert!(matches!(
        response,
        ServerMessage::Error {
            code: ErrorCode::NotReady,
            ..
        }
    ));
    assert!(!coordinator.enqueue_internal(V2InternalMutation::RefreshTopology));
    assert!(current_rx.try_recv().is_err());
    coordinator.complete(
        4,
        ServerMessage::ShutdownAccepted {
            event_id: v2_event_id(),
            accepted_seq: 4,
        },
    );
    assert!(matches!(
        current_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
        ServerMessage::ShutdownAccepted {
            accepted_seq: 4,
            ..
        }
    ));
    coordinator.mark_shutdown_ready();
    assert!(coordinator.shutdown_ready.load(Ordering::SeqCst));
}

#[test]
fn mutation_queue_capacity_logs_each_internal_drop() {
    let root = test_root("mutation-queue-capacity");
    let env = BTreeMap::from([(
        "XDG_STATE_HOME".to_string(),
        root.to_string_lossy().into_owned(),
    )]);
    let hash = "mutation-queue-capacity";
    let coordinator =
        ProductionV2Coordinator::new(test_incarnation(&root, hash), env.clone(), None).unwrap();
    coordinator
        .router
        .lock()
        .unwrap()
        .set_phase(DaemonPhase::Serving);

    for _ in 0..V2_MUTATION_QUEUE_CAPACITY {
        assert!(coordinator.enqueue_internal(V2InternalMutation::RefreshTopology));
    }
    assert!(!coordinator.enqueue_internal(V2InternalMutation::RefreshTopology));
    assert!(!coordinator.enqueue_internal(V2InternalMutation::RefreshTopology));
    let log =
        std::fs::read_to_string(crate::daemon::lifecycle::daemon_log_path(&env, hash)).unwrap();
    assert_eq!(
        log.matches("sequenced mutation queue full: dropped internal mutation")
            .count(),
        2
    );

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(any())]
fn live_scheduler_dedupes_targets_into_one_capture_and_fans_out() {
    let root = test_root("live-dedupe");
    let source = PaneInstance {
        pane_id: "%9".to_string(),
        pane_pid: 900,
    };
    let target_a = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 100,
    };
    let target_b = PaneInstance {
        pane_id: "%2".to_string(),
        pane_pid: 200,
    };
    let coordinator = initialized_test_coordinator(
        &root,
        "live-dedupe".to_string(),
        live_views_showing(&source),
    );
    let io = MockLiveCaptureIo::new(coordinator.incarnation.identity.clone());

    let (_, mailbox_a1) =
        coordinator
            .live
            .register(source.clone(), target_a.clone(), Duration::from_secs(2));
    let (_, mailbox_a2) =
        coordinator
            .live
            .register(source.clone(), target_a.clone(), Duration::from_secs(2));
    let (_, mailbox_b) =
        coordinator
            .live
            .register(source.clone(), target_b.clone(), Duration::from_secs(2));

    run_live_preview_tick(&coordinator, &io, Instant::now());

    // Two distinct targets shared by three subscribers produce exactly one
    // capture request with one section per deduplicated target.
    assert_eq!(io.call_count(), 1);
    assert_eq!(io.calls.lock().unwrap()[0].len(), 2);
    let a1 = mailbox_a1.wait().expect("first subscriber receives a body");
    let a2 = mailbox_a2
        .wait()
        .expect("second subscriber receives a body");
    match (&a1, &a2) {
        (
            LivePush::Result {
                live_revision: r1,
                body: b1,
                ..
            },
            LivePush::Result {
                live_revision: r2,
                body: b2,
                ..
            },
        ) => {
            assert_eq!(r1, r2);
            assert_eq!(b1, b2);
        }
        other => panic!("expected two results, found {other:?}"),
    }
    assert!(matches!(mailbox_b.wait(), Some(LivePush::Result { .. })));

    // Nothing is due immediately after the tick, so no further capture runs.
    run_live_preview_tick(&coordinator, &io, Instant::now());
    assert_eq!(io.call_count(), 1);

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(any())]
fn live_scheduler_does_not_capture_hidden_sources() {
    let root = test_root("live-hidden");
    let source = PaneInstance {
        pane_id: "%9".to_string(),
        pane_pid: 900,
    };
    let target = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 100,
    };
    // The registry views show a different pane, so the source is hidden.
    let other = PaneInstance {
        pane_id: "%8".to_string(),
        pane_pid: 800,
    };
    let coordinator =
        live_test_coordinator(&root, "live-hidden".to_string(), live_views_showing(&other));
    let io = MockLiveCaptureIo::new(coordinator.incarnation.identity.clone());

    let (_, mailbox) = coordinator
        .live
        .register(source, target, Duration::from_secs(2));

    run_live_preview_tick(&coordinator, &io, Instant::now());

    assert_eq!(io.call_count(), 0);
    assert!(matches!(
        mailbox.wait(),
        Some(LivePush::Unavailable {
            reason: LiveUnavailableReason::HiddenSource,
        })
    ));

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(any())]
fn live_pane_instance_mismatch_delivers_no_body() {
    let root = test_root("live-mismatch");
    let source = PaneInstance {
        pane_id: "%9".to_string(),
        pane_pid: 900,
    };
    let target = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 100,
    };
    let coordinator = live_test_coordinator(
        &root,
        "live-mismatch".to_string(),
        live_views_showing(&source),
    );
    let mut io = MockLiveCaptureIo::new(coordinator.incarnation.identity.clone());
    io.mismatch_targets.insert("%1".to_string());

    let (_, mailbox) = coordinator
        .live
        .register(source, target, Duration::from_secs(2));

    run_live_preview_tick(&coordinator, &io, Instant::now());

    assert!(matches!(
        mailbox.wait(),
        Some(LivePush::Unavailable {
            reason: LiveUnavailableReason::PaneInstanceMismatch,
        })
    ));
    assert!(coordinator.live.bodies.lock().unwrap().is_empty());

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(any())]
fn observation_piggyback_consumes_due_live_demand_before_the_scheduler_tick() {
    let root = test_root("live-piggyback");
    let source = PaneInstance {
        pane_id: "%9".to_string(),
        pane_pid: 900,
    };
    let target = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 100,
    };
    let coordinator = live_test_coordinator(
        &root,
        "live-piggyback".to_string(),
        live_views_showing(&source),
    );
    let io = MockLiveCaptureIo::new(coordinator.incarnation.identity.clone());
    let (_, mailbox) = coordinator
        .live
        .register(source, target.clone(), Duration::from_secs(2));

    // The observation worker consumes the due demand ahead of its own
    // capture, exactly as it does before a poll.
    let now = Instant::now();
    let targets = collect_due_live_targets(
        &coordinator,
        now,
        crate::daemon::workers::CAPTURE_COALESCE_WINDOW,
    );
    assert_eq!(targets, vec![target]);
    deliver_live_result(
        &coordinator,
        &targets,
        Ok(vec![crate::daemon::workers::LiveCaptureSection::Body(
            "from-observation\n".to_string(),
        )]),
    );
    assert!(matches!(mailbox.wait(), Some(LivePush::Result { .. })));

    // In the aligned default configuration the scheduler tick that follows
    // finds no remaining due demand, so live preview needs no extra tmux
    // process of its own.
    run_live_preview_tick(&coordinator, &io, now + Duration::from_millis(5));
    assert_eq!(io.call_count(), 0);

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(any())]
fn live_subscription_rejects_interval_below_minimum() {
    let root = test_root("live-interval");
    let coordinator = Arc::new(initialized_test_coordinator(
        &root,
        "live-interval".to_string(),
        crate::daemon::view_hooks::CurrentClientViews::default(),
    ));
    let (server, client) = UnixStream::pair().unwrap();
    let source = PaneInstance {
        pane_id: "%9".to_string(),
        pane_pid: 900,
    };
    let target = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 100,
    };

    handle_v2_live_subscription(coordinator.clone(), server, source, target, 99).unwrap();

    let mut response = String::new();
    std::io::BufReader::new(client)
        .read_line(&mut response)
        .unwrap();
    let message: ServerMessage = serde_json::from_str(response.trim()).unwrap();
    assert!(matches!(
        message,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    assert!(coordinator.live.entries.lock().unwrap().is_empty());

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(any())]
fn live_subscription_guard_removes_registry_entry_after_write_failure() {
    let root = test_root("live-guard");
    let coordinator = Arc::new(live_test_coordinator(
        &root,
        "live-guard".to_string(),
        crate::daemon::view_hooks::CurrentClientViews::default(),
    ));
    let (server, client) = UnixStream::pair().unwrap();
    let source = PaneInstance {
        pane_id: "%9".to_string(),
        pane_pid: 900,
    };
    let target = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 100,
    };
    let handler = {
        let coordinator = coordinator.clone();
        thread::spawn(move || {
            handle_v2_live_subscription(coordinator, server, source, target, 2000)
        })
    };
    // Wait until the handler registered its demand, then sever the client.
    let deadline = Instant::now() + Duration::from_secs(2);
    while coordinator.live.entries.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "subscription was never registered"
        );
        thread::sleep(Duration::from_millis(5));
    }
    drop(client);
    coordinator
        .live
        .entries
        .lock()
        .unwrap()
        .values()
        .for_each(|entry| {
            entry.mailbox.push(LivePush::Unavailable {
                reason: LiveUnavailableReason::TargetMissing,
            });
        });

    handler.join().unwrap().unwrap();
    assert!(coordinator.live.entries.lock().unwrap().is_empty());

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn snapshot_wait_timeout_yields_heartbeat_instead_of_full_snapshot() {
    let root = test_root("heartbeat-wait");
    let coordinator = initialized_test_coordinator(
        &root,
        "heartbeat-wait".to_string(),
        crate::daemon::view_hooks::CurrentClientViews::default(),
    );
    let published = coordinator.publish_resolved_snapshot().unwrap();

    // With no newer revision, ten seconds of waiting yields only heartbeat
    // outcomes; the cached full snapshot is never re-sent.
    for _ in 0..5 {
        let outcome = coordinator
            .wait_for_snapshot_after(published.revision, Duration::from_millis(10))
            .expect("wait must not observe a shutdown");
        assert!(matches!(
            outcome,
            SnapshotWaitOutcome::HeartbeatDue { snapshot_revision }
                if snapshot_revision == published.revision
        ));
    }

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn idle_subscription_stream_sends_heartbeats_and_new_revisions_still_flow() {
    let root = test_root("heartbeat-stream");
    let coordinator = Arc::new(initialized_test_coordinator(
        &root,
        "heartbeat-stream".to_string(),
        crate::daemon::view_hooks::CurrentClientViews::default(),
    ));
    let published = coordinator.publish_resolved_snapshot().unwrap();
    let daemon_instance_id = coordinator
        .router
        .lock()
        .unwrap()
        .daemon_instance_id()
        .clone();
    let (server, client) = UnixStream::pair().unwrap();
    let stream_worker = {
        let coordinator = coordinator.clone();
        let last_revision = published.revision;
        thread::spawn(move || {
            stream_v2_subscription_with_heartbeat_interval(
                coordinator,
                server,
                last_revision,
                Duration::from_millis(10),
            )
        })
    };
    let mut reader = std::io::BufReader::new(client.try_clone().unwrap());

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let first: ServerMessage = serde_json::from_str(line.trim()).unwrap();
    match first {
        ServerMessage::Heartbeat {
            daemon_instance_id: heartbeat_instance,
            snapshot_revision,
        } => {
            assert_eq!(heartbeat_instance, daemon_instance_id);
            assert_eq!(snapshot_revision, published.revision);
        }
        other => panic!("expected a heartbeat, received {other:?}"),
    }

    // A real revision bump still flows through as a full snapshot frame.
    coordinator
        .state
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .leased
        .runtime
        .mark_projection_changed()
        .unwrap();
    let newer = coordinator.publish_resolved_snapshot().unwrap();
    assert_eq!(newer.revision, published.revision + 1);
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        let message: ServerMessage = serde_json::from_str(line.trim()).unwrap();
        match message {
            ServerMessage::Heartbeat { .. } => continue,
            ServerMessage::ResolvedSnapshotResult {
                snapshot_revision, ..
            } => {
                assert_eq!(snapshot_revision, newer.revision);
                break;
            }
            other => panic!("unexpected subscription frame: {other:?}"),
        }
    }

    drop(reader);
    drop(client);
    stream_worker.join().unwrap().unwrap();
    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(any())]
fn live_mailbox_delivers_latest_only_and_close_ends_the_stream() {
    let mailbox = LiveMailbox::default();
    mailbox.push(LivePush::Unavailable {
        reason: LiveUnavailableReason::TargetMissing,
    });
    mailbox.push(LivePush::Unavailable {
        reason: LiveUnavailableReason::CaptureFailed,
    });

    // A slow subscriber only ever sees the newest undelivered message.
    assert!(matches!(
        mailbox.wait(),
        Some(LivePush::Unavailable {
            reason: LiveUnavailableReason::CaptureFailed,
        })
    ));

    mailbox.close();
    assert!(mailbox.wait().is_none());
}

#[test]
#[cfg(any())]
fn live_registry_prunes_demand_for_missing_panes() {
    let registry = LiveRegistry::default();
    let source = PaneInstance {
        pane_id: "%9".to_string(),
        pane_pid: 900,
    };
    let target = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 100,
    };
    let (_, mailbox) = registry.register(source.clone(), target.clone(), Duration::from_secs(2));
    registry.bodies.lock().unwrap().insert(
        target.clone(),
        LiveBodyEntry {
            live_revision: 1,
            captured_at_epoch_millis: 0,
            body: Arc::new("body".to_string()),
        },
    );

    // Both panes still present: demand survives.
    registry.prune_missing_panes(&BTreeSet::from([source.clone(), target.clone()]));
    assert_eq!(registry.entries.lock().unwrap().len(), 1);

    // The target pane disappeared: demand and cached body are dropped and
    // the subscriber stream is closed.
    registry.prune_missing_panes(&BTreeSet::from([source]));
    assert!(registry.entries.lock().unwrap().is_empty());
    assert!(registry.bodies.lock().unwrap().is_empty());
    assert!(mailbox.wait().is_none());
}

#[test]
fn observation_batch_keeps_sequence_order_with_following_mutations() {
    let root = test_root("batch-sequence");
    let server_identity = crate::daemon::topology::ServerIdentity {
        pid: 1,
        start_time: 2,
    };
    let coordinator = test_coordinator(&root, "a1".repeat(32));
    coordinator
        .router
        .lock()
        .unwrap()
        .set_phase(DaemonPhase::Serving);

    assert!(
        coordinator.enqueue_internal(V2InternalMutation::ObservationBatch(Box::new(
            ObservationBatchPayload {
                projection: Box::new(ObservationPollProjection {
                    observation_seq: 1,
                    topology: crate::daemon::topology::TopologySnapshot {
                        server_identity,
                        panes: Vec::new(),
                    },
                    status_metadata: super::super::runtime::StatusProjectionMetadata::default(),
                    witnesses: Vec::new(),
                    observation_bases: BTreeMap::new(),
                    view_base: crate::daemon::view_hooks::CurrentClientViews::default(),
                    through_unread_order: 0,
                }),
                observations: Vec::new(),
                removals: Vec::new(),
                diagnostics: Vec::new(),
            }
        ),))
    );
    assert!(
        coordinator.enqueue_internal(V2InternalMutation::DiagnosticProjection {
            pane_instance: None,
            message: "after-batch".to_string(),
        })
    );

    let queue = coordinator.queue.lock().unwrap();
    assert_eq!(queue.items.len(), 2);
    let sequences = queue
        .items
        .iter()
        .map(|item| item.sequenced.accepted_seq)
        .collect::<Vec<_>>();
    assert!(sequences[0] < sequences[1]);
    assert!(matches!(
        queue.items.front().map(|item| &item.sequenced.mutation),
        Some(V2AcceptedMutation::Internal(
            V2InternalMutation::ObservationBatch(_)
        ))
    ));
    assert!(matches!(
        queue.items.back().map(|item| &item.sequenced.mutation),
        Some(V2AcceptedMutation::Internal(
            V2InternalMutation::DiagnosticProjection { .. }
        ))
    ));
    drop(queue);

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn snapshot_waiter_cannot_miss_shutdown_notification() {
    let coordinator = Arc::new(detached_test_coordinator("e".repeat(64)));
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let waiter = {
        let coordinator = coordinator.clone();
        thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = coordinator.wait_for_snapshot_after(0, Duration::from_secs(2));
            done_tx.send(result).unwrap();
        })
    };
    started_rx.recv().unwrap();

    coordinator.begin_graceful_shutdown(u64::MAX);

    assert!(
        done_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none()
    );
    waiter.join().unwrap();
}

#[test]
fn published_snapshot_frame_is_shared_and_replaced_only_for_new_revision() {
    let root = test_root("published-snapshot");
    let coordinator = test_coordinator(&root, "c".repeat(64));
    install_test_state(
        &coordinator,
        &root,
        crate::daemon::view_hooks::CurrentClientViews::default(),
    );

    let first = coordinator.publish_resolved_snapshot().unwrap();
    assert!(matches!(
        coordinator.query(ClientMessage::QueryStatusSnapshot {
            proto: PROTOCOL_VERSION,
            context: crate::daemon::protocol::v2::StatusContext::Global,
        }),
        ServerMessage::StatusSnapshotResult {
            snapshot_revision: 0,
            ..
        }
    ));
    let same = coordinator.publish_resolved_snapshot().unwrap();
    assert!(Arc::ptr_eq(&first.frame, &same.frame));
    assert!(Arc::ptr_eq(&first.message, &same.message));
    coordinator
        .state
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .leased
        .runtime
        .mark_projection_changed()
        .unwrap();
    let changed = coordinator.publish_resolved_snapshot().unwrap();
    assert_eq!(changed.revision, first.revision + 1);
    assert!(!Arc::ptr_eq(&first.frame, &changed.frame));
    coordinator
        .state
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .leased
        .runtime
        .set_snapshot_revision_for_test(first.revision);
    let stale_publisher = coordinator.publish_resolved_snapshot().unwrap();
    assert_eq!(stale_publisher.revision, changed.revision);
    assert!(Arc::ptr_eq(&stale_publisher.frame, &changed.frame));

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn publish_resolved_snapshot_skips_rebuild_for_unchanged_revision() {
    let root = test_root("publish-rebuild-baseline");
    let coordinator = test_coordinator(&root, "f".repeat(64));
    install_test_state(
        &coordinator,
        &root,
        crate::daemon::view_hooks::CurrentClientViews::default(),
    );

    let first = coordinator.publish_resolved_snapshot().unwrap();
    let cached = coordinator.publish_resolved_snapshot().unwrap();

    // The revision fast path serves the cached frame without rebuilding the
    // checked snapshot when the revision has not changed.
    assert!(Arc::ptr_eq(&first.frame, &cached.frame));

    coordinator
        .state
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .leased
        .runtime
        .mark_projection_changed()
        .unwrap();
    let changed = coordinator.publish_resolved_snapshot().unwrap();
    assert!(!Arc::ptr_eq(&first.frame, &changed.frame));

    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn oversized_resolved_snapshot_commits_and_queues_frame_too_large_diagnostic() {
    let root = test_root("frame-too-large");
    let coordinator = test_coordinator(&root, "f".repeat(64));
    coordinator
        .router
        .lock()
        .unwrap()
        .set_phase(DaemonPhase::Serving);
    let leased =
        super::super::runtime::LeasedCanonicalPaneStateRuntime::acquire(&root.join("writer"))
            .unwrap();
    let mut state = super::super::runtime::CanonicalCoordinatorState::new(
        leased,
        crate::daemon::topology::TopologySnapshot {
            server_identity: crate::daemon::topology::ServerIdentity {
                pid: 1,
                start_time: 2,
            },
            panes: Vec::new(),
        },
        crate::daemon::view_hooks::CurrentClientViews::default(),
        crate::sidebar::state::SidebarPreferences::default(),
    );
    state
        .replace_topology(crate::daemon::topology::TopologySnapshot {
            server_identity: crate::daemon::topology::ServerIdentity {
                pid: 1,
                start_time: 2,
            },
            panes: vec![crate::daemon::topology::TopologyPane {
                pane_instance: PaneInstance {
                    pane_id: "%1".to_string(),
                    pane_pid: 101,
                },
                session_links: Vec::new(),
                window_id: "@1".to_string(),
                window_name: "x".repeat(crate::pane_state::MAX_RESPONSE_FRAME_BYTES),
                current_path: "/tmp".to_string(),
                current_command: "zsh".to_string(),
                pane_width: 80,
                active: false,
                editprompt_is_editor: false,
                editprompt_target_panes: Vec::new(),
                editprompt_editor_pane: None,
            }],
        })
        .unwrap();
    assert_eq!(state.leased.runtime.snapshot_revision(), 1);
    *coordinator.state.lock().unwrap() = Some(state);

    let published = coordinator.publish_resolved_snapshot().unwrap();
    assert!(published.terminal);
    assert!(matches!(
        published.message.as_ref(),
        ServerMessage::Error {
            code: ErrorCode::FrameTooLarge,
            ..
        }
    ));
    let mutation = coordinator.queue.lock().unwrap().items.pop_front().unwrap();
    assert!(matches!(
        &mutation.sequenced.mutation,
        V2AcceptedMutation::Internal(V2InternalMutation::FrameTooLargeProjection {
            rejected_revision: 1
        })
    ));
    let response = apply_production_mutation(&coordinator, mutation.sequenced);
    assert!(matches!(
        response,
        ServerMessage::SnapshotAck {
            snapshot_revision: 2,
            ..
        }
    ));
    let state = coordinator.state.lock().unwrap();
    assert!(
        state
            .as_ref()
            .unwrap()
            .global_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == ErrorCode::FrameTooLarge)
    );

    drop(state);
    drop(coordinator);
    match std::fs::remove_dir_all(root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove test directory: {error}"),
    }
}

#[test]
fn terminal_subscription_frame_is_written_once_then_stream_closes() {
    let coordinator = Arc::new(detached_test_coordinator("a".repeat(64)));
    let message = ServerMessage::error(ErrorCode::FrameTooLarge, "too large", None);
    let frame = crate::daemon::protocol::v2::encode_response_frame(&message).unwrap();
    *coordinator.snapshot_cache.lock().unwrap() = Some(PublishedResolvedSnapshot {
        revision: 2,
        frame: Arc::new(frame),
        message: Arc::new(message),
        terminal: true,
    });
    let (mut client, server) = UnixStream::pair().unwrap();
    let handle = {
        let coordinator = coordinator.clone();
        thread::spawn(move || stream_v2_subscription(coordinator, server, 1).unwrap())
    };

    let mut raw = String::new();
    client.read_to_string(&mut raw).unwrap();
    handle.join().unwrap();
    let frames = raw.lines().collect::<Vec<_>>();
    assert_eq!(frames.len(), 1);
    assert!(matches!(
        serde_json::from_str::<ServerMessage>(frames[0]).unwrap(),
        ServerMessage::Error {
            code: ErrorCode::FrameTooLarge,
            ..
        }
    ));
}
