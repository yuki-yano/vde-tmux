use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
            crate::options::KEY_TASK_ITEMS,
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
            crate::options::KEY_TASK_ITEM_IDS,
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
            crate::options::KEY_TASK_ITEMS,
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
            crate::options::KEY_TASK_ITEM_IDS,
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
    mock.stub(
        &[
            "set-option",
            "-p",
            "-u",
            "-t",
            "%1",
            crate::options::KEY_WORKTREE_ACTIVITY,
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
    assert_eq!(mock.calls().len(), 11);
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
    let root = unique_temp_dir("claude-stop-parent-transcript");
    fs::create_dir_all(&root).unwrap();
    let transcript = root.join("root.jsonl");
    fs::write(
        &transcript,
        r#"{"type":"user","isSidechain":false,"parentUuid":"parent-message"}"#,
    )
    .unwrap();
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
            crate::options::KEY_TASK_ITEMS,
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
            crate::options::KEY_TASK_ITEM_IDS,
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

    run_with_input_at(
        ["vt", "hook", "claude", "Stop"],
        &serde_json::json!({"transcript_path": transcript.display().to_string()}).to_string(),
        &mock,
        &env,
        456,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 6);
    fs::remove_dir_all(root).unwrap();
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
fn dispatch_hook_codex_tool_use_does_not_write_running() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);

    run_with_input_at(
        ["vt", "hook", "codex", "PreToolUse"],
        "{}",
        &mock,
        &env,
        123,
    )
    .unwrap();
    run_with_input_at(
        ["vt", "hook", "codex", "PostToolUse"],
        "{}",
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert!(mock.calls().is_empty());
}

#[test]
fn dispatch_hook_codex_update_plan_writes_task_snapshot() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    let task_items = crate::hook::encode_task_items(&[
        crate::hook::TaskItem {
            step: "Explore".to_string(),
            status: crate::hook::TaskItemStatus::Completed,
        },
        crate::hook::TaskItem {
            step: "Implement".to_string(),
            status: crate::hook::TaskItemStatus::InProgress,
        },
        crate::hook::TaskItem {
            step: "Verify".to_string(),
            status: crate::hook::TaskItemStatus::Pending,
        },
    ]);
    mock.stub(&["show-options", "-p", "-t", "%1"], "");
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_TASKS,
            "1/3",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_TASK_ITEMS,
            &task_items,
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
            crate::options::KEY_TASK_ITEM_IDS,
        ],
        "",
    );

    run_with_input_at(
        ["vt", "hook", "codex", "PostToolUse"],
        r#"{"tool_name":"update_plan","tool_input":{"plan":[{"step":"Explore","status":"completed"},{"step":"Implement","status":"in_progress"},{"step":"Verify","status":"pending"}]}}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 4);
}

#[test]
fn dispatch_hook_codex_create_goal_writes_goal_prompt_and_running() {
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
            "Ship the goal",
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
            "goal",
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
            crate::options::KEY_TASK_ITEMS,
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
            crate::options::KEY_TASK_ITEM_IDS,
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
            crate::options::KEY_WORKTREE_ACTIVITY,
        ],
        "",
    );

    run_with_input_at(
        ["vt", "hook", "codex", "PostToolUse"],
        r#"{"tool_name":"create_goal","tool_input":{"objective":"Ship the\ngoal"}}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 10);
    assert!(mock.calls().iter().any(|call| {
        call == &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_PROMPT,
            "Ship the goal",
        ]
    }));
}

#[test]
fn dispatch_hook_codex_subagent_lifecycle_origin_guard_ignores_parent_state_writes() {
    let codex_home = unique_temp_dir("codex-subagent-lifecycle");
    let session_id = "subagent-lifecycle-session";
    write_codex_subagent_session(&codex_home, session_id);
    let env = codex_env(&codex_home);
    let cases = vec![
        (
            "UserPromptSubmit",
            serde_json::json!({"session_id": session_id, "prompt": "child prompt"}).to_string(),
        ),
        (
            "SessionStart",
            serde_json::json!({"session_id": session_id, "source": "startup"}).to_string(),
        ),
        (
            "Stop",
            serde_json::json!({"session_id": session_id}).to_string(),
        ),
        (
            "PermissionRequest",
            serde_json::json!({"session_id": session_id}).to_string(),
        ),
    ];

    for (event, input) in cases {
        let mock = MockTmuxRunner::new();
        run_with_input_at(["vt", "hook", "codex", event], &input, &mock, &env, 123).unwrap();
        assert!(
            mock.calls().is_empty(),
            "{event} should not write parent pane state: {:?}",
            mock.calls()
        );
    }

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn dispatch_hook_codex_subagent_progress_origin_guard_ignores_task_and_worktree_writes() {
    let codex_home = unique_temp_dir("codex-subagent-progress");
    let session_id = "subagent-progress-session";
    write_codex_subagent_session(&codex_home, session_id);
    let env = codex_env(&codex_home);
    let cases = vec![
        serde_json::json!({
            "session_id": session_id,
            "tool_name": "update_plan",
            "tool_input": {
                "plan": [
                    {"step": "Explore", "status": "completed"},
                    {"step": "Implement", "status": "in_progress"}
                ]
            }
        })
        .to_string(),
        serde_json::json!({
            "session_id": session_id,
            "tool_name": "Bash",
            "tool_input": {"command": "vw exec /tmp/worktrees/feature -- cargo test"}
        })
        .to_string(),
        serde_json::json!({
            "session_id": session_id,
            "tool_name": "create_goal",
            "tool_input": {"objective": "child goal"}
        })
        .to_string(),
    ];

    for input in cases {
        let mock = MockTmuxRunner::new();
        run_with_input_at(
            ["vt", "hook", "codex", "PostToolUse"],
            &input,
            &mock,
            &env,
            123,
        )
        .unwrap();
        assert!(
            mock.calls().is_empty(),
            "subagent PostToolUse should not write parent pane state: {:?}",
            mock.calls()
        );
    }

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn dispatch_hook_codex_subagent_lifecycle_origin_guard_keeps_subagent_rows() {
    let codex_home = unique_temp_dir("codex-subagent-rows");
    write_codex_subagent_session(&codex_home, "agent-a");
    let env = codex_env(&codex_home);

    let start_mock = MockTmuxRunner::new();
    start_mock.stub(&["show-options", "-p", "-t", "%1"], "");
    start_mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_SUBAGENTS,
            "agent-a:Plan",
        ],
        "",
    );
    run_with_input_at(
        ["vt", "hook", "codex", "SubagentStart"],
        r#"{"session_id":"agent-a","agent_id":"agent-a","agent_type":"Plan"}"#,
        &start_mock,
        &env,
        123,
    )
    .unwrap();
    assert_eq!(start_mock.calls().len(), 2);

    let stop_mock = MockTmuxRunner::new();
    stop_mock.stub(
        &["show-options", "-p", "-t", "%1"],
        "@vde_subagents \"agent-a:Plan\"\n",
    );
    stop_mock.stub(
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
        ["vt", "hook", "codex", "SubagentStop"],
        r#"{"session_id":"agent-a","agent_id":"agent-a"}"#,
        &stop_mock,
        &env,
        123,
    )
    .unwrap();
    assert_eq!(stop_mock.calls().len(), 2);

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn dispatch_hook_codex_update_plan_empty_unsets_task_options() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    mock.stub(&["show-options", "-p", "-t", "%1"], "@vde_tasks \"1/1\"\n");
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
            crate::options::KEY_TASK_ITEMS,
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
            crate::options::KEY_TASK_ITEM_IDS,
        ],
        "",
    );

    run_with_input_at(
        ["vt", "hook", "codex", "PostToolUse"],
        r#"{"tool_name":"update_plan","tool_input":{"plan":[]}}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 4);
}

#[test]
fn dispatch_hook_claude_todo_write_writes_task_snapshot() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    let task_items = crate::hook::encode_task_items(&[
        crate::hook::TaskItem {
            step: "Explore".to_string(),
            status: crate::hook::TaskItemStatus::Completed,
        },
        crate::hook::TaskItem {
            step: "Implement".to_string(),
            status: crate::hook::TaskItemStatus::InProgress,
        },
        crate::hook::TaskItem {
            step: "Verify".to_string(),
            status: crate::hook::TaskItemStatus::Pending,
        },
    ]);
    mock.stub(&["show-options", "-p", "-t", "%1"], "");
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_TASKS,
            "1/3",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_TASK_ITEMS,
            &task_items,
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
            crate::options::KEY_TASK_ITEM_IDS,
        ],
        "",
    );

    run_with_input_at(
        ["vt", "hook", "claude", "PostToolUse"],
        r#"{"tool_name":"TodoWrite","tool_input":{"todos":[{"content":"Explore","status":"completed","activeForm":"Exploring"},{"content":"Implement","status":"in_progress","activeForm":"Implementing"},{"content":"Verify","status":"pending","activeForm":"Verifying"}]}}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 4);
}

#[test]
fn dispatch_hook_claude_subagent_lifecycle_origin_guard_ignores_parent_state_writes() {
    let root = unique_temp_dir("claude-subagent-lifecycle");
    let transcript = write_claude_subagent_transcript(&root, "lifecycle");
    let transcript = transcript.display().to_string();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    let cases = vec![
        (
            "UserPromptSubmit",
            serde_json::json!({"transcript_path": transcript.clone(), "prompt": "child prompt"})
                .to_string(),
        ),
        (
            "SessionStart",
            serde_json::json!({"transcript_path": transcript.clone(), "source": "startup"})
                .to_string(),
        ),
        (
            "Stop",
            serde_json::json!({"transcript_path": transcript.clone()}).to_string(),
        ),
        (
            "Notification",
            serde_json::json!({
                "transcript_path": transcript.clone(),
                "notification_type": "permission_prompt"
            })
            .to_string(),
        ),
        (
            "PreToolUse",
            serde_json::json!({"transcript_path": transcript.clone()}).to_string(),
        ),
        (
            "PostToolUse",
            serde_json::json!({
                "transcript_path": transcript.clone(),
                "tool_name": "Read",
                "tool_input": {"file_path": "README.md"}
            })
            .to_string(),
        ),
    ];

    for (event, input) in cases {
        let mock = MockTmuxRunner::new();
        run_with_input_at(["vt", "hook", "claude", event], &input, &mock, &env, 123).unwrap();
        assert!(
            mock.calls().is_empty(),
            "{event} should not write parent pane state: {:?}",
            mock.calls()
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_hook_claude_subagent_task_origin_guard_ignores_task_writes() {
    let root = unique_temp_dir("claude-subagent-task");
    let transcript = write_claude_subagent_transcript(&root, "task");
    let transcript = transcript.display().to_string();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    let cases = vec![
        (
            "PostToolUse",
            serde_json::json!({
                "transcript_path": transcript.clone(),
                "tool_name": "TodoWrite",
                "tool_input": {
                    "todos": [
                        {"content": "Explore", "status": "completed"},
                        {"content": "Implement", "status": "in_progress"}
                    ]
                }
            })
            .to_string(),
        ),
        (
            "PostToolUse",
            serde_json::json!({
                "transcript_path": transcript.clone(),
                "tool_name": "TaskCreate",
                "tool_input": {"subject": "Explore"},
                "tool_response": {"task": {"id": "1", "subject": "Explore"}}
            })
            .to_string(),
        ),
        (
            "PostToolUse",
            serde_json::json!({
                "transcript_path": transcript.clone(),
                "tool_name": "TaskUpdate",
                "tool_input": {"taskId": "1", "status": "completed"}
            })
            .to_string(),
        ),
        (
            "TaskCreated",
            serde_json::json!({"transcript_path": transcript.clone()}).to_string(),
        ),
        (
            "TaskCompleted",
            serde_json::json!({"transcript_path": transcript.clone()}).to_string(),
        ),
    ];

    for (event, input) in cases {
        let mock = MockTmuxRunner::new();
        run_with_input_at(["vt", "hook", "claude", event], &input, &mock, &env, 123).unwrap();
        assert!(
            mock.calls().is_empty(),
            "{event} should not write parent task state: {:?}",
            mock.calls()
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_hook_claude_todo_write_empty_unsets_task_options() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    mock.stub(
        &["show-options", "-p", "-t", "%1"],
        "@vde_tasks \"1/1\"\n@vde_task_items '[{\"step\":\"Explore\",\"status\":\"completed\"}]'\n",
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
            crate::options::KEY_TASK_ITEMS,
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
            crate::options::KEY_TASK_ITEM_IDS,
        ],
        "",
    );

    run_with_input_at(
        ["vt", "hook", "claude", "PostToolUse"],
        r#"{"tool_name":"TodoWrite","tool_input":{"todos":[]}}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 4);
}

#[test]
fn dispatch_hook_claude_task_create_writes_task_item_with_id() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    let task_items = crate::hook::encode_task_items(&[crate::hook::TaskItem {
        step: "ブランチと最新コミットを確認".to_string(),
        status: crate::hook::TaskItemStatus::Pending,
    }]);
    let task_item_ids = serde_json::to_string(&["1"]).unwrap();
    mock.stub(&["show-options", "-p", "-t", "%1"], "");
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
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_TASK_ITEMS,
            &task_items,
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_TASK_ITEM_IDS,
            &task_item_ids,
        ],
        "",
    );

    run_with_input_at(
        ["vt", "hook", "claude", "PostToolUse"],
        r#"{"tool_name":"TaskCreate","tool_input":{"subject":"ブランチと最新コミットを確認","description":"git でカレントブランチと最新コミットを表示する（読み取り専用）","activeForm":"ブランチと最新コミットを確認中"},"tool_response":{"task":{"id":"1","subject":"ブランチと最新コミットを確認"}}}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 4);
}

#[test]
fn dispatch_hook_claude_task_update_updates_existing_task_item() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    let initial_items = crate::hook::encode_task_items(&[
        crate::hook::TaskItem {
            step: "現在日時".to_string(),
            status: crate::hook::TaskItemStatus::Completed,
        },
        crate::hook::TaskItem {
            step: "カレントディレクトリ".to_string(),
            status: crate::hook::TaskItemStatus::Pending,
        },
    ]);
    let initial_ids = serde_json::to_string(&["1", "2"]).unwrap();
    let updated_items = crate::hook::encode_task_items(&[
        crate::hook::TaskItem {
            step: "現在日時".to_string(),
            status: crate::hook::TaskItemStatus::Completed,
        },
        crate::hook::TaskItem {
            step: "カレントディレクトリ".to_string(),
            status: crate::hook::TaskItemStatus::InProgress,
        },
    ]);
    let escaped_items = serde_json::to_string(&initial_items).unwrap();
    let escaped_ids = serde_json::to_string(&initial_ids).unwrap();
    mock.stub(
        &["show-options", "-p", "-t", "%1"],
        &format!(
            "@vde_tasks \"1/2\"\n@vde_task_items {escaped_items}\n@vde_task_item_ids {escaped_ids}\n"
        ),
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_TASKS,
            "1/2",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_TASK_ITEMS,
            &updated_items,
        ],
        "",
    );

    run_with_input_at(
        ["vt", "hook", "claude", "PostToolUse"],
        r#"{"tool_name":"TaskUpdate","tool_input":{"taskId":"2","status":"in_progress"},"tool_response":{"success":true,"taskId":"2","updatedFields":["status"],"statusChange":{"from":"pending","to":"in_progress"}}}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 3);
}

#[test]
fn dispatch_hook_codex_update_plan_missing_fields_is_ignored() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);

    run_with_input_at(
        ["vt", "hook", "codex", "PostToolUse"],
        r#"{"tool_name":"update_plan","tool_input":{"plan":[{"step":"Explore"}]}}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert!(mock.calls().is_empty());
}

#[test]
fn dispatch_hook_codex_bash_vw_exec_writes_worktree_activity() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    let expected = crate::hook::encode_worktree_activity(&crate::hook::WorktreeActivity {
        kind: crate::hook::WorktreeActivityKind::VwExec,
        name: "feature".to_string(),
        path: "/tmp/worktrees/feature".to_string(),
        command: "vw exec /tmp/worktrees/feature -- cargo test".to_string(),
        observed_at: 123,
    });
    mock.stub(&["show-options", "-p", "-t", "%1"], "");
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_WORKTREE_ACTIVITY,
            &expected,
        ],
        "",
    );

    run_with_input_at(
        ["vt", "hook", "codex", "PostToolUse"],
        r#"{"tool_name":"Bash","tool_input":{"command":"vw exec /tmp/worktrees/feature -- cargo test"}}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 2);
}

#[test]
fn dispatch_hook_codex_bash_non_vw_exec_is_ignored() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);

    run_with_input_at(
        ["vt", "hook", "codex", "PostToolUse"],
        r#"{"tool_name":"Bash","tool_input":{"command":"cargo test"}}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert!(mock.calls().is_empty());
}

#[test]
fn dispatch_hook_codex_non_plan_non_bash_post_tool_use_is_ignored() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);

    run_with_input_at(
        ["vt", "hook", "codex", "PostToolUse"],
        r#"{"tool_name":"Read","tool_input":{"file_path":"README.md"}}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert!(mock.calls().is_empty());
}

#[test]
fn dispatch_hook_codex_subagent_start_appends_or_replaces() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    mock.stub(
        &["show-options", "-p", "-t", "%1"],
        "@vde_subagents \"a:Explore\"\n",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_SUBAGENTS,
            "a:Plan",
        ],
        "",
    );

    run_with_input_at(
        ["vt", "hook", "codex", "SubagentStart"],
        r#"{"agent_id":"a","agent_type":"Plan"}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 2);
}

#[test]
fn dispatch_hook_codex_subagent_stop_removes_and_unsets_last_entry() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    mock.stub(
        &["show-options", "-p", "-t", "%1"],
        "@vde_subagents \"a:Explore\"\n",
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
        ["vt", "hook", "codex", "SubagentStop"],
        r#"{"agent_id":"a"}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 2);
}

#[test]
fn dispatch_hook_codex_subagent_stop_removes_one_entry() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    mock.stub(
        &["show-options", "-p", "-t", "%1"],
        "@vde_subagents \"a:Explore|b:Plan\"\n",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_SUBAGENTS,
            "b:Plan",
        ],
        "",
    );

    run_with_input_at(
        ["vt", "hook", "codex", "SubagentStop"],
        r#"{"agent_id":"a"}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 2);
}

#[test]
fn dispatch_hook_codex_user_prompt_submit_clears_worktree_activity() {
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
            crate::options::KEY_TASK_ITEMS,
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
            crate::options::KEY_TASK_ITEM_IDS,
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
            crate::options::KEY_STARTED_AT,
            "123",
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
            crate::options::KEY_WORKTREE_ACTIVITY,
        ],
        "",
    );

    run_with_input_at(
        ["vt", "hook", "codex", "UserPromptSubmit"],
        "{}",
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 8);
}

#[test]
fn dispatch_hook_codex_session_start_clears_worktree_activity() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    for key in crate::options::PANE_STATE_KEYS {
        mock.stub(&["set-option", "-p", "-u", "-t", "%1", key], "");
    }
    mock.stub(
        &[
            "set-option",
            "-p",
            "-u",
            "-t",
            "%1",
            crate::options::KEY_TASK_ITEM_IDS,
        ],
        "",
    );
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
            "0",
        ],
        "",
    );

    run_with_input_at(
        ["vt", "hook", "codex", "SessionStart"],
        r#"{"source":"startup"}"#,
        &mock,
        &env,
        123,
    )
    .unwrap();

    assert!(mock.calls().iter().any(|call| {
        call == &[
            "set-option",
            "-p",
            "-u",
            "-t",
            "%1",
            crate::options::KEY_WORKTREE_ACTIVITY,
        ]
    }));
    assert_eq!(
        mock.calls().len(),
        crate::options::PANE_STATE_KEYS.len() + 5
    );
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
fn dispatch_hook_claude_stop_payload_subagent_stop_updates_subagents_without_done() {
    let root = unique_temp_dir("claude-subagent-stop");
    let agent_transcript = write_claude_subagent_transcript(&root, "stop");
    let agent_transcript = agent_transcript.display().to_string();
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
    mock.stub(
        &["show-options", "-p", "-t", "%1"],
        "@vde_subagents \"a:Explore|b:Plan\"\n",
    );
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            crate::options::KEY_SUBAGENTS,
            "b:Plan",
        ],
        "",
    );

    let input = serde_json::json!({
        "hook_event_name": "SubagentStop",
        "agent_id": "a",
        "agent_type": "Explore",
        "agent_transcript_path": agent_transcript
    })
    .to_string();
    run_with_input_at(["vt", "hook", "claude", "Stop"], &input, &mock, &env, 456).unwrap();

    assert_eq!(
        mock.calls(),
        vec![
            vec![
                "show-options".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                "%1".to_string(),
            ],
            vec![
                "set-option".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                "%1".to_string(),
                crate::options::KEY_SUBAGENTS.to_string(),
                "b:Plan".to_string(),
            ],
        ]
    );

    fs::remove_dir_all(root).unwrap();
}

fn codex_env(codex_home: &Path) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("TMUX_PANE".to_string(), "%1".to_string()),
        ("CODEX_HOME".to_string(), codex_home.display().to_string()),
    ])
}

fn write_codex_subagent_session(codex_home: &Path, session_id: &str) {
    let sessions = codex_home
        .join("sessions")
        .join("2026")
        .join("07")
        .join("08");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join(format!("rollout-{session_id}.jsonl")),
        serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": session_id,
                "thread_source": "subagent",
                "parent_thread_id": "parent-session"
            }
        })
        .to_string(),
    )
    .unwrap();
}

fn write_claude_subagent_transcript(root: &Path, name: &str) -> PathBuf {
    let dir = root.join("subagents");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.jsonl"));
    fs::write(&path, r#"{"isSidechain":true}"#).unwrap();
    path
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("vde-tmux-{name}-{}-{nanos}", std::process::id()))
}
