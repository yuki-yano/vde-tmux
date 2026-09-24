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
