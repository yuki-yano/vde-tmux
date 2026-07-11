use super::*;

#[test]
fn generic_emit_requires_session_id_before_tmux_access() {
    let mock = MockTmuxRunner::new();
    let error = run_with(
        [
            "vt", "hook", "emit", "--agent", "generic", "--status", "running",
        ],
        &mock,
        &BTreeMap::new(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("--session-id"));
    assert!(mock.calls().is_empty());
}

#[test]
fn codex_legacy_notify_is_rejected_before_tmux_access() {
    let mock = MockTmuxRunner::new();
    let error = run_with_input_at(
        ["vt", "hook", "codex", r#"{"type":"agent-turn-complete"}"#],
        "",
        &mock,
        &BTreeMap::new(),
        1_700_000_000,
    )
    .unwrap_err();

    assert!(error.to_string().contains("UnsupportedLegacyNotify"));
    assert!(mock.calls().is_empty());
}

#[test]
fn codex_missing_event_is_rejected_before_tmux_access() {
    let mock = MockTmuxRunner::new();
    let error = run_with_input_at(
        ["vt", "hook", "codex"],
        "",
        &mock,
        &BTreeMap::new(),
        1_700_000_000,
    )
    .unwrap_err();

    assert!(error.to_string().contains("Codex hook event is required"));
    assert!(mock.calls().is_empty());
}

#[test]
fn view_hook_rejects_foreign_owner_before_tmux_access() {
    let mock = MockTmuxRunner::new();
    let error = run_with(
        [
            "vt",
            "hooks",
            "pane-state-view",
            "window-pane-changed",
            "--owner",
            "foreign",
            "--protocol",
            "2",
        ],
        &mock,
        &BTreeMap::new(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("ownership marker mismatch"));
    assert!(mock.calls().is_empty());
}

#[test]
fn view_hook_rejects_unknown_protocol_before_tmux_access() {
    let mock = MockTmuxRunner::new();
    let error = run_with(
        [
            "vt",
            "hooks",
            "pane-state-view",
            "window-pane-changed",
            "--owner",
            crate::daemon::view_hooks::HOOK_OWNER,
            "--protocol",
            "1",
        ],
        &mock,
        &BTreeMap::new(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("ownership marker mismatch"));
    assert!(mock.calls().is_empty());
}
