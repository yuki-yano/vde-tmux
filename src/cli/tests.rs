use super::*;
use crate::tmux::mock::MockTmuxRunner;
use std::collections::BTreeMap;

fn env() -> BTreeMap<String, String> {
    BTreeMap::new()
}

mod hook;
mod sidebar;

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
