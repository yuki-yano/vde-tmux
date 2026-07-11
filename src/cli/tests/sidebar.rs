use super::*;

fn spawn_sidebar_snapshot_server(
    sidebar: crate::daemon::SidebarFrame,
) -> (std::path::PathBuf, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, Write};

    let socket = std::env::temp_dir().join(format!(
        "vt2-cli-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
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
            sidebar,
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

fn sidebar_row(
    id: &str,
    kind: crate::sidebar::tree::SidebarRowKind,
    label: &str,
    pane_id: Option<&str>,
) -> crate::sidebar::tree::SidebarRow {
    crate::sidebar::tree::SidebarRow {
        id: id.to_string(),
        kind,
        depth: 0,
        label: label.to_string(),
        chat_count: 1,
        rollup: crate::hook::RollupLevel::Running,
        badge_state: None,
        expanded: true,
        pane_id: pane_id.map(ToOwned::to_owned),
        git: None,
        active: false,
        meta: None,
    }
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
    let (socket, server) = spawn_sidebar_snapshot_server(crate::daemon::SidebarFrame {
        state: crate::sidebar::state::SidebarState::default(),
        counts: crate::sidebar::tree::BadgeCounts::default(),
        rows: vec![sidebar_row(
            "chat::%1",
            crate::sidebar::tree::SidebarRowKind::Chat,
            "codex (%1)",
            Some("%1"),
        )],
    });

    let output = crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Attach { once: true },
        &mock,
        &env,
        &crate::config::Config::default(),
        |_, _| Ok(("scratch".to_string(), socket.clone())),
    )
    .unwrap();

    assert!(output.unwrap().contains("Codex"));
    server.join().unwrap();
    std::fs::remove_file(socket).unwrap();
}

#[test]
fn dispatch_sidebar_attach_once_uses_sidebar_state_from_v2_snapshot() {
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%9".to_string())]);
    let mock = MockTmuxRunner::new();
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
    let mut state = crate::sidebar::state::SidebarState::default();
    state.collapsed.insert("repo::misc::app".to_string());
    let mut repo = sidebar_row(
        "repo::misc::app",
        crate::sidebar::tree::SidebarRowKind::Repo,
        "app",
        None,
    );
    repo.expanded = false;
    let (socket, server) = spawn_sidebar_snapshot_server(crate::daemon::SidebarFrame {
        state,
        counts: crate::sidebar::tree::BadgeCounts::default(),
        rows: vec![
            repo,
            sidebar_row(
                "chat::%1",
                crate::sidebar::tree::SidebarRowKind::Chat,
                "codex (%1)",
                Some("%1"),
            ),
        ],
    });

    let output = crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Attach { once: true },
        &mock,
        &env,
        &crate::config::Config::default(),
        |_, _| Ok(("scratch".to_string(), socket.clone())),
    )
    .unwrap();
    let output = output.unwrap();

    assert!(output.contains(" ▸ app"));
    assert!(!output.contains("Codex %1"));
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
        &[
            "split-window",
            "-d",
            "-t",
            "@1",
            "-hbf",
            "-l",
            "64",
            &command,
        ],
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

    assert_eq!(mock.calls().len(), 7);
}

#[test]
fn dispatch_sidebar_focus_sends_current_selection_context() {
    let (socket, rx, handle) = spawn_v2_sidebar_command_server();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%source".to_string())]);
    let mock = MockTmuxRunner::new();
    mock.stub(
        &[
            "display-message",
            "-p",
            "-t",
            "%source",
            "-F",
            "#{pane_id}\u{1f}#{session_name}",
        ],
        "%source\u{1f}main\n",
    );
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
        crate::cli::sidebar::SidebarCommand::Focus {
            window: Some("@1".to_string()),
        },
        &mock,
        &env,
        &crate::config::Config::default(),
        |_, _| Ok(("scratch".to_string(), socket.clone())),
    )
    .unwrap();

    let message = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let event_id = message.event_id().unwrap().clone();
    assert_eq!(
        message,
        crate::daemon::protocol::v2::ClientMessage::SidebarCommand {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            daemon_instance_id: crate::pane_state::DaemonInstanceId::parse(
                "ffeeddccbbaa99887766554433221100"
            )
            .unwrap(),
            event_id,
            command: crate::daemon::protocol::v2::SidebarCommand::SelectContext {
                pane_id: Some("%source".to_string()),
                session_id: Some("main".to_string()),
            },
        }
    );
    handle.join().unwrap();
    std::fs::remove_file(socket).unwrap();
}

#[test]
fn dispatch_sidebar_jump_forwards_to_daemon_when_socket_exists() {
    let (socket, rx, handle) = spawn_v2_sidebar_command_server();
    let mock = MockTmuxRunner::new();

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
            command: crate::daemon::protocol::v2::SidebarCommand::JumpPane { pane_id },
            ..
        } if pane_id == "%1"
    ));
    handle.join().unwrap();
    std::fs::remove_file(socket).unwrap();
}

#[test]
fn dispatch_sidebar_input_forwards_to_daemon_when_socket_exists() {
    let (socket, rx, handle) = spawn_v2_sidebar_command_server();
    let mock = MockTmuxRunner::new();

    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Input {
            key: "j".to_string(),
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
            command: crate::daemon::protocol::v2::SidebarCommand::Key { key },
            ..
        } if key == "j"
    ));
    handle.join().unwrap();
    std::fs::remove_file(socket).unwrap();
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
            "#{pane_id}\u{1f}#{session_name}",
        ],
        "%1\u{1f}main\n",
    );
}

fn sidebar_attach_command_for_selection_test(exe: &std::path::Path) -> String {
    format!(
        "VDE_TMUX_SELECTION_PANE='%1' VDE_TMUX_SELECTION_SESSION='main' {} sidebar attach",
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
