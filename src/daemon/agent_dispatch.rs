use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

use crate::daemon::lifecycle::TmuxServerIncarnation;
use crate::pane_state::PaneInstance;
use crate::tmux::TmuxRunner;

const PROMPT_BUFFER_PREFIX: &str = "vde-agent-prompt-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DispatchOutcome {
    Submitted,
    Rejected(String),
    DeliveryUnknown(String),
}

struct GuardedPromptCommand {
    args: Vec<String>,
    buffer: String,
    success: String,
    server_mismatch: String,
    pane_mismatch: String,
}

pub(crate) fn dispatch_prompt_guarded(
    runner: &dyn TmuxRunner,
    incarnation: &TmuxServerIncarnation,
    pane: &PaneInstance,
    prompt: &[u8],
    operation_id: &str,
) -> DispatchOutcome {
    let nonce = dispatch_nonce(incarnation, pane, operation_id);
    let expected_pane_command = match runner.run(&[
        "display-message",
        "-p",
        "-t",
        &pane.pane_id,
        "#{pane_current_command}",
    ]) {
        Ok(command) if safe_pane_command(command.trim()) => command,
        Ok(_) => {
            return DispatchOutcome::Rejected(
                "pane foreground command is unavailable before guarded dispatch".to_string(),
            );
        }
        Err(error) => {
            return DispatchOutcome::Rejected(format!(
                "pane foreground command query failed before guarded dispatch: {error:#}"
            ));
        }
    };
    let command =
        build_guarded_prompt_command(incarnation, pane, expected_pane_command.trim(), &nonce);
    let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    let result = runner.run_with_input(&args, prompt);
    let _ = runner.run(&["delete-buffer", "-b", &command.buffer]);
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            return match error.stage {
                crate::tmux::InputWriteStage::BeforeSpawn => DispatchOutcome::Rejected(format!(
                    "guarded tmux dispatch failed before spawn: {error}"
                )),
                crate::tmux::InputWriteStage::AfterSpawnBeforeWrite
                | crate::tmux::InputWriteStage::AfterPartialWrite
                | crate::tmux::InputWriteStage::AfterFullWrite => {
                    DispatchOutcome::DeliveryUnknown(format!(
                        "guarded tmux dispatch became ambiguous at {:?}",
                        error.stage
                    ))
                }
            };
        }
    };
    let markers = output.lines().map(str::trim).collect::<BTreeSet<_>>();
    if markers.contains(command.success.as_str()) {
        return DispatchOutcome::Submitted;
    }
    if markers.contains(command.server_mismatch.as_str())
        || markers.contains(command.pane_mismatch.as_str())
    {
        return DispatchOutcome::Rejected(
            "tmux server or pane identity changed before guarded dispatch".to_string(),
        );
    }
    DispatchOutcome::DeliveryUnknown(
        "guarded dispatch returned without an unambiguous submission marker".to_string(),
    )
}

pub(crate) fn cleanup_stale_prompt_buffers(runner: &dyn TmuxRunner) -> anyhow::Result<usize> {
    let output = runner.run(&["list-buffers", "-F", "#{buffer_name}"])?;
    let owned_buffers = output
        .lines()
        .map(str::trim)
        .filter(|name| {
            name.strip_prefix(PROMPT_BUFFER_PREFIX)
                .is_some_and(|suffix| {
                    suffix.len() == 24
                        && suffix
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                })
        })
        .collect::<Vec<_>>();
    for buffer in &owned_buffers {
        runner.run(&["delete-buffer", "-b", buffer])?;
    }
    Ok(owned_buffers.len())
}

fn safe_pane_command(command: &str) -> bool {
    !command.is_empty()
        && command.len() <= 128
        && command
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
}

fn dispatch_nonce(
    incarnation: &TmuxServerIncarnation,
    pane: &PaneInstance,
    operation_id: &str,
) -> String {
    let mut hasher = Sha256::new();
    for field in [
        b"vde-tmux:daemon-dispatch:v1".as_slice(),
        incarnation.hash.as_bytes(),
        pane.pane_id.as_bytes(),
        &pane.pane_pid.to_be_bytes(),
        operation_id.as_bytes(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    format!("{:x}", hasher.finalize())
}

fn build_guarded_prompt_command(
    incarnation: &TmuxServerIncarnation,
    pane: &PaneInstance,
    expected_pane_command: &str,
    nonce: &str,
) -> GuardedPromptCommand {
    const SUCCESS_PREFIX: &str = "__vde_agent_prompt_submitted__";
    const SERVER_MISMATCH_PREFIX: &str = "__vde_agent_prompt_server_mismatch__";
    const PANE_MISMATCH_PREFIX: &str = "__vde_agent_prompt_pane_mismatch__";

    let buffer = format!("{PROMPT_BUFFER_PREFIX}{}", &nonce[..24]);
    let success = format!("{SUCCESS_PREFIX}:{nonce}");
    let server_mismatch = format!("{SERVER_MISMATCH_PREFIX}:{nonce}");
    let pane_mismatch = format!("{PANE_MISMATCH_PREFIX}:{nonce}");
    let delete_buffer = || {
        vec![
            "delete-buffer".to_string(),
            "-b".to_string(),
            buffer.clone(),
        ]
    };
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
    let mut pane_mismatch_args = delete_buffer();
    pane_mismatch_args.extend([
        ";".to_string(),
        "display-message".to_string(),
        "-p".to_string(),
        pane_mismatch.clone(),
    ]);
    let pane_pid_guard = ["#{==:#{pane_pid},", &pane.pane_pid.to_string(), "}"].concat();
    let pane_command_guard = ["#{==:#{pane_current_command},", expected_pane_command, "}"].concat();
    let exact_pane_guard = ["#{&&:", &pane_pid_guard, ",", &pane_command_guard, "}"].concat();
    let pane_guard = crate::pane_state::store::tmux_command_string(&[
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        pane.pane_id.clone(),
        exact_pane_guard,
        submitted,
        crate::pane_state::store::tmux_command_string(&pane_mismatch_args),
    ]);
    let mut server_mismatch_args = delete_buffer();
    server_mismatch_args.extend([
        ";".to_string(),
        "display-message".to_string(),
        "-p".to_string(),
        server_mismatch.clone(),
    ]);
    let server_guard = format!(
        "#{{&&:#{{==:#{{pid}},{}}},#{{==:#{{start_time}},{}}}}}",
        incarnation.identity.pid, incarnation.identity.start_time
    );
    GuardedPromptCommand {
        args: vec![
            "load-buffer".to_string(),
            "-b".to_string(),
            buffer.clone(),
            "-".to_string(),
            ";".to_string(),
            "if-shell".to_string(),
            "-F".to_string(),
            server_guard,
            pane_guard,
            crate::pane_state::store::tmux_command_string(&server_mismatch_args),
        ],
        buffer,
        success,
        server_mismatch,
        pane_mismatch,
    }
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
    fn command_guards_server_and_pane_and_submits_once() {
        let command =
            build_guarded_prompt_command(&incarnation(), &pane(), "codex", &"a".repeat(64));
        assert_eq!(
            command
                .args
                .iter()
                .filter(|arg| *arg == "load-buffer")
                .count(),
            1
        );
        let rendered = command.args.join(" ");
        assert!(rendered.contains("#{==:#{pane_pid},789}"));
        assert!(rendered.contains("#{==:#{pane_current_command},codex}"));
        assert!(rendered.contains("#{==:#{pid},123}"));
        assert!(rendered.contains("#{==:#{start_time},456}"));
        assert!(rendered.contains("send-keys"));
        assert!(!rendered.contains("secret prompt"));
    }

    #[test]
    fn operation_identity_makes_the_nonce_deterministic() {
        assert_eq!(
            dispatch_nonce(&incarnation(), &pane(), "operation-1"),
            dispatch_nonce(&incarnation(), &pane(), "operation-1")
        );
        assert_ne!(
            dispatch_nonce(&incarnation(), &pane(), "operation-1"),
            dispatch_nonce(&incarnation(), &pane(), "operation-2")
        );
    }

    #[test]
    fn startup_cleanup_deletes_only_exact_owned_prompt_buffers() {
        let runner = MockTmuxRunner::new();
        runner.stub(
            &["list-buffers", "-F", "#{buffer_name}"],
            concat!(
                "vde-agent-prompt-0123456789abcdef01234567\n",
                "vde-agent-prompt-ABCDEF0123456789ABCDEF01\n",
                "vde-agent-prompt-too-short\n",
                "user-buffer\n"
            ),
        );
        runner.stub(
            &[
                "delete-buffer",
                "-b",
                "vde-agent-prompt-0123456789abcdef01234567",
            ],
            "",
        );

        assert_eq!(cleanup_stale_prompt_buffers(&runner).unwrap(), 1);
        assert_eq!(
            runner.calls(),
            vec![
                vec![
                    "list-buffers".to_string(),
                    "-F".to_string(),
                    "#{buffer_name}".to_string(),
                ],
                vec![
                    "delete-buffer".to_string(),
                    "-b".to_string(),
                    "vde-agent-prompt-0123456789abcdef01234567".to_string(),
                ],
            ]
        );
    }
}
