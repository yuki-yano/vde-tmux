use super::*;
use crate::tmux::mock::MockTmuxRunner;
use std::collections::BTreeMap;

fn env() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn tmux_env() -> BTreeMap<String, String> {
    BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())])
}

mod hook;
mod sidebar;

#[test]
fn dispatch_statusline_sessions_prints_output() {
    let mock = MockTmuxRunner::new();
    let format = crate::session::session_list_format();
    mock.stub(
        &["list-sessions", "-F", &format],
        "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\n",
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
        "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\n",
    );
    mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
    mock.stub(&["show-option", "-gqv", "@vde_heartbeat"], "");

    let output = run_with(["vt", "statusline-sessions", "--show-index"], &mock, &env())
        .unwrap()
        .unwrap();

    assert!(output.contains("1 main"));
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
        "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\n",
    );
    mock.stub(&["show-option", "-gqv", "@vde_client_616263_work"], "");
    mock.stub(&["switch-client", "-c", "abc", "-t", "=main:"], "");
    mock.stub(&["set-option", "-g", "@vde_client_616263_work", "main"], "");
    run_with(["vt", "category", "use", "work"], &mock, &env()).unwrap();
    assert_eq!(mock.calls().len(), 5);
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
    let window_format = [
        "#{session_name}",
        "#{window_index}",
        "#{window_id}",
        "#{window_name}",
        "#{window_panes}",
        "#{window_active}",
        "#{pane_current_command}",
    ]
    .join("\t");
    mock.stub(
        &["list-sessions", "-F", &session_format],
        "ni.zsh\u{1f}1\u{1f}100\u{1f}public\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}$2\n",
    );
    mock.stub(
        &["list-windows", "-t", "=ni.zsh:", "-F", &window_format],
        "ni.zsh\t2\t@9\teditor\t2\t1\tnvim\n",
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
        "main\u{1f}1\u{1f}100\u{1f}misc\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\n",
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
