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
                question_notice: None,
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
fn query_pane_cache_miss_records_refresh_failures() {
    for (failure, expected_message) in [
        (
            crate::daemon::topology::TopologyError::Query("tmux query failed".to_string()),
            "tmux query failed",
        ),
        (
            crate::daemon::topology::TopologyError::Deadline,
            "deadline exceeded",
        ),
    ] {
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
        assert!(diagnostics[0].message.contains(expected_message));
    }
}
