use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

use crate::daemon::lifecycle::TmuxServerIncarnation;
use crate::pane_state::PaneInstance;
use crate::tmux::{InputWriteStage, TmuxRunner};

const INPUT_BUFFER_PREFIX: &str = "vde-agent-input-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TerminalMutationOutcome {
    Applied,
    Rejected(String),
    DeliveryUnknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SplitMutationOutcome {
    Applied(PaneInstance),
    Rejected(String),
    DeliveryUnknown(String),
}

struct GuardedCommand {
    args: Vec<String>,
    buffer: Option<String>,
    success: String,
    server_mismatch: String,
    pane_mismatch: String,
}

pub(super) fn submit_text_guarded(
    runner: &dyn TmuxRunner,
    incarnation: &TmuxServerIncarnation,
    pane: &PaneInstance,
    expected_pane_command: &str,
    body: &[u8],
    nonce_seed: &str,
) -> TerminalMutationOutcome {
    if !safe_pane_command(expected_pane_command) {
        return TerminalMutationOutcome::Rejected(
            "pane foreground command is unavailable before guarded input".to_string(),
        );
    }
    let nonce = mutation_nonce(incarnation, pane, nonce_seed, b"text");
    let command = build_guarded_text_command(incarnation, pane, expected_pane_command, &nonce);
    run_guarded_command(runner, command, body)
}

pub(super) fn send_keys_guarded(
    runner: &dyn TmuxRunner,
    incarnation: &TmuxServerIncarnation,
    pane: &PaneInstance,
    expected_pane_command: &str,
    keys: &[String],
    nonce_seed: &str,
) -> TerminalMutationOutcome {
    if !safe_pane_command(expected_pane_command) {
        return TerminalMutationOutcome::Rejected(
            "pane foreground command is unavailable before guarded keys".to_string(),
        );
    }
    let nonce = mutation_nonce(incarnation, pane, nonce_seed, b"keys");
    let command =
        build_guarded_keys_command(incarnation, pane, expected_pane_command, keys, &nonce);
    run_guarded_command(runner, command, &[])
}

pub(super) struct SplitMutation<'a> {
    pub direction_flag: &'a str,
    pub size_percent: Option<u8>,
    pub cwd: &'a str,
    pub focus: bool,
    pub nonce_seed: &'a str,
}

pub(super) fn split_pane_guarded(
    runner: &dyn TmuxRunner,
    incarnation: &TmuxServerIncarnation,
    pane: &PaneInstance,
    request: SplitMutation<'_>,
) -> SplitMutationOutcome {
    let nonce = mutation_nonce(incarnation, pane, request.nonce_seed, b"split");
    let success_prefix = format!("__vde_pane_split__:{nonce}");
    let server_mismatch = format!("__vde_pane_split_server_mismatch__:{nonce}");
    let pane_mismatch = format!("__vde_pane_split_pane_mismatch__:{nonce}");
    let format = format!("{success_prefix}\t#{{pane_id}}\t#{{pane_pid}}");

    let mut split = vec!["split-window".to_string()];
    if !request.focus {
        split.push("-d".to_string());
    }
    split.extend([
        "-P".to_string(),
        "-F".to_string(),
        format,
        "-t".to_string(),
        pane.pane_id.clone(),
        request.direction_flag.to_string(),
        "-c".to_string(),
        request.cwd.to_string(),
    ]);
    if let Some(percent) = request.size_percent {
        split.extend(["-p".to_string(), percent.to_string()]);
    }

    let pane_guard = exact_pane_guard(pane, None);
    let guarded_split = crate::pane_state::store::tmux_command_string(&[
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        pane.pane_id.clone(),
        pane_guard,
        crate::pane_state::store::tmux_command_string(&split),
        crate::pane_state::store::tmux_command_string(&[
            "display-message".to_string(),
            "-p".to_string(),
            pane_mismatch.clone(),
        ]),
    ]);
    let args = [
        "if-shell".to_string(),
        "-F".to_string(),
        server_guard(incarnation),
        guarded_split,
        crate::pane_state::store::tmux_command_string(&[
            "display-message".to_string(),
            "-p".to_string(),
            server_mismatch.clone(),
        ]),
    ];
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = match runner.run_with_input(&refs, &[]) {
        Ok(output) => output,
        Err(error) => {
            return match error.stage {
                InputWriteStage::BeforeSpawn => SplitMutationOutcome::Rejected(format!(
                    "guarded pane split failed before spawn: {error}"
                )),
                InputWriteStage::AfterSpawnBeforeWrite
                | InputWriteStage::AfterPartialWrite
                | InputWriteStage::AfterFullWrite => SplitMutationOutcome::DeliveryUnknown(
                    format!("guarded pane split became ambiguous at {:?}", error.stage),
                ),
            };
        }
    };
    let lines = output.lines().map(str::trim).collect::<Vec<_>>();
    if lines
        .iter()
        .any(|line| *line == server_mismatch || *line == pane_mismatch)
    {
        return SplitMutationOutcome::Rejected(
            "tmux server or target pane changed before guarded split".to_string(),
        );
    }
    let Some(line) = lines
        .iter()
        .find(|line| line.starts_with(&format!("{success_prefix}\t")))
    else {
        return SplitMutationOutcome::DeliveryUnknown(
            "guarded pane split returned without an unambiguous creation marker".to_string(),
        );
    };
    let fields = line.split('\t').collect::<Vec<_>>();
    if fields.len() != 3 {
        return SplitMutationOutcome::DeliveryUnknown(
            "guarded pane split returned an invalid creation marker".to_string(),
        );
    }
    let pane = PaneInstance {
        pane_id: fields[1].to_string(),
        pane_pid: fields[2].parse().unwrap_or_default(),
    };
    if pane.validate().is_err() {
        return SplitMutationOutcome::DeliveryUnknown(
            "guarded pane split returned an invalid pane identity".to_string(),
        );
    }
    SplitMutationOutcome::Applied(pane)
}

fn run_guarded_command(
    runner: &dyn TmuxRunner,
    command: GuardedCommand,
    input: &[u8],
) -> TerminalMutationOutcome {
    let refs = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    let result = runner.run_with_input(&refs, input);
    if let Some(buffer) = &command.buffer {
        let _ = runner.run(&["delete-buffer", "-b", buffer]);
    }
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            return match error.stage {
                InputWriteStage::BeforeSpawn => TerminalMutationOutcome::Rejected(format!(
                    "guarded terminal mutation failed before spawn: {error}"
                )),
                InputWriteStage::AfterSpawnBeforeWrite
                | InputWriteStage::AfterPartialWrite
                | InputWriteStage::AfterFullWrite => {
                    TerminalMutationOutcome::DeliveryUnknown(format!(
                        "guarded terminal mutation became ambiguous at {:?}",
                        error.stage
                    ))
                }
            };
        }
    };
    let markers = output.lines().map(str::trim).collect::<BTreeSet<_>>();
    if markers.contains(command.success.as_str()) {
        return TerminalMutationOutcome::Applied;
    }
    if markers.contains(command.server_mismatch.as_str())
        || markers.contains(command.pane_mismatch.as_str())
    {
        return TerminalMutationOutcome::Rejected(
            "tmux server, pane, or foreground command changed before guarded input".to_string(),
        );
    }
    TerminalMutationOutcome::DeliveryUnknown(
        "guarded terminal mutation returned without an unambiguous side-effect marker".to_string(),
    )
}

fn build_guarded_text_command(
    incarnation: &TmuxServerIncarnation,
    pane: &PaneInstance,
    expected_pane_command: &str,
    nonce: &str,
) -> GuardedCommand {
    let buffer = format!("{INPUT_BUFFER_PREFIX}{}", &nonce[..24]);
    let (success, server_mismatch, pane_mismatch) = markers(nonce, "text");
    let submitted = crate::pane_state::store::tmux_command_string(&[
        "paste-buffer".to_string(),
        "-p".to_string(),
        "-r".to_string(),
        "-d".to_string(),
        "-b".to_string(),
        buffer.clone(),
        "-t".to_string(),
        pane.pane_id.clone(),
        ";".to_string(),
        "send-keys".to_string(),
        "-t".to_string(),
        pane.pane_id.clone(),
        "Enter".to_string(),
        ";".to_string(),
        "display-message".to_string(),
        "-p".to_string(),
        success.clone(),
    ]);
    let guarded = guard_after_copy_mode_cancel(
        pane,
        expected_pane_command,
        submitted,
        &pane_mismatch,
        Some(&buffer),
    );
    let mut server_mismatch_args = delete_buffer_args(&buffer);
    server_mismatch_args.extend([
        ";".to_string(),
        "display-message".to_string(),
        "-p".to_string(),
        server_mismatch.clone(),
    ]);
    GuardedCommand {
        args: vec![
            "load-buffer".to_string(),
            "-b".to_string(),
            buffer.clone(),
            "-".to_string(),
            ";".to_string(),
            "if-shell".to_string(),
            "-F".to_string(),
            server_guard(incarnation),
            guarded,
            crate::pane_state::store::tmux_command_string(&server_mismatch_args),
        ],
        buffer: Some(buffer),
        success,
        server_mismatch,
        pane_mismatch,
    }
}

fn build_guarded_keys_command(
    incarnation: &TmuxServerIncarnation,
    pane: &PaneInstance,
    expected_pane_command: &str,
    keys: &[String],
    nonce: &str,
) -> GuardedCommand {
    let (success, server_mismatch, pane_mismatch) = markers(nonce, "keys");
    let mut submitted = vec![
        "send-keys".to_string(),
        "-t".to_string(),
        pane.pane_id.clone(),
    ];
    submitted.extend(keys.iter().cloned());
    submitted.extend([
        ";".to_string(),
        "display-message".to_string(),
        "-p".to_string(),
        success.clone(),
    ]);
    let guarded = guard_after_copy_mode_cancel(
        pane,
        expected_pane_command,
        crate::pane_state::store::tmux_command_string(&submitted),
        &pane_mismatch,
        None,
    );
    GuardedCommand {
        args: vec![
            "if-shell".to_string(),
            "-F".to_string(),
            server_guard(incarnation),
            guarded,
            crate::pane_state::store::tmux_command_string(&[
                "display-message".to_string(),
                "-p".to_string(),
                server_mismatch.clone(),
            ]),
        ],
        buffer: None,
        success,
        server_mismatch,
        pane_mismatch,
    }
}

fn guard_after_copy_mode_cancel(
    pane: &PaneInstance,
    expected_pane_command: &str,
    applied: String,
    pane_mismatch: &str,
    buffer: Option<&str>,
) -> String {
    let mut mismatch = buffer.map(delete_buffer_args).unwrap_or_default();
    if !mismatch.is_empty() {
        mismatch.push(";".to_string());
    }
    mismatch.extend([
        "display-message".to_string(),
        "-p".to_string(),
        pane_mismatch.to_string(),
    ]);
    let mismatch = crate::pane_state::store::tmux_command_string(&mismatch);
    let exact = exact_pane_guard(pane, Some(expected_pane_command));
    let ready = format!("#{{&&:{exact},#{{==:#{{pane_in_mode}},0}}}}");
    let after_cancel = crate::pane_state::store::tmux_command_string(&[
        "copy-mode".to_string(),
        "-q".to_string(),
        "-t".to_string(),
        pane.pane_id.clone(),
        ";".to_string(),
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        pane.pane_id.clone(),
        ready,
        applied.clone(),
        mismatch.clone(),
    ]);
    let with_cancel = crate::pane_state::store::tmux_command_string(&[
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        pane.pane_id.clone(),
        "#{>:#{pane_in_mode},0}".to_string(),
        after_cancel,
        applied,
    ]);
    crate::pane_state::store::tmux_command_string(&[
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        pane.pane_id.clone(),
        exact,
        with_cancel,
        mismatch,
    ])
}

fn exact_pane_guard(pane: &PaneInstance, expected_pane_command: Option<&str>) -> String {
    let pid = format!("#{{==:#{{pane_pid}},{}}}", pane.pane_pid);
    match expected_pane_command {
        Some(command) => format!("#{{&&:{pid},#{{==:#{{pane_current_command}},{command}}}}}"),
        None => pid,
    }
}

fn server_guard(incarnation: &TmuxServerIncarnation) -> String {
    format!(
        "#{{&&:#{{==:#{{pid}},{}}},#{{==:#{{start_time}},{}}}}}",
        incarnation.identity.pid, incarnation.identity.start_time
    )
}

fn markers(nonce: &str, kind: &str) -> (String, String, String) {
    (
        format!("__vde_agent_{kind}_applied__:{nonce}"),
        format!("__vde_agent_{kind}_server_mismatch__:{nonce}"),
        format!("__vde_agent_{kind}_pane_mismatch__:{nonce}"),
    )
}

fn delete_buffer_args(buffer: &str) -> Vec<String> {
    vec![
        "delete-buffer".to_string(),
        "-b".to_string(),
        buffer.to_string(),
    ]
}

fn safe_pane_command(command: &str) -> bool {
    !command.is_empty()
        && command.len() <= 128
        && command
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
}

fn mutation_nonce(
    incarnation: &TmuxServerIncarnation,
    pane: &PaneInstance,
    seed: &str,
    domain: &[u8],
) -> String {
    let mut hasher = Sha256::new();
    for field in [
        b"vde-tmux:terminal-mutation:v1".as_slice(),
        domain,
        incarnation.hash.as_bytes(),
        pane.pane_id.as_bytes(),
        &pane.pane_pid.to_be_bytes(),
        seed.as_bytes(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::daemon::topology::ServerIdentity;
    use crate::tmux::mock::MockTmuxRunner;

    fn incarnation() -> TmuxServerIncarnation {
        TmuxServerIncarnation {
            socket_path: PathBuf::from("/tmp/tmux-test"),
            identity: ServerIdentity {
                pid: 123,
                start_time: 456,
            },
            hash: "server".to_string(),
        }
    }

    fn pane() -> PaneInstance {
        PaneInstance {
            pane_id: "%4".to_string(),
            pane_pid: 789,
        }
    }

    #[test]
    fn text_guard_cancels_copy_mode_without_exposing_body_in_argv() {
        let runner = MockTmuxRunner::new();
        let nonce = mutation_nonce(&incarnation(), &pane(), "request", b"text");
        let command = build_guarded_text_command(&incarnation(), &pane(), "claude", &nonce);
        runner.stub(
            &command.args.iter().map(String::as_str).collect::<Vec<_>>(),
            &format!("{}\n", command.success),
        );
        runner.stub(
            &["delete-buffer", "-b", command.buffer.as_deref().unwrap()],
            "",
        );

        assert_eq!(
            submit_text_guarded(
                &runner,
                &incarnation(),
                &pane(),
                "claude",
                b"private prompt",
                "request",
            ),
            TerminalMutationOutcome::Applied
        );
        let (args, body) = &runner.input_calls()[0];
        assert!(!args.join(" ").contains("private prompt"));
        assert_eq!(body, b"private prompt");
        let rendered = args.join(" ");
        assert!(rendered.contains("pane_in_mode"));
        assert!(rendered.contains("copy-mode"));
        assert!(rendered.contains("paste-buffer"));
    }

    #[test]
    fn key_guard_keeps_keys_as_tmux_arguments_and_cancels_copy_mode() {
        let keys = vec!["y".to_string(), "Enter".to_string()];
        let nonce = mutation_nonce(&incarnation(), &pane(), "request", b"keys");
        let command = build_guarded_keys_command(&incarnation(), &pane(), "codex", &keys, &nonce);
        let rendered = command.args.join(" ");
        assert!(rendered.contains("send-keys"));
        assert!(rendered.contains("'y'"));
        assert!(rendered.contains("'Enter'"));
        assert!(rendered.contains("copy-mode"));
    }

    #[test]
    fn split_guard_is_detached_by_default_and_returns_exact_pane() {
        let runner = MockTmuxRunner::new();
        let request = SplitMutation {
            direction_flag: "-h",
            size_percent: Some(40),
            cwd: "/tmp/project",
            focus: false,
            nonce_seed: "request",
        };
        let nonce = mutation_nonce(&incarnation(), &pane(), "request", b"split");
        let success = format!("__vde_pane_split__:{nonce}\t%9\t900\n");
        // Build once through a permissive probe so the exact argv can be stubbed.
        let expected_args = {
            let probe = MockTmuxRunner::new();
            let _ = split_pane_guarded(&probe, &incarnation(), &pane(), request);
            probe.calls()[0].clone()
        };
        runner.stub(
            &expected_args.iter().map(String::as_str).collect::<Vec<_>>(),
            &success,
        );
        assert_eq!(
            split_pane_guarded(
                &runner,
                &incarnation(),
                &pane(),
                SplitMutation {
                    direction_flag: "-h",
                    size_percent: Some(40),
                    cwd: "/tmp/project",
                    focus: false,
                    nonce_seed: "request",
                },
            ),
            SplitMutationOutcome::Applied(PaneInstance {
                pane_id: "%9".to_string(),
                pane_pid: 900,
            })
        );
        let rendered = expected_args.join(" ");
        assert!(rendered.contains("'-d'"));
        assert!(rendered.contains("'-h'"));
        assert!(rendered.contains("'/tmp/project'"));
    }
}
