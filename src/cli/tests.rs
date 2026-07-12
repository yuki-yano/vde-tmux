use super::*;
use crate::tmux::mock::MockTmuxRunner;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

static V2_QUERY_FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn env() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn tmux_env() -> BTreeMap<String, String> {
    BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())])
}

fn stub_action_client(mock: &MockTmuxRunner, client: &str, session_id: &str) {
    let format = crate::session::client_session_context_format();
    let sep = '\u{1f}';
    mock.stub(
        &["list-clients", "-F", &format],
        &format!("{client}{sep}/dev/ttys001{sep}{session_id}{sep}%1{sep}0\n"),
    );
}

fn stub_shared_action_clients(mock: &MockTmuxRunner) {
    let format = crate::session::client_session_context_format();
    let sep = '\u{1f}';
    mock.stub(
        &["list-clients", "-F", &format],
        &format!(
            "client-1{sep}/dev/ttys001{sep}$9{sep}%1{sep}0\n\
             client-2{sep}/dev/ttys002{sep}$2{sep}%1{sep}0\n"
        ),
    );
}

fn stub_category_switch(mock: &MockTmuxRunner, client: &str, category: &str, target_session: &str) {
    let format = crate::session::session_list_format();
    mock.stub(
        &["list-sessions", "-F", &format],
        "a\u{1f}1\u{1f}100\u{1f}alpha\u{1f}\u{1f}\u{1f}$1\n\
         b\u{1f}1\u{1f}90\u{1f}beta\u{1f}\u{1f}\u{1f}$2\n\
         c\u{1f}1\u{1f}80\u{1f}gamma\u{1f}\u{1f}\u{1f}$3\n",
    );
    let memory_key = crate::session::client_memory_key(client, category);
    mock.stub(&["show-option", "-gqv", &memory_key], "");
    let exact_target = crate::session::exact_session_target(target_session);
    mock.stub(&["switch-client", "-c", client, "-t", &exact_target], "");
    mock.stub(&["set-option", "-g", &memory_key, target_session], "");
}

struct V2QueryFixture {
    mock: MockTmuxRunner,
    env: BTreeMap<String, String>,
    daemon_socket: std::path::PathBuf,
    root: std::path::PathBuf,
    tmux_listener: std::os::unix::net::UnixListener,
    server: std::thread::JoinHandle<()>,
}

impl V2QueryFixture {
    fn finish(self) {
        self.server.join().unwrap();
        if let Err(error) = std::fs::remove_file(&self.daemon_socket)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            panic!("failed to remove fixture socket: {error}");
        }
        drop(self.tmux_listener);
        std::fs::remove_dir_all(self.root).unwrap();
    }
}

fn spawn_v2_query_fixture(
    expected: crate::daemon::protocol::v2::ClientMessage,
    response: crate::daemon::protocol::v2::ServerMessage,
) -> V2QueryFixture {
    use std::io::{BufRead, Write};

    let root = std::env::temp_dir().join(format!(
        "v2q-{:x}-{:x}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        V2_QUERY_FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&root).unwrap();
    let tmux_socket = root.join("tmux.sock");
    let tmux_listener = std::os::unix::net::UnixListener::bind(&tmux_socket).unwrap();
    let mock = MockTmuxRunner::new();
    mock.stub(
        &[
            "display-message",
            "-p",
            "#{pid}\t#{start_time}\t#{socket_path}",
        ],
        &format!("321\t654\t{}\n", tmux_socket.display()),
    );
    let env = BTreeMap::from([(
        "TMUX".to_string(),
        format!("{},321,0", tmux_socket.display()),
    )]);
    let incarnation =
        crate::daemon::lifecycle::TmuxServerIncarnation::resolve(&mock, &env).unwrap();
    let daemon_socket =
        crate::daemon::daemon_socket_path_for_incarnation(&env, None, &incarnation.hash);
    std::fs::create_dir_all(daemon_socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&daemon_socket).unwrap();
    let server_identity = incarnation.hash;
    let server = std::thread::spawn(move || {
        loop {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(
                serde_json::from_str::<crate::daemon::protocol::v2::ClientMessage>(line.trim())
                    .unwrap(),
                crate::daemon::protocol::v2::ClientMessage::Hello {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                }
            );
            serde_json::to_writer(
                &mut stream,
                &crate::daemon::protocol::v2::ServerMessage::HelloAck {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                    daemon_instance_id: crate::pane_state::DaemonInstanceId::parse(
                        "ffeeddccbbaa99887766554433221100",
                    )
                    .unwrap(),
                    server_identity: server_identity.clone(),
                    phase: crate::daemon::protocol::v2::DaemonPhase::Serving,
                    hook_health: crate::daemon::protocol::v2::HookHealth::Healthy,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line.is_empty() {
                continue;
            }
            assert_eq!(
                serde_json::from_str::<crate::daemon::protocol::v2::ClientMessage>(line.trim())
                    .unwrap(),
                expected
            );
            serde_json::to_writer(&mut stream, &response).unwrap();
            stream.write_all(b"\n").unwrap();
            // Keep the peer alive until the one-request client has consumed the response and
            // closed. Closing immediately after write races socket timeout setup on Darwin.
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert!(line.is_empty());
            break;
        }
    });
    V2QueryFixture {
        mock,
        env,
        daemon_socket,
        root,
        tmux_listener,
        server,
    }
}

fn spawn_active_config_guard_fixture() -> V2QueryFixture {
    let config_hash = crate::daemon::lifecycle::config_hash(&crate::config::Config::default());
    let response = crate::daemon::protocol::v2::ServerMessage::HealthResult {
        health: crate::daemon::protocol::v2::DaemonHealth {
            config_hash,
            projection_revision: 1,
            projection_updated_at_epoch_seconds: 1,
            notification_enabled: false,
            notification_failures: 0,
            notification_queue_drops: 0,
            notification_degraded: false,
            last_notification_error_code: None,
            current_quarantine_count: 0,
            quarantine_observed_total: 0,
            recent_error_code: None,
            hook_delivery_failures: 0,
            hook_delivery_degraded: false,
            last_hook_error_code: None,
            status_push_failures: 0,
            status_push_degraded: false,
            last_status_push_error: None,
            last_status_push_error_at_epoch_seconds: None,
        },
    };
    let mut fixture = spawn_v2_query_fixture(
        crate::daemon::protocol::v2::ClientMessage::QueryHealth {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
        },
        response,
    );
    fixture.env.insert(
        "HOME".to_string(),
        fixture.root.join("home").display().to_string(),
    );
    fixture
}

fn spawn_v2_handshake_fixture() -> V2QueryFixture {
    use std::io::{BufRead, Write};

    let root = std::env::temp_dir().join(format!(
        "vde-ds-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let tmux_socket = root.join("tmux.sock");
    let tmux_listener = std::os::unix::net::UnixListener::bind(&tmux_socket).unwrap();
    let mock = MockTmuxRunner::new();
    mock.stub(
        &[
            "display-message",
            "-p",
            "#{pid}\t#{start_time}\t#{socket_path}",
        ],
        &format!("321\t654\t{}\n", tmux_socket.display()),
    );
    let env = BTreeMap::from([
        (
            "TMUX".to_string(),
            format!("{},321,0", tmux_socket.display()),
        ),
        (
            "XDG_STATE_HOME".to_string(),
            root.join("state").display().to_string(),
        ),
    ]);
    let incarnation =
        crate::daemon::lifecycle::TmuxServerIncarnation::resolve(&mock, &env).unwrap();
    let daemon_socket =
        crate::daemon::daemon_socket_path_for_incarnation(&env, None, &incarnation.hash);
    std::fs::create_dir_all(daemon_socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&daemon_socket).unwrap();
    let server_identity = incarnation.hash;
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<crate::daemon::protocol::v2::ClientMessage>(line.trim())
                .unwrap(),
            crate::daemon::protocol::v2::ClientMessage::Hello {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            }
        );
        serde_json::to_writer(
            &mut stream,
            &crate::daemon::protocol::v2::ServerMessage::HelloAck {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                daemon_instance_id: crate::pane_state::DaemonInstanceId::parse(
                    "ffeeddccbbaa99887766554433221100",
                )
                .unwrap(),
                server_identity,
                phase: crate::daemon::protocol::v2::DaemonPhase::Serving,
                hook_health: crate::daemon::protocol::v2::HookHealth::Degraded,
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<crate::daemon::protocol::v2::ClientMessage>(line.trim())
                .unwrap(),
            crate::daemon::protocol::v2::ClientMessage::QueryHealth {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            }
        );
        serde_json::to_writer(
            &mut stream,
            &crate::daemon::protocol::v2::ServerMessage::HealthResult {
                health: crate::daemon::protocol::v2::DaemonHealth {
                    config_hash: "test-config".to_string(),
                    projection_revision: 7,
                    projection_updated_at_epoch_seconds: 1,
                    notification_enabled: false,
                    notification_failures: 0,
                    notification_queue_drops: 0,
                    notification_degraded: false,
                    last_notification_error_code: None,
                    current_quarantine_count: 0,
                    quarantine_observed_total: 0,
                    recent_error_code: None,
                    hook_delivery_failures: 0,
                    hook_delivery_degraded: false,
                    last_hook_error_code: None,
                    status_push_failures: 0,
                    status_push_degraded: false,
                    last_status_push_error: None,
                    last_status_push_error_at_epoch_seconds: None,
                },
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(line.is_empty(), "status sent a mutating request: {line}");
    });
    V2QueryFixture {
        mock,
        env,
        daemon_socket,
        root,
        tmux_listener,
        server,
    }
}

fn status_snapshot(
    context: crate::daemon::protocol::v2::StatusContext,
) -> crate::daemon::protocol::v2::StatusSnapshot {
    use crate::daemon::protocol::v2::{
        AttentionEntry, CategoryStatusPresentation, SessionStatusPresentation,
        WindowStatusPresentation,
    };
    use crate::daemon::session_badge::{BadgeState, BadgeStateCounts};

    crate::daemon::protocol::v2::StatusSnapshot {
        snapshot_revision: 7,
        context,
        summary: BadgeStateCounts {
            working: 1,
            ..BadgeStateCounts::default()
        },
        sessions: vec![SessionStatusPresentation {
            session_id: "$1".to_string(),
            session_name: "main".to_string(),
            category: Some("work".to_string()),
            attached: Some(true),
            created_at: Some(100),
            active: true,
            counts: BadgeStateCounts {
                working: 1,
                ..BadgeStateCounts::default()
            },
        }],
        windows: vec![
            WindowStatusPresentation {
                window_id: "@1".to_string(),
                window_name: "zsh".to_string(),
                pane_count: 1,
                session_ids: vec!["$1".to_string()],
                window_index: Some(1),
                active: false,
                last: false,
                bell: Some(false),
                activity: Some(false),
                silence: Some(false),
                current_command: Some("zsh".to_string()),
                counts: BadgeStateCounts::default(),
            },
            WindowStatusPresentation {
                window_id: "@2".to_string(),
                window_name: "editor".to_string(),
                pane_count: 2,
                session_ids: vec!["$1".to_string()],
                window_index: Some(2),
                active: true,
                last: false,
                bell: Some(false),
                activity: Some(false),
                silence: Some(false),
                current_command: Some("nvim".to_string()),
                counts: BadgeStateCounts {
                    working: 1,
                    ..BadgeStateCounts::default()
                },
            },
        ],
        categories: vec![CategoryStatusPresentation {
            category: "work".to_string(),
            session_ids: vec!["$1".to_string()],
            active: true,
            counts: BadgeStateCounts {
                working: 1,
                ..BadgeStateCounts::default()
            },
        }],
        attention: vec![AttentionEntry {
            pane_instance: crate::pane_state::PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 101,
            },
            session_name: "main".to_string(),
            badge: BadgeState::Blocked,
            reason: Some("permission_prompt".to_string()),
            elapsed_seconds: 120,
        }],
    }
}

fn status_query_fixture(context: crate::daemon::protocol::v2::StatusContext) -> V2QueryFixture {
    let snapshot = status_snapshot(context.clone());
    spawn_v2_query_fixture(
        crate::daemon::protocol::v2::ClientMessage::QueryStatusSnapshot {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            context,
        },
        crate::daemon::protocol::v2::ServerMessage::StatusSnapshotResult {
            snapshot_revision: snapshot.snapshot_revision,
            snapshot,
        },
    )
}

fn pane_query_fixture(pane_id: &str) -> V2QueryFixture {
    use crate::pane_state::{
        AgentKind, LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneInstance, PaneState,
        ResolvedPaneState, StateId, TaskState,
    };

    let pane_instance = PaneInstance {
        pane_id: pane_id.to_string(),
        pane_pid: 101,
    };
    let pane = crate::daemon::protocol::v2::PanePresentation {
        pane_instance: pane_instance.clone(),
        session_links: vec![crate::daemon::protocol::v2::SessionLinkPresentation {
            session_id: "$1".to_string(),
            session_name: "main".to_string(),
            window_index: 1,
            window_active: true,
            window_last: false,
        }],
        window_id: "@1".to_string(),
        window_name: "main".to_string(),
        current_path: "/tmp".to_string(),
        current_command: "node".to_string(),
        pane_width: 80,
        active: true,
        stored: None,
        resolved: Some(ResolvedPaneState {
            canonical: PaneState {
                schema_version: PANE_STATE_SCHEMA_VERSION,
                state_id: StateId::parse("00112233445566778899aabbccddeeff").unwrap(),
                revision: 1,
                pane_instance,
                agent: AgentKind::parse("codex").unwrap(),
                agent_session_id: None,
                agent_epoch: 1,
                agent_present: true,
                scan_verified: true,
                synthetic_completion_armed: false,
                lifecycle: LifecycleState::Running,
                run_seq: 1,
                completed_seq: 0,
                acknowledged_seq: 0,
                started_at: Some(crate::sidebar::tree::now_epoch_secs()),
                completed_at: None,
                prompt: None,
                tasks: TaskState::default(),
                subagents: Vec::new(),
                worktree_activity: None,
            },
            window_id: "@1".to_string(),
            pane_id: pane_id.to_string(),
            current_path: "/tmp".to_string(),
            badge: crate::daemon::session_badge::BadgeState::Working,
        }),
        diagnostic: None,
    };
    spawn_v2_query_fixture(
        crate::daemon::protocol::v2::ClientMessage::QueryPane {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            pane_id: pane_id.to_string(),
        },
        crate::daemon::protocol::v2::ServerMessage::PaneResult {
            snapshot_revision: 7,
            pane,
        },
    )
}

fn spawn_v2_reset_fixture(pane_id: &str) -> V2QueryFixture {
    use std::io::{BufRead, Write};

    use crate::daemon::protocol::v2::{
        ClientMessage, DaemonPhase, HookHealth, PanePresentation, ResetOutcome, ServerMessage,
    };
    use crate::pane_state::{
        DaemonInstanceId, PaneInstance, StateId, StateVersion, StoredStateDescriptor,
    };

    let root = std::env::temp_dir().join(format!(
        "vde-cli-reset-v2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let tmux_socket = root.join("tmux.sock");
    let tmux_listener = std::os::unix::net::UnixListener::bind(&tmux_socket).unwrap();
    let mock = MockTmuxRunner::new();
    mock.stub(
        &[
            "display-message",
            "-p",
            "#{pid}\t#{start_time}\t#{socket_path}",
        ],
        &format!("321\t654\t{}\n", tmux_socket.display()),
    );
    let env = BTreeMap::from([(
        "TMUX".to_string(),
        format!("{},321,0", tmux_socket.display()),
    )]);
    let incarnation =
        crate::daemon::lifecycle::TmuxServerIncarnation::resolve(&mock, &env).unwrap();
    let daemon_socket =
        crate::daemon::daemon_socket_path_for_incarnation(&env, None, &incarnation.hash);
    std::fs::create_dir_all(daemon_socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&daemon_socket).unwrap();
    let server_identity = incarnation.hash;
    let pane_instance = PaneInstance {
        pane_id: pane_id.to_string(),
        pane_pid: 101,
    };
    let expected = StoredStateDescriptor::Canonical {
        version: StateVersion {
            state_id: StateId::parse("00112233445566778899aabbccddeeff").unwrap(),
            agent_epoch: 1,
            revision: 7,
        },
    };
    let daemon_instance_id = DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap();
    let server = std::thread::spawn({
        let pane_id = pane_id.to_string();
        let pane_instance = pane_instance.clone();
        let expected = expected.clone();
        move || {
            let handshake = |stream: &mut std::os::unix::net::UnixStream| {
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(
                    serde_json::from_str::<ClientMessage>(line.trim()).unwrap(),
                    ClientMessage::Hello {
                        proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                    }
                );
                serde_json::to_writer(
                    &mut *stream,
                    &ServerMessage::HelloAck {
                        proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                        daemon_instance_id: daemon_instance_id.clone(),
                        server_identity: server_identity.clone(),
                        phase: DaemonPhase::Serving,
                        hook_health: HookHealth::Healthy,
                    },
                )
                .unwrap();
                stream.write_all(b"\n").unwrap();
                reader
            };

            let (mut query_stream, mut query_reader, mut line) = loop {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = handshake(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.is_empty() {
                    continue;
                }
                break (stream, reader, line);
            };
            assert_eq!(
                serde_json::from_str::<ClientMessage>(line.trim()).unwrap(),
                ClientMessage::QueryPane {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                    pane_id: pane_id.clone(),
                }
            );
            serde_json::to_writer(
                &mut query_stream,
                &ServerMessage::PaneResult {
                    snapshot_revision: 7,
                    pane: PanePresentation {
                        pane_instance: pane_instance.clone(),
                        session_links: Vec::new(),
                        window_id: "@1".to_string(),
                        window_name: "main".to_string(),
                        current_path: "/tmp".to_string(),
                        current_command: "node".to_string(),
                        pane_width: 80,
                        active: true,
                        stored: Some(expected.clone()),
                        resolved: None,
                        diagnostic: None,
                    },
                },
            )
            .unwrap();
            query_stream.write_all(b"\n").unwrap();

            // The query connection must be closed before the mutation handshake begins.
            line.clear();
            query_reader.read_line(&mut line).unwrap();
            assert!(
                line.is_empty(),
                "reset reused the QueryPane connection: {line}"
            );

            let (mut reset_stream, _reset_reader, line) = loop {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = handshake(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.is_empty() {
                    continue;
                }
                break (stream, reader, line);
            };
            let request = serde_json::from_str::<ClientMessage>(line.trim()).unwrap();
            let ClientMessage::ResetPaneState {
                proto,
                daemon_instance_id: request_instance_id,
                event_id,
                pane_instance: request_pane,
                expected: request_expected,
            } = request
            else {
                panic!("expected ResetPaneState, got {request:?}");
            };
            assert_eq!(proto, crate::daemon::protocol::v2::PROTOCOL_VERSION);
            assert_eq!(request_instance_id, daemon_instance_id);
            assert_eq!(request_pane, pane_instance);
            assert_eq!(request_expected, expected);
            serde_json::to_writer(
                &mut reset_stream,
                &ServerMessage::ResetResult {
                    event_id,
                    accepted_seq: 8,
                    previous: expected.clone(),
                    current: expected,
                    outcome: ResetOutcome::AlreadyReset,
                    snapshot_revision: 8,
                },
            )
            .unwrap();
            reset_stream.write_all(b"\n").unwrap();
        }
    });
    V2QueryFixture {
        mock,
        env,
        daemon_socket,
        root,
        tmux_listener,
        server,
    }
}

fn window_row(
    session: &str,
    index: &str,
    id: &str,
    name: &str,
    panes: &str,
    active: &str,
    command: &str,
) -> String {
    [
        session, index, id, name, panes, active, "0", "0", "0", "0", command, "", "", "",
    ]
    .join("\u{1f}")
}

mod hook;
mod sidebar;

#[test]
fn pane_state_commands_use_the_documented_command_tree() {
    let cleanup = Cli::try_parse_from(["vt", "pane-state", "cleanup-legacy", "--all"])
        .expect("cleanup command should parse");
    assert!(matches!(
        cleanup.command,
        Command::PaneState {
            command: PaneStateCommand::CleanupLegacy {
                all: true,
                dry_run: false,
            }
        }
    ));
    assert!(Cli::try_parse_from(["vt", "pane-state", "cleanup-legacy"]).is_err());
    assert!(matches!(
        Cli::try_parse_from(["vt", "pane-state", "cleanup-legacy", "--all", "--dry-run",])
            .unwrap()
            .command,
        Command::PaneState {
            command: PaneStateCommand::CleanupLegacy {
                all: true,
                dry_run: true,
            }
        }
    ));

    let reset = Cli::try_parse_from(["vt", "pane-state", "reset", "--target", "%31"])
        .expect("reset command should parse");
    let Command::PaneState {
        command: PaneStateCommand::Reset { target },
    } = reset.command
    else {
        panic!("expected pane-state reset command");
    };
    assert_eq!(target, "%31");

    let uninstall = Cli::try_parse_from(["vt", "pane-state", "hooks", "uninstall"])
        .expect("hook uninstall command should parse");
    assert!(matches!(
        uninstall.command,
        Command::PaneState {
            command: PaneStateCommand::Hooks {
                command: PaneStateHooksCommand::Uninstall
            }
        }
    ));
}

#[test]
fn daemon_lifecycle_commands_parse_with_force_scoped_to_stop() {
    for command in [
        "ensure", "start", "disable", "enable", "status", "doctor", "reload", "logs",
    ] {
        Cli::try_parse_from(["vt", "daemon", command])
            .unwrap_or_else(|error| panic!("daemon {command} must parse: {error}"));
    }
    assert!(matches!(
        Cli::try_parse_from(["vt", "daemon", "stop", "--force"])
            .unwrap()
            .command,
        Command::Daemon {
            command: Some(DaemonCommand::Stop { force: true }),
            ..
        }
    ));
    assert!(Cli::try_parse_from(["vt", "daemon", "start", "--force"]).is_err());
    assert!(Cli::try_parse_from(["vt", "daemon", "logs", "--lines", "501"]).is_err());
}

#[test]
fn dispatch_pane_state_reset_uses_separate_query_and_mutation_connections() {
    let fixture = spawn_v2_reset_fixture("%31");

    let output = run_with(
        ["vt", "pane-state", "reset", "--target", "%31"],
        &fixture.mock,
        &fixture.env,
    )
    .unwrap()
    .unwrap();

    assert_eq!(output, "pane state reset: %31 (already reset)");
    fixture.finish();
}

#[test]
fn dispatch_statusline_sessions_prints_output() {
    let fixture = status_query_fixture(crate::daemon::protocol::v2::StatusContext::Session {
        session_id: "$1".to_string(),
    });
    let output = run_with(
        ["vt", "statusline-sessions", "--session-id", "$1"],
        &fixture.mock,
        &fixture.env,
    );
    assert!(output.unwrap().unwrap().contains("main"));
    fixture.finish();
}

#[test]
fn dispatch_statusline_sessions_show_index_overrides_config() {
    let fixture = status_query_fixture(crate::daemon::protocol::v2::StatusContext::Session {
        session_id: "$1".to_string(),
    });

    let output = run_with(
        [
            "vt",
            "statusline-sessions",
            "--session-id",
            "$1",
            "--show-index",
        ],
        &fixture.mock,
        &fixture.env,
    )
    .unwrap()
    .unwrap();

    assert!(output.contains("1: main"));
    fixture.finish();
}

#[test]
fn daemon_status_reports_handshake_without_sending_a_mutation() {
    let fixture = spawn_v2_handshake_fixture();

    let output = run_with(["vt", "daemon", "status"], &fixture.mock, &fixture.env)
        .unwrap()
        .unwrap();

    assert!(output.contains("daemon: running"));
    assert!(output.contains("phase: Serving"));
    assert!(output.contains("hooks: Degraded"));
    assert!(output.contains("daemon.log"));
    fixture.finish();
}

#[test]
fn dispatch_statusline_sessions_switch_missing_index_is_an_error_without_wrong_target() {
    let mock = MockTmuxRunner::new();
    stub_action_client(&mock, "client-1", "$1");
    mock.stub(
        &[
            "show-option",
            "-qv",
            "-t",
            "$1",
            crate::options::KEY_STATUS_SESSIONS,
        ],
        "#[range=user|session:$1] main #[norange]",
    );

    let error = run_with(
        [
            "vt",
            "statusline-sessions",
            "--session-id",
            "$1",
            "switch",
            "2",
        ],
        &mock,
        &tmux_env(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("no longer available"));
    assert!(
        mock.calls()
            .iter()
            .all(|call| call.first().map(String::as_str) != Some("switch-client"))
    );
}

#[test]
fn dispatch_session_cycle_resolves_current_session_id_for_existing_key_bindings() {
    let rendered = (1..=6)
        .map(|index| format!("#[range=user|session:${index}] session-{index} #[norange]"))
        .collect::<String>();
    for (command, expected) in [("next", "$4"), ("prev", "$2")] {
        let mock = MockTmuxRunner::new();
        stub_action_client(&mock, "client-1", "$3");
        mock.stub(
            &[
                "show-option",
                "-qv",
                "-t",
                "$3",
                crate::options::KEY_STATUS_SESSIONS,
            ],
            &rendered,
        );
        mock.stub(&["switch-client", "-c", "client-1", "-t", expected], "");

        run_with(["vt", "session-cycle", command], &mock, &tmux_env()).unwrap();

        assert!(mock.calls().iter().any(|call| {
            call == &vec![
                "switch-client".to_string(),
                "-c".to_string(),
                "client-1".to_string(),
                "-t".to_string(),
                expected.to_string(),
            ]
        }));
    }
}

#[test]
fn dispatch_session_cycle_uses_the_explicit_client_when_a_pane_is_shared() {
    let mock = MockTmuxRunner::new();
    let format = crate::session::client_session_context_format();
    let sep = '\u{1f}';
    mock.stub(
        &["list-clients", "-F", &format],
        &format!(
            "client-1{sep}/dev/ttys001{sep}$1{sep}%1{sep}0\n\
             client-2{sep}/dev/ttys002{sep}$2{sep}%1{sep}0\n"
        ),
    );
    mock.stub(
        &[
            "show-option",
            "-qv",
            "-t",
            "$2",
            crate::options::KEY_STATUS_SESSIONS,
        ],
        "#[range=user|session:$1] one #[norange]#[range=user|session:$2] two #[norange]#[range=user|session:$3] three #[norange]",
    );
    mock.stub(&["switch-client", "-c", "client-2", "-t", "$3"], "");

    run_with(
        [
            "vt",
            "session-cycle",
            "next",
            "--client-name",
            "client-2",
            "--session-id",
            "$2",
        ],
        &mock,
        &tmux_env(),
    )
    .unwrap();

    assert!(mock.calls().iter().any(|call| {
        call == &vec![
            "switch-client".to_string(),
            "-c".to_string(),
            "client-2".to_string(),
            "-t".to_string(),
            "$3".to_string(),
        ]
    }));
}

#[test]
fn dispatch_statusline_session_switch_uses_explicit_scope_when_a_pane_is_shared() {
    let mock = MockTmuxRunner::new();
    stub_shared_action_clients(&mock);
    mock.stub(
        &[
            "show-option",
            "-qv",
            "-t",
            "$2",
            crate::options::KEY_STATUS_SESSIONS,
        ],
        "#[range=user|session:$1] one #[norange]\
         #[range=user|session:$2] two #[norange]\
         #[range=user|session:$3] three #[norange]",
    );
    mock.stub(&["switch-client", "-c", "client-2", "-t", "$3"], "");

    run_with(
        [
            "vt",
            "statusline-sessions",
            "--client-name",
            "client-2",
            "--session-id",
            "$2",
            "switch",
            "3",
        ],
        &mock,
        &tmux_env(),
    )
    .unwrap();

    assert!(mock.calls().iter().any(|call| {
        call == &vec![
            "switch-client".to_string(),
            "-c".to_string(),
            "client-2".to_string(),
            "-t".to_string(),
            "$3".to_string(),
        ]
    }));
}

#[test]
fn dispatch_session_actions_fail_closed_when_shared_pane_scope_is_omitted() {
    for args in [
        vec!["vt", "session-cycle", "next"],
        vec!["vt", "session-cycle", "prev"],
        vec!["vt", "statusline-sessions", "switch", "1"],
    ] {
        let mock = MockTmuxRunner::new();
        stub_shared_action_clients(&mock);

        let error = run_with(args, &mock, &tmux_env()).unwrap_err();

        assert!(error.to_string().contains("multiple tmux clients"));
        assert!(mock.calls().iter().all(|call| !matches!(
            call.first().map(String::as_str),
            Some("show-option" | "switch-client")
        )));
    }
}

#[test]
fn dispatch_category_cycle_uses_explicit_scope_when_a_pane_is_shared() {
    let alpha = crate::statusline::encode_category_key("alpha").unwrap();
    let beta = crate::statusline::encode_category_key("beta").unwrap();
    let gamma = crate::statusline::encode_category_key("gamma").unwrap();
    let rendered = format!(
        "#[range=user|category:{alpha}] alpha #[norange]\
         #[range=user|category-current:{beta}] beta #[norange]\
         #[range=user|category:{gamma}] gamma #[norange]"
    );

    for (command, target_category, target_session) in
        [("next", "gamma", "c"), ("prev", "alpha", "a")]
    {
        let mut fixture = spawn_active_config_guard_fixture();
        fixture
            .env
            .insert("TMUX_PANE".to_string(), "%1".to_string());
        let mock = &fixture.mock;
        stub_shared_action_clients(mock);
        mock.stub(
            &[
                "show-option",
                "-qv",
                "-t",
                "$2",
                crate::options::KEY_STATUS_CATEGORY,
            ],
            &rendered,
        );
        stub_category_switch(mock, "client-2", target_category, target_session);

        run_with(
            [
                "vt",
                "category",
                command,
                "--client-name",
                "client-2",
                "--session-id",
                "$2",
            ],
            mock,
            &fixture.env,
        )
        .unwrap();

        let exact_target = crate::session::exact_session_target(target_session);
        assert!(mock.calls().iter().any(|call| {
            call == &vec![
                "switch-client".to_string(),
                "-c".to_string(),
                "client-2".to_string(),
                "-t".to_string(),
                exact_target.clone(),
            ]
        }));
        fixture.finish();
    }
}

#[test]
fn dispatch_category_click_uses_explicit_scope_when_a_pane_is_shared() {
    let mut fixture = spawn_active_config_guard_fixture();
    fixture
        .env
        .insert("TMUX_PANE".to_string(), "%1".to_string());
    let mock = &fixture.mock;
    stub_shared_action_clients(mock);
    stub_category_switch(mock, "client-2", "alpha", "a");
    let alpha = crate::statusline::encode_category_key("alpha").unwrap();

    run_with(
        [
            "vt",
            "statusline-click",
            "--client-name",
            "client-2",
            "--session-id",
            "$2",
            &format!("category:{alpha}"),
        ],
        mock,
        &fixture.env,
    )
    .unwrap();

    assert!(mock.calls().iter().any(|call| {
        call == &vec![
            "switch-client".to_string(),
            "-c".to_string(),
            "client-2".to_string(),
            "-t".to_string(),
            "=a:".to_string(),
        ]
    }));
    fixture.finish();
}

#[test]
fn dispatch_category_actions_fail_closed_when_shared_pane_scope_is_omitted() {
    for args in [
        vec!["vt", "category", "next"],
        vec!["vt", "category", "prev"],
        vec!["vt", "statusline-click", "category:YWxwaGE"],
    ] {
        let mock = MockTmuxRunner::new();
        stub_shared_action_clients(&mock);

        let error = run_with(args, &mock, &tmux_env()).unwrap_err();

        assert!(error.to_string().contains("multiple tmux clients"));
        assert!(mock.calls().iter().all(|call| !matches!(
            call.first().map(String::as_str),
            Some("show-option" | "list-sessions" | "switch-client" | "set-option")
        )));
    }
}

#[test]
fn dispatch_statusline_session_switch_resolves_current_session_id() {
    let mock = MockTmuxRunner::new();
    stub_action_client(&mock, "client-1", "$3");
    mock.stub(
        &[
            "show-option",
            "-qv",
            "-t",
            "$3",
            crate::options::KEY_STATUS_SESSIONS,
        ],
        "#[range=user|session:$1] one #[norange]#[range=user|session:$2] two #[norange]",
    );
    mock.stub(&["switch-client", "-c", "client-1", "-t", "$2"], "");

    run_with(
        ["vt", "statusline-sessions", "switch", "2"],
        &mock,
        &tmux_env(),
    )
    .unwrap();

    assert!(mock.calls().iter().any(|call| {
        call == &vec![
            "switch-client".to_string(),
            "-c".to_string(),
            "client-1".to_string(),
            "-t".to_string(),
            "$2".to_string(),
        ]
    }));
}

#[test]
fn session_action_rejects_an_explicit_source_that_differs_from_the_pinned_client() {
    let mock = MockTmuxRunner::new();
    stub_action_client(&mock, "client-1", "$3");

    let error = run_with(
        ["vt", "session-cycle", "next", "--session-id", "$2"],
        &mock,
        &tmux_env(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("does not match invoking client"));
    assert!(mock.calls().iter().all(|call| !matches!(
        call.first().map(String::as_str),
        Some("show-option" | "switch-client")
    )));
}

#[test]
fn dispatch_statusline_windows_prints_output() {
    let fixture = status_query_fixture(crate::daemon::protocol::v2::StatusContext::Session {
        session_id: "$1".to_string(),
    });

    let output = run_with(
        ["vt", "statusline-windows", "--session-id", "$1"],
        &fixture.mock,
        &fixture.env,
    )
    .unwrap()
    .unwrap();

    assert!(output.contains("#[range=user|window:@1]"), "{output}");
    assert!(output.contains("1:zsh"), "{output}");
    assert!(output.contains("2:editor"), "{output}");
    fixture.finish();
}

#[test]
fn dispatch_statusline_pane_prints_pane_segment() {
    let fixture = pane_query_fixture("%1");

    let output = run_with(
        ["vt", "statusline-pane", "--target", "%1"],
        &fixture.mock,
        &fixture.env,
    )
    .unwrap()
    .unwrap();

    assert!(output.contains("%1"), "{output}");
    assert!(output.contains("Codex"), "{output}");
    assert!(output.contains("running"), "{output}");
    assert!(output.contains("#[fg=#4fd08a]●"), "{output}");
    fixture.finish();
}

#[test]
fn dispatch_statusline_windows_switch_selects_window() {
    let mock = MockTmuxRunner::new();
    mock.stub(&["select-window", "-t", "@2"], "");

    run_with(["vt", "statusline-windows", "switch", "@2"], &mock, &env()).unwrap();

    assert_eq!(
        mock.calls(),
        vec![vec![
            "select-window".to_string(),
            "-t".to_string(),
            "@2".to_string()
        ]]
    );
}

#[test]
fn dispatch_statusline_click_routes_window_range() {
    let mock = MockTmuxRunner::new();
    mock.stub(&["select-window", "-t", "@2"], "");

    run_with(["vt", "statusline-click", "window:@2"], &mock, &env()).unwrap();

    assert_eq!(
        mock.calls(),
        vec![vec![
            "select-window".to_string(),
            "-t".to_string(),
            "@2".to_string()
        ]]
    );
}

#[test]
fn dispatch_statusline_click_routes_session_range() {
    let mock = MockTmuxRunner::new();
    stub_action_client(&mock, "client-1", "$1");
    mock.stub(&["switch-client", "-c", "client-1", "-t", "$2"], "");

    run_with(["vt", "statusline-click", "session:$2"], &mock, &tmux_env()).unwrap();

    assert!(mock.calls().iter().any(|call| {
        call == &vec![
            "switch-client".to_string(),
            "-c".to_string(),
            "client-1".to_string(),
            "-t".to_string(),
            "$2".to_string(),
        ]
    }));
}

#[test]
fn dispatch_statusline_click_routes_stable_category_target() {
    let mut fixture = spawn_active_config_guard_fixture();
    fixture
        .env
        .insert("TMUX_PANE".to_string(), "%1".to_string());
    let mock = &fixture.mock;
    stub_action_client(mock, "abc", "$1");
    let format = crate::session::session_list_format();
    mock.stub(
        &["list-sessions", "-F", &format],
        "a\u{1f}1\u{1f}100\u{1f}alpha\u{1f}\u{1f}\u{1f}$1\nb\u{1f}1\u{1f}100\u{1f}beta\u{1f}\u{1f}\u{1f}$2\n",
    );
    mock.stub(&["show-option", "-gqv", "@vde_client_616263_beta"], "");
    mock.stub(&["switch-client", "-c", "abc", "-t", "=b:"], "");
    mock.stub(&["set-option", "-g", "@vde_client_616263_beta", "b"], "");

    run_with(
        ["vt", "statusline-click", "category:YmV0YQ"],
        mock,
        &fixture.env,
    )
    .unwrap();

    assert!(
        mock.calls()
            .iter()
            .any(|call| call == &vec!["switch-client", "-c", "abc", "-t", "=b:"])
    );
    fixture.finish();
}

#[test]
fn dispatch_statusline_click_ignores_empty_zero_and_unknown_ranges() {
    for range in ["", "0", "pane:%1"] {
        let mock = MockTmuxRunner::new();

        run_with(["vt", "statusline-click", range], &mock, &env()).unwrap();

        assert!(mock.calls().is_empty(), "{range}");
    }
}

#[test]
fn dispatch_category_use_switches_category() {
    let mut fixture = spawn_active_config_guard_fixture();
    fixture
        .env
        .insert("TMUX_PANE".to_string(), "%1".to_string());
    let mock = &fixture.mock;
    stub_action_client(mock, "abc", "$1");
    let format = crate::session::session_list_format();
    mock.stub(
        &["list-sessions", "-F", &format],
        "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}$1\n",
    );
    mock.stub(&["show-option", "-gqv", "@vde_client_616263_work"], "");
    mock.stub(&["switch-client", "-c", "abc", "-t", "=main:"], "");
    mock.stub(&["set-option", "-g", "@vde_client_616263_work", "main"], "");
    run_with(["vt", "category", "use", "work"], mock, &fixture.env).unwrap();
    assert!(
        mock.calls()
            .iter()
            .any(|call| { call == &vec!["switch-client", "-c", "abc", "-t", "=main:"] })
    );
    fixture.finish();
}

#[test]
fn dispatch_session_new_creates_managed_session() {
    let mut fixture = spawn_active_config_guard_fixture();
    fixture
        .env
        .insert("TMUX_PANE".to_string(), "%1".to_string());
    let mock = &fixture.mock;
    stub_action_client(mock, "abc", "$1");
    mock.stub(
        &[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{session_name}\u{1f}#{window_id}",
            "-c",
            "/tmp/repo",
        ],
        "repo\u{1f}@9\n",
    );
    mock.stub(
        &[
            "set-option",
            "-t",
            "repo",
            crate::options::KEY_PROJECT_PATH,
            "/tmp/repo",
        ],
        "",
    );
    mock.stub(
        &["set-option", "-t", "repo", crate::options::KEY_CATEGORY, ""],
        "",
    );
    mock.stub(&["switch-client", "-c", "abc", "-t", "=repo:"], "");
    mock.stub(
        &["show-hooks", "-g", "after-new-window[90]"],
        "after-new-window[90] \n",
    );

    run_with(
        ["vt", "session", "new", "-c", "/tmp/repo"],
        mock,
        &fixture.env,
    )
    .unwrap();

    assert!(
        mock.calls()
            .iter()
            .any(|call| { call.first().map(String::as_str) == Some("new-session") })
    );
    fixture.finish();
}

#[test]
fn dispatch_session_new_fails_closed_when_shared_pane_scope_is_omitted() {
    let mock = MockTmuxRunner::new();
    stub_shared_action_clients(&mock);

    let error = run_with(
        ["vt", "session", "new", "-c", "/tmp/repo"],
        &mock,
        &tmux_env(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("multiple tmux clients"));
    assert!(mock.calls().iter().all(|call| !matches!(
        call.first().map(String::as_str),
        Some("new-session" | "switch-client")
    )));
}

#[test]
fn dispatch_session_new_uses_explicit_scope_when_a_pane_is_shared() {
    let mut fixture = spawn_active_config_guard_fixture();
    fixture
        .env
        .insert("TMUX_PANE".to_string(), "%1".to_string());
    let mock = &fixture.mock;
    stub_shared_action_clients(mock);
    mock.stub(
        &[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{session_name}\u{1f}#{window_id}",
            "-c",
            "/tmp/repo",
        ],
        "repo\u{1f}@9\n",
    );
    mock.stub(
        &[
            "set-option",
            "-t",
            "repo",
            crate::options::KEY_PROJECT_PATH,
            "/tmp/repo",
        ],
        "",
    );
    mock.stub(
        &["set-option", "-t", "repo", crate::options::KEY_CATEGORY, ""],
        "",
    );
    mock.stub(&["switch-client", "-c", "client-2", "-t", "=repo:"], "");
    mock.stub(
        &["show-hooks", "-g", "after-new-window[90]"],
        "after-new-window[90] \n",
    );

    run_with(
        [
            "vt",
            "session",
            "new",
            "-c",
            "/tmp/repo",
            "--client-name",
            "client-2",
            "--session-id",
            "$2",
        ],
        mock,
        &fixture.env,
    )
    .unwrap();

    assert!(mock.calls().iter().any(|call| {
        call == &vec![
            "switch-client".to_string(),
            "-c".to_string(),
            "client-2".to_string(),
            "-t".to_string(),
            "=repo:".to_string(),
        ]
    }));
    fixture.finish();
}

#[test]
fn dispatch_project_selector_popup_opens_popup() {
    let mock = MockTmuxRunner::new();
    let exe = std::env::current_exe().unwrap().display().to_string();
    let command = crate::project::project_selector_popup_command(&exe);
    mock.stub(
        &[
            "display-popup",
            "-E",
            "-w",
            "50%",
            "-h",
            "50%",
            "-d",
            "#{pane_current_path}",
            &command,
        ],
        "",
    );

    run_with(["vt", "project", "selector", "--popup"], &mock, &env()).unwrap();

    assert_eq!(mock.calls().len(), 1);
}

#[test]
fn dispatch_session_manager_opens_popup() {
    let mock = MockTmuxRunner::new();
    let exe = std::env::current_exe().unwrap().display().to_string();
    mock.stub(
        &["display-message", "-p", "#{pane_current_path}"],
        "/tmp/project\n",
    );
    mock.stub(
        &[
            "display-popup",
            "-E",
            "-w",
            "50%",
            "-h",
            "50%",
            "-d",
            "/tmp/project",
            &exe,
            "session-manager",
            "--popup",
        ],
        "",
    );

    run_with(["vt", "session-manager"], &mock, &tmux_env()).unwrap();

    assert_eq!(mock.calls().len(), 2);
}

#[test]
fn session_manager_popup_wrap_is_used_only_inside_tmux() {
    assert!(!should_wrap_session_manager_in_popup(&env()));
    assert!(should_wrap_session_manager_in_popup(&BTreeMap::from([(
        "TMUX".to_string(),
        "/tmp/tmux-501/default,1,0".to_string(),
    )])));
    assert!(should_wrap_session_manager_in_popup(&BTreeMap::from([(
        "TMUX_PANE".to_string(),
        "%1".to_string(),
    )])));
}

#[test]
fn dispatch_popups_use_configured_size() {
    let config_home = std::env::temp_dir().join(format!(
        "vde-tmux-popup-size-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let config_dir = config_home.join("vde").join("tmux");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.yml"),
        "popup:\n  width: \"72%\"\n  height: \"60%\"\n",
    )
    .unwrap();
    let env = BTreeMap::from([
        (
            "XDG_CONFIG_HOME".to_string(),
            config_home.display().to_string(),
        ),
        ("TMUX_PANE".to_string(), "%1".to_string()),
    ]);
    let exe = std::env::current_exe().unwrap().display().to_string();

    let session_mock = MockTmuxRunner::new();
    session_mock.stub(
        &["display-message", "-p", "#{pane_current_path}"],
        "/tmp/project\n",
    );
    session_mock.stub(
        &[
            "display-popup",
            "-E",
            "-w",
            "72%",
            "-h",
            "60%",
            "-d",
            "/tmp/project",
            &exe,
            "session-manager",
            "--popup",
        ],
        "",
    );
    run_with(["vt", "session-manager"], &session_mock, &env).unwrap();

    let project_mock = MockTmuxRunner::new();
    let command = crate::project::project_selector_popup_command(&exe);
    project_mock.stub(
        &[
            "display-popup",
            "-E",
            "-w",
            "72%",
            "-h",
            "60%",
            "-d",
            "#{pane_current_path}",
            &command,
        ],
        "",
    );
    run_with(
        ["vt", "project", "selector", "--popup"],
        &project_mock,
        &env,
    )
    .unwrap();

    std::fs::remove_dir_all(config_home).unwrap();
}

#[test]
fn dispatch_session_manager_renders_preview() {
    let mock = MockTmuxRunner::new();
    let session_format = crate::session::session_list_format();
    let window_format = crate::window::window_list_format();
    mock.stub(
        &["list-sessions", "-F", &session_format],
        "ni.zsh\u{1f}1\u{1f}100\u{1f}public\u{1f}\u{1f}\u{1f}$2\n",
    );
    mock.stub(
        &["list-windows", "-t", "=ni.zsh:", "-F", &window_format],
        &format!(
            "{}\n",
            window_row("ni.zsh", "2", "@9", "editor", "2", "1", "nvim")
        ),
    );
    mock.stub(
        &[
            "display-message",
            "-p",
            "-t",
            "=ni.zsh:",
            "#{pane_current_path}",
        ],
        "/tmp/project\n",
    );
    mock.stub(
        &["capture-pane", "-epJ", "-t", "=ni.zsh:", "-S", "-30"],
        "tail\n",
    );

    let output = run_with(
        [
            "vt",
            "session-manager",
            "--popup",
            "--render-preview",
            "session",
            "--preview-name",
            "ni.zsh",
        ],
        &mock,
        &env(),
    )
    .unwrap()
    .unwrap();

    assert!(output.contains("Session ni.zsh"));
    assert!(output.contains("tail"));
}

#[test]
fn config_hash_guard_rejects_empty_and_mismatched_active_hashes_with_reload_guidance() {
    assert!(super::verify_active_config_hash("same", "same").is_ok());
    for active in ["", "different"] {
        let error = super::verify_active_config_hash("disk", active).unwrap_err();
        assert!(error.to_string().contains("vt daemon reload"));
    }
}

#[test]
fn invalid_config_blocks_category_mutation_before_daemon_or_session_queries() {
    let root = std::env::temp_dir().join(format!(
        "vde-invalid-config-{}-{}",
        std::process::id(),
        V2_QUERY_FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let config_dir = root.join("vde/tmux");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.yml"),
        "daemon:\n  poll_ms: [broken\n",
    )
    .unwrap();
    let env = BTreeMap::from([
        ("XDG_CONFIG_HOME".to_string(), root.display().to_string()),
        ("TMUX_PANE".to_string(), "%1".to_string()),
    ]);
    let mock = MockTmuxRunner::new();
    stub_action_client(&mock, "abc", "$1");

    let error = run_with(["vt", "category", "use", "work"], &mock, &env).unwrap_err();

    assert!(error.to_string().contains("vt daemon reload"));
    assert!(mock.calls().iter().all(|call| {
        !matches!(
            call.first().map(String::as_str),
            Some("list-sessions" | "switch-client" | "set-option" | "display-message")
        )
    }));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_statusline_summary_renders_v2_status_snapshot() {
    let fixture = status_query_fixture(crate::daemon::protocol::v2::StatusContext::Global);
    let output = run_with(["vt", "statusline-summary"], &fixture.mock, &fixture.env).unwrap();
    assert_eq!(output, Some("#[fg=#4fd08a]●1#[default]".to_string()));
    fixture.finish();
}

#[test]
fn dispatch_statusline_attention_renders_v2_session_snapshot() {
    let fixture = status_query_fixture(crate::daemon::protocol::v2::StatusContext::Session {
        session_id: "$1".to_string(),
    });

    let output = run_with(
        ["vt", "statusline-attention", "--session-id", "$1"],
        &fixture.mock,
        &fixture.env,
    )
    .unwrap();

    let text = output.unwrap();
    assert!(text.contains("▲ main · perm 2m00s"), "{text}");
    fixture.finish();
}

#[test]
fn dispatch_statusline_summary_is_empty_when_disabled() {
    let config_home = std::env::temp_dir().join(format!(
        "vde-tmux-summary-disabled-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let config_dir = config_home.join("vde").join("tmux");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.yml"),
        "statusline:\n  summary:\n    enabled: false\n",
    )
    .unwrap();
    let mut fixture = status_query_fixture(crate::daemon::protocol::v2::StatusContext::Global);
    fixture.env.insert(
        "XDG_CONFIG_HOME".to_string(),
        config_home.display().to_string(),
    );

    let output = run_with(["vt", "statusline-summary"], &fixture.mock, &fixture.env).unwrap();

    assert_eq!(output, Some(String::new()));
    fixture.finish();
    std::fs::remove_dir_all(config_home).unwrap();
}

#[test]
fn dispatch_config_schema_prints_json_schema() {
    let mock = MockTmuxRunner::new();

    let output = run_with(["vt", "config", "schema"], &mock, &env()).unwrap();
    let schema: serde_json::Value = serde_json::from_str(&output.unwrap()).unwrap();

    assert_eq!(
        schema.get("$schema").and_then(|value| value.as_str()),
        Some("https://json-schema.org/draft/2020-12/schema")
    );
    assert!(schema["properties"].get("sidebar").is_some());
}

#[test]
fn config_warning_is_written_to_stderr_without_polluting_statusline_stdout() {
    let config_home = std::env::temp_dir().join(format!(
        "vde-tmux-broken-config-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let config_dir = config_home.join("vde").join("tmux");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.yml"),
        "daemon:\n  poll_ms: [broken\n",
    )
    .unwrap();

    let mut fixture = status_query_fixture(crate::daemon::protocol::v2::StatusContext::Session {
        session_id: "$1".to_string(),
    });
    fixture.env.insert(
        "XDG_CONFIG_HOME".to_string(),
        config_home.display().to_string(),
    );

    let mut stderr = Vec::new();
    let output = run_with_input_at_writing_warnings(
        ["vt", "statusline-category", "--session-id", "$1"],
        "",
        &fixture.mock,
        &fixture.env,
        0,
        &mut stderr,
    )
    .unwrap()
    .unwrap();

    let stderr = String::from_utf8(stderr).unwrap();
    assert!(stderr.contains("vde-tmux config warning: invalid config"));
    assert!(output.contains("work"));
    assert!(!output.contains("invalid config"));
    fixture.finish();
    std::fs::remove_dir_all(config_home).unwrap();
}

#[test]
fn statusline_cli_index_rejects_zero_instead_of_aliasing_the_first_item() {
    assert_eq!(super::cli_index(1).unwrap(), 0);
    assert_eq!(super::cli_index(2).unwrap(), 1);
    assert!(
        super::cli_index(0)
            .unwrap_err()
            .to_string()
            .contains("1 or greater")
    );
}
