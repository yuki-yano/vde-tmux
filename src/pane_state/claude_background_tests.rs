use super::*;
use crate::hook::adapter::{TypedAdapterContext, claude_typed_event_from_json};
use serde_json::{Value, json};

struct Harness {
    state: Option<PaneState>,
    tracker: CaptureTrackerSnapshot,
    at: i64,
}

impl Harness {
    fn new() -> Self {
        Self {
            state: None,
            tracker: Default::default(),
            at: 10,
        }
    }
    fn context(&self) -> TypedAdapterContext {
        TypedAdapterContext {
            daemon_instance_id: DaemonInstanceId::parse("00112233445566778899aabbccddeeff")
                .unwrap(),
            event_id: EventId::generate().unwrap(),
            pane_instance: PaneInstance {
                pane_id: "%1".into(),
                pane_pid: 42,
            },
            observed_at: self.at,
        }
    }
    fn hook(&mut self, event: &str, payload: Value) {
        let Some(envelope) =
            claude_typed_event_from_json(event, &payload.to_string(), &self.context()).unwrap()
        else {
            return;
        };
        self.apply(envelope);
    }
    fn apply(&mut self, envelope: PaneEventEnvelope) {
        let result = reduce(
            self.state.as_ref(),
            &envelope,
            ReductionContext {
                tracker: &self.tracker,
                visibility: &VisibilitySnapshot::default(),
                private_task_prompt: None,
                new_state_id: Some(StateId::parse("ffeeddccbbaa99887766554433221100").unwrap()),
                latest_unread_order: 0,
            },
        )
        .unwrap();
        self.state = result.record;
        if let Some(delta) = result.tracker_delta {
            self.tracker = delta.next;
        }
        self.at += 1;
    }
    fn s(&self) -> &PaneState {
        self.state.as_ref().unwrap()
    }
    fn start(&mut self) {
        self.hook(
            "UserPromptSubmit",
            json!({"session_id":"s1","prompt":"original request"}),
        );
    }
    fn register(&mut self, id: &str) {
        self.hook("PostToolUse", json!({"session_id":"s1","tool_name":"Bash","tool_use_id":format!("tool-{id}"),"tool_input":{"command":"vt agent wait %2 --until done --timeout-ms 60000"},"tool_response":{"backgroundTaskId":id}}));
    }
    fn stop(&mut self, tasks: Value, response: Option<&str>) {
        let mut value = json!({"session_id":"s1","background_tasks":tasks});
        if let Some(response) = response {
            value["last_assistant_message"] = json!(response);
        }
        self.hook("Stop", value);
    }
    fn notify(&mut self, id: &str, status: &str) {
        self.hook("UserPromptSubmit",json!({"session_id":"s1","prompt":format!("<task-notification><task-id>{id}</task-id><tool-use-id>tool-{id}</tool-use-id><status>{status}</status></task-notification>")}));
    }
    fn manual_done(&mut self) {
        let mut envelope = self.context().envelope(
            AgentKind::parse("claude").unwrap(),
            AgentSessionId::parse("s1").unwrap(),
            PaneEvent::MarkDone {
                expected: self.s().version(),
                completed_at: self.at,
            },
        );
        envelope.agent = None;
        envelope.agent_session_id = None;
        self.apply(envelope);
    }
}

#[test]
fn empty_stop_then_delivery_keeps_one_run_and_saves_only_final_response() {
    let mut h = Harness::new();
    h.start();
    h.register("b1");
    h.stop(json!([]), Some("QUEUED"));
    assert_eq!(
        (
            h.s().run_seq,
            h.s().completed_seq,
            h.s().unread.occurrence_seq
        ),
        (1, 0, 0)
    );
    assert!(h.s().claude_background.is_awaiting_result());
    assert_eq!(
        h.s().claude_background.tasks[0].last_registry_presence,
        RegistryPresence::Absent
    );
    assert!(h.s().latest_response.is_none());
    h.notify("b1", "completed");
    assert_eq!(h.s().prompt.as_ref().unwrap().text, "original request");
    assert!(!h.s().claude_background.paused);
    assert_eq!(h.s().completed_seq, 0);
    h.stop(json!([]), Some("RECEIVED"));
    assert_eq!(h.s().completed_seq, 1);
    assert_eq!(h.s().unread.occurrence_seq, 1);
    assert_eq!(h.s().latest_response.as_ref().unwrap().text, "RECEIVED");
    h.stop(json!([]), Some("duplicate stop"));
    assert_eq!(h.s().unread.occurrence_seq, 1);
}

#[test]
fn delivery_before_stop_and_missing_final_body() {
    let mut h = Harness::new();
    h.start();
    h.register("b1");
    h.notify("b1", "failed");
    h.stop(json!([]), None);
    assert_eq!(h.s().completed_seq, 1);
    assert!(h.s().latest_response.is_none());
}

#[test]
fn delivered_receipt_still_requires_parent_stop_even_after_capture_completion() {
    let mut h = Harness::new();
    h.start();
    h.register("b1");
    h.notify("b1", "completed");
    h.apply(h.context().envelope(
        AgentKind::parse("claude").unwrap(),
        AgentSessionId::parse("s1").unwrap(),
        PaneEvent::CompleteRun { completed_at: h.at },
    ));
    assert_eq!(h.s().completed_seq, 0);
    assert_eq!(h.s().claude_background.tasks.len(), 1);
    h.stop(json!([]), Some("final response"));
    assert_eq!(h.s().completed_seq, 1);
}

#[test]
fn agent_exit_after_delivery_is_an_error_without_successful_completion() {
    let mut h = Harness::new();
    h.start();
    h.register("b1");
    h.notify("b1", "completed");
    h.hook("SessionEnd", json!({"session_id":"s1"}));
    assert_eq!(h.s().completed_seq, 0);
    assert!(
        matches!(&h.s().lifecycle, LifecycleState::Error { reason: Some(reason) }
        if reason == "await_agent_ended")
    );
    assert!(h.s().claude_background.tasks.is_empty());
}

#[test]
fn subagent_deferral_keeps_the_receipt_gate_until_another_parent_stop() {
    let mut h = Harness::new();
    h.start();
    h.register("b1");
    h.notify("b1", "completed");
    h.apply(h.context().envelope(
        AgentKind::parse("claude").unwrap(),
        AgentSessionId::parse("s1").unwrap(),
        PaneEvent::ProgressUpdated {
            observed_at: h.at,
            operations: vec![ProgressOperation::UpsertSubagent(SubagentState {
                agent_id: "worker".into(),
                agent_type: "review".into(),
                display_name: None,
            })],
        },
    ));
    h.stop(json!([]), Some("interim"));
    assert_eq!(h.s().completed_seq, 0);
    assert!(h.s().latest_response.is_none());
    assert_eq!(h.s().claude_background.tasks.len(), 1);
    h.apply(h.context().envelope(
        AgentKind::parse("claude").unwrap(),
        AgentSessionId::parse("s1").unwrap(),
        PaneEvent::ProgressUpdated {
            observed_at: h.at,
            operations: vec![ProgressOperation::ClearSubagents],
        },
    ));
    h.apply(h.context().envelope(
        AgentKind::parse("claude").unwrap(),
        AgentSessionId::parse("s1").unwrap(),
        PaneEvent::CompleteRun { completed_at: h.at },
    ));
    assert_eq!(h.s().completed_seq, 0);
    h.stop(json!([]), Some("final"));
    assert_eq!(h.s().completed_seq, 1);
    assert_eq!(h.s().latest_response.as_ref().unwrap().text, "final");
}

#[test]
fn all_waits_required_and_registry_unknown_never_finishes() {
    let mut h = Harness::new();
    h.start();
    h.register("b1");
    h.register("b2");
    h.stop(json!([{ "id":"b1","type":"shell" }]), None);
    assert_eq!(
        h.s().claude_background.tasks[0].last_registry_presence,
        RegistryPresence::Present
    );
    h.notify("b1", "new-status");
    h.at += 100000;
    h.stop(Value::Null, None);
    assert_eq!(h.s().completed_seq, 0);
    assert!(matches!(h.s().lifecycle, LifecycleState::Running));
    assert_eq!(
        h.s().claude_background.tasks[1].last_registry_presence,
        RegistryPresence::Unknown
    );
    h.notify("b2", "killed");
    h.stop(json!([]), None);
    assert_eq!(h.s().completed_seq, 1);
}

#[test]
fn cancellation_requires_matching_success_receipt_and_late_status_cannot_undo_it() {
    let mut h = Harness::new();
    h.start();
    h.register("b1");
    let stop = |command| json!({"session_id":"s1","tool_name":"TaskStop","tool_response":{"task_id":"b1","task_type":"local_bash","command":command}});
    h.hook(
        "PostToolUseFailure",
        stop("vt agent wait %2 --until done --timeout-ms 60000"),
    );
    h.hook("PostToolUse", stop("wrong command"));
    let mut unknown = stop("vt agent wait %2 --until done --timeout-ms 60000");
    unknown["tool_response"]["task_id"] = json!("unknown-task");
    h.hook("PostToolUse", unknown);
    h.stop(json!([]), None);
    assert_eq!(h.s().completed_seq, 0);
    h.hook(
        "PostToolUse",
        stop("vt agent wait %2 --until done --timeout-ms 60000"),
    );
    h.notify("b1", "killed");
    assert_eq!(
        h.s().claude_background.tasks[0].receipt,
        BackgroundReceipt::Cancelled
    );
    h.stop(json!([]), None);
    assert_eq!(h.s().completed_seq, 1);
    h.notify("b1", "killed");
    assert!(h.s().claude_background.tasks.is_empty());
    h.stop(json!([]), None);
    assert_eq!(h.s().completed_seq, 2);
}

#[test]
fn invalid_notification_error_is_reported_once_and_has_manual_exit() {
    let mut h = Harness::new();
    h.start();
    h.register("b1");
    h.notify("b1", "");
    h.stop(json!([]), None);
    assert!(matches!(h.s().lifecycle, LifecycleState::Error { .. }));
    assert_eq!(h.s().unread.occurrence_seq, 1);
    for _ in 0..2 {
        h.start();
        h.stop(json!([]), None);
    }
    assert_eq!(h.s().unread.occurrence_seq, 1);
    assert_eq!(h.s().completed_seq, 0);
    h.manual_done();
    assert_eq!(h.s().completed_seq, 1);
    assert!(!h.s().claude_background.blocks_completion());
    h.notify("b1", "completed");
    assert!(h.s().claude_background.tasks.is_empty());
}

#[test]
fn external_notification_preserves_prompt_and_wrong_identity_never_releases() {
    let mut h = Harness::new();
    h.start();
    h.register("b1");
    h.notify("service", "completed");
    assert_eq!(h.s().prompt.as_ref().unwrap().text, "original request");
    h.hook("UserPromptSubmit",json!({"session_id":"s1","prompt":"<task-notification><task-id>b1</task-id><tool-use-id>wrong</tool-use-id><status>completed</status></task-notification>"}));
    h.stop(json!([]), None);
    assert_eq!(h.s().completed_seq, 0);
}

#[test]
fn denied_interrupted_and_nonparent_tools_never_register_or_fault() {
    let mut h = Harness::new();
    h.start();
    let input = json!({"session_id":"s1","tool_name":"Bash","tool_use_id":"t1","tool_input":{"command":"vt agent wait %2"}});
    h.hook("PreToolUse", input.clone());
    h.start();
    h.stop(json!([]), None);
    assert_eq!(h.s().completed_seq, 1);
    h.start();
    let mut interrupted = input.clone();
    interrupted["tool_response"] = json!({"interrupted":true});
    h.hook("PostToolUse", interrupted);
    let mut child = input;
    child["agent_transcript_path"] = json!("/tmp/subagent.jsonl");
    child["tool_response"] = json!({"backgroundTaskId":"child"});
    h.hook("PostToolUse", child);
    h.stop(json!([]), None);
    assert_eq!(h.s().completed_seq, 2);
    assert!(h.s().claude_background.tasks.is_empty());
}

#[test]
fn quota_and_provider_error_are_not_cleared_by_receipts() {
    let mut h = Harness::new();
    h.start();
    h.register("b1");
    h.hook(
        "StopFailure",
        json!({"session_id":"s1","error":"rate_limit"}),
    );
    h.notify("b1", "completed");
    assert!(h.s().lifecycle.is_usage_limited());
}

#[test]
fn normal_stop_recovers_provider_failure_while_other_receipts_remain_pending() {
    for error in ["rate_limit", "overloaded"] {
        let mut h = Harness::new();
        h.start();
        h.register("b1");
        h.register("b2");
        h.hook("StopFailure", json!({"session_id":"s1","error":error}));
        h.notify("b1", "completed");
        assert!(!matches!(h.s().lifecycle, LifecycleState::Running));
        h.stop(json!([]), Some("recovered"));
        assert!(matches!(h.s().lifecycle, LifecycleState::Running));
        assert_eq!(h.s().completed_seq, 0);
        h.notify("b2", "completed");
        h.stop(json!([]), Some("final"));
        assert_eq!(h.s().completed_seq, 1);
    }
}

#[test]
fn batched_receipts_preserve_prompt_and_require_final_stop() {
    let mut h = Harness::new();
    h.start();
    h.register("b1");
    h.register("b2");
    h.stop(json!([]), None);
    let notifications = ["b1","b2"].map(|id| format!("<task-notification><task-id>{id}</task-id><tool-use-id>tool-{id}</tool-use-id><status>completed</status><summary>raw <angle> description</summary></task-notification>")).join("\n");
    h.hook(
        "UserPromptSubmit",
        json!({"session_id":"s1","prompt":notifications}),
    );
    assert_eq!(h.s().prompt.as_ref().unwrap().text, "original request");
    assert_eq!(h.s().claude_background.pending_count(), 0);
    assert_eq!(h.s().completed_seq, 0);
    h.stop(json!([]), None);
    assert_eq!(h.s().completed_seq, 1);
}

#[test]
fn distinct_faults_each_report_once_even_when_already_blocked() {
    let mut h = Harness::new();
    h.start();
    h.register("b1");
    h.notify("b1", "");
    h.stop(json!([]), None);
    assert_eq!(h.s().unread.occurrence_seq, 1);
    h.hook("PostToolUse", json!({"session_id":"s1","tool_name":"Bash","tool_use_id":"invalid-tool","tool_input":{"command":"vt agent wait %2"},"tool_response":{"backgroundTaskId":42}}));
    h.stop(json!([]), None);
    assert!(
        matches!(&h.s().lifecycle,LifecycleState::Error { reason:Some(reason) } if reason=="await_launch_unconfirmed")
    );
    assert_eq!(h.s().unread.occurrence_seq, 2);
    for _ in 0..2 {
        h.start();
        h.stop(json!([]), None);
    }
    assert_eq!(h.s().unread.occurrence_seq, 2);
}

#[test]
fn claude_observations_cannot_complete_a_different_provider() {
    let h = Harness::new();
    let envelope = h.context().envelope(
        AgentKind::parse("codex").unwrap(),
        AgentSessionId::parse("s1").unwrap(),
        PaneEvent::ClaudeStopped {
            observed_at: h.at,
            response: None,
            registry: Some(vec![]),
            crons: None,
        },
    );
    let result = reduce(
        None,
        &envelope,
        ReductionContext {
            tracker: &h.tracker,
            visibility: &VisibilitySnapshot::default(),
            private_task_prompt: None,
            new_state_id: Some(StateId::parse("ffeeddccbbaa99887766554433221100").unwrap()),
            latest_unread_order: 0,
        },
    );
    assert!(matches!(result, Err(ReduceError::InvalidRequest(_))));
}

#[test]
fn whitespace_background_id_faults_and_unreceipted_delivery_cannot_be_hydrated() {
    let mut h = Harness::new();
    h.start();
    h.hook("PostToolUse", json!({"session_id":"s1","tool_name":"Bash","tool_use_id":"t1","tool_input":{"command":"vt agent wait %2"},"tool_response":{"backgroundTaskId":"   "}}));
    h.stop(json!([]), None);
    assert!(
        matches!(&h.s().lifecycle,LifecycleState::Error { reason:Some(reason) } if reason=="await_launch_unconfirmed")
    );
    assert!(h.s().claude_background.tasks.is_empty());
    let mut other = Harness::new();
    other.start();
    other.register("b1");
    let mut snapshot = other.s().clone();
    snapshot.claude_background.tasks[0].receipt = BackgroundReceipt::Delivered;
    assert!(snapshot.validate().is_err());
}

#[test]
fn duplicate_registration_is_idempotent_but_both_identity_collisions_fault() {
    for same_task in [true, false] {
        let mut h = Harness::new();
        h.start();
        h.register("b1");
        h.register("b1");
        assert_eq!(h.s().claude_background.tasks.len(), 1);
        assert!(h.s().claude_background.faults.is_empty());
        h.hook("PostToolUse",json!({"session_id":"s1","tool_name":"Bash","tool_use_id":if same_task { "different-tool" } else { "tool-b1" },"tool_input":{"command":"vt agent wait %2 --until done --timeout-ms 60000"},"tool_response":{"backgroundTaskId":if same_task { "b1" } else { "b2" }}}));
        h.stop(json!([]), None);
        assert_eq!(h.s().claude_background.tasks.len(), 1);
        assert_eq!(h.s().completed_seq, 0);
        assert!(
            matches!(&h.s().lifecycle,LifecycleState::Error { reason:Some(reason) } if reason=="await_identity_conflict")
        );
    }
}

#[test]
fn task_limit_and_epoch_end_never_generate_completion() {
    let mut h = Harness::new();
    h.start();
    for i in 0..=256 {
        h.register(&format!("b{i}"));
    }
    h.stop(json!([]), None);
    assert_eq!(h.s().claude_background.tasks.len(), 256);
    assert_eq!(h.s().completed_seq, 0);
    assert!(
        matches!(&h.s().lifecycle,LifecycleState::Error{reason:Some(r)} if r=="await_tracking_overflow")
    );
    h.hook("SessionEnd", json!({"session_id":"s1"}));
    assert!(h.s().claude_background.tasks.is_empty());
    assert_eq!(h.s().completed_seq, 0);
    h.hook("SessionStart", json!({"session_id":"s2","source":"resume"}));
    assert!(!h.s().claude_background.blocks_completion());
    assert_eq!(h.s().run_seq, 0);
}

#[test]
fn cron_snapshot_is_independent_and_missing_is_unknown() {
    let mut h = Harness::new();
    h.start();
    h.hook("Stop",json!({"session_id":"s1","session_crons":[{"id":"c1","schedule":"* * * * *","recurring":true}]}));
    assert_eq!(h.s().completed_seq, 1);
    assert_eq!(h.s().claude_crons.entries.len(), 1);
    h.start();
    h.stop(json!([]), None);
    assert!(h.s().claude_crons.entries.is_empty());
}

#[test]
fn native_probe_shapes_replay_with_target_command_remapped() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/claude-background-hooks.json"
    ))
    .unwrap();
    for observation in fixture["observations"].as_array().unwrap() {
        let case = observation["case"].as_str().unwrap();
        let mut h = Harness::new();
        let mut stops = 0;
        for event in observation["events"].as_array().unwrap() {
            let mut event = event.clone();
            let name = event["hook_event_name"].as_str().unwrap().to_string();
            if name == "SessionEnd" {
                continue;
            }
            if matches!(case, "finite" | "quick" | "fail" | "stop") && event["tool_name"] == "Bash"
            {
                event["tool_input"]["command"] =
                    json!("vt agent wait %2 --until done --timeout-ms 60000");
            }
            if case == "stop" && event["tool_name"] == "TaskStop" {
                event["tool_response"]["command"] =
                    json!("vt agent wait %2 --until done --timeout-ms 60000");
            }
            h.hook(&name, event);
            if name == "Stop" {
                stops += 1;
                if stops == 1 && matches!(case, "finite" | "quick" | "fail") {
                    assert_eq!(h.s().completed_seq, 0, "{case}");
                }
            }
        }
        assert!(h.s().completed_seq > 0, "{case}");
        assert!(!h.s().claude_background.blocks_completion(), "{case}");
    }
}

#[test]
fn recognizer_accepts_only_literal_valid_wait_calls() {
    for command in [
        "vt agent wait %2",
        "vt agent run wait 'run-1' --until completed",
        "vt agent operation wait op-1 --until prompt-confirmed",
        "vt agent --json wait \"%2\" --timeout-ms 3000",
    ] {
        assert!(crate::cli::is_literal_agent_wait(command), "{command}");
    }
    for command in [
        "vt agent wait $ID",
        "vt agent wait $(id)",
        "vt agent wait %2 | cat",
        "vt agent wait %2 > out",
        "python3 x.py",
        "sh -c 'vt agent wait %2'",
        "vt agent wait 'broken",
        "vt agent wait %2 --wrong",
        "X=1 vt agent wait %2",
        "vt agent wait %2; true",
    ] {
        assert!(!crate::cli::is_literal_agent_wait(command), "{command}");
    }
}
