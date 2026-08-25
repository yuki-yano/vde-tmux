use crate::daemon::protocol::v2::PROTOCOL_VERSION;

const V2_EVENT_ID: &str = "102132435465768798a9bacbdcedfe0f";
const V2_DAEMON_ID: &str = "ffeeddccbbaa99887766554433221100";
const POLL_TOKEN: &str = "00112233445566778899aabbccddeeff";

fn codex_provider_test_event(
    daemon_instance_id: DaemonInstanceId,
    pane_instance: PaneInstance,
    event: &str,
    payload: &str,
    observed_at: i64,
) -> (
    PaneEventEnvelope,
    crate::hook::provider::ProviderObservation,
) {
    use crate::hook::provider::ProviderHookKind;
    use crate::pane_state::{AgentSessionSource, PromptState, ResponseState};

    let event_id = EventId::generate().unwrap();
    let observation = crate::hook::provider::observation_from_json(
        "codex",
        event,
        payload,
        event_id.clone(),
        observed_at,
    )
    .unwrap()
    .unwrap();
    let payload: serde_json::Value = serde_json::from_str(payload).unwrap();
    let pane_event = match observation.hook_kind {
        ProviderHookKind::SessionStart => PaneEvent::AgentSessionStarted {
            observed_at,
            source: AgentSessionSource::Startup,
            resumed_prompt: None,
        },
        ProviderHookKind::UserPromptSubmit => PaneEvent::BeginRun {
            started_at: observed_at,
            prompt: payload
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .map(|text| PromptState {
                    text: text.to_string(),
                    source: "user".to_string(),
                    digest: observation.prompt_digest.clone(),
                }),
        },
        ProviderHookKind::Stop => PaneEvent::ResponseAndCompleteRun {
            completed_at: observed_at,
            response: ResponseState {
                text: payload
                    .get("last_assistant_message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                observed_at,
            },
        },
        _ => panic!("unsupported provider test hook {event}"),
    };
    (
        PaneEventEnvelope {
            daemon_instance_id,
            event_id,
            pane_instance,
            agent: Some(observation.provider.clone()),
            agent_session_id: Some(observation.session_id.clone()),
            event: pane_event,
        },
        observation,
    )
}
fn guarded_prompt_test_binding() -> crate::agent_state::AgentBinding {
    crate::agent_state::AgentBinding {
        server_identity: crate::daemon::topology::ServerIdentity {
            pid: std::process::id(),
            start_time: 424_242,
        },
        pane_instance: crate::pane_state::PaneInstance {
            pane_id: "%4242".to_string(),
            pane_pid: 42_424,
        },
        pane_state_id: crate::pane_state::StateId::parse("1".repeat(32)).unwrap(),
        agent_epoch: 1,
        agent_kind: crate::pane_state::AgentKind::parse("codex").unwrap(),
        provider_session_id: crate::pane_state::AgentSessionId::parse("session-guarded")
            .unwrap(),
        process: crate::pane_state::AgentProcessIdentity {
            pid: 52_424,
            start_token: "guarded-process-start".to_string(),
        },
    }
}

fn guarded_prompt_test_operation(
    binding: crate::agent_state::AgentBinding,
) -> crate::agent_state::OperationRecord {
    crate::agent_state::OperationRecord {
        state_format_version: crate::agent_state::PRIVATE_STATE_FORMAT_VERSION,
        generation: crate::agent_state::StateGeneration::parse("2".repeat(32)).unwrap(),
        operation_id: crate::agent_state::OperationId::parse("operation_guarded_test").unwrap(),
        revision: 1,
        request_fingerprint: crate::agent_state::Sha256Digest::of(b"request"),
        target_agent_ref: "vta1:guarded-target".to_string(),
        prompt_digest: crate::agent_state::Sha256Digest::of(b"prompt"),
        dispatch_option: "paste_enter".to_string(),
        expected_pane_version: crate::pane_state::StateVersion {
            state_id: binding.pane_state_id.clone(),
            agent_epoch: binding.agent_epoch,
            revision: 7,
        },
        expected_current_run: None,
        expected_run_seq: 4,
        confirmation_deadline_at: 20,
        dispatch_state: crate::agent_state::DispatchState::Prepared,
        run_id: None,
        result_receipt: None,
        created_at: 10,
        updated_at: 10,
        binding: binding.into(),
    }
}

fn guarded_prompt_test_pane_state(
    binding: &crate::agent_state::AgentBinding,
) -> crate::pane_state::PaneState {
    crate::pane_state::PaneState {
        schema_version: crate::pane_state::PANE_STATE_SCHEMA_VERSION,
        state_id: binding.pane_state_id.clone(),
        revision: 7,
        pane_instance: binding.pane_instance.clone(),
        agent: binding.agent_kind.clone(),
        agent_session_id: Some(binding.provider_session_id.clone()),
        agent_process: Some(binding.process.clone()),
        agent_epoch: binding.agent_epoch,
        agent_present: true,
        scan_verified: true,
        synthetic_completion_armed: false,
        lifecycle: crate::pane_state::LifecycleState::Idle,
        run_seq: 3,
        current_run: None,
        completed_seq: 0,
        unread: crate::pane_state::UnreadState::default(),
        started_at: None,
        completed_at: None,
        prompt: None,
        latest_response: None,
        task_context: crate::pane_state::TaskContextState::default(),
        tasks: crate::pane_state::TaskState::default(),
        subagents: Vec::new(),
        worktree_activity: None,
        background_process: None,
        listening_ports: Vec::new(),
    }
}

fn test_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "vde-{label}-{}-{}",
        std::process::id(),
        EventId::generate().unwrap().as_str()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn test_incarnation(
    root: &Path,
    hash: impl Into<String>,
) -> crate::daemon::lifecycle::TmuxServerIncarnation {
    crate::daemon::lifecycle::TmuxServerIncarnation {
        socket_path: root.join("tmux.sock"),
        identity: crate::daemon::topology::ServerIdentity {
            pid: 1,
            start_time: 2,
        },
        hash: hash.into(),
    }
}

fn test_coordinator(root: &Path, hash: impl Into<String>) -> ProductionV2Coordinator {
    ProductionV2Coordinator::new(test_incarnation(root, hash), BTreeMap::new(), None).unwrap()
}

fn initialized_test_coordinator(
    root: &Path,
    hash: impl Into<String>,
    views: crate::daemon::view_hooks::CurrentClientViews,
) -> ProductionV2Coordinator {
    let coordinator = test_coordinator(root, hash);
    install_test_state(&coordinator, root, views);
    coordinator
}

fn detached_test_coordinator(hash: impl Into<String>) -> ProductionV2Coordinator {
    test_coordinator(Path::new("/tmp/vde-test"), hash)
}

fn install_test_state(
    coordinator: &ProductionV2Coordinator,
    root: &Path,
    views: crate::daemon::view_hooks::CurrentClientViews,
) {
    let leased =
        crate::daemon::runtime::LeasedCanonicalPaneStateRuntime::acquire(&root.join("writer"))
            .unwrap();
    *coordinator.state.lock().unwrap() =
        Some(crate::daemon::runtime::CanonicalCoordinatorState::new(
            leased,
            crate::daemon::topology::TopologySnapshot {
                server_identity: coordinator.incarnation.identity.clone(),
                panes: Vec::new(),
            },
            views,
            crate::sidebar::state::SidebarPreferences::default(),
        ));
}

fn read_peek_test_pane_state(
    pane_instance: PaneInstance,
    state_id: &str,
    order: u64,
) -> crate::pane_state::PaneState {
    crate::pane_state::PaneState {
        schema_version: crate::pane_state::PANE_STATE_SCHEMA_VERSION,
        state_id: crate::pane_state::StateId::parse(state_id).unwrap(),
        revision: 1,
        pane_instance,
        agent: crate::pane_state::AgentKind::parse("codex").unwrap(),
        agent_session_id: Some(
            crate::pane_state::AgentSessionId::parse("read-peek-session").unwrap(),
        ),
        agent_process: None,
        agent_epoch: 1,
        agent_present: true,
        scan_verified: true,
        synthetic_completion_armed: false,
        lifecycle: crate::pane_state::LifecycleState::Idle,
        run_seq: 1,
        current_run: None,
        completed_seq: 1,
        unread: crate::pane_state::UnreadState {
            occurrence_seq: 1,
            read_seq: 0,
            latest: Some(crate::pane_state::UnreadOccurrence {
                seq: 1,
                order,
                reason: crate::pane_state::UnreadReason::Completed,
                occurred_at: 1,
            }),
        },
        started_at: Some(1),
        completed_at: Some(1),
        prompt: None,
        latest_response: None,
        task_context: crate::pane_state::TaskContextState::default(),
        tasks: crate::pane_state::TaskState::default(),
        subagents: Vec::new(),
        worktree_activity: None,
        background_process: None,
        listening_ports: Vec::new(),
    }
}

fn read_peek_test_topology_pane(
    pane_instance: PaneInstance,
    active: bool,
) -> crate::daemon::topology::TopologyPane {
    crate::daemon::topology::TopologyPane {
        pane_instance,
        session_links: Vec::new(),
        window_id: "@1".to_string(),
        window_name: "peek".to_string(),
        current_path: "/tmp".to_string(),
        current_command: "codex".to_string(),
        pane_width: 80,
        active,
        editprompt_is_editor: false,
        editprompt_target_panes: Vec::new(),
        editprompt_editor_pane: None,
    }
}

fn read_peek_test_state(
    root: &Path,
) -> (
    crate::daemon::runtime::CanonicalCoordinatorState,
    PaneInstance,
    PaneInstance,
) {
    let target = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 101,
    };
    let candidate = PaneInstance {
        pane_id: "%2".to_string(),
        pane_pid: 102,
    };
    let mut leased =
        crate::daemon::runtime::LeasedCanonicalPaneStateRuntime::acquire(&root.join("writer"))
            .unwrap();
    leased
        .hydrate(BTreeMap::from([
            (
                target.clone(),
                read_peek_test_pane_state(
                    target.clone(),
                    "11111111111111111111111111111111",
                    1,
                ),
            ),
            (
                candidate.clone(),
                read_peek_test_pane_state(
                    candidate.clone(),
                    "22222222222222222222222222222222",
                    2,
                ),
            ),
        ]))
        .unwrap();
    let mut state = crate::daemon::runtime::CanonicalCoordinatorState::new(
        leased,
        crate::daemon::topology::TopologySnapshot {
            server_identity: crate::daemon::topology::ServerIdentity {
                pid: 1,
                start_time: 2,
            },
            panes: vec![
                read_peek_test_topology_pane(target.clone(), true),
                read_peek_test_topology_pane(candidate.clone(), false),
            ],
        },
        crate::daemon::view_hooks::CurrentClientViews::default(),
        crate::sidebar::state::SidebarPreferences::default(),
    );
    assert!(state.begin_peek(10, target.clone(), [target.clone()], 1));
    state.activate_peek(10, 1, target.clone(), 0);
    (state, target, candidate)
}

fn read_peek_test_witness(
    client_pid: u32,
    pane: &PaneInstance,
) -> crate::pane_state::ClientWitness {
    crate::pane_state::ClientWitness {
        client_pid,
        session_id: format!("${client_pid}"),
        window_id: "@1".to_string(),
        active_pane: pane.clone(),
        control_mode: false,
        active_pane_flag: false,
    }
}

struct ReadPeekStoreIo {
    fail: bool,
}

impl crate::pane_state::snapshot::PaneSnapshotStoreIo for ReadPeekStoreIo {
    fn save(
        &mut self,
        _records: &BTreeMap<PaneInstance, crate::pane_state::PaneState>,
    ) -> Result<(), crate::pane_state::store::StoreError> {
        if self.fail {
            Err(crate::pane_state::store::StoreError::PersistFailed(
                "injected read failure".to_string(),
            ))
        } else {
            Ok(())
        }
    }
}

fn emit_read_peek_waiting_occurrence(
    state: &mut crate::daemon::runtime::CanonicalCoordinatorState,
    target: &PaneInstance,
) {
    for event in [
        PaneEvent::BeginRun {
            started_at: 2,
            prompt: None,
        },
        PaneEvent::WaitRequested {
            observed_at: 3,
            reason: crate::pane_state::WaitReason::PermissionPrompt,
        },
    ] {
        apply_read_peek_event(
            state,
            target,
            event,
            &crate::pane_state::VisibilitySnapshot::default(),
        );
    }
}

fn apply_read_peek_event(
    state: &mut crate::daemon::runtime::CanonicalCoordinatorState,
    target: &PaneInstance,
    event: PaneEvent,
    visibility: &crate::pane_state::VisibilitySnapshot,
) {
    state
        .leased
        .runtime
        .apply_event(
            &mut ReadPeekStoreIo { fail: false },
            &PaneEventEnvelope {
                daemon_instance_id: v2_daemon_id(),
                event_id: EventId::generate().unwrap(),
                pane_instance: target.clone(),
                agent: Some(crate::pane_state::AgentKind::parse("codex").unwrap()),
                agent_session_id: Some(
                    crate::pane_state::AgentSessionId::parse("read-peek-session").unwrap(),
                ),
                event,
            },
            visibility,
        )
        .unwrap();
}

fn observation_poll_framing() -> ObservationPollFraming {
    ObservationPollFraming::from_query(
        crate::daemon::topology::QueryFraming::from_token(POLL_TOKEN).unwrap(),
    )
    .unwrap()
}

fn observation_poll_output(framing: &ObservationPollFraming) -> String {
    let field = framing.query.field_separator();
    let row = framing.query.row_separator();
    let identity = framing
        .query
        .identity_format()
        .replace("#{pid}", "123")
        .replace("#{start_time}", "456");
    let topology = [
        "$1", "main", "@1", "0", "1", "0", "window", "%1", "100", "/tmp", "zsh", "80", "1", "",
        "", "",
    ]
    .join(field);
    let status_session = [
        "__vde_sm_00112233445566778899aabbccddeeff__",
        "$1",
        "main",
        "work",
        "/tmp",
        "",
        "1",
        "10",
    ]
    .join(field);
    let status_window = [
        "__vde_wm_00112233445566778899aabbccddeeff__",
        "@1",
        "0",
        "1",
        "0",
    ]
    .join(field);
    let client = ["99", "$1", "@1", "%1", "100", "0", ""]
        .join(&format!("__vde_client_field_{POLL_TOKEN}__"));
    format!(
        "{identity}\n{topology}{row}\n{}\n{identity}\n{status_session}{row}\n{status_window}{row}\n{}\n__vde_client_identity_{POLL_TOKEN}__123:456\n{client}__vde_client_row_{POLL_TOKEN}__\n{}\n{}\n",
        framing.topology_end, framing.status_end, framing.client_end, framing.final_end
    )
}

fn empty_observation_poll_output(framing: &ObservationPollFraming) -> String {
    let identity = framing
        .query
        .identity_format()
        .replace("#{pid}", "123")
        .replace("#{start_time}", "456");
    format!(
        "{identity}\n{}\n{identity}\n{}\n__vde_client_identity_{POLL_TOKEN}__123:456\n{}\n{}\n",
        framing.topology_end, framing.status_end, framing.client_end, framing.final_end
    )
}

fn v2_daemon_id() -> DaemonInstanceId {
    DaemonInstanceId::parse(V2_DAEMON_ID).unwrap()
}

fn v2_event_id() -> EventId {
    EventId::parse(V2_EVENT_ID).unwrap()
}

fn v2_sidebar_command(command: crate::daemon::protocol::v2::SidebarCommand) -> ClientMessage {
    ClientMessage::SidebarCommand {
        proto: PROTOCOL_VERSION,
        daemon_instance_id: v2_daemon_id(),
        event_id: v2_event_id(),
        command,
    }
}

fn v2_pane_event(event: PaneEvent) -> ClientMessage {
    ClientMessage::SubmitPaneEvent {
        proto: PROTOCOL_VERSION,
        envelope: PaneEventEnvelope {
            daemon_instance_id: v2_daemon_id(),
            event_id: v2_event_id(),
            pane_instance: PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 100,
            },
            agent: Some(crate::pane_state::AgentKind::parse("codex").unwrap()),
            agent_session_id: Some(
                crate::pane_state::AgentSessionId::parse("session").unwrap(),
            ),
            event,
        },
    }
}

fn v2_begin() -> ClientMessage {
    v2_pane_event(PaneEvent::BeginRun {
        started_at: 1,
        prompt: None,
    })
}

fn v2_handshake(router: &mut V2Router, connection: &mut V2ConnectionState) {
    let route = router.route(
        connection,
        ClientMessage::Hello {
            proto: PROTOCOL_VERSION,
        },
    );
    assert!(matches!(
        route,
        V2Route::Response(ServerMessage::HelloAck {
            proto: PROTOCOL_VERSION,
            ..
        })
    ));
}

fn query_pane_cache_miss_with_refresh_outcome(
    outcome: Result<
        crate::daemon::topology::TargetedRefreshOutcome,
        crate::daemon::topology::TopologyError,
    >,
) -> (
    ServerMessage,
    Vec<crate::daemon::protocol::v2::DaemonDiagnostic>,
) {
    let root = test_root("query-pane-refresh");
    let coordinator = Arc::new(test_coordinator(&root, "9".repeat(64)));
    coordinator
        .router
        .lock()
        .unwrap()
        .set_phase(DaemonPhase::Serving);
    install_test_state(
        &coordinator,
        &root,
        crate::daemon::view_hooks::CurrentClientViews::default(),
    );

    let (result_tx, result_rx) = mpsc::channel();
    let query_coordinator = coordinator.clone();
    let query = thread::spawn(move || {
        result_tx
            .send(query_coordinator.query(ClientMessage::QueryPane {
                proto: PROTOCOL_VERSION,
                pane_id: "%7".to_string(),
            }))
            .unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    let queued = loop {
        if let Some(queued) = coordinator.queue.lock().unwrap().items.pop_front() {
            break queued;
        }
        assert!(
            Instant::now() < deadline,
            "QueryPane refresh was not queued"
        );
        thread::yield_now();
    };
    assert!(matches!(
        &queued.sequenced.mutation,
        V2AcceptedMutation::Internal(V2InternalMutation::TargetedPaneRefresh { pane_id })
            if pane_id == "%7"
    ));
    let refresh_response = targeted_pane_refresh_outcome_response(&coordinator, "%7", outcome);
    coordinator.complete(queued.sequenced.accepted_seq, refresh_response);

    let response = result_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    query.join().unwrap();
    let diagnostics = coordinator
        .state
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .global_diagnostics
        .iter()
        .cloned()
        .collect();
    drop(coordinator);
    std::fs::remove_dir_all(root).unwrap();
    (response, diagnostics)
}
