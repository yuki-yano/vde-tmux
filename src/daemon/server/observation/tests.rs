use super::super::*;
use super::*;

#[test]
fn observation_poll_query_is_one_guarded_command_group() {
    let framing = observation_poll_framing();
    let args = framing.query_args();
    let rendered = args.join(" ");

    assert!(rendered.contains("list-panes"));
    assert!(rendered.contains("list-sessions"));
    assert!(rendered.contains("list-windows"));
    assert!(rendered.contains("list-clients"));
    assert_eq!(rendered.matches("#{>:#{server_sessions},0}").count(), 3);
    assert!(rendered.contains(&framing.topology_end));
    assert!(rendered.contains(&framing.status_end));
    assert!(rendered.contains(&framing.client_end));
    assert!(rendered.contains(&framing.final_end));
    assert!(!rendered.contains("capture-pane"));
}

#[test]
fn observation_poll_parser_is_all_or_nothing() {
    let framing = observation_poll_framing();
    let identity = crate::daemon::topology::ServerIdentity {
        pid: 123,
        start_time: 456,
    };
    let output = observation_poll_output(&framing);
    let projection = parse_observation_poll_projection(&output, &framing, &identity).unwrap();

    assert_eq!(projection.topology.panes.len(), 1);
    assert_eq!(projection.status_metadata.sessions.len(), 1);
    assert_eq!(projection.status_metadata.windows.len(), 1);
    assert_eq!(projection.witnesses.len(), 1);

    let truncated = output.replace(&format!("{}\n", framing.final_end), "");
    assert!(matches!(
        parse_observation_poll_projection(&truncated, &framing, &identity),
        Err(ObservationPollQueryError::Framing(_))
    ));
    let malformed = output.replace("$1__vde_f_", "$1__broken_f_");
    assert!(parse_observation_poll_projection(&malformed, &framing, &identity).is_err());

    let empty = parse_observation_poll_projection(
        &empty_observation_poll_output(&framing),
        &framing,
        &identity,
    )
    .unwrap();
    assert!(empty.topology.panes.is_empty());
    assert!(empty.status_metadata.sessions.is_empty());
    assert!(empty.status_metadata.windows.is_empty());
    assert!(empty.witnesses.is_empty());

    let duplicated = output.replacen(
        &format!("{}\n", framing.topology_end),
        &format!("{}\n{}\n", framing.topology_end, framing.topology_end),
        1,
    );
    assert!(matches!(
        parse_observation_poll_projection(&duplicated, &framing, &identity),
        Err(ObservationPollQueryError::Framing(message))
            if message.contains("duplicated")
    ));
}

#[test]
fn observation_poll_parser_rejects_oversized_combined_output() {
    let framing = observation_poll_framing();
    let identity = crate::daemon::topology::ServerIdentity {
        pid: 123,
        start_time: 456,
    };
    let mut output = observation_poll_output(&framing);
    output.push_str(
        &"x".repeat(crate::daemon::topology::MAX_TMUX_QUERY_OUTPUT_BYTES - output.len() + 1),
    );

    assert!(matches!(
        parse_observation_poll_projection(&output, &framing, &identity),
        Err(ObservationPollQueryError::Topology(
            crate::daemon::topology::TopologyError::OutputTooLarge { .. }
        ))
    ));
}

#[test]
fn stale_poll_view_base_blocks_full_replacement() {
    let pane = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 100,
    };
    let view_base = crate::daemon::view_hooks::CurrentClientViews::default();
    let mut current = view_base.clone();
    current
        .reconcile(
            &[crate::pane_state::ClientWitness {
                client_pid: 10,
                session_id: "$1".to_string(),
                window_id: "@1".to_string(),
                active_pane: pane.clone(),
                control_mode: false,
                active_pane_flag: false,
            }],
            &BTreeMap::from([("@1".to_string(), vec![pane])]),
        )
        .unwrap();
    assert!(!observation_view_base_matches(&current, Some(&view_base)));
}

#[test]
fn observation_poll_store_fail_stop_reaches_coordinator() {
    let coordinator = detached_test_coordinator("b".repeat(64));

    let response = observation_poll_error_response(
        &coordinator,
        anyhow::Error::new(crate::pane_state::store::StoreError::FailStop(
            "projection invariant failed".to_string(),
        )),
    );

    assert!(coordinator.router.lock().unwrap().is_fatal());
    assert!(matches!(
        response,
        ServerMessage::Error {
            code: ErrorCode::InternalError,
            ..
        }
    ));
}

#[test]
fn observation_poll_burst_enqueues_one_batch_mutation() {
    for pane_count in [63, 256, 512] {
        let root = test_root(&format!("observation-burst-{pane_count}"));
        let server_identity = crate::daemon::topology::ServerIdentity {
            pid: 1,
            start_time: 2,
        };
        let coordinator = test_coordinator(&root, format!("observation-burst-{pane_count}"));
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
        assert!(
            coordinator.enqueue_internal(V2InternalMutation::ObservationBatch(Box::new(
                ObservationBatchPayload {
                    projection: Box::new(ObservationPollProjection {
                        observation_seq: 1,
                        topology: crate::daemon::topology::TopologySnapshot {
                            server_identity,
                            panes: Vec::new(),
                        },
                        status_metadata: crate::daemon::runtime::StatusProjectionMetadata::default(
                        ),
                        witnesses: Vec::new(),
                        observation_bases: BTreeMap::new(),
                        view_base: crate::daemon::view_hooks::CurrentClientViews::default(),
                        through_unread_order: 0,
                    }),
                    observations,
                    removals: Vec::new(),
                    diagnostics: Vec::new(),
                }
            ),))
        );

        // One successful poll is exactly one queued mutation regardless of
        // pane count, and the payload preserves the per-pane events.
        let queue = coordinator.queue.lock().unwrap();
        assert_eq!(queue.items.len(), 1);
        match queue.items.front().map(|item| &item.sequenced.mutation) {
            Some(V2AcceptedMutation::Internal(V2InternalMutation::ObservationBatch(payload))) => {
                assert_eq!(payload.observations.len(), pane_count);
                assert!(payload.removals.is_empty());
                assert!(payload.diagnostics.is_empty());
            }
            other => panic!("expected one observation batch, found {other:?}"),
        }
        drop(queue);

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn query_pane_cache_miss_waits_for_targeted_refresh_and_returns_found() {
    let (response, diagnostics) = query_pane_cache_miss_with_refresh_outcome(Ok(
        crate::daemon::topology::TargetedRefreshOutcome::Found(Box::new(
            crate::daemon::topology::TopologyPane {
                pane_instance: PaneInstance {
                    pane_id: "%7".to_string(),
                    pane_pid: 700,
                },
                session_links: Vec::new(),
                window_id: "@1".to_string(),
                window_name: "main".to_string(),
                current_path: "/tmp".to_string(),
                current_command: "zsh".to_string(),
                pane_width: 80,
                active: true,
                editprompt_is_editor: false,
                editprompt_target_panes: Vec::new(),
                editprompt_editor_pane: None,
            },
        )),
    ));
    assert!(diagnostics.is_empty());
    assert!(matches!(
        response,
        ServerMessage::PaneResult {
            pane: crate::daemon::protocol::v2::PanePresentation {
                pane_instance: PaneInstance {
                    pane_id,
                    pane_pid: 700,
                },
                ..
            },
            ..
        } if pane_id == "%7"
    ));
}

#[test]
fn query_pane_cache_miss_returns_pane_not_found_after_fresh_absence() {
    assert!(matches!(
        query_pane_cache_miss_with_refresh_outcome(Ok(
            crate::daemon::topology::TargetedRefreshOutcome::NotFound,
        ))
        .0,
        ServerMessage::Error {
            code: ErrorCode::PaneNotFound,
            ..
        }
    ));
}

#[test]
fn query_pane_cache_miss_returns_internal_error_after_refresh_failure() {
    let failure = crate::daemon::topology::TopologyError::Query("tmux query failed".to_string());
    let (response, diagnostics) = query_pane_cache_miss_with_refresh_outcome(Err(failure));
    assert!(matches!(
        response,
        ServerMessage::Error {
            code: ErrorCode::InternalError,
            ..
        }
    ));
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, ErrorCode::InternalError);
    assert!(diagnostics[0].message.contains("tmux query failed"));
}

#[test]
fn query_pane_cache_miss_records_refresh_timeout_diagnostic() {
    let (response, diagnostics) = query_pane_cache_miss_with_refresh_outcome(Err(
        crate::daemon::topology::TopologyError::Deadline,
    ));
    assert!(matches!(
        response,
        ServerMessage::Error {
            code: ErrorCode::InternalError,
            ..
        }
    ));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("deadline exceeded"));
}
