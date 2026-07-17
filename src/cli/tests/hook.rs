use super::*;

fn os_args(values: &[&str]) -> Vec<std::ffi::OsString> {
    values.iter().map(std::ffi::OsString::from).collect()
}

#[test]
fn generic_emit_and_hook_help_do_not_read_stdin() {
    assert!(!agent_hook_requires_stdin(&os_args(&[
        "vt",
        "hook",
        "emit",
        "--agent",
        "generic",
        "--session-id",
        "run-1",
        "--status",
        "running",
    ])));
    assert!(!agent_hook_requires_stdin(&os_args(&[
        "vt", "hook", "--help",
    ])));
    assert!(!agent_hook_requires_stdin(&os_args(&[
        "vt", "hook", "codex", "--help",
    ])));
}

#[test]
fn payload_hooks_read_stdin_but_legacy_codex_notify_does_not() {
    assert!(agent_hook_requires_stdin(&os_args(&[
        "vt", "hook", "claude", "Stop",
    ])));
    assert!(agent_hook_requires_stdin(&os_args(&[
        "vt", "hook", "codex", "Stop",
    ])));
    assert!(!agent_hook_requires_stdin(&os_args(&[
        "vt",
        "hook",
        "codex",
        r#"{"type":"agent-turn-complete"}"#,
    ])));
}

#[test]
fn agent_hook_stdin_deadline_returns_partial_unclosed_input() {
    use std::os::unix::net::UnixStream;

    let (mut writer, mut reader) = UnixStream::pair().unwrap();
    writer.write_all(b"{").unwrap();
    let started = Instant::now();
    // A sender that keeps the pipe open past the deadline: the bytes that did
    // arrive are returned (downstream JSON parsing rejects an incomplete body)
    // and the read still stays time-bounded rather than discarding them.
    let payload =
        read_agent_hook_input_from_until(&mut reader, Instant::now() + Duration::from_millis(30))
            .unwrap();

    assert_eq!(payload, "{");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn agent_hook_closed_stdin_is_an_empty_payload() {
    struct ClosedStdin;

    impl std::io::Read for ClosedStdin {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            panic!("a closed stdin must not be read");
        }
    }

    impl std::os::fd::AsRawFd for ClosedStdin {
        fn as_raw_fd(&self) -> std::os::fd::RawFd {
            -1
        }
    }

    assert_eq!(
        read_agent_hook_input_from_until(
            &mut ClosedStdin,
            Instant::now() + Duration::from_millis(30),
        )
        .unwrap(),
        ""
    );
}

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
