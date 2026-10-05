use super::*;
use std::os::unix::fs::PermissionsExt;

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "vde-question-{}",
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.join("question-notices-v1.json")
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn pane() -> PaneInstance {
    PaneInstance {
        pane_id: "%7".into(),
        pane_pid: 700,
    }
}
fn process() -> AgentProcessIdentity {
    AgentProcessIdentity {
        pid: 701,
        start_token: "start-1".into(),
    }
}
fn issue(store: &mut QuestionNotices, tool: &str) -> NoticeResult {
    store.issue(pane(), process(), ("session", "turn", tool), 42)
}
fn summary(store: &QuestionNotices) -> QuestionNoticeSummary {
    store.summary(&pane(), Some(&process()))
}

fn tracked_issue(store: &mut QuestionNotices, call: &str, count: usize) -> String {
    issue(store, call);
    let summary = summary(store);
    let owner = summary.owner_ref.unwrap();
    store.remember_reply_items(&owner, summary.latest_order, "session", call, count);
    owner
}

fn answer(
    store: &mut QuestionNotices,
    owner: &str,
    items: &[(&str, usize)],
) -> Result<bool, &'static str> {
    store.acknowledge_reply(
        &pane(),
        owner,
        &turn_order::identifier_digest("session"),
        &reply::ReplyEvidence {
            items: items
                .iter()
                .map(|(call, index)| reply::item_digest(call, *index))
                .collect(),
        },
        Instant::now() + Duration::from_secs(2),
    )
}

#[test]
fn partial_and_out_of_order_replies_only_advance_a_fully_answered_prefix() {
    let mut store = QuestionNotices::default();
    let owner = tracked_issue(&mut store, "first", 2);
    tracked_issue(&mut store, "second", 1);
    tracked_issue(&mut store, "third", 1);
    assert_eq!(answer(&mut store, &owner, &[("second", 0)]), Ok(false));
    assert_eq!(answer(&mut store, &owner, &[("second", 0)]), Ok(false));
    assert_eq!(answer(&mut store, &owner, &[("first", 0)]), Ok(false));
    assert_eq!(summary(&store).acknowledged_order, 0);
    assert_eq!(answer(&mut store, &owner, &[("first", 1)]), Ok(true));
    assert_eq!(summary(&store).acknowledged_order, 2);
    assert!(summary(&store).unacknowledged);
    assert_eq!(
        answer(&mut store, &owner, &[("second", 0)]),
        Err("unknown_reply_item")
    );
    assert_eq!(summary(&store).acknowledged_order, 2);
    assert_eq!(answer(&mut store, &owner, &[("third", 0)]), Ok(true));
    assert!(!summary(&store).unacknowledged);
    assert!(store.reply_items.is_empty());
    assert_eq!(answer(&mut store, &owner, &[("third", 0)]), Ok(false));
}

#[test]
fn wrong_owner_session_unknown_or_mixed_reply_cannot_consume_known_pending_items() {
    let mut store = QuestionNotices::default();
    let owner = tracked_issue(&mut store, "first", 1);
    assert_eq!(
        answer(&mut store, "other-owner", &[("first", 0)]),
        Err("stale_notice_owner")
    );
    assert_eq!(
        store.acknowledge_reply(
            &pane(),
            &owner,
            &turn_order::identifier_digest("other"),
            &reply::ReplyEvidence {
                items: vec![reply::item_digest("first", 0)]
            },
            Instant::now() + Duration::from_secs(2)
        ),
        Err("unknown_reply_item")
    );
    assert_eq!(
        answer(&mut store, &owner, &[("first", 0), ("unknown", 0)]),
        Err("unknown_reply_item")
    );
    assert_eq!(summary(&store).acknowledged_order, 0);
    assert_eq!(answer(&mut store, &owner, &[("first", 0)]), Ok(true));
}

#[test]
fn reply_ack_rollback_preserves_notice_and_restart_does_not_rebuild_reply_evidence() {
    let temp = Temp::new();
    let mut store = QuestionNotices::open(temp.path(), "server".into());
    let owner = tracked_issue(&mut store, "private-call-canary", 1);
    let id = reply::item_digest("private-call-canary", 0);
    let body = std::fs::read_to_string(temp.path()).unwrap();
    assert!(!body.contains("private-call-canary") && !body.contains(&id));
    store.storage_fault = Some(storage::FaultPoint::BeforeRename);
    assert_eq!(
        answer(&mut store, &owner, &[("private-call-canary", 0)]),
        Err("persistence_pending")
    );
    assert!(summary(&store).unacknowledged);
    assert!(store.reply_items[&(owner.clone(), 1)].pending.contains(&id));
    drop(store);
    let mut reopened = QuestionNotices::open(temp.path(), "server".into());
    assert!(summary(&reopened).unacknowledged);
    assert_eq!(
        answer(&mut reopened, &owner, &[("private-call-canary", 0)]),
        Err("unknown_reply_item")
    );
    reopened.acknowledge(&pane(), &owner, 1).unwrap();
    assert!(!summary(&reopened).unacknowledged);
}

#[test]
fn absent_issuance_evidence_blocks_later_answered_prefix_and_dead_owner_cleanup_removes_it() {
    let mut store = QuestionNotices::default();
    issue(&mut store, "untracked");
    let owner = tracked_issue(&mut store, "second", 1);
    assert_eq!(answer(&mut store, &owner, &[("second", 0)]), Ok(false));
    assert_eq!(summary(&store).acknowledged_order, 0);
    assert!(summary(&store).unacknowledged);
    store.reconcile(|_, _| false);
    assert!(store.reply_items.is_empty());
    assert!(store.reply_calls.is_empty());
}

#[test]
fn reused_call_ids_do_not_allow_delayed_replies_to_clear_a_new_notice() {
    let mut store = QuestionNotices::default();
    let owner = tracked_issue(&mut store, "reused", 1);
    answer(&mut store, &owner, &[("reused", 0)]).unwrap();
    store.issue(pane(), process(), ("session", "new-turn", "reused"), 43);
    store.remember_reply_items(&owner, 2, "session", "reused", 1);
    assert_eq!(
        answer(&mut store, &owner, &[("reused", 0)]),
        Err("unknown_reply_item")
    );
    assert_eq!(summary(&store).acknowledged_order, 1);
    assert!(summary(&store).unacknowledged);
}

#[test]
fn untracked_or_over_capacity_calls_remain_tombstoned_after_q() {
    for count in [0, reply::MAX_ISSUED_ITEMS + 1] {
        let mut store = QuestionNotices::default();
        let owner = tracked_issue(&mut store, "reused", count);
        store.acknowledge(&pane(), &owner, 1).unwrap();
        store.issue(pane(), process(), ("session", "later", "reused"), 43);
        store.remember_reply_items(&owner, 2, "session", "reused", 1);
        assert_eq!(
            answer(&mut store, &owner, &[("reused", 0)]),
            Err("unknown_reply_item")
        );
        assert!(summary(&store).unacknowledged);
    }
    let mut store = QuestionNotices::default();
    for index in 0..MAX_TEXT_NOTICES_PER_OWNER {
        tracked_issue(&mut store, &format!("filled-{index}"), 1);
    }
    let owner = tracked_issue(&mut store, "reused", 1);
    assert_eq!(store.reply_items.len(), MAX_TEXT_NOTICES_PER_OWNER);
    store
        .acknowledge(&pane(), &owner, summary(&store).latest_order)
        .unwrap();
    store.issue(pane(), process(), ("session", "later", "reused"), 43);
    let latest = summary(&store).latest_order;
    store.remember_reply_items(&owner, latest, "session", "reused", 1);
    assert_eq!(
        answer(&mut store, &owner, &[("reused", 0)]),
        Err("unknown_reply_item")
    );
}

#[test]
fn exhausted_call_history_disables_reply_ack_until_owner_cleanup() {
    let mut store = QuestionNotices::default();
    let owner = tracked_issue(&mut store, "first", 1);
    store.reply_calls.insert(
        owner.clone(),
        (0..MAX_KEYS_PER_OWNER)
            .map(|index| format!("{index:064x}"))
            .collect(),
    );
    tracked_issue(&mut store, "untracked", 1);
    assert!(store.reply_blocked_owners.contains(&owner));
    assert!(store.reply_items.is_empty());
    store.acknowledge(&pane(), &owner, 2).unwrap();
    tracked_issue(&mut store, "new", 1);
    assert_eq!(
        answer(&mut store, &owner, &[("new", 0)]),
        Err("unknown_reply_item")
    );
    store.reconcile(|_, _| false);
    assert!(store.reply_blocked_owners.is_empty());
}

#[test]
fn expired_partial_reply_does_not_consume_pending_items() {
    let mut store = QuestionNotices::default();
    let owner = tracked_issue(&mut store, "first", 2);
    let result = store.acknowledge_reply(
        &pane(),
        &owner,
        &turn_order::identifier_digest("session"),
        &reply::ReplyEvidence {
            items: vec![reply::item_digest("first", 0)],
        },
        Instant::now(),
    );
    assert_eq!(result, Err("reply_deadline"));
    assert_eq!(answer(&mut store, &owner, &[("first", 1)]), Ok(false));
    assert!(summary(&store).unacknowledged);
}

#[test]
fn fenced_ack_preserves_new_arrivals_and_deduplicates_after_ack() {
    let mut store = QuestionNotices::default();
    assert_eq!(
        issue(&mut store, "one").disposition,
        NoticeDisposition::Applied
    );
    let displayed = summary(&store);
    issue(&mut store, "two");
    let owner = displayed.owner_ref.unwrap();
    assert!(
        store
            .acknowledge(&pane(), &owner, displayed.latest_order)
            .unwrap()
    );
    assert!(summary(&store).unacknowledged);
    assert_eq!(summary(&store).acknowledged_order, 1);
    assert!(!store.acknowledge(&pane(), &owner, 1).unwrap());
    assert_eq!(
        store.acknowledge(&pane(), &owner, 3),
        Err("future_notice_order")
    );
    store.acknowledge(&pane(), &owner, 2).unwrap();
    assert_eq!(
        issue(&mut store, "one").disposition,
        NoticeDisposition::Duplicate
    );
    assert!(!summary(&store).unacknowledged);
    issue(&mut store, "three");
    assert!(summary(&store).unacknowledged);
}

#[test]
fn restart_retains_notice_and_dedup_but_does_not_transfer_process_ownership() {
    let temp = Temp::new();
    let mut store = QuestionNotices::open(temp.path(), "server".into());
    issue(&mut store, "one");
    let owner = summary(&store).owner_ref.unwrap();
    drop(store);
    let mut store = QuestionNotices::open(temp.path(), "server".into());
    assert!(summary(&store).unacknowledged);
    store.acknowledge(&pane(), &owner, 1).unwrap();
    drop(store);
    let mut store = QuestionNotices::open(temp.path(), "server".into());
    assert_eq!(
        issue(&mut store, "one").disposition,
        NoticeDisposition::Duplicate
    );
    assert!(!summary(&store).unacknowledged);
    let replaced = AgentProcessIdentity {
        start_token: "start-2".into(),
        ..process()
    };
    assert!(!store.summary(&pane(), Some(&replaced)).unacknowledged);
    store.issue(pane(), replaced.clone(), ("session", "turn", "one"), 43);
    let new_owner = store.summary(&pane(), Some(&replaced)).owner_ref.unwrap();
    assert_ne!(new_owner, owner);
    store.reconcile(|_, identity| identity == &replaced);
    assert_eq!(
        store.acknowledge(&pane(), &owner, 1),
        Err("stale_notice_owner")
    );
    assert!(store.summary(&pane(), Some(&replaced)).unacknowledged);
}

#[test]
fn question_fingerprints_are_owner_session_order_bound_and_never_persisted() {
    use super::text::QuestionEvidence;
    let temp = Temp::new();
    let mut store = QuestionNotices::open(temp.path(), "server".into());
    let questions = QuestionEvidence::from_tool_input(Some(
        &serde_json::json!({"questions":[{"title":"private-title-canary", "options":["private-option-canary"]}]}),
    ));
    issue(&mut store, "one");
    let owner = summary(&store).owner_ref.unwrap();
    let session = turn_order::identifier_digest("session");
    store.remember_questions(owner.clone(), 1, "session", questions.clone());
    assert_eq!(store.capture_questions(&owner, &session, 0, 1), questions);
    assert_eq!(
        store.capture_questions(&owner, "wrong-session", 0, 1),
        QuestionEvidence::Unavailable
    );
    assert_eq!(
        store.capture_questions("wrong-owner", &session, 0, 1),
        QuestionEvidence::Unavailable
    );
    issue(&mut store, "two");
    assert_eq!(
        store.capture_questions(&owner, &session, 0, 2),
        QuestionEvidence::Unavailable
    );
    store.remember_questions(owner.clone(), 2, "session", questions.clone());
    store.acknowledge(&pane(), &owner, 1).unwrap();
    assert!(!store.question_text.contains_key(&(owner.clone(), 1)));
    assert_eq!(store.capture_questions(&owner, &session, 1, 2), questions);
    store.persist().unwrap();
    let disk = std::fs::read_to_string(temp.path()).unwrap();
    assert!(!disk.contains("private-title-canary"));
    assert!(!disk.contains("private-option-canary"));
    let text::QuestionEvidence::Fingerprints(hashes) = questions else {
        unreachable!()
    };
    assert!(!disk.contains(&hashes[0].title));
    let restarted = QuestionNotices::open(temp.path(), "server".into());
    assert!(summary(&restarted).unacknowledged);
    assert_eq!(
        restarted.capture_questions(&owner, &session, 1, 2),
        QuestionEvidence::Unavailable
    );
    store.reconcile(|_, _| false);
    assert!(store.question_text.is_empty());
}

#[test]
fn long_pending_prefix_deduplicates_fingerprints_without_losing_order_coverage() {
    use super::text::QuestionEvidence;
    let mut store = QuestionNotices::default();
    let evidence = QuestionEvidence::from_tool_input(Some(&serde_json::json!({
        "questions":[{"title":"Repeated pending question", "options":["Yes"]}]
    })));
    let session = turn_order::identifier_digest("session");
    for order in 1..=512 {
        issue(&mut store, &format!("pending-{order}"));
        let owner = summary(&store).owner_ref.unwrap();
        store.remember_questions(owner, order, "session", evidence.clone());
    }
    let owner = summary(&store).owner_ref.unwrap();
    assert_eq!(store.capture_questions(&owner, &session, 0, 512), evidence);
    store.question_text.remove(&(owner.clone(), 200));
    assert_eq!(
        store.capture_questions(&owner, &session, 0, 512),
        QuestionEvidence::Unavailable
    );
    store.acknowledge(&pane(), &owner, 256).unwrap();
    assert_eq!(
        store.capture_questions(&owner, &session, 256, 512),
        evidence
    );
    issue(&mut store, "pending-513");
    store.remember_questions(owner.clone(), 513, "session", evidence.clone());
    assert_eq!(
        store.capture_questions(&owner, &session, 256, 513),
        evidence
    );
}

#[test]
fn write_failure_displays_notice_retries_dirty_state_and_never_retries_failed_ack() {
    let temp = Temp::new();
    let mut store = QuestionNotices::open(temp.path(), "server".into());
    issue(&mut store, "one");
    let owner = summary(&store).owner_ref.unwrap();
    std::fs::remove_file(temp.path()).unwrap();
    std::fs::create_dir(temp.path()).unwrap();
    assert_eq!(
        issue(&mut store, "two").durability,
        Some(NoticeDurability::MemoryOnly)
    );
    assert_eq!(
        summary(&store).reason,
        Some(NoticeReason::PersistencePending)
    );
    let attempted = store.last_write_attempt;
    issue(&mut store, "three");
    assert_eq!(store.last_write_attempt, attempted);
    assert_eq!(
        store.acknowledge(&pane(), &owner, 2),
        Err("persistence_pending")
    );
    assert_eq!(summary(&store).acknowledged_order, 0);
    std::fs::remove_dir(temp.path()).unwrap();
    store.last_write_attempt = Some(Instant::now() - RETRY_INTERVAL);
    assert!(store.reconcile(|_, _| true));
    assert!(!summary(&store).degraded());
    let reopened = QuestionNotices::open(temp.path(), "server".into());
    assert!(summary(&reopened).unacknowledged);
    assert_eq!(summary(&reopened).latest_order, 3);
    assert_eq!(summary(&reopened).acknowledged_order, 0);
}

#[test]
fn duplicate_reports_memory_only_until_a_successful_save_promotes_it() {
    let temp = Temp::new();
    let mut store = QuestionNotices::open(temp.path(), "server".into());
    std::fs::create_dir(temp.path()).unwrap();
    let first = issue(&mut store, "one");
    assert_eq!(first.disposition, NoticeDisposition::Applied);
    assert_eq!(first.durability, Some(NoticeDurability::MemoryOnly));
    let retry = issue(&mut store, "one");
    assert_eq!(retry.disposition, NoticeDisposition::Duplicate);
    assert_eq!(retry.durability, Some(NoticeDurability::MemoryOnly));
    std::fs::remove_dir(temp.path()).unwrap();
    let retry = issue(&mut store, "one");
    assert_eq!(retry.disposition, NoticeDisposition::Duplicate);
    assert_eq!(retry.durability, Some(NoticeDurability::Persisted));
    assert_eq!(summary(&store).latest_order, 1);
    assert_eq!(
        issue(
            &mut QuestionNotices::open(temp.path(), "server".into()),
            "one"
        )
        .durability,
        Some(NoticeDurability::Persisted)
    );
}

#[test]
fn ack_commit_faults_distinguish_rollback_from_logical_commit_and_restart() {
    for fault in [
        storage::FaultPoint::BeforeRename,
        storage::FaultPoint::AfterRename,
        storage::FaultPoint::DirectorySync,
    ] {
        let temp = Temp::new();
        let mut store = QuestionNotices::open(temp.path(), "server".into());
        issue(&mut store, "one");
        let owner = summary(&store).owner_ref.unwrap();
        let old = std::fs::read(temp.path()).unwrap();
        store.storage_fault = Some(fault);
        let result = store.acknowledge(&pane(), &owner, 1);
        if fault == storage::FaultPoint::BeforeRename {
            assert_eq!(result, Err("persistence_pending"));
            assert_eq!(summary(&store).acknowledged_order, 0);
            assert_eq!(std::fs::read(temp.path()).unwrap(), old);
        } else {
            assert_eq!(result, Ok(true));
            assert_eq!(summary(&store).acknowledged_order, 1);
            assert_eq!(
                summary(&store).reason,
                Some(NoticeReason::QuestionAckDirectoryFsyncFailed)
            );
            assert!(!store.dirty);
            let committed = std::fs::read(temp.path()).unwrap();
            store.storage_fault = None;
            assert!(!store.reconcile(|_, _| true));
            assert_eq!(std::fs::read(temp.path()).unwrap(), committed);
        }
        let reopened = QuestionNotices::open(temp.path(), "server".into());
        assert_eq!(
            summary(&reopened).acknowledged_order,
            if fault == storage::FaultPoint::BeforeRename {
                0
            } else {
                1
            }
        );
        // A filesystem recovering the older valid sidecar produces a retained notification.
        std::fs::write(temp.path(), old).unwrap();
        let reopened = QuestionNotices::open(temp.path(), "server".into());
        assert!(summary(&reopened).unacknowledged);
        assert_eq!(summary(&reopened).acknowledged_order, 0);
    }
}

#[test]
fn sidecar_expectation_distinguishes_initial_absence_from_loss_without_schema_changes() {
    let temp = Temp::new();
    let mut store = QuestionNotices::open(temp.path(), "server".into());
    assert!(!store.invalid_sidecar);
    issue(&mut store, "one");
    assert_eq!(
        std::fs::read(temp.0.join("question-notices-v1.expected")).unwrap(),
        b"1\n"
    );
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path()).unwrap()).unwrap();
    assert_eq!(snapshot["schema_version"], 1);
    assert!(snapshot["owners"][0]["seen"].is_array());
    assert!(!snapshot.to_string().contains("resolver"));
    std::fs::remove_file(temp.path()).unwrap();
    assert!(QuestionNotices::open(temp.path(), "server".into()).invalid_sidecar);
}

#[test]
fn invalid_or_symlink_sidecar_is_not_silently_replaced() {
    let temp = Temp::new();
    std::fs::write(temp.path(), b"invalid sidecar").unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut store = QuestionNotices::open(temp.path(), "server".into());
    assert_eq!(
        issue(&mut store, "one").reason,
        Some(NoticeReason::InvalidSidecar)
    );
    assert_eq!(std::fs::read(temp.path()).unwrap(), b"invalid sidecar");
    std::fs::remove_file(temp.path()).unwrap();
    let destination = temp.0.join("other");
    std::fs::write(&destination, b"other").unwrap();
    std::os::unix::fs::symlink(&destination, temp.path()).unwrap();
    let mut store = QuestionNotices::open(temp.path(), "server".into());
    assert_eq!(
        issue(&mut store, "one").reason,
        Some(NoticeReason::InvalidSidecar)
    );
    assert_eq!(std::fs::read(destination).unwrap(), b"other");
}

#[test]
fn capacity_keeps_seen_keys_after_ack_and_recovers_only_when_owners_are_removed() {
    let mut store = QuestionNotices::default();
    for index in 0..MAX_KEYS_PER_OWNER {
        issue(&mut store, &index.to_string());
    }
    let owner = summary(&store).owner_ref.unwrap();
    store
        .acknowledge(&pane(), &owner, MAX_KEYS_PER_OWNER as u64)
        .unwrap();
    assert_eq!(
        issue(&mut store, "overflow").reason,
        Some(NoticeReason::CapacityExceeded)
    );
    assert_eq!(
        issue(&mut store, "0").disposition,
        NoticeDisposition::Duplicate
    );
    assert!(!summary(&store).unacknowledged);
    store.reconcile(|_, _| false);
    assert_eq!(
        issue(&mut store, "overflow").disposition,
        NoticeDisposition::Applied
    );
    assert!(!summary(&store).degraded());
    assert!(serde_json::to_vec(&summary(&store)).unwrap().len() < 1024);
}

#[test]
fn total_key_and_owner_limits_reject_without_eviction_and_survive_reload() {
    let temp = Temp::new();
    let mut store = QuestionNotices::default();
    let owner_pane = |index: usize| PaneInstance {
        pane_id: format!("%{}", index + 1),
        pane_pid: index as u32 + 1000,
    };
    for index in 0..MAX_KEYS {
        let result = store.issue(
            owner_pane(index / MAX_KEYS_PER_OWNER),
            process(),
            ("session", "turn", &index.to_string()),
            42,
        );
        assert_eq!(result.disposition, NoticeDisposition::Applied);
    }
    let next = owner_pane(MAX_KEYS / MAX_KEYS_PER_OWNER);
    assert_eq!(
        store
            .issue(next.clone(), process(), ("session", "turn", "next"), 43)
            .reason,
        Some(NoticeReason::CapacityExceeded)
    );
    assert_eq!(
        store
            .owners
            .values()
            .map(|owner| owner.seen.len())
            .sum::<usize>(),
        MAX_KEYS
    );
    store.path = Some(temp.path());
    store.persist().unwrap();
    let mut reopened = QuestionNotices::open(temp.path(), String::new());
    assert!(!reopened.invalid_sidecar);
    assert_eq!(
        reopened
            .issue(owner_pane(0), process(), ("session", "turn", "0"), 43)
            .disposition,
        NoticeDisposition::Duplicate
    );
    reopened.reconcile(|pane, _| pane != &owner_pane(0));
    assert_eq!(
        reopened
            .issue(next, process(), ("session", "turn", "next"), 43)
            .disposition,
        NoticeDisposition::Applied
    );

    let mut store = QuestionNotices::default();
    for index in 0..MAX_OWNERS {
        assert_eq!(
            store
                .issue(owner_pane(index), process(), ("session", "turn", "one"), 42)
                .disposition,
            NoticeDisposition::Applied
        );
    }
    assert_eq!(
        store
            .issue(
                owner_pane(MAX_OWNERS),
                process(),
                ("session", "turn", "one"),
                42
            )
            .reason,
        Some(NoticeReason::CapacityExceeded)
    );
    assert_eq!(store.owners.len(), MAX_OWNERS);
    store.path = Some(temp.path());
    store.persist().unwrap();
    let reopened = QuestionNotices::open(temp.path(), String::new());
    assert_eq!(reopened.owners.len(), MAX_OWNERS);
    for index in 0..MAX_OWNERS {
        assert!(
            reopened
                .summary(&owner_pane(index), Some(&process()))
                .unacknowledged
        );
    }
}

#[test]
fn issued_payload_is_strict_and_ignores_other_tools_and_subagents() {
    let temp = Temp::new();
    let transcript = temp.0.join("root.jsonl");
    std::fs::write(&transcript, r#"{"type":"session_meta","payload":{"id":"session","session_id":"session","thread_source":"user"}}"#).unwrap();
    let payload = serde_json::json!({
        "hook_event_name": "PostToolUse", "session_id": "session", "turn_id": "turn", "tool_use_id": "call-1",
        "tool_name": "request_user_input_async", "tool_response": "{\"accepted\":true}",
        "transcript_path": transcript,
        "tool_input": {"questions": [{"title":"Never persist this question", "options":["private answer"]}]},
    });
    let notice = ingress::from_payload("PostToolUse", &payload.to_string(), None).unwrap();
    assert!(matches!(notice, QuestionNoticeInput::Issued { .. }));
    assert!(!serde_json::to_string(&notice).unwrap().contains("private"));
    assert!(ingress::from_payload("PreToolUse", &payload.to_string(), None).is_none());
    for (key, value) in [
        ("tool_response", serde_json::json!({"accepted":true})),
        (
            "tool_response",
            serde_json::json!("{\"accepted\":\"true\"}"),
        ),
        ("tool_response", serde_json::json!("broken")),
        ("tool_response", serde_json::json!("{\"accepted\":false}")),
        ("hook_event_name", serde_json::json!("PreToolUse")),
        ("tool_use_id", serde_json::json!("")),
        ("turn_id", serde_json::json!("x".repeat(257))),
    ] {
        let mut bad = payload.clone();
        bad[key] = value;
        assert_eq!(
            ingress::from_payload("PostToolUse", &bad.to_string(), None),
            Some(QuestionNoticeInput::Rejected {
                reason: NoticeReason::InvalidPayload
            })
        );
    }
    let mut child = payload.clone();
    child["agent_id"] = serde_json::json!("child");
    assert_eq!(
        ingress::from_payload("PostToolUse", &child.to_string(), None),
        Some(QuestionNoticeInput::Rejected {
            reason: NoticeReason::OriginUnverified
        })
    );
    let mut unverified = payload.clone();
    unverified["transcript_path"] = serde_json::json!(temp.0.join("missing.jsonl"));
    assert_eq!(
        ingress::from_payload("PostToolUse", &unverified.to_string(), Some(&temp.0)),
        Some(QuestionNoticeInput::Rejected {
            reason: NoticeReason::OriginUnverified
        })
    );
    for tool in ["request_user_input", "exec_command", "other"] {
        let mut other = payload.clone();
        other["tool_name"] = serde_json::json!(tool);
        assert!(ingress::from_payload("PostToolUse", &other.to_string(), None).is_none());
    }
}

#[test]
fn captured_ancestry_contains_current_exact_process() {
    let ancestors = ingress::capture_ancestors().unwrap();
    assert!(!ancestors.is_empty() && ancestors.len() <= MAX_ANCESTORS);
    assert_eq!(ancestors[0].pid, std::process::id());
    assert_eq!(
        ancestors[0].start_token,
        crate::daemon::lifecycle::agent_process_start_token(std::process::id()).unwrap()
    );
}

#[test]
fn partial_reply_preserves_an_existing_ordinary_candidate() {
    use ingress::{InputClass, SessionSource, TranscriptLocator};
    use profile::{CodexProfile, ExecutableFingerprint, ProfileRequest};
    use resolver::{Binding, JournalView, NoticeFence, session_key};
    let mut store = QuestionNotices::default();
    let owner = tracked_issue(&mut store, "call", 2);
    let binding = Binding {
        owner: owner.clone(),
        pane: pane(),
        process: process(),
        executable: ProfileRequest {
            process: process(),
            executable: ExecutableFingerprint {
                dev: 1,
                ino: 1,
                size: 1,
                mtime_sec: 0,
                mtime_nsec: 0,
                ctime_sec: 0,
                ctime_nsec: 0,
            },
        },
        profile: CodexProfile::V01593,
        locator: TranscriptLocator {
            home: PathBuf::from("/synthetic"),
            transcript: PathBuf::from("/synthetic/transcript"),
            dev: 1,
            ino: 1,
        },
    };
    let journal = JournalView {
        epoch: 0,
        veto: false,
        session_dirty: false,
    };
    store.resolver.session_start(
        binding.locator.home_digest(),
        session_key("session"),
        SessionSource::Startup,
        Some(binding.clone()),
        journal,
        true,
    );
    store.resolver.issue(
        &binding,
        &binding.locator.home_digest(),
        &session_key("session"),
        &session_key("a"),
        1,
        0,
    );
    store
        .resolver
        .ordinary(
            InputClass::OrdinaryPrompt,
            &binding,
            &session_key("session"),
            "input",
            &session_key("b"),
            NoticeFence {
                acknowledged: 0,
                latest: 1,
            },
            journal,
            Instant::now(),
        )
        .unwrap();
    assert_eq!(answer(&mut store, &owner, &[("call", 0)]), Ok(false));
    assert!(store.resolver.candidate(&owner).is_some());
    assert_eq!(answer(&mut store, &owner, &[("call", 1)]), Ok(true));
    assert!(store.resolver.candidate(&owner).is_none());
}

#[test]
fn reply_acceptance_freezes_id_orders_and_cannot_adopt_future_issuance() {
    let mut store = QuestionNotices::default();
    let owner = tracked_issue(&mut store, "a", 1);
    let session = turn_order::identifier_digest("session");
    let mixed = reply::ReplyEvidence {
        items: vec![reply::item_digest("a", 0), reply::item_digest("b", 0)],
    };
    assert_eq!(
        store.match_reply_orders(&pane(), &owner, &session, &mixed),
        Err("unknown_reply_item")
    );
    let known = reply::ReplyEvidence {
        items: vec![reply::item_digest("a", 0)],
    };
    let orders = store
        .match_reply_orders(&pane(), &owner, &session, &known)
        .unwrap();
    tracked_issue(&mut store, "b", 1);
    assert_eq!(
        store.acknowledge_reply_orders(
            &pane(),
            &owner,
            &session,
            &known,
            &orders,
            Instant::now() + Duration::from_secs(1)
        ),
        Ok(true)
    );
    assert_eq!(summary(&store).acknowledged_order, 1);
    assert!(summary(&store).unacknowledged);
    // An old job cannot rebind a reused call after Q.
    store.acknowledge(&pane(), &owner, 2).unwrap();
    store.issue(pane(), process(), ("session", "later", "a"), 43);
    store.remember_reply_items(&owner, 3, "session", "a", 1);
    assert_eq!(
        store.acknowledge_reply_orders(
            &pane(),
            &owner,
            &session,
            &known,
            &orders,
            Instant::now() + Duration::from_secs(1)
        ),
        Ok(false)
    );
    assert_eq!(summary(&store).acknowledged_order, 2);
    assert!(summary(&store).unacknowledged);
}

#[test]
fn manual_ack_of_an_earlier_pair_does_not_discard_the_remaining_accepted_reply() {
    let mut store = QuestionNotices::default();
    let owner = tracked_issue(&mut store, "a", 1);
    tracked_issue(&mut store, "b", 1);
    let session = turn_order::identifier_digest("session");
    let reply = reply::ReplyEvidence {
        items: vec![reply::item_digest("a", 0), reply::item_digest("b", 0)],
    };
    let orders = store
        .match_reply_orders(&pane(), &owner, &session, &reply)
        .unwrap();
    store.acknowledge(&pane(), &owner, 1).unwrap();
    tracked_issue(&mut store, "new-unanswered", 1);
    assert_eq!(
        store.acknowledge_reply_orders(
            &pane(),
            &owner,
            &session,
            &reply,
            &orders,
            Instant::now() + Duration::from_secs(1)
        ),
        Ok(true)
    );
    assert_eq!(summary(&store).acknowledged_order, 2);
    assert_eq!(summary(&store).latest_order, 3);
    assert!(summary(&store).unacknowledged);
}

#[test]
fn nearly_expired_reply_and_failed_ack_do_not_create_a_global_persistence_backlog() {
    let temp = Temp::new();
    let mut store = QuestionNotices::open(temp.path(), "server".into());
    let owner = tracked_issue(&mut store, "first", 1);
    let reply = reply::ReplyEvidence {
        items: vec![reply::item_digest("first", 0)],
    };
    let session = turn_order::identifier_digest("session");
    let orders = store
        .match_reply_orders(&pane(), &owner, &session, &reply)
        .unwrap();
    let before = std::fs::read(temp.path()).unwrap();
    let last = store.last_write_attempt;
    assert_eq!(
        store.acknowledge_reply_orders(
            &pane(),
            &owner,
            &session,
            &reply,
            &orders,
            Instant::now() + Duration::from_millis(10)
        ),
        Err("reply_deadline")
    );
    assert!(!store.dirty);
    assert_eq!(store.last_write_attempt, last);
    assert_eq!(std::fs::read(temp.path()).unwrap(), before);
    store.storage_fault = Some(storage::FaultPoint::BeforeRename);
    assert_eq!(
        answer(&mut store, &owner, &[("first", 0)]),
        Err("persistence_pending")
    );
    assert!(!store.dirty);
    assert_eq!(store.last_write_attempt, last);
    assert!(summary(&store).unacknowledged && summary(&store).degraded());
    let other = PaneInstance {
        pane_id: "%8".into(),
        pane_pid: 800,
    };
    assert!(!store.summary(&other, None).degraded());
    store.storage_fault = None;
    assert_eq!(
        store
            .issue(other.clone(), process(), ("session", "turn", "new"), 43)
            .durability,
        Some(NoticeDurability::Persisted)
    );
    assert!(!store.dirty && !summary(&store).degraded());
    assert_eq!(summary(&store).acknowledged_order, 0);
}
