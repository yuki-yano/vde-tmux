use super::*;

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
    let format = crate::options::snapshot::snapshot_format();
    let line = [
        "main",
        "@1",
        "%1",
        "/tmp/app",
        "codex",
        "/dev/ttys001",
        "123",
        "0",
        "0",
        "0",
        "",
        "",
        "",
        "",
        "codex",
        "running",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
    ]
    .join("\u{1f}");
    mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));

    let output = crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Attach { once: true },
        &mock,
        &env,
        &crate::config::Config::default(),
        |_| Ok(()),
    )
    .unwrap();

    assert!(output.unwrap().contains("Codex"));
}

#[test]
fn dispatch_sidebar_attach_once_restores_persisted_state() {
    let state_home = std::env::temp_dir().join(format!(
        "vde-tmux-sidebar-state-cli-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let env = BTreeMap::from([
        ("TMUX_PANE".to_string(), "%9".to_string()),
        (
            "XDG_STATE_HOME".to_string(),
            state_home.display().to_string(),
        ),
    ]);
    let state_path = crate::sidebar::store::state_path(&env);
    let mut state = crate::sidebar::state::SidebarState {
        selection: Some("repo::misc::app".to_string()),
        ..crate::sidebar::state::SidebarState::default()
    };
    state.collapsed.insert("repo::misc::app".to_string());
    crate::sidebar::store::save_state(&state_path, &state).unwrap();

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
    let format = crate::options::snapshot::snapshot_format();
    let line = [
        "main",
        "@1",
        "%1",
        "/tmp/app",
        "codex",
        "/dev/ttys001",
        "123",
        "0",
        "0",
        "0",
        "",
        "",
        "",
        "",
        "codex",
        "running",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
    ]
    .join("\u{1f}");
    mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));

    let output = crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Attach { once: true },
        &mock,
        &env,
        &crate::config::Config::default(),
        |_| Ok(()),
    )
    .unwrap();
    let output = output.unwrap();

    assert!(output.contains(" ▸ app"));
    assert!(!output.contains("Codex %1"));
    std::fs::remove_dir_all(state_home).unwrap();
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
        |_| Ok(()),
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
        |_| Ok(()),
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
        |_| Ok(()),
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

    crate::cli::run_with(
        ["vt", "sidebar", "open", "--window", "@1", "--width", "10%"],
        &mock,
        &env(),
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
        |_| Ok(()),
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 7);
}

#[test]
fn dispatch_sidebar_focus_sends_current_selection_context() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let socket = unique_socket_path("vde-tmux-sidebar-select-context");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let mut line = String::new();
                    BufReader::new(&mut stream).read_line(&mut line).unwrap();
                    let message: crate::daemon::protocol::ClientMessage =
                        serde_json::from_str(line.trim()).unwrap();
                    tx.send(message).unwrap();
                    serde_json::to_writer(
                        &mut stream,
                        &crate::daemon::protocol::ServerMessage::Ack,
                    )
                    .unwrap();
                    stream.write_all(b"\n").unwrap();
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return,
            }
        }
    });
    let env = BTreeMap::from([
        (
            "VDE_DAEMON_SOCKET".to_string(),
            socket.display().to_string(),
        ),
        ("TMUX_PANE".to_string(), "%source".to_string()),
    ]);
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
        |_| Ok(()),
    )
    .unwrap();

    let message = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        message,
        crate::daemon::protocol::ClientMessage::SidebarEvent {
            proto: 1,
            event: crate::daemon::protocol::SidebarClientEvent::SelectContext {
                pane: Some("%source".to_string()),
                session: Some("main".to_string())
            }
        }
    );
    handle.join().unwrap();
    std::fs::remove_file(socket).unwrap();
}

#[test]
fn dispatch_sidebar_jump_forwards_to_daemon_when_socket_exists() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let socket = unique_socket_path("vde-tmux-sidebar-jump");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let mut line = String::new();
                    BufReader::new(&mut stream).read_line(&mut line).unwrap();
                    let message: crate::daemon::protocol::ClientMessage =
                        serde_json::from_str(line.trim()).unwrap();
                    tx.send(message).unwrap();
                    serde_json::to_writer(
                        &mut stream,
                        &crate::daemon::protocol::ServerMessage::Ack,
                    )
                    .unwrap();
                    stream.write_all(b"\n").unwrap();
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return,
            }
        }
    });
    let env = BTreeMap::from([(
        "VDE_DAEMON_SOCKET".to_string(),
        socket.display().to_string(),
    )]);
    let mock = MockTmuxRunner::new();

    run_with(["vt", "sidebar", "jump", "%1"], &mock, &env).unwrap();

    let message = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        message,
        crate::daemon::protocol::ClientMessage::SidebarEvent {
            proto: 1,
            event: crate::daemon::protocol::SidebarClientEvent::JumpPane {
                pane: "%1".to_string()
            }
        }
    );
    handle.join().unwrap();
    std::fs::remove_file(socket).unwrap();
}

#[test]
fn dispatch_sidebar_input_forwards_to_daemon_when_socket_exists() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let socket = unique_socket_path("vde-tmux-sidebar-input");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let mut line = String::new();
                    BufReader::new(&mut stream).read_line(&mut line).unwrap();
                    let message: crate::daemon::protocol::ClientMessage =
                        serde_json::from_str(line.trim()).unwrap();
                    tx.send(message).unwrap();
                    serde_json::to_writer(
                        &mut stream,
                        &crate::daemon::protocol::ServerMessage::Ack,
                    )
                    .unwrap();
                    stream.write_all(b"\n").unwrap();
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return,
            }
        }
    });
    let env = BTreeMap::from([(
        "VDE_DAEMON_SOCKET".to_string(),
        socket.display().to_string(),
    )]);
    let mock = MockTmuxRunner::new();

    run_with(["vt", "sidebar", "input", "j"], &mock, &env).unwrap();

    let message = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        message,
        crate::daemon::protocol::ClientMessage::SidebarEvent {
            proto: 1,
            event: crate::daemon::protocol::SidebarClientEvent::Key {
                key: "j".to_string()
            }
        }
    );
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
        |_| {
            called.set(true);
            Ok(())
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
        |_| {
            called.set(true);
            Ok(())
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
    std::path::PathBuf::from(format!(
        "/tmp/{label}-{}.sock",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[test]
fn unique_socket_path_uses_short_tmp_path() {
    let path = unique_socket_path("vde-tmux-sidebar-input");

    assert!(path.starts_with("/tmp"));
    assert!(path.display().to_string().len() < 104);
}
