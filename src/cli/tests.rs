use super::*;
use crate::tmux::mock::MockTmuxRunner;
use std::collections::BTreeMap;

fn env() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn tmux_env() -> BTreeMap<String, String> {
    BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())])
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
fn dispatch_statusline_sessions_prints_output() {
    let mock = MockTmuxRunner::new();
    let format = crate::session::session_list_format();
    mock.stub(
        &["list-sessions", "-F", &format],
        "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}$1\n",
    );
    mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
    mock.stub(&["show-option", "-gqv", "@vde_heartbeat"], "");
    let output = run_with(["vt", "statusline-sessions"], &mock, &env()).unwrap();
    assert!(output.unwrap().contains("main"));
}

#[test]
fn dispatch_statusline_sessions_show_index_overrides_config() {
    let mock = MockTmuxRunner::new();
    let format = crate::session::session_list_format();
    mock.stub(
        &["list-sessions", "-F", &format],
        "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}$1\n",
    );
    mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
    mock.stub(&["show-option", "-gqv", "@vde_heartbeat"], "");

    let output = run_with(["vt", "statusline-sessions", "--show-index"], &mock, &env())
        .unwrap()
        .unwrap();

    assert!(output.contains("1: main"));
}

#[test]
fn dispatch_statusline_windows_prints_output() {
    let mock = MockTmuxRunner::new();
    let format = crate::window::window_list_format();
    mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
    mock.stub(
        &["list-windows", "-t", "=main:", "-F", &format],
        &format!(
            "{}\n{}\n",
            window_row("main", "1", "@1", "zsh", "1", "0", "zsh"),
            window_row("main", "2", "@2", "editor", "2", "1", "nvim")
        ),
    );

    let output = run_with(["vt", "statusline-windows"], &mock, &env())
        .unwrap()
        .unwrap();

    assert!(output.contains("#[range=user|window:@1]"), "{output}");
    assert!(output.contains("1:zsh"), "{output}");
    assert!(output.contains("2:editor"), "{output}");
}

#[test]
fn dispatch_statusline_pane_prints_pane_segment() {
    let mock = MockTmuxRunner::new();
    let format = [
        "#{pane_id}",
        "#{pane_active}",
        "#{pane_current_command}",
        "#{@vde_agent}",
        "#{@vde_status}",
        "#{@vde_wait_reason}",
        "#{@vde_attention}",
        "#{@vde_started_at}",
        "#{@vde_completed_at}",
    ]
    .join("\u{1f}");
    mock.stub(
        &["display-message", "-p", "-t", "%1", &format],
        "%1\u{1f}1\u{1f}node\u{1f}codex\u{1f}running\u{1f}\u{1f}0\u{1f}\u{1f}\n",
    );

    let output = run_with(["vt", "statusline-pane", "--target", "%1"], &mock, &env())
        .unwrap()
        .unwrap();

    assert_eq!(
        output,
        "#[fg=#4a4a70,bg=#1C1C1C]#[fg=#e7e3f6,bg=#4a4a70] %1  #[fg=#4fd08a]● #[fg=#e7e3f6]Codex #[fg=#e7e3f6] #[fg=#4fd08a]running#[fg=#e7e3f6] #[default]#[fg=#4a4a70,bg=#1C1C1C]#[default]"
    );
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
        "a\u{1f}1\u{1f}100\u{1f}alpha\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}$1\nb\u{1f}1\u{1f}100\u{1f}beta\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}$2\n",
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
        "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}$1\n",
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
        "ni.zsh\u{1f}1\u{1f}100\u{1f}public\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}$2\n",
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
fn dispatch_statusline_summary_falls_back_to_tmux_snapshot() {
    let mock = MockTmuxRunner::new();
    let format = crate::options::snapshot::snapshot_format();
    let line = [
        "main",
        "@1",
        "%1",
        "/tmp",
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
        "100",
        "",
        "",
        "",
        "",
        "",
    ]
    .join("\u{1f}");
    mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));
    let env = BTreeMap::from([(
        "VDE_DAEMON_SOCKET".to_string(),
        format!(
            "/tmp/vde-tmux-test-missing-{}.sock",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ),
    )]);
    let output = run_with(["vt", "statusline-summary"], &mock, &env).unwrap();
    assert_eq!(output, Some("#[fg=#4fd08a]●1#[default]".to_string()));
}

#[test]
fn dispatch_statusline_attention_falls_back_to_tmux_snapshot() {
    let mock = MockTmuxRunner::new();
    let format = crate::options::snapshot::snapshot_format();
    let started = crate::sidebar::tree::now_epoch_secs() - 120;
    let line = [
        "proxy",
        "@1",
        "%1",
        "/tmp",
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
        "waiting",
        "",
        "",
        "permission_prompt",
        "",
        &started.to_string(),
        "",
        "",
        "",
        "",
        "",
    ]
    .join("\u{1f}");
    mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));
    let env = BTreeMap::from([(
        "VDE_DAEMON_SOCKET".to_string(),
        format!(
            "/tmp/vde-tmux-test-missing-{}.sock",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ),
    )]);

    let output = run_with(["vt", "statusline-attention"], &mock, &env).unwrap();

    let text = output.unwrap();
    assert!(text.contains("▲ proxy · perm"), "{text}");
}

#[test]
fn dispatch_hooks_on_client_session_changed_requests_pane_refresh() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let socket = unique_socket_path("vde-tmux-session-hook-refresh");
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
    let format = crate::session::session_list_format();
    mock.stub(
        &["list-sessions", "-F", &format],
        "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}$1\n",
    );
    mock.stub(&["set-option", "-g", "@vde_client_616263_work", "main"], "");

    run_with(
        ["vt", "hooks", "on-client-session-changed", "abc", "main"],
        &mock,
        &env,
    )
    .unwrap();

    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        crate::daemon::protocol::ClientMessage::RefreshPanes { proto: 1 }
    );
    handle.join().unwrap();
    std::fs::remove_file(socket).unwrap();
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
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([(
        "XDG_CONFIG_HOME".to_string(),
        config_home.display().to_string(),
    )]);

    let output = run_with(["vt", "statusline-summary"], &mock, &env).unwrap();

    assert_eq!(output, Some(String::new()));
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

    let env = BTreeMap::from([(
        "XDG_CONFIG_HOME".to_string(),
        config_home.display().to_string(),
    )]);
    let mock = MockTmuxRunner::new();
    let format = crate::session::session_list_format();
    mock.stub(
        &["list-sessions", "-F", &format],
        "main\u{1f}1\u{1f}100\u{1f}misc\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}$1\n",
    );
    mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");

    let mut stderr = Vec::new();
    let output = run_with_input_at_writing_warnings(
        ["vt", "statusline-category"],
        "",
        &mock,
        &env,
        0,
        &mut stderr,
    )
    .unwrap()
    .unwrap();

    let stderr = String::from_utf8(stderr).unwrap();
    assert!(stderr.contains("vde-tmux config warning: invalid config"));
    assert!(output.contains("misc"));
    assert!(!output.contains("invalid config"));
    std::fs::remove_dir_all(config_home).unwrap();
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
