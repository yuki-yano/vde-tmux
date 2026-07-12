use super::*;

static SIDEBAR_SOCKET_COUNTER: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn spawn_sidebar_snapshot_server(
    sidebar_model: crate::daemon::SidebarModel,
) -> (std::path::PathBuf, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, Write};

    let socket = std::env::temp_dir().join(format!(
        "vt2-cli-{}-{}.sock",
        std::process::id(),
        SIDEBAR_SOCKET_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(
            serde_json::from_str::<crate::daemon::protocol::v2::ClientMessage>(line.trim())
                .unwrap(),
            crate::daemon::protocol::v2::ClientMessage::Hello { .. }
        ));
        serde_json::to_writer(
            &mut stream,
            &crate::daemon::protocol::v2::ServerMessage::HelloAck {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                daemon_instance_id: crate::pane_state::DaemonInstanceId::parse(
                    "ffeeddccbbaa99887766554433221100",
                )
                .unwrap(),
                server_identity: "scratch".to_string(),
                phase: crate::daemon::protocol::v2::DaemonPhase::Serving,
                hook_health: crate::daemon::protocol::v2::HookHealth::Healthy,
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<crate::daemon::protocol::v2::ClientMessage>(line.trim())
                .unwrap(),
            crate::daemon::protocol::v2::ClientMessage::QueryResolvedSnapshot {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            }
        );
        let snapshot = crate::daemon::protocol::v2::ResolvedSnapshot {
            snapshot_revision: 3,
            panes: Vec::new(),
            sidebar_model,
            attention: Vec::new(),
            events: Vec::new(),
            diagnostics: Vec::new(),
        };
        serde_json::to_writer(
            &mut stream,
            &crate::daemon::protocol::v2::ServerMessage::ResolvedSnapshotResult {
                snapshot_revision: 3,
                snapshot,
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
    });
    (socket, handle)
}

fn spawn_v2_sidebar_command_server() -> (
    std::path::PathBuf,
    std::sync::mpsc::Receiver<crate::daemon::protocol::v2::ClientMessage>,
    std::thread::JoinHandle<()>,
) {
    use std::io::{BufRead, Write};

    let socket = unique_socket_path("vt2-cmd");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let daemon_instance_id =
            crate::pane_state::DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap();
        serde_json::to_writer(
            &mut stream,
            &crate::daemon::protocol::v2::ServerMessage::HelloAck {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                daemon_instance_id,
                server_identity: "scratch".to_string(),
                phase: crate::daemon::protocol::v2::DaemonPhase::Serving,
                hook_health: crate::daemon::protocol::v2::HookHealth::Healthy,
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let message =
            serde_json::from_str::<crate::daemon::protocol::v2::ClientMessage>(line.trim())
                .unwrap();
        let event_id = message.event_id().cloned().unwrap();
        tx.send(message).unwrap();
        serde_json::to_writer(
            &mut stream,
            &crate::daemon::protocol::v2::ServerMessage::SnapshotAck {
                event_id,
                accepted_seq: 1,
                snapshot_revision: 2,
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
    });
    (socket, rx, handle)
}

#[test]
fn dispatch_sidebar_attach_once_marks_and_renders() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%9".to_string())]);
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%9",
            crate::options::KEY_SIDEBAR_MARKER,
            "1",
        ],
        "",
    );
    let (socket, server) = spawn_sidebar_snapshot_server(crate::daemon::SidebarModel::default());

    let output = crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Attach { once: true },
        &mock,
        &env,
        &crate::config::Config::default(),
        |_, _| Ok(("scratch".to_string(), socket.clone())),
    )
    .unwrap();

    assert_eq!(output.unwrap(), "No agents detected");
    server.join().unwrap();
    std::fs::remove_file(socket).unwrap();
}

#[test]
fn dispatch_sidebar_open_uses_layout_operations() {
    let mock = MockTmuxRunner::new();
    let exe = std::env::current_exe().unwrap();
    let command = sidebar_attach_command_for_selection_test(&exe);
    stub_selection_context(&mock);
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
        ],
        "%1\t\t80\n",
    );
    mock.stub(
        &[
            "display-message",
            "-p",
            "-t",
            "@1",
            "-F",
            "#{window_layout}",
        ],
        "layout-before\n",
    );
    mock.stub(
        &["split-window", "-t", "@1", "-hbf", "-l", "40", &command],
        "",
    );
    mock.stub(
        &[
            "set-hook",
            "-g",
            "after-new-window[90]",
            &format!(
                "run-shell {}",
                shell_quote_for_test(&format!(
                    "{} sidebar layout-applied --window {} --width {}",
                    shell_quote_for_test(&exe.display().to_string()),
                    shell_quote_for_test("#{window_id}"),
                    shell_quote_for_test("40")
                ))
            ),
        ],
        "",
    );
    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Open {
            window: Some("@1".to_string()),
            width: Some(crate::config::SidebarWidth::Columns(40)),
            delay_ms: Some(0),
        },
        &mock,
        &env(),
        &crate::config::Config::default(),
        |_, _| {
            Ok((
                "scratch".to_string(),
                std::path::PathBuf::from("/tmp/vde-sidebar-test.sock"),
            ))
        },
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 4);
}

#[test]
fn dispatch_sidebar_focus_selects_sidebar_pane() {
    let mock = MockTmuxRunner::new();
    mock.stub(&["display-message", "-p", "#{window_id}"], "@1\n");
    stub_selection_context(&mock);
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
        ],
        "%9\t1\t40\n%1\t\t80\n",
    );
    mock.stub(&["select-pane", "-t", "%9"], "");

    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Focus { window: None },
        &mock,
        &env(),
        &crate::config::Config::default(),
        |_, _| {
            Ok((
                "scratch".to_string(),
                std::path::PathBuf::from("/tmp/vde-sidebar-test.sock"),
            ))
        },
    )
    .unwrap();

    assert!(mock.calls().contains(&vec![
        "select-pane".to_string(),
        "-t".to_string(),
        "%9".to_string(),
    ]));
}

#[test]
fn dispatch_sidebar_focus_resolves_context_and_targets_sidebar_instance_listener() {
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%source".to_string())]);
    let mock = MockTmuxRunner::new();
    mock.stub(
        &[
            "display-message",
            "-p",
            "-t",
            "%source",
            "-F",
            "#{pane_id}\u{1f}#{pane_pid}\u{1f}#{session_id}",
        ],
        "%source\u{1f}101\u{1f}$1\n",
    );
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            "#{pane_id}\u{1f}#{pane_pid}\u{1f}#{@vde_sidebar}",
        ],
        "%9\u{1f}909\u{1f}1\n%source\u{1f}101\u{1f}\n",
    );
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
        ],
        "%9\t1\t40\n%source\t\t80\n",
    );
    mock.stub(&["select-pane", "-t", "%9"], "");
    let server_identity = format!(
        "cli_focus_{}_{}",
        std::process::id(),
        SIDEBAR_SOCKET_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let sidebar = crate::pane_state::PaneInstance {
        pane_id: "%9".to_string(),
        pane_pid: 909,
    };
    let listener =
        crate::sidebar::control::ControlListener::bind(&server_identity, &sidebar).unwrap();

    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Focus {
            window: Some("@1".to_string()),
        },
        &mock,
        &env,
        &crate::config::Config::default(),
        |_, _| Ok((server_identity.clone(), std::path::PathBuf::new())),
    )
    .unwrap();

    assert_eq!(
        listener.try_recv().unwrap(),
        Some(crate::sidebar::control::ControlMessage::Focus {
            pane_instance: crate::pane_state::PaneInstance {
                pane_id: "%source".to_string(),
                pane_pid: 101,
            },
            session_id: "$1".to_string(),
        })
    );
}

#[test]
fn dispatch_sidebar_focus_without_sidebar_is_noop() {
    let mock = MockTmuxRunner::new();
    mock.stub(&["display-message", "-p", "#{window_id}"], "@1\n");
    stub_selection_context(&mock);
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
        ],
        "%1\t\t80\n",
    );

    let output = crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Focus { window: None },
        &mock,
        &env(),
        &crate::config::Config::default(),
        |_, _| {
            Ok((
                "scratch".to_string(),
                std::path::PathBuf::from("/tmp/vde-sidebar-test.sock"),
            ))
        },
    )
    .unwrap();

    assert_eq!(output, None);
    assert!(
        !mock
            .calls()
            .iter()
            .any(|call| call.first().map(String::as_str) == Some("select-pane"))
    );
}

#[test]
fn dispatch_sidebar_open_accepts_percent_width() {
    let mock = MockTmuxRunner::new();
    let exe = std::env::current_exe().unwrap();
    let command = sidebar_attach_command_for_selection_test(&exe);
    let env = BTreeMap::new();
    stub_selection_context(&mock);
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
        ],
        "%1\t\t640\n",
    );
    let layout = "abcd,640x132,0,0,1";
    mock.stub(
        &[
            "display-message",
            "-p",
            "-t",
            "@1",
            "-F",
            "#{window_layout}",
        ],
        &format!("{layout}\n"),
    );
    mock.stub(
        &["split-window", "-t", "@1", "-hbf", "-l", "64", &command],
        "",
    );

    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Open {
            window: Some("@1".to_string()),
            width: Some(crate::config::SidebarWidth::Percent(10)),
            delay_ms: None,
        },
        &mock,
        &env,
        &crate::config::Config::default(),
        |_, _| {
            Ok((
                "scratch".to_string(),
                std::path::PathBuf::from("/tmp/vde-sidebar-test.sock"),
            ))
        },
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 4);
}

#[test]
fn dispatch_sidebar_toggle_all_uses_all_windows() {
    let mock = MockTmuxRunner::new();
    let exe = std::env::current_exe().unwrap();
    let command = sidebar_attach_command_for_selection_test(&exe);
    stub_selection_context(&mock);
    mock.stub(
        &[
            "list-panes",
            "-F",
            "#{pane_id}\u{1f}#{pane_pid}\u{1f}#{@vde_sidebar}",
        ],
        "%1\u{1f}101\u{1f}\n",
    );
    mock.stub(&["list-windows", "-a", "-F", "#{window_id}"], "@1\n");
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
        ],
        "%1\t\t80\n",
    );
    mock.stub(
        &[
            "display-message",
            "-p",
            "-t",
            "@1",
            "-F",
            "#{window_layout}",
        ],
        "layout-before\n",
    );
    mock.stub(
        &[
            "split-window",
            "-d",
            "-t",
            "@1",
            "-hbf",
            "-l",
            "40",
            &command,
        ],
        "",
    );
    mock.stub(
        &[
            "set-hook",
            "-g",
            "after-new-window[90]",
            &format!(
                "run-shell {}",
                shell_quote_for_test(&format!(
                    "{} sidebar layout-applied --window {} --width {}",
                    shell_quote_for_test(&exe.display().to_string()),
                    shell_quote_for_test("#{window_id}"),
                    shell_quote_for_test("40")
                ))
            ),
        ],
        "",
    );
    mock.stub(
        &[
            "set-hook",
            "-g",
            "pane-exited[90]",
            &format!(
                "run-shell {}",
                shell_quote_for_test(&format!(
                    "{} sidebar layout-changed --window {}",
                    shell_quote_for_test(&exe.display().to_string()),
                    shell_quote_for_test("#{window_id}"),
                ))
            ),
        ],
        "",
    );

    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Toggle {
            all: true,
            window: None,
            width: Some(crate::config::SidebarWidth::Columns(40)),
        },
        &mock,
        &env(),
        &crate::config::Config::default(),
        |_, _| {
            Ok((
                "scratch".to_string(),
                std::path::PathBuf::from("/tmp/vde-sidebar-test.sock"),
            ))
        },
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 8);
    assert!(mock.calls().iter().any(|call| {
        call == &vec![
            "list-panes".to_string(),
            "-F".to_string(),
            "#{pane_id}\u{1f}#{pane_pid}\u{1f}#{@vde_sidebar}".to_string(),
        ]
    }));
}

#[test]
fn dispatch_sidebar_jump_forwards_to_daemon_when_socket_exists() {
    let (socket, rx, handle) = spawn_v2_sidebar_command_server();
    let mock = MockTmuxRunner::new();
    mock.stub(
        &[
            "display-message",
            "-p",
            "-t",
            "%1",
            "-F",
            "#{pane_id}\u{1f}#{pane_pid}",
        ],
        "%1\u{1f}101\n",
    );
    mock.stub(
        &["display-message", "-p", "-F", "#{pane_id}\u{1f}#{pane_pid}"],
        "%9\u{1f}909\n",
    );

    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Jump {
            pane: "%1".to_string(),
        },
        &mock,
        &BTreeMap::new(),
        &crate::config::Config::default(),
        |_, _| Ok(("scratch".to_string(), socket.clone())),
    )
    .unwrap();

    let message = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(
        message,
        crate::daemon::protocol::v2::ClientMessage::SidebarCommand {
            command: crate::daemon::protocol::v2::SidebarCommand::JumpPane {
                pane_instance,
                source_pane,
            },
            ..
        } if pane_instance.pane_id == "%1"
            && pane_instance.pane_pid == 101
            && source_pane.pane_id == "%9"
            && source_pane.pane_pid == 909
    ));
    handle.join().unwrap();
    std::fs::remove_file(socket).unwrap();
}

#[test]
fn dispatch_sidebar_input_targets_the_invoking_sidebar_instance() {
    let mock = MockTmuxRunner::new();
    mock.stub(
        &[
            "display-message",
            "-p",
            "-t",
            "%9",
            "-F",
            "#{pane_id}\u{1f}#{pane_pid}",
        ],
        "%9\u{1f}909\n",
    );
    let server_identity = format!("cli_input_{}", std::process::id());
    let sidebar = crate::pane_state::PaneInstance {
        pane_id: "%9".to_string(),
        pane_pid: 909,
    };
    let listener =
        crate::sidebar::control::ControlListener::bind(&server_identity, &sidebar).unwrap();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%9".to_string())]);

    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Input {
            key: "j".to_string(),
        },
        &mock,
        &env,
        &crate::config::Config::default(),
        |_, _| Ok((server_identity.clone(), std::path::PathBuf::new())),
    )
    .unwrap();

    assert_eq!(
        listener.try_recv().unwrap(),
        Some(crate::sidebar::control::ControlMessage::Input {
            key: "j".to_string()
        })
    );
}

#[test]
fn sidebar_layout_applied_ensures_daemon_started() {
    use std::cell::Cell;

    let mock = MockTmuxRunner::new();
    let exe = std::env::current_exe().unwrap();
    let command = format!(
        "{} sidebar attach",
        shell_quote_for_test(&exe.display().to_string())
    );
    mock.stub(&["display-message", "-p", "#{window_id}"], "@1\n");
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
        ],
        "%1\t\t80\n",
    );
    mock.stub(
        &[
            "display-message",
            "-p",
            "-t",
            "@1",
            "-F",
            "#{window_layout}",
        ],
        "layout-before\n",
    );
    mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
    mock.stub(
        &[
            "split-window",
            "-d",
            "-t",
            "@1",
            "-hbf",
            "-l",
            "40",
            &command,
        ],
        "",
    );
    let called = Cell::new(false);

    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::LayoutApplied {
            window: Some("@1".to_string()),
            width: Some(crate::config::SidebarWidth::Columns(40)),
        },
        &mock,
        &env(),
        &crate::config::Config::default(),
        |_, _| {
            called.set(true);
            Ok((
                "scratch".to_string(),
                std::path::PathBuf::from("/tmp/vde-sidebar-test.sock"),
            ))
        },
    )
    .unwrap();

    assert!(called.get());
    assert_eq!(mock.calls().len(), 4);
}

#[test]
fn sidebar_layout_changed_closes_lonely_sidebar_without_starting_daemon() {
    use std::cell::Cell;

    let mock = MockTmuxRunner::new();
    mock.stub(&["display-message", "-p", "#{window_id}"], "@1\n");
    mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%9\n");
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
        ],
        "%9\t1\t40\n",
    );
    mock.stub(&["kill-pane", "-t", "%9"], "");
    let called = Cell::new(false);

    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::LayoutChanged { window: None },
        &mock,
        &env(),
        &crate::config::Config::default(),
        |_, _| {
            called.set(true);
            Ok((
                "scratch".to_string(),
                std::path::PathBuf::from("/tmp/vde-sidebar-test.sock"),
            ))
        },
    )
    .unwrap();

    assert!(!called.get());
    assert!(mock.calls().contains(&vec![
        "kill-pane".to_string(),
        "-t".to_string(),
        "%9".to_string(),
    ]));
}

#[test]
fn dispatch_sidebar_focus_toggle_existing_sidebar_skips_daemon_ensure() {
    let mock = MockTmuxRunner::new();
    stub_selection_context(&mock);
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
        ],
        "%1\t\t80\n%9\t1\t40\n",
    );
    mock.stub(&["display-message", "-p", "-t", "@1", "#{pane_id}"], "%1\n");
    mock.stub(&["select-pane", "-t", "%9"], "");

    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::FocusToggle {
            window: Some("@1".to_string()),
            width: None,
        },
        &mock,
        &env(),
        &crate::config::Config::default(),
        |_, _| panic!("existing sidebar focus must not ensure the daemon"),
    )
    .unwrap();

    assert!(mock.calls().contains(&vec![
        "select-pane".to_string(),
        "-t".to_string(),
        "%9".to_string(),
    ]));
}

#[test]
fn focus_toggle_defers_active_config_until_sidebar_open_is_needed() {
    let command = crate::cli::sidebar::SidebarCommand::FocusToggle {
        window: Some("@1".to_string()),
        width: None,
    };

    assert!(!command.requires_active_config());
}

#[test]
fn focus_toggle_loads_active_config_only_when_opening_a_missing_sidebar() {
    let mock = MockTmuxRunner::new();
    stub_selection_context(&mock);
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
        ],
        "%1\t\t80\n",
    );
    mock.stub(
        &[
            "display-message",
            "-p",
            "-t",
            "@1",
            "-F",
            "#{window_layout}",
        ],
        "layout-before\n",
    );
    let exe = std::env::current_exe().unwrap();
    let command = sidebar_attach_command_for_selection_test(&exe);
    mock.stub(
        &["split-window", "-t", "@1", "-hbf", "-l", "52", &command],
        "",
    );
    let config_loaded = std::cell::Cell::new(0_u8);

    crate::cli::sidebar::run_focus_toggle_command(
        &mock,
        &env(),
        Some("@1".to_string()),
        None,
        || {
            config_loaded.set(config_loaded.get() + 1);
            let mut config = crate::config::Config::default();
            config.sidebar.width = crate::config::SidebarWidth::Columns(52);
            Ok(config)
        },
    )
    .unwrap();

    assert_eq!(config_loaded.get(), 1);
    assert!(mock.calls().contains(&vec![
        "split-window".to_string(),
        "-t".to_string(),
        "@1".to_string(),
        "-hbf".to_string(),
        "-l".to_string(),
        "52".to_string(),
        command,
    ]));
}

fn shell_quote_for_test(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn stub_selection_context(mock: &MockTmuxRunner) {
    mock.stub(
        &[
            "display-message",
            "-p",
            "-F",
            "#{pane_id}\u{1f}#{pane_pid}\u{1f}#{session_id}",
        ],
        "%1\u{1f}101\u{1f}$1\n",
    );
}

fn sidebar_attach_command_for_selection_test(exe: &std::path::Path) -> String {
    format!(
        "VDE_TMUX_SELECTION_PANE='%1' VDE_TMUX_SELECTION_PANE_PID=101 VDE_TMUX_SELECTION_SESSION='$1' {} sidebar attach",
        shell_quote_for_test(&exe.display().to_string())
    )
}

fn unique_socket_path(label: &str) -> std::path::PathBuf {
    static NEXT_SOCKET_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let socket_id = NEXT_SOCKET_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::path::PathBuf::from(format!(
        "/tmp/{label}-{}-{socket_id}.sock",
        std::process::id()
    ))
}

#[test]
fn unique_socket_path_uses_short_tmp_path() {
    let path = unique_socket_path("vde-tmux-sidebar-input");

    assert!(path.starts_with("/tmp"));
    assert!(path.display().to_string().len() < 104);
}
