use super::*;

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
fn dispatch_hook_claude_notification_permission_writes_waiting() {
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
            "claude",
        ],
        "",
    );

    run_with_input_at(
        ["vt", "hook", "claude", "Notification"],
        r#"{"notification_type":"permission_prompt"}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 3);
}

#[test]
fn dispatch_hook_claude_stop_writes_idle_attention_and_completed_at() {
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

    run_with_input_at(["vt", "hook", "claude", "Stop"], "{}", &mock, &env, 456).unwrap();

    assert_eq!(mock.calls().len(), 6);
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
