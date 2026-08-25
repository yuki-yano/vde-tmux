use super::super::*;
use super::*;

#[test]
fn nvim_marker_parser_ignores_empty_and_malformed_values() {
    let output = "%6\u{1f}94451\u{1f}68736\n%8\u{1f}95025\u{1f}\ninvalid\n";
    assert_eq!(
        parse_nvim_pane_markers(output),
        vec![NvimPaneMarker {
            pane_id: "%6".to_string(),
            pane_pid: 94451,
            process_pid: 68736,
        }]
    );
}

#[test]
fn nvim_marker_cleanup_is_guarded_by_server_pane_and_marker_identity() {
    let marker = NvimPaneMarker {
        pane_id: "%6".to_string(),
        pane_pid: 94451,
        process_pid: 68736,
    };
    let command = stale_nvim_marker_cleanup_command(74133, &[marker]).unwrap();
    assert!(command.contains("#{==:#{pid},74133}"));
    assert!(command.contains("#{==:#{pane_pid},94451}"));
    assert!(command.contains("#{==:#{@vde_nvim_process_pid},68736}"));
    assert!(command.contains("set-option"));
    assert!(command.contains("-pu"));
    assert!(command.contains("%6"));
    assert!(command.contains(NVIM_PROCESS_PID_OPTION));
    assert!(stale_nvim_marker_cleanup_command(74133, &[]).is_none());
}

#[test]
fn canonical_notification_worker_exports_blocked_environment() {
    let root = test_root("notification-worker");
    let output = root.join("env.txt");
    let command = format!(
        "printf '%s|%s|%s' \"$VDE_PANE_ID\" \"$VDE_AGENT\" \"$VDE_BADGE_STATE\" > '{}'",
        output.display()
    );
    let sender = start_notification_worker(command);
    sender
        .try_send(NotificationWorkerJob {
            pane_id: "%7".to_string(),
            agent: "codex".to_string(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !std::fs::read_to_string(&output).is_ok_and(|contents| contents == "%7|codex|Blocked")
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::read_to_string(&output).unwrap(),
        "%7|codex|Blocked"
    );
    drop(sender);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn canonical_notification_timeout_kills_descendant_processes() {
    let root = test_root("notification-timeout");
    let pid_file = root.join("child.pid");
    let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
    let sender =
        start_notification_worker_with_timeout_and_log(command, Duration::from_millis(100), None);
    sender
        .try_send(NotificationWorkerJob {
            pane_id: "%7".to_string(),
            agent: "codex".to_string(),
        })
        .unwrap();
    let file_deadline = Instant::now() + Duration::from_secs(1);
    let pid = loop {
        if let Ok(contents) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = contents.trim().parse::<i32>()
        {
            break pid;
        }
        assert!(
            Instant::now() < file_deadline,
            "notification descendant PID was not written"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let exit_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let exists = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !exists {
            break;
        }
        assert!(
            Instant::now() < exit_deadline,
            "notification descendant survived timeout"
        );
        thread::sleep(Duration::from_millis(10));
    }
    drop(sender);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn canonical_notification_successful_leader_exit_kills_background_descendants() {
    let root = test_root("notification-background");
    let pid_file = root.join("child.pid");
    let command = format!("sleep 30 & echo $! > '{}'", pid_file.display());
    let sender =
        start_notification_worker_with_timeout_and_log(command, Duration::from_secs(2), None);
    sender
        .try_send(NotificationWorkerJob {
            pane_id: "%7".to_string(),
            agent: "codex".to_string(),
        })
        .unwrap();
    let file_deadline = Instant::now() + Duration::from_secs(1);
    let pid = loop {
        if let Ok(contents) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = contents.trim().parse::<u32>()
        {
            break pid;
        }
        assert!(
            Instant::now() < file_deadline,
            "notification descendant PID was not written"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let exit_deadline = Instant::now() + Duration::from_secs(1);
    while crate::daemon::lifecycle::process_start_token(pid).is_ok() {
        assert!(
            Instant::now() < exit_deadline,
            "notification descendant survived successful leader exit"
        );
        thread::sleep(Duration::from_millis(10));
    }
    drop(sender);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn canonical_notification_failure_is_written_to_private_incarnation_log() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = test_root("notification-log");
    let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
    let hash = "c".repeat(64);
    let sender = start_notification_worker_with_timeout_and_log(
        "exit 7".to_string(),
        Duration::from_secs(1),
        Some((env.clone(), hash.clone())),
    );
    sender
        .try_send(NotificationWorkerJob {
            pane_id: "%7".to_string(),
            agent: "codex".to_string(),
        })
        .unwrap();
    let path = crate::daemon::lifecycle::daemon_log_path(&env, &hash);
    let deadline = Instant::now() + Duration::from_secs(1);
    let contents = loop {
        if let Ok(contents) = std::fs::read_to_string(&path)
            && contents.contains("notification:")
        {
            break contents;
        }
        assert!(
            Instant::now() < deadline,
            "notification failure was not written"
        );
        thread::sleep(Duration::from_millis(10));
    };

    assert!(contents.contains("notification:"));
    assert!(contents.contains("exited with status"));
    assert!(contents.contains("pane %7"));
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let clear_deadline = Instant::now() + Duration::from_secs(1);
    while crate::daemon::lifecycle::read_lifecycle_record(&env, &hash)
        .is_ok_and(|record| record.active_notification.is_some())
    {
        assert!(
            Instant::now() < clear_deadline,
            "notification identity was not cleared"
        );
        thread::sleep(Duration::from_millis(10));
    }
    drop(sender);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn status_push_failure_uses_the_shared_daemon_log() {
    let root = test_root("status-push-log");
    let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
    let hash = "d".repeat(64);
    let coordinator =
        ProductionV2Coordinator::new(test_incarnation(&root, hash.clone()), env.clone(), None)
            .unwrap();

    coordinator.log_status_push_error("test failure");

    let contents =
        std::fs::read_to_string(crate::daemon::lifecycle::daemon_log_path(&env, &hash)).unwrap();
    assert!(contents.contains("status_push: test failure"));
    for dedicated in ["notification.log", "status-push.log", "pane-state-hook.log"] {
        assert!(!root.join("vde-tmux").join(&hash).join(dedicated).exists());
    }
    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn tmux_server_liveness_monitor_fail_stops_after_server_process_exits() {
    let root = test_root("tmux-server-liveness");
    let mut server = Command::new("sleep").arg("30").spawn().unwrap();
    let server_pid = server.id();
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation {
        socket_path: root.join("tmux.sock"),
        identity: crate::daemon::topology::ServerIdentity {
            pid: server_pid,
            start_time: 1,
        },
        hash: "b".repeat(64),
    };
    let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
    let coordinator = Arc::new(ProductionV2Coordinator::new(incarnation, env, None).unwrap());
    start_tmux_server_liveness_monitor(coordinator.clone()).unwrap();

    server.kill().unwrap();
    server.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !coordinator.shutdown.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "liveness monitor did not stop the daemon"
        );
        thread::sleep(Duration::from_millis(10));
    }

    assert!(coordinator.shutdown_ready.load(Ordering::SeqCst));
    let cleanup_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match std::fs::remove_dir_all(&root) {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                assert!(
                    Instant::now() < cleanup_deadline,
                    "liveness monitor did not finish writing its fail-stop log"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("failed to remove test state: {error}"),
        }
    }
}

#[test]
fn sidebar_worker_completion_reenters_the_shared_sequence_after_external_command() {
    let mut router = V2Router::new(v2_daemon_id(), "server");
    router.set_phase(DaemonPhase::Serving);
    let mut connection = V2ConnectionState::default();
    v2_handshake(&mut router, &mut connection);
    let command = ClientMessage::SidebarCommand {
        proto: PROTOCOL_VERSION,
        daemon_instance_id: v2_daemon_id(),
        event_id: v2_event_id(),
        command: crate::daemon::protocol::v2::SidebarCommand::JumpPane {
            pane_instance: PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 101,
            },
            source_pane: PaneInstance {
                pane_id: "%9".to_string(),
                pane_pid: 909,
            },
        },
    };
    let V2Route::Mutation(external) = router.route(&mut connection, command) else {
        panic!("expected sidebar mutation");
    };
    let completion = SidebarEffectCompletion {
        original_accepted_seq: external.accepted_seq,
        event_id: v2_event_id(),
        snapshot_revision: 7,
        witness_observation_floor: 0,
        result: SidebarEffectResult::Succeeded(PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        }),
        effect: crate::daemon::runtime::CanonicalSidebarEffect::JumpPane {
            pane_instance: PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 101,
            },
            client_pid: 10,
            source_pane: PaneInstance {
                pane_id: "%9".to_string(),
                pane_pid: 909,
            },
        },
    };
    let V2Route::Mutation(internal) =
        router.accept_internal(V2InternalMutation::SidebarEffectCompleted(completion))
    else {
        panic!("expected sequenced sidebar completion");
    };

    assert_eq!(external.accepted_seq, 1);
    assert_eq!(internal.accepted_seq, 2);
    assert!(matches!(
        internal.mutation,
        V2AcceptedMutation::Internal(V2InternalMutation::SidebarEffectCompleted(
            SidebarEffectCompletion {
                original_accepted_seq: 1,
                ..
            }
        ))
    ));
}

#[test]
fn sidebar_dispatch_returns_before_worker_completion_and_releases_original_waiter_after_event() {
    let (job_tx, job_rx) = mpsc::sync_channel(1);
    let deferred = Mutex::new(BTreeSet::new());
    enqueue_sidebar_tmux_job(
        &job_tx,
        &deferred,
        SidebarTmuxJob {
            effect: crate::daemon::runtime::CanonicalSidebarEffect::JumpPane {
                pane_instance: PaneInstance {
                    pane_id: "%1".to_string(),
                    pane_pid: 101,
                },
                client_pid: 10,
                source_pane: PaneInstance {
                    pane_id: "%9".to_string(),
                    pane_pid: 909,
                },
            },
            original_accepted_seq: 1,
            event_id: v2_event_id(),
            snapshot_revision: 7,
        },
    )
    .unwrap();
    assert!(deferred.lock().unwrap().contains(&1));
    let pending = job_rx.try_recv().expect("job is queued without waiting");

    let coordinator = detached_test_coordinator("0".repeat(64));
    coordinator
        .deferred_responses
        .lock()
        .unwrap()
        .insert(pending.original_accepted_seq);
    let (waiter_tx, waiter_rx) = mpsc::channel();
    coordinator.waiters.lock().unwrap().insert(1, waiter_tx);
    assert!(waiter_rx.try_recv().is_err());

    let internal_response = apply_production_mutation(
        &coordinator,
        V2SequencedMutation {
            accepted_seq: 2,
            mutation: V2AcceptedMutation::Internal(V2InternalMutation::SidebarEffectCompleted(
                SidebarEffectCompletion {
                    original_accepted_seq: pending.original_accepted_seq,
                    event_id: pending.event_id,
                    snapshot_revision: pending.snapshot_revision,
                    witness_observation_floor: 0,
                    result: SidebarEffectResult::Succeeded(PaneInstance {
                        pane_id: "%1".to_string(),
                        pane_pid: 101,
                    }),
                    effect: pending.effect,
                },
            )),
        },
    );

    assert!(matches!(
        internal_response,
        ServerMessage::SnapshotAck {
            accepted_seq: 2,
            ..
        }
    ));
    assert!(matches!(
        waiter_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
        ServerMessage::SnapshotAck {
            accepted_seq: 1,
            snapshot_revision: 7,
            ..
        }
    ));
    assert!(!coordinator.is_deferred_response(1));
}

#[test]
fn waiterless_view_queue_completion_does_not_emit_disconnected_diagnostic() {
    let coordinator = detached_test_coordinator("f".repeat(64));
    coordinator
        .router
        .lock()
        .unwrap()
        .set_phase(DaemonPhase::Serving);
    let mut connection = V2ConnectionState::default();
    assert!(matches!(
        coordinator.route_external(
            &mut connection,
            ClientMessage::Hello {
                proto: PROTOCOL_VERSION,
            },
            0,
        ),
        ServerMessage::HelloAck { .. }
    ));
    let pane = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 100,
    };
    let daemon_instance_id = coordinator
        .router
        .lock()
        .unwrap()
        .daemon_instance_id()
        .clone();
    let response = coordinator.route_external(
        &mut connection,
        ClientMessage::SubmitViewEvent {
            proto: PROTOCOL_VERSION,
            event: crate::pane_state::ViewEvent {
                daemon_instance_id,
                event_id: v2_event_id(),
                hook_kind: crate::pane_state::ViewHookKind::WindowPaneChanged,
                active_pane: Some(pane.clone()),
                window_panes: vec![pane],
                visibility: crate::pane_state::ViewVisibilityProof {
                    pane_visible: false,
                    window_visible: false,
                },
            },
        },
        128,
    );
    assert!(matches!(
        response,
        ServerMessage::ViewQueued {
            accepted_seq: 1,
            ..
        }
    ));
    assert!(coordinator.waiters.lock().unwrap().is_empty());
    let queued = coordinator.queue.lock().unwrap().items.pop_front().unwrap();
    assert_eq!(queued.sequenced.accepted_seq, 1);
    assert!(matches!(
        queued.sequenced.mutation,
        V2AcceptedMutation::External(ClientMessage::SubmitViewEvent { .. })
    ));

    coordinator.complete(
        1,
        ServerMessage::SnapshotAck {
            event_id: v2_event_id(),
            accepted_seq: 1,
            snapshot_revision: 0,
        },
    );

    assert!(coordinator.waiters.lock().unwrap().is_empty());
    assert!(coordinator.queue.lock().unwrap().items.is_empty());
}
