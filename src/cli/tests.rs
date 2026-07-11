use super::*;
use crate::tmux::mock::MockTmuxRunner;
use std::collections::BTreeMap;

fn env() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn tmux_env() -> BTreeMap<String, String> {
    BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())])
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
        std::fs::remove_file(&self.daemon_socket).unwrap();
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
        "vde-cli-status-v2-{}-{}",
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
            command: PaneStateCommand::CleanupLegacy { all: true }
        }
    ));
    assert!(Cli::try_parse_from(["vt", "pane-state", "cleanup-legacy"]).is_err());

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
fn dispatch_statusline_sessions_switch_missing_index_is_noop() {
    let mock = MockTmuxRunner::new();
    let format = crate::session::session_list_format();
    mock.stub(
        &["list-sessions", "-F", &format],
        "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\u{1f}$1\n",
    );
    mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");

    run_with(["vt", "statusline-sessions", "switch", "2"], &mock, &env()).unwrap();

    assert!(
        mock.calls()
            .iter()
            .all(|call| call.first().map(String::as_str) != Some("switch-client"))
    );
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
    mock.stub(&["switch-client", "-t", "$2"], "");

    run_with(["vt", "statusline-click", "session:$2"], &mock, &env()).unwrap();

    assert_eq!(
        mock.calls(),
        vec![vec![
            "switch-client".to_string(),
            "-t".to_string(),
            "$2".to_string()
        ]]
    );
}

#[test]
fn dispatch_statusline_click_routes_category_index() {
    let mock = MockTmuxRunner::new();
    let format = crate::session::session_list_format();
    mock.stub(
        &["list-sessions", "-F", &format],
        "a\u{1f}1\u{1f}100\u{1f}alpha\u{1f}\u{1f}\u{1f}$1\nb\u{1f}1\u{1f}100\u{1f}beta\u{1f}\u{1f}\u{1f}$2\n",
    );
    mock.stub(
        &["display-message", "-p", "#{client_name}\t#{client_tty}"],
        "abc\t/dev/ttys001\n",
    );
    mock.stub(&["show-option", "-gqv", "@vde_client_616263_beta"], "");
    mock.stub(&["switch-client", "-c", "abc", "-t", "=b:"], "");
    mock.stub(&["set-option", "-g", "@vde_client_616263_beta", "b"], "");

    run_with(["vt", "statusline-click", "2"], &mock, &env()).unwrap();

    assert!(
        mock.calls()
            .iter()
            .any(|call| call == &vec!["switch-client", "-c", "abc", "-t", "=b:"])
    );
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
    let mock = MockTmuxRunner::new();
    let format = crate::session::session_list_format();
    mock.stub(
        &["display-message", "-p", "#{client_name}\t#{client_tty}"],
        "abc\t/dev/ttys001\n",
    );
    mock.stub(
        &["list-sessions", "-F", &format],
        "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}$1\n",
    );
    mock.stub(&["show-option", "-gqv", "@vde_client_616263_work"], "");
    mock.stub(&["switch-client", "-c", "abc", "-t", "=main:"], "");
    mock.stub(&["set-option", "-g", "@vde_client_616263_work", "main"], "");
    run_with(["vt", "category", "use", "work"], &mock, &env()).unwrap();
    assert_eq!(mock.calls().len(), 5);
}

#[test]
fn dispatch_session_new_creates_managed_session() {
    let mock = MockTmuxRunner::new();
    mock.stub(
        &["display-message", "-p", "#{client_name}\t#{client_tty}"],
        "abc\t/dev/ttys001\n",
    );
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

    run_with(["vt", "session", "new", "-c", "/tmp/repo"], &mock, &env()).unwrap();

    assert_eq!(mock.calls().len(), 6);
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
    assert!(text.contains("▲ main · perm 2m"), "{text}");
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
