use super::*;
#[test]
fn live_frame_requires_cursor_styling_and_identity() {
    let error = policy::ERROR;
    let frame = format!(
        "__vde_capacity__123:120:4:2:2:0\n■ {error}\n\n› \x1b[2mAsk Codex to do anything\x1b[0m\n  ? for shortcuts\n__vde_capacity__123:120:4:2:2:0\n"
    );
    assert!(parse_frame(&frame, 123).is_ok());
    assert!(parse_frame(&frame, 124).is_err());
    for (old, new) in [
        ("4:2:2:0", "4:2:2:1"),
        ("4:2:2:0", "4:3:2:0"),
        ("\x1b[2m", "\x1b[0m"),
        ("›", "!"),
        ("120:4", "120:5"),
    ] {
        assert!(parse_frame(&frame.replace(old, new), 123).is_err());
    }
}

#[test]
fn live_frame_matches_plain_capture_with_background_padding_and_osc8_links() {
    let plain = format!(
        "docs\n■ {}\n\n› Ask Codex to do anything\n  ? for shortcuts",
        policy::ERROR
    );
    let frame = format!(
        "__vde_capacity__123:120:5:2:3:0\n\x1b]8;;https://example.test\x1b\\docs\x1b]8;;\x1b\\\n■ {}\n\x1b[48;5;236m          \x1b[49m\n› \x1b[2mAsk Codex to do anything\x1b[0m\n  \x1b]8;;https://example.test\x07? for shortcuts\x1b]8;;\x07\n__vde_capacity__123:120:5:2:3:0\n",
        policy::ERROR
    );
    assert_eq!(
        parse_frame(&frame, 123).unwrap().digest,
        policy::frame(&plain).unwrap().1
    );
    assert!(
        parse_frame(
            &frame.replace("\x1b]8;;https://example.test", "\x1b]52;;unknown"),
            123
        )
        .is_err()
    );
    assert!(!policy::styled_placeholder_at(
        "\x1b]8;;unterminated\n› \x1b[2mAsk Codex to do anything",
        1
    ));
    // Footer attributes do not affect the composer's inherited dim proof.
    assert!(policy::styled_placeholder_at(
        "› \x1b[2mAsk Codex to do anything\n\x1b[999mfooter",
        0
    ));
}

use super::super::test_root;
use super::super::{
    codex_provider_test_event, install_test_state, read_peek_test_topology_pane, test_incarnation,
};
use crate::question_notice::ingress::{InputClass, ResolverInput, SessionSource};
use crate::question_notice::profile::CodexProfile;
use crate::tmux::mock::MockTmuxRunner;
use std::cell::RefCell;
use std::io::Write;

#[derive(Default)]
struct DispatchRunner {
    mock: MockTmuxRunner,
    sent: RefCell<Vec<Vec<u8>>>,
    unknown: std::cell::Cell<bool>,
}
impl TmuxRunner for DispatchRunner {
    fn run(&self, args: &[&str]) -> anyhow::Result<String> {
        self.mock.run(args)
    }
    fn run_with_input(
        &self,
        args: &[&str],
        body: &[u8],
    ) -> Result<String, crate::tmux::InputCommandError> {
        self.sent.borrow_mut().push(body.to_vec());
        if self.unknown.get() {
            return Ok(String::new());
        }
        let joined = args.join(" ");
        let re = regex::Regex::new("__vde_agent_prompt_submitted__:[0-9a-f]{64}").unwrap();
        Ok(format!(
            "{}\n",
            re.find(&joined).expect("guarded command marker").as_str()
        ))
    }
    fn resolve_agent_process(
        &self,
        pid: u32,
        kind: &crate::pane_state::AgentKind,
    ) -> anyhow::Result<Option<crate::pane_state::AgentProcessIdentity>> {
        self.mock.resolve_agent_process(pid, kind)
    }
    fn verify_agent_input_owner(&self, root: u32, pid: u32) -> anyhow::Result<()> {
        self.mock.verify_agent_input_owner(root, pid)
    }
}
#[derive(Clone)]
struct Target {
    pane: PaneInstance,
    process: crate::pane_state::AgentProcessIdentity,
    session: String,
    path: std::path::PathBuf,
}
struct Fixture {
    root: std::path::PathBuf,
    coordinator: ProductionV2Coordinator,
    runner: DispatchRunner,
    start: Instant,
    epoch: i64,
    event_clock: Arc<std::sync::atomic::AtomicI64>,
}
impl Fixture {
    fn new() -> Self {
        let start = Instant::now();
        let root = test_root("capacity-lifecycle");
        let home = root.join("codex");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        let mut coordinator = ProductionV2Coordinator::new(
            test_incarnation(
                &root,
                format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes())),
            ),
            BTreeMap::from([
                ("XDG_STATE_HOME".into(), root.display().to_string()),
                ("CODEX_HOME".into(), home.display().to_string()),
                (
                    "VDE_TMUX_SOCKET_NAME".into(),
                    format!("missing-capacity-{}", std::process::id()),
                ),
            ]),
            None,
        )
        .unwrap();
        let event_clock = Arc::new(std::sync::atomic::AtomicI64::new(1000));
        let clock = event_clock.clone();
        coordinator.event_clock = Arc::new(move || clock.load(Ordering::SeqCst));
        let clock = event_clock.clone();
        coordinator.dispatch_clock = Arc::new(move || {
            start + Duration::from_secs(clock.load(Ordering::SeqCst).saturating_sub(1000) as u64)
        });
        install_test_state(&coordinator, &root, Default::default());
        *coordinator.agent_runtime.lock().unwrap() = Some(
            crate::agent_state::runtime::AgentRuntime::open(
                root.join("agent-state"),
                coordinator.incarnation.hash.clone(),
            )
            .unwrap(),
        );
        coordinator
            .router
            .lock()
            .unwrap()
            .set_phase(crate::daemon::protocol::v2::DaemonPhase::Serving);
        coordinator.capacity.lock().unwrap().config.enabled = true;
        Self {
            root,
            coordinator,
            runner: DispatchRunner::default(),
            start,
            epoch: 1000,
            event_clock,
        }
    }
    fn target(&self, n: u32) -> Target {
        // Distinct dispatch locks across parallel unit tests.
        let n = n
            + (u32::from_le_bytes(
                Sha256::digest(self.root.as_os_str().as_encoded_bytes())[..4]
                    .try_into()
                    .unwrap(),
            ) % 1_000_000);
        let pane = PaneInstance {
            pane_id: format!("%{n}"),
            pane_pid: n + 1_100_000,
        };
        let process = crate::pane_state::AgentProcessIdentity {
            pid: n + 2_200_000,
            start_token: format!("test-{n}"),
        };
        self.runner
            .mock
            .stub_agent_process(pane.pane_pid, "codex", Some(process.clone()));
        self.runner
            .mock
            .stub_agent_input_owner(pane.pane_pid, process.pid, true);
        self.runner.mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                &pane.pane_id,
                "#{pane_current_command}",
            ],
            "codex\n",
        );
        self.runner.mock.stub(
            &[
                "list-clients",
                "-F",
                "#{client_control_mode}:#{client_activity}:#{pane_id}",
            ],
            "1:99999:%0\n",
        );
        self.coordinator
            .state
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .topology
            .panes
            .push(read_peek_test_topology_pane(pane.clone(), false));
        let session = format!("capacity-session-{n}");
        let path = self
            .root
            .join("codex/sessions")
            .join(format!("rollout-{session}.jsonl"));
        std::fs::write(&path,format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session}\",\"thread_source\":\"user\"}}}}\n")).unwrap();
        let target = Target {
            pane,
            process,
            session,
            path,
        };
        self.event(
            &target,
            "SessionStart",
            "startup",
            "",
            self.epoch,
            CodexProfile::Unknown,
        );
        self.stub_frame(&target);
        target
    }
    fn stub_frame(&self, t: &Target) {
        self.stub_raw_frame(t, &self.raw_frame(t));
    }
    fn raw_frame(&self, t: &Target) -> String {
        let header = format!("__vde_capacity__{}:120:4:2:2:0", t.pane.pane_pid);
        format!(
            "{header}\n■ {}\n\n› \x1b[2mAsk Codex to do anything\x1b[0m\n  ? for shortcuts\n{header}\n",
            policy::ERROR
        )
    }
    fn stub_raw_frame(&self, t: &Target, output: &str) {
        let format = "__vde_capacity__#{pane_pid}:#{pane_width}:#{pane_height}:#{cursor_x}:#{cursor_y}:#{pane_in_mode}";
        self.runner.mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                &t.pane.pane_id,
                format,
                ";",
                "capture-pane",
                "-p",
                "-e",
                "-t",
                &t.pane.pane_id,
                ";",
                "display-message",
                "-p",
                "-t",
                &t.pane.pane_id,
                format,
            ],
            output,
        );
    }
    fn event(
        &self,
        t: &Target,
        kind: &str,
        turn: &str,
        prompt: &str,
        epoch: i64,
        profile: CodexProfile,
    ) {
        self.event_clock.store(epoch, Ordering::SeqCst);
        let daemon = self
            .coordinator
            .router
            .lock()
            .unwrap()
            .daemon_instance_id()
            .clone();
        let (envelope,mut observation)=codex_provider_test_event(daemon.clone(),t.pane.clone(),kind,&serde_json::json!({"session_id":t.session,"turn_id":turn,"source":"startup","prompt":prompt,"last_assistant_message":"done"}).to_string(),epoch);
        let locator = TranscriptLocator::capture(&self.root.join("codex"), &t.path).unwrap();
        observation.question_resolver = Some(ResolverInput {
            reply: (prompt == policy::DEFAULT_PROMPT).then(|| {
                crate::question_notice::reply::ReplyEvidence {
                    items: vec![crate::question_notice::reply::item_digest(
                        "question-tool",
                        0,
                    )],
                }
            }),
            daemon_generation: Some(daemon),
            startup_header_verified: false,
            startup_journal: None,
            parent_origin_verified: true,
            ancestors: vec![],
            profile,
            process: None,
            input_class: if profile == CodexProfile::Unknown {
                InputClass::NonAuthoritativeInput
            } else {
                InputClass::OrdinaryPrompt
            },
            source: SessionSource::Startup,
            home_digest: Some(locator.home_digest()),
            journal_root_digest: None,
            locator: Some(locator),
            journal_failure: None,
            journal_failure_reported: false,
        });
        let response =
            super::super::mutations::provider::apply_external_provider_notice_with_runner(
                &self.coordinator,
                1,
                envelope,
                observation,
                None,
                &self.runner,
            );
        assert!(
            matches!(
                response,
                crate::daemon::protocol::v2::ServerMessage::PaneEventResult { .. }
            ),
            "{response:?}"
        );
    }
    fn submit(&self, t: &Target, turn: &str, prompt: &str, seconds: u64, profile: CodexProfile) {
        self.line(t, serde_json::json!({"type":"task_started","turn_id":turn}));
        self.event(
            t,
            "UserPromptSubmit",
            turn,
            prompt,
            self.epoch + seconds as i64,
            profile,
        );
    }
    fn line(&self, t: &Target, payload: serde_json::Value) {
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&t.path)
                .unwrap(),
            "{}",
            serde_json::json!({"type":"event_msg","payload":payload})
        )
        .unwrap();
    }
    fn fail(&self, t: &Target, turn: &str, seconds: u64) {
        self.line(t,serde_json::json!({"type":"task_complete","turn_id":turn,"error":{"codex_error_info":"server_overloaded"}}));
        self.detect(t, seconds, true, false);
    }
    fn detect(&self, t: &Target, seconds: u64, hint: bool, working: bool) {
        let frame = policy::frame(&format!(
            "■ {}\n\n› Ask Codex to do anything\n  ? for shortcuts",
            policy::ERROR
        ))
        .unwrap()
        .1;
        detect_with(
            &self.coordinator,
            &self.runner,
            1,
            &[policy::CapacitySample {
                pane: t.pane.clone(),
                failure_hint: hint,
                working,
                frame: Some(frame),
            }],
            self.start + Duration::from_secs(seconds),
            self.epoch + seconds as i64,
            0,
            &|_| true,
        );
    }
    fn tick(&self, seconds: u64) {
        self.event_clock
            .store(self.epoch + seconds as i64, Ordering::SeqCst);
        tick_with(
            &self.coordinator,
            &self.runner,
            self.start + Duration::from_secs(seconds),
            self.epoch + seconds as i64,
        );
    }
    fn chain(&self, t: &Target) -> Chain {
        self.coordinator
            .capacity
            .lock()
            .unwrap()
            .chains
            .get(&t.pane)
            .unwrap()
            .clone()
    }
    fn run(&self, t: &Target) -> RunRecord {
        let r = record(&self.coordinator, &t.pane).unwrap();
        let b = binding(&self.coordinator, &r).unwrap();
        self.coordinator
            .agent_runtime
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .current_run_for_binding(&b)
            .unwrap()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn unknown_question_profile_starts_a_chain_and_three_retries_never_reset_budget() {
    let f = Fixture::new();
    let t = f.target(1);
    f.submit(&t, "first", "human objective", 0, CodexProfile::Unknown);
    f.fail(&t, "first", 1);
    assert_eq!(f.run(&t).semantic_outcome, SemanticOutcome::Unresolved);
    assert_eq!(f.run(&t).execution_phase, ExecutionPhase::Error);
    for (index, (due, turn)) in [(61, "auto1"), (3721, "auto2"), (4022, "auto3")]
        .into_iter()
        .enumerate()
    {
        assert_eq!(f.chain(&t).due, Some(f.start + Duration::from_secs(due)));
        let calls = f.runner.mock.calls().len();
        f.tick(due - 1);
        assert_eq!(
            f.runner.mock.calls().len(),
            calls,
            "waiting performs no terminal/file IO"
        );
        f.tick(due);
        assert_eq!(f.runner.sent.borrow().len(), index + 1);
        f.submit(
            &t,
            turn,
            policy::DEFAULT_PROMPT,
            due + 1,
            CodexProfile::Unknown,
        );
        assert_eq!(f.chain(&t).attempts, index + 1);
        let completed_before = record(&f.coordinator, &t.pane).unwrap().completed_seq;
        f.fail(&t, turn, if index == 0 { 3601 } else { due + 1 });
        assert_eq!(
            record(&f.coordinator, &t.pane).unwrap().completed_seq,
            completed_before,
            "failure never advances completion"
        );
    }
    assert_eq!(f.chain(&t).reason.as_deref(), Some("attempt_limit"));
    f.tick(60_000);
    assert_eq!(f.runner.sent.borrow().len(), 3);
    assert_eq!(f.coordinator.capacity.lock().unwrap().failures, 4);
    assert_eq!(record(&f.coordinator, &t.pane).unwrap().completed_seq, 3);
}

#[test]
fn manual_session_disable_identity_draft_and_other_terminal_error_stop_without_send() {
    for cause in [
        "manual", "session", "disabled", "identity", "draft", "other",
    ] {
        let f = Fixture::new();
        let t = f.target(2);
        f.submit(&t, "human", "objective", 0, CodexProfile::Unknown);
        f.fail(&t, "human", 1);
        match cause {
            "manual" => {
                f.submit(&t, "manual", "new request", 2, CodexProfile::Unknown);
                assert!(
                    !f.coordinator
                        .capacity
                        .lock()
                        .unwrap()
                        .chains
                        .contains_key(&t.pane)
                );
            }
            "session" => f.event(&t, "SessionStart", "new", "", 1002, CodexProfile::Unknown),
            "disabled" => f.coordinator.capacity.lock().unwrap().config.enabled = false,
            "identity" => {
                *f.coordinator.state.lock().unwrap() = None;
            }
            "draft" => detect_with(
                &f.coordinator,
                &f.runner,
                1,
                &[policy::CapacitySample {
                    pane: t.pane.clone(),
                    failure_hint: true,
                    working: false,
                    frame: None,
                }],
                f.start + Duration::from_secs(2),
                1002,
                0,
                &|_| true,
            ),
            "other" => {
                f.coordinator
                    .capacity
                    .lock()
                    .unwrap()
                    .chains
                    .get_mut(&t.pane)
                    .unwrap()
                    .due = None;
                f.line(&t,serde_json::json!({"type":"task_complete","turn_id":"human","error":{"codex_error_info":"unauthorized"}}));
                f.detect(&t, 2, false, false);
            }
            _ => unreachable!(),
        }
        f.tick(1000);
        assert!(f.runner.sent.borrow().is_empty(), "{cause}");
        if cause != "manual" {
            assert!(f.chain(&t).reason.is_some(), "{cause}");
        }
    }
}

#[test]
fn multi_pane_dispatch_has_two_second_spacing_and_unknown_stops_without_resend() {
    let f = Fixture::new();
    let a = f.target(3);
    let b = f.target(4);
    for t in [&a, &b] {
        f.submit(t, "first", "human", 0, CodexProfile::Unknown);
        f.fail(t, "first", 1);
    }
    f.tick(61);
    assert_eq!(f.runner.sent.borrow().len(), 1);
    f.tick(62);
    assert_eq!(f.runner.sent.borrow().len(), 1);
    f.runner.unknown.set(true);
    f.tick(63);
    assert_eq!(f.runner.sent.borrow().len(), 2);
    let unknown = if f.chain(&a).reason.is_some() { &a } else { &b };
    assert_eq!(f.chain(unknown).reason.as_deref(), Some("delivery_unknown"));
    f.tick(1000);
    assert_eq!(f.runner.sent.borrow().len(), 2);
}

#[test]
fn success_clears_wait_and_restart_run_failure_cannot_start_a_chain() {
    let f = Fixture::new();
    let t = f.target(5);
    f.submit(&t, "first", "human", 0, CodexProfile::Unknown);
    f.fail(&t, "first", 1);
    f.tick(61);
    f.submit(
        &t,
        "auto",
        policy::DEFAULT_PROMPT,
        62,
        CodexProfile::Unknown,
    );
    f.event(&t, "Stop", "auto", "", 1063, CodexProfile::Unknown);
    f.tick(63);
    assert_eq!(f.chain(&t).reason.as_deref(), Some("completed"));
    assert_eq!(f.runner.sent.borrow().len(), 1);
    f.submit(
        &t,
        "restart",
        "human after restart",
        64,
        CodexProfile::Unknown,
    );
    *f.coordinator.capacity.lock().unwrap() = CapacityState {
        config: CapacityConfig {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };
    f.fail(&t, "restart", 65);
    assert_eq!(f.run(&t).execution_phase, ExecutionPhase::Error);
    assert!(f.coordinator.capacity.lock().unwrap().chains.is_empty());
}

impl Fixture {
    fn prepare_auto(&self, t: &Target) -> OperationId {
        let record = record(&self.coordinator, &t.pane).unwrap();
        let binding = binding(&self.coordinator, &record).unwrap();
        let id = OperationId::generate().unwrap();
        let target = format!(
            "vta1:{}:{}:{}:{}:{}:{}:{:x}",
            self.coordinator.incarnation.hash,
            t.pane.pane_id.trim_start_matches('%'),
            t.pane.pane_pid,
            record.state_id.as_str(),
            record.agent_epoch,
            t.process.pid,
            Sha256::digest(t.process.start_token.as_bytes())
        );
        let mut guard = self.coordinator.agent_runtime.lock().unwrap();
        let runtime = guard.as_mut().unwrap();
        runtime
            .prepare_operation(
                id.clone(),
                target,
                policy::DEFAULT_PROMPT.as_bytes(),
                crate::agent_state::Sha256Digest::parse(
                    crate::pane_state::PromptState::digest_decoded_prompt(policy::DEFAULT_PROMPT),
                )
                .unwrap(),
                policy::ORIGIN.into(),
                binding,
                record.version(),
                record.current_run,
                record.run_seq + 1,
                self.epoch + 2,
            )
            .unwrap();
        runtime.mark_dispatch_started(&id, self.epoch + 2).unwrap();
        id
    }
}

#[test]
fn public_origin_and_id_forgery_are_rejected_without_staging_or_sending() {
    let f = Fixture::new();
    let t = f.target(6);
    f.submit(&t, "first", "human", 0, CodexProfile::Unknown);
    f.fail(&t, "first", 1);
    let id = f.prepare_auto(&t);
    let operation = f
        .coordinator
        .agent_runtime
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .store()
        .load_operation(&id)
        .unwrap()
        .unwrap();
    for (option, code) in [
        (
            policy::ORIGIN,
            crate::daemon::protocol::v2::ErrorCode::InvalidRequest,
        ),
        (
            "paste_enter",
            crate::daemon::protocol::v2::ErrorCode::OperationConflict,
        ),
    ] {
        let response = super::super::mutations::agent::apply_start_agent_prompt_with_runner(
            &f.coordinator,
            &f.runner,
            crate::pane_state::EventId::generate().unwrap(),
            operation.target_agent_ref.clone(),
            id.clone(),
            base64::engine::general_purpose::STANDARD.encode(policy::DEFAULT_PROMPT),
            operation.prompt_digest.clone(),
            option.into(),
            1002,
        );
        assert!(
            matches!(&response,crate::daemon::protocol::v2::ServerMessage::Error {code:c,..} if *c==code),
            "{response:?}"
        );
    }
    assert!(f.runner.sent.borrow().is_empty());
    assert_eq!(
        f.coordinator
            .agent_runtime
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .store()
            .load_operation(&id)
            .unwrap()
            .unwrap(),
        operation
    );
    f.tick(61);
    assert_eq!(f.chain(&t).reason.as_deref(), Some("binding_ambiguous"));
    assert_eq!(f.chain(&t).attempts, 0);
}

#[test]
fn known_question_profile_late_auto_input_neither_acks_nor_changes_original_context() {
    for (late, abandoned) in [(false, false), (true, false), (true, true)] {
        let f = Fixture::new();
        let t = f.target(if late { 7 } else { 8 });
        f.submit(&t, "human", "original objective", 0, CodexProfile::V01561);
        let before = record(&f.coordinator, &t.pane).unwrap().task_context;
        f.coordinator
            .state
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .question_notices
            .issue(
                t.pane.clone(),
                t.process.clone(),
                (&t.session, "question-turn", "question-tool"),
                1001,
            );
        let id = f.prepare_auto(&t);
        if late {
            f.coordinator
                .agent_runtime
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .settle_dispatch(&id, DispatchState::DeliveryUnknown, "unknown", 1003)
                .unwrap();
        }
        if abandoned {
            let mut runtime = f.coordinator.agent_runtime.lock().unwrap();
            let runtime = runtime.as_mut().unwrap();
            let reference = runtime.operation_ref(id.clone());
            let revision = runtime.get_operation(&reference).unwrap().revision;
            runtime
                .abandon_operation(&reference, revision, "queue inspected", 1004)
                .unwrap();
        }
        f.submit(
            &t,
            "auto",
            policy::DEFAULT_PROMPT,
            if late { 100 } else { 3 },
            CodexProfile::V01561,
        );
        let record = record(&f.coordinator, &t.pane).unwrap();
        assert_eq!(record.task_context, before);
        assert!(record.prompt.is_none());
        let linked = f.run(&t).operation_id.is_some();
        let state = f.coordinator.state.lock().unwrap();
        let state = state.as_ref().unwrap();
        let summary = state.question_notices.summary(&t.pane, Some(&t.process));
        assert!(summary.unacknowledged);
        assert_eq!(summary.acknowledged_order, 0);
        assert!(state.question_notices.resolver.non_authoritative >= 1);
        assert_eq!(state.question_notices.resolver.replies_received, 0);
        assert_eq!(linked, !abandoned);
        let operation = f
            .coordinator
            .agent_runtime
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .store()
            .load_operation(&id)
            .unwrap()
            .unwrap();
        assert_eq!(
            operation.dispatch_state,
            if abandoned {
                DispatchState::DeliveryUnknown
            } else {
                DispatchState::PromptConfirmed
            }
        );
        if abandoned {
            assert!(operation.operator_abandoned());
            continue;
        }
        assert_eq!(
            operation
                .result_receipt
                .as_ref()
                .unwrap()
                .confirmation_basis
                .as_deref(),
            Some(if late {
                "binding_sequence_digest"
            } else {
                "guarded_window_digest"
            })
        );
    }
}

#[test]
fn rollover_after_send_stops_even_when_operation_is_confirmed() {
    let f = Fixture::new();
    let t = f.target(9);
    f.submit(&t, "first", "human", 0, CodexProfile::Unknown);
    f.fail(&t, "first", 1);
    f.tick(61);
    let id = f.chain(&t).pending.unwrap();
    let mut other = t.clone();
    other.session = "another-session".into();
    f.event(
        &other,
        "SessionStart",
        "new",
        "",
        1062,
        CodexProfile::Unknown,
    );
    let actual = record(&f.coordinator, &other.pane).unwrap();
    assert_eq!(actual.agent_epoch, 2, "{actual:?}");
    assert_eq!(
        f.chain(&t).reason.as_deref(),
        Some("session_rollover_after_send")
    );
    f.submit(
        &other,
        "auto",
        policy::DEFAULT_PROMPT,
        63,
        CodexProfile::Unknown,
    );
    assert_eq!(
        f.chain(&t).reason.as_deref(),
        Some("session_rollover_after_send")
    );
    assert_eq!(
        f.coordinator
            .agent_runtime
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .store()
            .load_operation(&id)
            .unwrap()
            .unwrap()
            .dispatch_state,
        DispatchState::PromptConfirmed
    );
    f.tick(1000);
    assert_eq!(f.runner.sent.borrow().len(), 1);
}

#[test]
fn final_recovery_guard_rejects_changed_frames_attention_and_terminal_identity() {
    for cause in [
        "copy_mode",
        "resize",
        "draft",
        "overlay",
        "activity",
        "clients_unknown",
        "question",
        "new_turn",
        "truncate",
        "process",
    ] {
        let f = Fixture::new();
        let t = f.target(10);
        f.submit(&t, "human", "objective", 0, CodexProfile::Unknown);
        f.fail(&t, "human", 1);
        match cause {
            "copy_mode" => f.stub_raw_frame(&t, &f.raw_frame(&t).replace("4:2:2:0", "4:2:2:1")),
            "resize" => f.stub_raw_frame(&t, &f.raw_frame(&t).replace(":120:", ":121:")),
            "draft" => f.stub_raw_frame(&t, &f.raw_frame(&t).replace("\x1b[2m", "\x1b[22m")),
            "overlay" => f.stub_raw_frame(
                &t,
                &f.raw_frame(&t)
                    .replace("  ? for shortcuts", "  Question 1/1 (1 unanswered)"),
            ),
            "activity" | "clients_unknown" => f.runner.mock.stub(
                &[
                    "list-clients",
                    "-F",
                    "#{client_control_mode}:#{client_activity}:#{pane_id}",
                ],
                &if cause == "activity" {
                    format!("0:1001:{}\n", t.pane.pane_id)
                } else {
                    "unknown\n".into()
                },
            ),
            "question" => {
                f.coordinator
                    .state
                    .lock()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .question_notices
                    .issue(
                        t.pane.clone(),
                        t.process.clone(),
                        (&t.session, "question", "tool"),
                        1002,
                    );
            }
            "new_turn" => f.line(
                &t,
                serde_json::json!({"type":"task_started","turn_id":"next"}),
            ),
            "truncate" => {
                std::fs::write(&t.path, "{}\n").unwrap();
            }
            "process" => {
                let mut replacement = t.process.clone();
                replacement.start_token = "replaced".into();
                f.runner
                    .mock
                    .stub_agent_process(t.pane.pane_pid, "codex", Some(replacement));
            }
            _ => unreachable!(),
        }
        f.tick(61);
        assert!(f.runner.sent.borrow().is_empty(), "{cause}");
        assert!(f.chain(&t).reason.is_some(), "{cause}");
        f.tick(1000);
        assert!(
            f.runner.sent.borrow().is_empty(),
            "{cause}: no retry after rejection"
        );
    }
}

#[test]
fn stale_binding_pruning_preserves_active_chain_and_missing_locator_is_cached() {
    let f = Fixture::new();
    let t = f.target(11);
    f.submit(&t, "first", "objective", 0, CodexProfile::Unknown);
    f.fail(&t, "first", 1);
    let run = f.run(&t);
    {
        let mut state = f.coordinator.capacity.lock().unwrap();
        let bound = state.bound[&run.run_id].clone();
        for n in 0..512 {
            state.bound.insert(
                StableRunId::parse(format!("{n:032x}")).unwrap(),
                bound.clone(),
            );
        }
    }
    let other = f.target(12);
    f.submit(
        &other,
        "second",
        "other objective",
        2,
        CodexProfile::Unknown,
    );
    assert_eq!(f.coordinator.capacity.lock().unwrap().bound.len(), 2);
    f.tick(61);
    assert_eq!(f.runner.sent.borrow().len(), 1);

    let missing = f.target(13);
    f.submit(&missing, "missing", "objective", 62, CodexProfile::Unknown);
    let id = f.run(&missing).run_id;
    f.coordinator.capacity.lock().unwrap().bound.remove(&id);
    std::fs::remove_file(&missing.path).unwrap();
    f.detect(&missing, 63, true, false);
    assert!(
        f.coordinator
            .capacity
            .lock()
            .unwrap()
            .missing_locator
            .contains(&id)
    );
    // A later file appearing does not cause an old unbound Run to be rescanned.
    std::fs::write(&missing.path,format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\",\"thread_source\":\"user\"}}}}\n",missing.session)).unwrap();
    f.line(&missing,serde_json::json!({"type":"task_complete","turn_id":"missing","error":{"codex_error_info":"server_overloaded"}}));
    f.detect(&missing, 64, true, false);
    assert_eq!(f.run(&missing).execution_phase, ExecutionPhase::Running);
}
