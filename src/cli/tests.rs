use super::*;
use crate::tmux::mock::MockTmuxRunner;
use std::collections::BTreeMap;

fn env() -> BTreeMap<String, String> {
    BTreeMap::new()
}

#[test]
fn dispatch_statusline_sessions_prints_output() {
    let mock = MockTmuxRunner::new();
    let format = crate::session::session_list_format();
    mock.stub(
        &["list-sessions", "-F", &format],
        "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\n",
    );
    mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
    let output = run_with(["vt", "statusline-sessions"], &mock, &env()).unwrap();
    assert!(output.unwrap().contains("main"));
}

#[test]
fn dispatch_statusline_sessions_show_index_overrides_config() {
    let mock = MockTmuxRunner::new();
    let format = crate::session::session_list_format();
    mock.stub(
        &["list-sessions", "-F", &format],
        "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\n",
    );
    mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");

    let output = run_with(["vt", "statusline-sessions", "--show-index"], &mock, &env())
        .unwrap()
        .unwrap();

    assert!(output.contains("1:main"));
}

#[test]
fn dispatch_category_use_switches_category() {
    let mock = MockTmuxRunner::new();
    let format = crate::session::session_list_format();
    mock.stub(&["display-message", "-p", "#{client_name}"], "abc\n");
    mock.stub(
        &["list-sessions", "-F", &format],
        "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\n",
    );
    mock.stub(&["show-option", "-gqv", "@vde_client_616263_work"], "");
    mock.stub(&["switch-client", "-t", "main"], "");
    mock.stub(&["set-option", "-g", "@vde_client_616263_work", "main"], "");
    run_with(["vt", "category", "use", "work"], &mock, &env()).unwrap();
    assert_eq!(mock.calls().len(), 5);
}

#[test]
fn dispatch_hook_emit_writes_pane_options() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_STATUS,
            "running",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-u",
            "-t",
            "%1",
            crate::options::KEY_WAIT_REASON,
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_AGENT,
            "codex",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_PROMPT,
            "hello",
        ],
        "",
    );
    run_with_input_at(
        [
            "vt", "hook", "emit", "--agent", "codex", "--status", "running", "--prompt", "hello",
        ],
        "",
        &mock,
        &env,
        123,
    )
    .unwrap();
    assert_eq!(mock.calls().len(), 4);
}

#[test]
fn dispatch_hook_claude_reads_stdin_json() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_STATUS,
            "running",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-u",
            "-t",
            "%1",
            crate::options::KEY_WAIT_REASON,
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_AGENT,
            "claude",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_STARTED_AT,
            "123",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_PROMPT,
            "hello",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_PROMPT_SOURCE,
            "user",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-u",
            "-t",
            "%1",
            crate::options::KEY_TASKS,
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-u",
            "-t",
            "%1",
            crate::options::KEY_SUBAGENTS,
        ],
        "",
    );
    run_with_input_at(
        ["vt", "hook", "claude", "UserPromptSubmit"],
        r#"{"prompt":"hello"}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();
    assert_eq!(mock.calls().len(), 8);
}

#[test]
fn dispatch_hook_codex_event_reads_stdin_json() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_STATUS,
            "waiting",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_WAIT_REASON,
            "permission_prompt",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_AGENT,
            "codex",
        ],
        "",
    );
    run_with_input_at(
        ["vt", "hook", "codex", "PermissionRequest"],
        "{}",
        &mock,
        &env,
        123,
    )
    .unwrap();
    assert_eq!(mock.calls().len(), 3);
}

#[test]
fn dispatch_hook_codex_notify_reads_argv_json() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_STATUS,
            "idle",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-u",
            "-t",
            "%1",
            crate::options::KEY_WAIT_REASON,
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_AGENT,
            "codex",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_ATTENTION,
            "1",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_COMPLETED_AT,
            "456",
        ],
        "",
    );
    run_with_input_at(
        ["vt", "hook", "codex", r#"{"type":"agent-turn-complete"}"#],
        "",
        &mock,
        &env,
        456,
    )
    .unwrap();
    assert_eq!(mock.calls().len(), 5);
}

#[test]
fn dispatch_hook_claude_task_created_updates_progress() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    mock.stub(&["show-options", "-p", "-t", "%1"], "@vde_tasks \"0/0\"\n");
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_TASKS,
            "0/1",
        ],
        "",
    );
    run_with_input_at(
        ["vt", "hook", "claude", "TaskCreated"],
        "{}",
        &mock,
        &env,
        456,
    )
    .unwrap();
    assert_eq!(mock.calls().len(), 2);
}

#[test]
fn dispatch_statusline_agent_badge_falls_back_to_tmux_snapshot() {
    let mock = MockTmuxRunner::new();
    let format = crate::options::snapshot::snapshot_format();
    let line = [
        "main", "@1", "%1", "/tmp", "zsh", "", "codex", "running", "", "", "", "", "", "", "", "",
    ]
    .join("\u{1f}");
    mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));
    let output = run_with(["vt", "statusline-agent-badge"], &mock, &env()).unwrap();
    assert_eq!(output, Some("running:1".to_string()));
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
        "main", "@1", "%1", "/tmp/app", "zsh", "", "codex", "running", "", "", "", "", "", "", "",
        "",
    ]
    .join("\u{1f}");
    mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));

    let output = run_with(["vt", "sidebar", "attach", "--once"], &mock, &env).unwrap();

    assert!(output.unwrap().contains("codex %1"));
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
        "main", "@1", "%1", "/tmp/app", "zsh", "", "codex", "running", "", "", "", "", "", "", "",
        "",
    ]
    .join("\u{1f}");
    mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));

    let output = run_with(["vt", "sidebar", "attach", "--once"], &mock, &env).unwrap();
    let output = output.unwrap();

    assert!(output.contains("> > app"));
    assert!(!output.contains("codex %1"));
    std::fs::remove_dir_all(state_home).unwrap();
}

#[test]
fn dispatch_sidebar_open_uses_layout_operations() {
    let mock = MockTmuxRunner::new();
    let exe = std::env::current_exe().unwrap();
    let command = format!(
        "{} sidebar attach",
        shell_quote_for_test(&exe.display().to_string())
    );
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
            "set-option",
            "-w",
            "-t",
            "@1",
            crate::options::KEY_LAYOUT_BASELINE,
            "layout-before",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-w",
            "-t",
            "@1",
            crate::options::KEY_LAYOUT_PANES,
            "%1",
        ],
        "",
    );
    mock.stub(
        &["split-window", "-t", "@1", "-hbf", "-l", "40", &command],
        "",
    );

    run_with(
        [
            "vt",
            "sidebar",
            "open",
            "--window",
            "@1",
            "--width",
            "40",
            "--delay-ms",
            "0",
        ],
        &mock,
        &env(),
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 6);
}

#[test]
fn dispatch_sidebar_toggle_all_uses_all_windows() {
    let mock = MockTmuxRunner::new();
    let exe = std::env::current_exe().unwrap();
    let command = format!(
        "{} sidebar attach",
        shell_quote_for_test(&exe.display().to_string())
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
    mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
    mock.stub(
        &[
            "set-option",
            "-w",
            "-t",
            "@1",
            crate::options::KEY_LAYOUT_BASELINE,
            "layout-before",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-w",
            "-t",
            "@1",
            crate::options::KEY_LAYOUT_PANES,
            "%1",
        ],
        "",
    );
    mock.stub(
        &["split-window", "-t", "@1", "-hbf", "-l", "40", &command],
        "",
    );

    run_with(
        ["vt", "sidebar", "toggle", "--all", "--width", "40"],
        &mock,
        &env(),
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 7);
}

#[test]
fn dispatch_sidebar_jump_switches_to_pane() {
    let mock = MockTmuxRunner::new();
    let format = crate::options::snapshot::snapshot_format();
    let line = [
        "main", "@1", "%1", "/tmp/app", "zsh", "", "codex", "running", "", "", "", "", "", "", "",
        "",
    ]
    .join("\u{1f}");
    mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));
    mock.stub(&["switch-client", "-t", "main"], "");
    mock.stub(&["select-window", "-t", "@1"], "");
    mock.stub(&["select-pane", "-t", "%1"], "");

    run_with(["vt", "sidebar", "jump", "%1"], &mock, &env()).unwrap();

    assert_eq!(mock.calls().len(), 4);
}

#[test]
fn dispatch_sidebar_input_moves_selection_and_saves_state() {
    let state_home = std::env::temp_dir().join(format!(
        "vde-tmux-sidebar-input-cli-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let env = BTreeMap::from([(
        "XDG_STATE_HOME".to_string(),
        state_home.display().to_string(),
    )]);
    let mock = MockTmuxRunner::new();
    let format = crate::options::snapshot::snapshot_format();
    let line = [
        "main", "@1", "%1", "/tmp/app", "zsh", "", "codex", "running", "", "", "", "", "", "", "",
        "",
    ]
    .join("\u{1f}");
    mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));

    run_with(["vt", "sidebar", "input", "j"], &mock, &env).unwrap();

    let state =
        crate::sidebar::store::load_state(&crate::sidebar::store::state_path(&env)).unwrap();
    assert_eq!(state.selection.as_deref(), Some("repo::misc::app"));
    std::fs::remove_dir_all(state_home).unwrap();
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
