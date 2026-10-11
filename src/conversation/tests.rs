//! Fixtures are synthetic and shaped after codex-cli 0.162.1 and Claude Code 2.1.287 records.
use serde_json::{Value, json};

use super::*;

const CODEX: &str = "codex-session";
const CLAUDE: &str = "claude-session";

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/conversation")
        .join(name);
    std::fs::read(path).unwrap()
}

fn jsonl(records: &[Value]) -> Vec<u8> {
    records
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>()
        .into_bytes()
}

fn view(parsed: &ParsedConversation) -> Vec<(MessageKind, &str)> {
    parsed
        .messages
        .iter()
        .map(|message| (message.kind, message.text.as_str()))
        .collect()
}

fn reasons(parsed: &ParsedConversation) -> Vec<&str> {
    parsed
        .omissions
        .iter()
        .map(|omission| omission.reason.as_str())
        .collect()
}

fn error(provider: &str, session: &str, records: &[Value]) -> String {
    parse(provider, session, &jsonl(records)).unwrap_err().0
}

fn long_tool_output() -> String {
    "synthetic tool output line\n".repeat(30)
}

use MessageKind::{Commentary, CompactionSummary, Final, Instruction, ToolCall, ToolResult};

// ---------- Codex ----------

fn codex_meta() -> Value {
    json!({"type":"session_meta","payload":{"id":CODEX,"thread_source":"user","history_mode":"paginated"}})
}
fn codex_event(payload: Value) -> Value {
    json!({"type":"event_msg","payload":payload})
}
fn codex_start(turn: &str) -> Value {
    codex_event(json!({"type":"task_started","turn_id":turn}))
}
fn codex_complete(turn: &str, last: Option<&str>) -> Value {
    codex_event(
        json!({"type":"task_complete","turn_id":turn,"last_agent_message":last,"error":null}),
    )
}
fn codex_user(turn: &str, text: &str) -> [Value; 2] {
    [
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}}),
        codex_event(
            json!({"type":"item_completed","turn_id":turn,"item":{"type":"UserMessage","id":"um","content":[{"type":"text","text":text}]}}),
        ),
    ]
}
fn codex_final(text: &str) -> Value {
    json!({"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":text}]}})
}
fn codex_turn(turn: &str, request: &str, answer: &str) -> Vec<Value> {
    let [user, item] = codex_user(turn, request);
    vec![
        codex_start(turn),
        user,
        item,
        codex_final(answer),
        codex_complete(turn, Some(answer)),
    ]
}

#[test]
fn codex_basic_separates_instructions_from_injected_context_and_tools() {
    let parsed = parse("codex", CODEX, &fixture("codex-basic.jsonl")).unwrap();
    let output = long_tool_output();
    assert_eq!(
        view(&parsed),
        vec![
            (Instruction, "Fix the parser."),
            (Commentary, "Reading the parser."),
            (ToolCall, "exec\ncat parser.rs"),
            (ToolResult, &output[..TOOL_LIMIT]),
            (ToolCall, "apply_patch\n{\"path\":\"parser.rs\"}"),
            (ToolResult, "patched"),
            (Final, "Parser fixed."),
            (Instruction, "What is in this screenshot?"),
            (Final, "A terminal window."),
        ]
    );
    let clipped = &parsed.messages[3];
    assert!(!clipped.text_complete);
    assert_eq!(clipped.text_digest, digest(output.as_bytes()));
    assert_eq!(clipped.tool_call_id.as_deref(), Some("call-1"));
    assert!(parsed.messages[0].text_complete);
    assert_eq!(parsed.messages[0].text_digest, digest(b"Fix the parser."));
    assert_eq!(parsed.messages[0].role, MessageRole::User);
    assert_eq!(parsed.messages[2].role, MessageRole::Tool);
    assert_eq!(
        parsed.messages.iter().map(|m| m.order).collect::<Vec<_>>(),
        (1..=9).collect::<Vec<_>>()
    );
    let truncation = parsed
        .omissions
        .iter()
        .find(|omission| omission.reason == "tool_text_truncated")
        .unwrap();
    assert_eq!(truncation.source_ref.as_ref(), Some(&clipped.source_ref));
    assert_eq!(
        truncation.omitted_text_digest.as_deref(),
        Some(digest(output.as_bytes()).as_str())
    );
    for reason in [
        "developer_context_excluded",
        "injected_context_excluded",
        "reasoning_excluded",
        "image_excluded",
        "inter_agent_excluded",
    ] {
        assert!(reasons(&parsed).contains(&reason), "{reason}");
    }
    assert_eq!(
        parsed.turns,
        vec![
            ConversationTurn {
                turn_index: 1,
                provider_turn_id: Some("t1".into()),
                user_source_refs: vec![source_ref("codex", CODEX, "l9")],
                final_source_ref: Some(source_ref("codex", CODEX, "l20")),
                completed: true,
            },
            ConversationTurn {
                turn_index: 2,
                provider_turn_id: Some("t2".into()),
                user_source_refs: vec![source_ref("codex", CODEX, "l25")],
                final_source_ref: Some(source_ref("codex", CODEX, "l29")),
                completed: true,
            },
        ]
    );
    assert!(!parsed.compacted && !parsed.partial_record);
}

const TOOL_LIMIT: usize = codex::TOOL_TEXT_MAX_BYTES;

#[test]
fn codex_turn_outcomes_cover_steer_abort_mismatch_failure_and_pending_final() {
    let parsed = parse("codex", CODEX, &fixture("codex-turns.jsonl")).unwrap();
    assert_eq!(
        view(&parsed),
        vec![
            (Instruction, "Implement A."),
            (Commentary, "Working on A."),
            (Instruction, "Also implement B."),
            (Final, "Done with A and B."),
            (Instruction, "Start C."),
            (Commentary, "Starting C."),
            (Instruction, "Summarize D."),
            (Commentary, "D summary."),
            (Instruction, "Try E."),
            (Instruction, "Finish F."),
            (Final, "F is finished."),
        ]
    );
    let outcome: Vec<_> = parsed
        .turns
        .iter()
        .map(|turn| {
            (
                turn.provider_turn_id.as_deref().unwrap(),
                turn.user_source_refs.len(),
                turn.final_source_ref.is_some(),
                turn.completed,
            )
        })
        .collect();
    assert_eq!(
        outcome,
        vec![
            ("t1", 2, true, true),
            ("t2", 1, false, false),
            ("t3", 1, false, false),
            ("t4", 1, false, false),
            // The final answer was written before the completion record: pending, not done.
            ("t5", 1, true, false),
        ]
    );
    let mismatch = parsed
        .omissions
        .iter()
        .find(|omission| omission.reason == "final_message_unverified")
        .unwrap();
    assert_eq!(
        mismatch.source_ref.as_ref(),
        Some(&parsed.messages[7].source_ref)
    );
}

#[test]
fn codex_compaction_uses_replacement_history_and_traces_retained_messages() {
    let parsed = parse("codex", CODEX, &fixture("codex-compaction.jsonl")).unwrap();
    assert_eq!(
        view(&parsed),
        vec![
            (Instruction, "First request."),
            (Instruction, "Second request."),
            (Instruction, "Third request."),
            (
                CompactionSummary,
                "Earlier context without an original record."
            ),
            (Final, "Third done."),
            (Instruction, "Fourth request."),
            (Final, "Fourth done."),
        ]
    );
    // Retained instructions keep the source and turn of their original record.
    let retained: Vec<_> = parsed.messages[..3]
        .iter()
        .map(|message| (message.source_ref.clone(), message.turn_index))
        .collect();
    assert_eq!(
        retained,
        vec![
            (source_ref("codex", CODEX, "l7"), 1),
            (source_ref("codex", CODEX, "l12"), 2),
            (source_ref("codex", CODEX, "l17"), 3),
        ]
    );
    let summary = &parsed.messages[3];
    assert_eq!(summary.role, MessageRole::Summary);
    assert_eq!(summary.source_ref, source_ref("codex", CODEX, "l21#rh6"));
    assert_eq!(summary.turn_index, 0);
    assert!(parsed.compacted);
    let unreadable = parsed
        .omissions
        .iter()
        .find(|omission| omission.reason == "compaction_summary_unreadable")
        .unwrap();
    assert_eq!(
        unreadable.source_ref,
        Some(source_ref("codex", CODEX, "l21#rh7"))
    );
    assert!(reasons(&parsed).contains(&"pre_compaction_excluded"));
    // The clipped tool output belonged to inactive history.
    assert!(!reasons(&parsed).contains(&"tool_text_truncated"));
    let outcome: Vec<_> = parsed
        .turns
        .iter()
        .map(|turn| {
            (
                turn.turn_index,
                turn.final_source_ref.is_some(),
                turn.completed,
            )
        })
        .collect();
    assert_eq!(
        outcome,
        vec![
            (1, false, true),
            (2, false, true),
            (3, true, true),
            (4, true, true)
        ]
    );
}

#[test]
fn codex_repeated_text_maps_to_the_latest_retained_occurrence() {
    let mut records = vec![codex_meta()];
    records.extend(codex_turn("t1", "continue", "one"));
    records.extend(codex_turn("t2", "continue", "two"));
    let [user, item] = codex_user("t3", "Third.");
    records.extend([codex_start("t3"), user, item]);
    records.push(
        json!({"type":"compacted","payload":{"message":"","replacement_history":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"Third."}]}
        ]}}),
    );
    let parsed = parse("codex", CODEX, &jsonl(&records)).unwrap();
    assert_eq!(
        view(&parsed),
        vec![(Instruction, "continue"), (Instruction, "Third.")]
    );
    assert_eq!(parsed.messages[0].turn_index, 2);
}

#[test]
fn codex_rollback_removes_the_last_user_turn() {
    let parsed = parse("codex", CODEX, &fixture("codex-rollback.jsonl")).unwrap();
    assert_eq!(
        view(&parsed),
        vec![
            (Instruction, "One."),
            (Final, "One done."),
            (Instruction, "Two."),
            (Final, "Two done."),
            (Instruction, "Three, revised."),
            (Final, "Revised three done."),
        ]
    );
    let turns: Vec<_> = parsed
        .turns
        .iter()
        .map(|turn| turn.provider_turn_id.as_deref().unwrap())
        .collect();
    assert_eq!(turns, vec!["t1", "t2", "t4"]);
    assert!(reasons(&parsed).contains(&"rolled_back_excluded"));
}

#[test]
fn codex_rollback_of_a_steer_keeps_the_earlier_instruction_without_a_final() {
    let [user_a, item_a] = codex_user("t1", "A.");
    let [user_b, item_b] = codex_user("t1", "B.");
    let records = vec![
        codex_meta(),
        codex_start("t1"),
        user_a,
        item_a,
        user_b,
        item_b,
        codex_final("Done."),
        codex_complete("t1", Some("Done.")),
        codex_event(json!({"type":"thread_rolled_back","num_turns":1})),
    ];
    let parsed = parse("codex", CODEX, &jsonl(&records)).unwrap();
    assert_eq!(view(&parsed), vec![(Instruction, "A.")]);
    assert!(!parsed.turns[0].completed);
    assert_eq!(parsed.turns[0].final_source_ref, None);
}

#[test]
fn codex_rollback_rejects_requests_it_cannot_reconstruct() {
    let mut base = vec![codex_meta()];
    base.extend(codex_turn("t1", "Only.", "Done."));
    for (count, expected) in [
        (json!("1"), "invalid_rollback"),
        (json!(4294967296u64), "invalid_rollback"),
    ] {
        let mut records = base.clone();
        records.push(codex_event(
            json!({"type":"thread_rolled_back","num_turns":count}),
        ));
        assert_eq!(error("codex", CODEX, &records), expected);
    }
    let mut records = base.clone();
    records.push(codex_start("t2"));
    records.push(codex_event(
        json!({"type":"thread_rolled_back","num_turns":1}),
    ));
    assert_eq!(error("codex", CODEX, &records), "rollback_during_turn");
}

#[test]
fn codex_rejects_unverified_sessions_and_turn_attribution() {
    let [user, item] = codex_user("t1", "Hello.");
    let meta = |payload: Value| json!({"type":"session_meta","payload":payload});
    let cases: Vec<(Vec<Value>, &str)> = vec![
        (vec![codex_start("t1")], "missing_session_header"),
        (
            vec![meta(json!({"id":"other","thread_source":"user"}))],
            "session_mismatch",
        ),
        (
            vec![meta(
                json!({"id":CODEX,"thread_source":"subagent","parent_thread_id":"root"}),
            )],
            "non_root_session",
        ),
        (
            vec![meta(
                json!({"id":CODEX,"thread_source":"user","forked_from_id":"parent"}),
            )],
            "forked_history_unsupported",
        ),
        (vec![codex_meta(), codex_meta()], "unexpected_session_meta"),
        (
            vec![
                codex_meta(),
                codex_start("t1"),
                codex_complete("t1", None),
                codex_start("t1"),
            ],
            "duplicate_turn",
        ),
        (
            vec![codex_meta(), codex_start("t1"), codex_start("t2")],
            "overlapping_turn",
        ),
        (
            vec![codex_meta(), codex_complete("t1", None)],
            "turn_end_without_start",
        ),
        (
            vec![codex_meta(), codex_start("t1"), codex_complete("t2", None)],
            "turn_end_mismatch",
        ),
        (
            vec![codex_meta(), codex_start("t1"), item.clone()],
            "user_message_unpaired",
        ),
        (
            vec![codex_meta(), codex_start("t2"), user.clone(), item.clone()],
            "turn_attribution_unknown",
        ),
        (
            vec![codex_meta(), codex_final("Outside.")],
            "turn_attribution_unknown",
        ),
        (
            vec![
                codex_meta(),
                json!({"type":"response_item","payload":{"type":"function_call","call_id":"c","name":"shell","arguments":"{}"}}),
            ],
            "turn_attribution_unknown",
        ),
    ];
    for (records, expected) in cases {
        assert_eq!(error("codex", CODEX, &records), expected, "{records:?}");
    }
}

#[test]
fn codex_rejects_unknown_shapes() {
    let turn = |record: Value| vec![codex_meta(), codex_start("t1"), record];
    let cases = vec![
        (
            turn(json!({"type":"future_record","payload":{}})),
            "unknown_codex_record",
        ),
        (
            turn(codex_event(
                json!({"type":"user_message","message":"legacy"}),
            )),
            "unknown_codex_event",
        ),
        (
            turn(
                json!({"type":"response_item","payload":{"type":"local_shell_call","call_id":"c"}}),
            ),
            "unknown_codex_response_item",
        ),
        (
            turn(
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","phase":"draft","content":[]}}),
            ),
            "unknown_assistant_phase",
        ),
        (
            turn(
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_audio"}]}}),
            ),
            "unknown_codex_content",
        ),
        (
            turn(json!({"type":"compacted","payload":{"message":"legacy summary"}})),
            "compaction_without_replacement_history",
        ),
        (
            turn(
                json!({"type":"compacted","payload":{"message":"","replacement_history":[
                    {"type":"message","role":"assistant","content":[{"type":"output_text","text":"x"}]}
                ]}}),
            ),
            "unsupported_compaction_item",
        ),
    ];
    for (records, expected) in cases {
        assert_eq!(error("codex", CODEX, &records), expected, "{records:?}");
    }
}

#[test]
fn codex_realtime_transcripts_are_reported_as_unsupported_history() {
    let mut records = vec![codex_meta()];
    records.push(json!({"type":"realtime_item","payload":{"type":"transcript_segment","role":"user","text":"voice"}}));
    records.extend(codex_turn("t1", "Typed.", "Done."));
    let parsed = parse("codex", CODEX, &jsonl(&records)).unwrap();
    assert!(reasons(&parsed).contains(&"realtime_transcript_unsupported"));
}

#[test]
fn records_ignore_only_an_unterminated_tail_and_reject_invalid_lines() {
    let mut bytes = fixture("codex-basic.jsonl");
    bytes.extend_from_slice(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_st");
    let parsed = parse("codex", CODEX, &bytes).unwrap();
    assert!(parsed.partial_record);
    assert!(reasons(&parsed).contains(&"incomplete_jsonl_tail"));
    assert_eq!(parsed.messages.len(), 9);

    let mut bytes = jsonl(&[codex_meta()]);
    bytes.extend_from_slice(b"{\"type\":\n");
    assert_eq!(
        parse("codex", CODEX, &bytes).unwrap_err().0,
        "invalid_jsonl_record"
    );
    assert_eq!(
        parse("codex", CODEX, b"").unwrap_err().0,
        "history_unavailable"
    );
    assert_eq!(
        parse("other", CODEX, &jsonl(&[codex_meta()]))
            .unwrap_err()
            .0,
        "unsupported_provider"
    );
}

// ---------- Claude ----------

fn claude_record(kind: &str, uuid: &str, parent: Option<&str>, extra: Value) -> Value {
    let mut record =
        json!({"type":kind,"uuid":uuid,"parentUuid":parent,"isSidechain":false,"sessionId":CLAUDE});
    for (key, value) in extra.as_object().unwrap() {
        record[key] = value.clone();
    }
    record
}
fn claude_prompt(uuid: &str, parent: Option<&str>, text: &str) -> Value {
    claude_record(
        "user",
        uuid,
        parent,
        json!({"promptId":format!("p-{uuid}"),"origin":{"kind":"human"},"message":{"role":"user","content":text}}),
    )
}
fn claude_reply(uuid: &str, parent: &str, text: &str) -> Value {
    claude_record(
        "assistant",
        uuid,
        Some(parent),
        json!({"message":{"id":format!("m-{uuid}"),"role":"assistant","content":[{"type":"text","text":text}],"stop_reason":"end_turn"}}),
    )
}

#[test]
fn claude_basic_classifies_records_and_keeps_parallel_tool_results() {
    let parsed = parse("claude", CLAUDE, &fixture("claude-basic.jsonl")).unwrap();
    let output = long_tool_output();
    assert_eq!(
        view(&parsed),
        vec![
            (Instruction, "Review the diff."),
            (Commentary, "Checking two files."),
            (ToolCall, "Read\n{\"file_path\":\"/work/a.rs\"}"),
            (ToolResult, &output[..TOOL_LIMIT]),
            (ToolCall, "Read\n{\"file_path\":\"/work/b.rs\"}"),
            (ToolResult, "b.rs is fine"),
            (Final, "Both files look fine."),
            (Instruction, "/review src/lib.rs"),
            (Final, "Review complete."),
            (Instruction, "Run the tests."),
            (ToolCall, "Bash\n{\"command\":\"cargo test\"}"),
            (Instruction, "Also check clippy."),
            (ToolResult, "tests passed"),
            (Commentary, "Tests pass; checking clippy."),
            (Instruction, "What does this show?"),
        ]
    );
    assert_eq!(
        parsed.messages[3].source_ref,
        source_ref("claude", CLAUDE, "r-a")
    );
    assert!(!parsed.messages[3].text_complete);
    assert_eq!(parsed.messages[3].tool_call_id.as_deref(), Some("tu-a"));
    let outcome: Vec<_> = parsed
        .turns
        .iter()
        .map(|turn| {
            (
                turn.provider_turn_id.as_deref().unwrap(),
                turn.user_source_refs.len(),
                turn.final_source_ref.is_some(),
                turn.completed,
            )
        })
        .collect();
    assert_eq!(
        outcome,
        vec![
            ("p1", 1, true, true),
            ("p2", 1, true, true),
            // A queued prompt joins the running turn; the interrupt leaves it incomplete.
            ("p3", 2, false, false),
            // An API error reply is not a completion.
            ("p4", 1, false, false),
        ]
    );
    assert_eq!(
        parsed.turns[0].final_source_ref,
        Some(source_ref("claude", CLAUDE, "a5"))
    );
    for reason in [
        "reasoning_excluded",
        "attachment_excluded",
        "tool_text_truncated",
        "tool_result_image_excluded",
        "system_excluded",
        "meta_excluded",
        "notification_excluded",
        "interrupt_marker_excluded",
        "image_excluded",
        "api_error_excluded",
    ] {
        assert!(reasons(&parsed).contains(&reason), "{reason}");
    }
    assert!(!reasons(&parsed).contains(&"abandoned_branch_excluded"));
}

#[test]
fn claude_unwritten_stop_reason_exposes_a_final_candidate_without_completion() {
    let parsed = parse("claude", CLAUDE, &fixture("claude-pending-final.jsonl")).unwrap();
    assert_eq!(
        view(&parsed),
        vec![
            (Instruction, "First question."),
            (Final, "First answer."),
            (Instruction, "Second question."),
            (Final, "Second answer, stop reason null."),
            (Instruction, "Third question."),
            (Final, "Third answer, stop reason absent."),
            (Instruction, "Fourth question."),
            // Text followed by a tool call is progress, not a final candidate.
            (Commentary, "Looking first."),
            (ToolCall, "Read\n{\"file_path\":\"/work/d.rs\"}"),
        ]
    );
    let outcome: Vec<_> = parsed
        .turns
        .iter()
        .map(|turn| {
            (
                turn.provider_turn_id.as_deref().unwrap(),
                turn.final_source_ref.clone(),
                turn.completed,
            )
        })
        .collect();
    let reference = |id: &str| Some(source_ref("claude", CLAUDE, id));
    assert_eq!(
        outcome,
        vec![
            ("p1", reference("a1"), true),
            ("p2", reference("a2"), false),
            ("p3", reference("a3"), false),
            ("p4", None, false),
        ]
    );
}

#[test]
fn claude_rewind_keeps_only_the_active_branch() {
    let parsed = parse("claude", CLAUDE, &fixture("claude-branch.jsonl")).unwrap();
    assert_eq!(
        view(&parsed),
        vec![
            (Instruction, "Start."),
            (Final, "Started."),
            (Instruction, "New request."),
            (Final, "New answer."),
        ]
    );
    assert!(reasons(&parsed).contains(&"abandoned_branch_excluded"));
}

#[test]
fn claude_compaction_starts_at_the_latest_boundary() {
    let parsed = parse("claude", CLAUDE, &fixture("claude-compaction.jsonl")).unwrap();
    assert_eq!(
        view(&parsed),
        vec![
            (CompactionSummary, "Synthetic summary of earlier work."),
            (Instruction, "Continue with tests."),
            (Final, "Tests added."),
        ]
    );
    assert_eq!(parsed.messages[0].role, MessageRole::Summary);
    assert_eq!(parsed.messages[0].order, 1);
    assert!(parsed.compacted);
    assert!(reasons(&parsed).contains(&"pre_compaction_excluded"));
}

#[test]
fn claude_preserved_segment_is_effective_for_either_link_shape() {
    for name in [
        "claude-preserved-anchor.jsonl",
        "claude-preserved-tail.jsonl",
    ] {
        let parsed = parse("claude", CLAUDE, &fixture(name)).unwrap();
        assert_eq!(
            view(&parsed),
            vec![
                (CompactionSummary, "Synthetic summary of earlier work."),
                (Instruction, "Recent request."),
                (Final, "Recent answer."),
                (Instruction, "Continue with tests."),
                (Final, "Tests added."),
            ],
            "{name}"
        );
        assert!(parsed.compacted, "{name}");
    }
}

#[test]
fn claude_identical_duplicates_are_merged_and_conflicts_rejected() {
    let prompt = claude_prompt("u1", None, "Hello.");
    let reply = claude_reply("a1", "u1", "Hi.");
    let parsed = parse(
        "claude",
        CLAUDE,
        &jsonl(&[prompt.clone(), reply.clone(), prompt.clone(), reply.clone()]),
    )
    .unwrap();
    assert_eq!(view(&parsed), vec![(Instruction, "Hello."), (Final, "Hi.")]);
    assert!(reasons(&parsed).contains(&"duplicate_record_excluded"));
    assert_eq!(
        error(
            "claude",
            CLAUDE,
            &[prompt, reply, claude_reply("a1", "u1", "Different.")]
        ),
        "conflicting_duplicate_record"
    );
}

#[test]
fn claude_prompt_queued_after_the_latest_reply_is_not_an_abandoned_branch() {
    let records = vec![
        claude_prompt("u1", None, "Run the tests."),
        claude_record(
            "assistant",
            "a1",
            Some("u1"),
            json!({"message":{"id":"m1","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}],"stop_reason":"tool_use"}}),
        ),
        claude_record(
            "attachment",
            "q1",
            Some("a1"),
            json!({"attachment":{"type":"queued_command","commandMode":"prompt","prompt":"Also run clippy."}}),
        ),
    ];
    let parsed = parse("claude", CLAUDE, &jsonl(&records)).unwrap();
    assert_eq!(
        view(&parsed),
        vec![
            (Instruction, "Run the tests."),
            (ToolCall, "Bash\n{}"),
            (Instruction, "Also run clippy."),
        ]
    );
    assert_eq!(parsed.turns.len(), 1);
    assert!(!reasons(&parsed).contains(&"abandoned_branch_excluded"));
}

#[test]
fn claude_sidechain_and_metadata_records_are_not_conversation() {
    let mut sidechain = claude_prompt("s1", Some("u1"), "Subagent task.");
    sidechain["isSidechain"] = json!(true);
    let records = vec![
        json!({"type":"future-metadata","sessionId":CLAUDE,"value":1}),
        claude_prompt("u1", None, "Hello."),
        sidechain,
        claude_reply("a1", "u1", "Hi."),
    ];
    let parsed = parse("claude", CLAUDE, &jsonl(&records)).unwrap();
    assert_eq!(view(&parsed), vec![(Instruction, "Hello."), (Final, "Hi.")]);
    assert!(reasons(&parsed).contains(&"subagent_excluded"));
}

#[test]
fn claude_rejects_unverified_shapes() {
    let prompt = claude_prompt("u1", None, "Hello.");
    let mut foreign = claude_prompt("u1", None, "Hello.");
    foreign["sessionId"] = json!("other");
    let mut unknown_origin = claude_prompt("u2", Some("u1"), "Hi.");
    unknown_origin["origin"] = json!({"kind":"future"});
    let mixed = claude_record(
        "user",
        "u2",
        Some("u1"),
        json!({"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"x"},{"type":"text","text":"y"}]}}),
    );
    let unknown_block = claude_record(
        "assistant",
        "a1",
        Some("u1"),
        json!({"message":{"id":"m","role":"assistant","content":[{"type":"future_block"}],"stop_reason":"end_turn"}}),
    );
    let queued = claude_record(
        "attachment",
        "q1",
        Some("u1"),
        json!({"attachment":{"type":"queued_command","commandMode":"future","prompt":"x"}}),
    );
    let command = claude_record(
        "user",
        "u2",
        Some("u1"),
        json!({"message":{"role":"user","content":"<command-message>x</command-message>"}}),
    );
    let cases: Vec<(Vec<Value>, &str)> = vec![
        (vec![foreign], "foreign_session_record"),
        (
            vec![claude_prompt("u1", Some("missing"), "Hello.")],
            "broken_chain",
        ),
        (
            vec![
                claude_prompt("u1", Some("a1"), "Hello."),
                claude_reply("a1", "u1", "Hi."),
            ],
            "cyclic_chain",
        ),
        (
            vec![
                prompt.clone(),
                json!({"type":"future","uuid":"f1","parentUuid":"u1","sessionId":CLAUDE}),
            ],
            "unknown_claude_record",
        ),
        (
            vec![prompt.clone(), unknown_origin],
            "unknown_prompt_origin",
        ),
        (
            vec![prompt.clone(), unknown_block],
            "unknown_claude_content",
        ),
        (vec![prompt.clone(), mixed], "mixed_tool_result_content"),
        (vec![prompt.clone(), queued], "unknown_queued_command_mode"),
        (vec![prompt.clone(), command], "invalid_command_record"),
        (
            vec![
                prompt.clone(),
                json!({"type":"continued-in","continuedInSessionId":"next","sessionId":CLAUDE}),
            ],
            "session_continued_elsewhere",
        ),
        (
            vec![
                prompt.clone(),
                claude_prompt("u2", Some("u1"), "Old."),
                claude_prompt("u3", Some("u1"), "New."),
                json!({"type":"last-prompt","leafUuid":"u2","sessionId":CLAUDE}),
            ],
            "ambiguous_leaf",
        ),
    ];
    for (records, expected) in cases {
        assert_eq!(error("claude", CLAUDE, &records), expected, "{records:?}");
    }
}

#[test]
fn codex_rollback_matches_upstream_zero_and_excess_counts_and_rejects_inter_agent_boundaries() {
    let mut base = vec![codex_meta()];
    base.extend(codex_turn("t1", "Only.", "Done."));
    let original = parse("codex", CODEX, &jsonl(&base)).unwrap();
    let mut zero = base.clone();
    zero.push(codex_event(
        json!({"type":"thread_rolled_back","num_turns":0}),
    ));
    assert_eq!(parse("codex", CODEX, &jsonl(&zero)).unwrap(), original);
    let mut excess = base;
    excess.push(codex_event(
        json!({"type":"thread_rolled_back","num_turns":99}),
    ));
    let parsed = parse("codex", CODEX, &jsonl(&excess)).unwrap();
    assert!(parsed.messages.is_empty() && parsed.turns.is_empty());
    for item in [
        json!({"type":"agent_message","content":"internal task"}),
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":r#"{"author":"/root","recipient":"/root/worker","content":"internal task","trigger_turn":true}"#}]}),
    ] {
        let mut records = vec![codex_meta(), codex_start("t1")];
        records.extend(codex_user("t1", "Only."));
        records.push(json!({"type":"response_item","payload":item}));
        records.push(codex_final("Done."));
        records.push(codex_complete("t1", Some("Done.")));
        records.push(codex_event(
            json!({"type":"thread_rolled_back","num_turns":1}),
        ));
        assert_eq!(
            error("codex", CODEX, &records),
            "rollback_history_unsupported"
        );
    }
}

#[test]
fn codex_rollback_rejects_unpaired_and_untraced_instruction_boundaries() {
    let mut records = vec![codex_meta(), codex_start("t1")];
    records.push(json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Unattributed input."}]}}));
    records.extend(codex_user("t1", "Only."));
    records.push(codex_final("Done."));
    records.push(codex_complete("t1", Some("Done.")));
    records.push(codex_event(
        json!({"type":"thread_rolled_back","num_turns":1}),
    ));
    assert_eq!(
        error("codex", CODEX, &records),
        "rollback_history_unsupported"
    );
    let mut records = vec![codex_meta()];
    records.extend(codex_turn("t1", "Only.", "Done."));
    records.push(json!({"type":"compacted","payload":{"replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Untraced replacement."}]}]}}));
    records.push(codex_event(
        json!({"type":"thread_rolled_back","num_turns":1}),
    ));
    assert_eq!(
        error("codex", CODEX, &records),
        "rollback_history_unsupported"
    );
}

#[test]
fn codex_rejects_unknown_completed_item_types() {
    let records = vec![
        codex_meta(),
        codex_start("t1"),
        codex_event(
            json!({"type":"item_completed","turn_id":"t1","item":{"type":"FutureUserMessage"}}),
        ),
    ];
    assert_eq!(error("codex", CODEX, &records), "unknown_codex_item");
}
