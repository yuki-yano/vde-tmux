use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::{Subcommand, ValueEnum};

use super::pane::ApiReadArgs;
use crate::tmux::TmuxRunner;

#[derive(Debug, Subcommand)]
pub(super) enum AgentCommand {
    /// List resolved agents from the daemon's cached canonical snapshot.
    List {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        status: Option<ApiAgentStatusArg>,
        #[arg(long = "cwd-prefix")]
        cwd_prefix: Option<String>,
        #[arg(long)]
        unread: bool,
        #[arg(long = "needs-action")]
        needs_action: bool,
    },
    /// Get one current agent by %pane_id or exact agent_ref.
    Get { target: String },
    /// Submit one guarded prompt to an exact idle/done Codex occupant.
    Prompt {
        target: String,
        /// Caller-supplied idempotency key (16-128 ASCII [A-Za-z0-9_-]).
        #[arg(long = "operation-id")]
        operation_id: String,
        /// Read the prompt from stdin until EOF, removing one terminal LF or CRLF.
        #[arg(
            long,
            required_unless_present = "prompt_file",
            conflicts_with = "prompt_file"
        )]
        stdin: bool,
        /// Read the prompt from a file, removing one terminal LF or CRLF, without using argv.
        #[arg(
            long = "prompt-file",
            required_unless_present = "stdin",
            conflicts_with = "stdin"
        )]
        prompt_file: Option<PathBuf>,
        /// Operation-wide deadline through daemon dispatch and durable hook confirmation.
        #[arg(
            long = "confirm-timeout-ms",
            default_value_t = crate::api::DEFAULT_PROMPT_CONFIRM_TIMEOUT.as_millis() as u64
        )]
        confirm_timeout_ms: u64,
    },
    /// Submit or resume one durable prompt using a vt-managed request-state file.
    Request {
        /// Exact agent_ref returned by agent get/list/start.
        target: String,
        /// Stable intent handle whose contents and lifecycle are owned by vt.
        #[arg(long = "state-file")]
        state_file: PathBuf,
        /// Read the initial prompt from stdin until EOF, removing one terminal LF or CRLF.
        #[arg(long, conflicts_with = "prompt_file")]
        stdin: bool,
        /// Read the initial prompt from a file, removing one terminal LF or CRLF.
        #[arg(long = "prompt-file", conflicts_with = "stdin")]
        prompt_file: Option<PathBuf>,
        /// Operation-wide deadline through daemon dispatch and durable hook confirmation.
        #[arg(
            long = "confirm-timeout-ms",
            default_value_t = crate::api::DEFAULT_PROMPT_CONFIRM_TIMEOUT.as_millis() as u64
        )]
        confirm_timeout_ms: u64,
    },
    /// Submit one guarded terminal prompt to an exact idle/done occupant.
    Send {
        /// Exact agent_ref returned by agent get/list/start.
        target: String,
        /// Read the prompt from stdin until EOF, removing one terminal LF or CRLF.
        #[arg(
            long,
            required_unless_present = "prompt_file",
            conflicts_with = "prompt_file"
        )]
        stdin: bool,
        /// Read the prompt from a file without putting prompt bytes in argv.
        #[arg(
            long = "prompt-file",
            required_unless_present = "stdin",
            conflicts_with = "stdin"
        )]
        prompt_file: Option<PathBuf>,
    },
    /// Best-effort steer an exact working Codex or Claude occupant.
    ///
    /// If the active turn completes concurrently, the prompt may start the next turn.
    Steer {
        /// Exact agent_ref returned by agent get/list/start.
        target: String,
        /// Read the prompt from stdin until EOF, removing one terminal LF or CRLF.
        #[arg(
            long,
            required_unless_present = "prompt_file",
            conflicts_with = "prompt_file"
        )]
        stdin: bool,
        /// Read the prompt from a file without putting prompt bytes in argv.
        #[arg(
            long = "prompt-file",
            required_unless_present = "stdin",
            conflicts_with = "stdin"
        )]
        prompt_file: Option<PathBuf>,
    },
    /// Send validated logical keys to an exact blocked agent occupant.
    SendKeys {
        /// Exact agent_ref for the blocked occupant.
        target: String,
        /// One allow-listed logical key or one literal Unicode character; repeat as needed.
        #[arg(long = "key", required = true)]
        keys: Vec<String>,
    },
    /// Start a supported provider in an exact shell pane and wait for provider-specific readiness.
    Start {
        /// Exact pane_ref returned by pane get/list/current/split.
        target: String,
        /// Provider kind advertised by the API schema.
        #[arg(long)]
        agent: String,
        /// One provider argv value; repeat for each argument.
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        /// Deadline for process, provider-readiness, and input-owner confirmation.
        #[arg(
            long = "timeout-ms",
            default_value_t = crate::api::DEFAULT_WAIT_TIMEOUT.as_millis() as u64
        )]
        timeout_ms: u64,
    },
    /// Wait on daemon snapshot events while pinning the exact agent occupant.
    Wait {
        target: String,
        /// Completion states. May be repeated or comma-separated.
        #[arg(long, value_delimiter = ',')]
        until: Vec<ApiAgentStatusArg>,
        #[arg(
            long = "timeout-ms",
            default_value_t = crate::api::DEFAULT_WAIT_TIMEOUT.as_millis() as u64
        )]
        timeout_ms: u64,
        /// Require a completion newer than this completed sequence.
        #[arg(long = "after-completed-seq")]
        after_completed_seq: Option<u64>,
    },
    /// Capture bounded terminal output after pinning the exact agent occupant.
    Read {
        target: String,
        #[command(flatten)]
        read: ApiReadArgs,
    },
    /// Inspect and wait for durable agent runs.
    Run {
        #[command(subcommand)]
        command: AgentRunCommand,
    },
    /// Inspect and wait for durable prompt operations.
    Operation {
        #[command(subcommand)]
        command: AgentOperationCommand,
    },
    /// Inspect durable agent-state storage.
    Storage {
        #[command(subcommand)]
        command: AgentStorageCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum AgentRunCommand {
    /// Get one run by exact run_ref.
    Get { run_ref: String },
    /// Wait for one exact run to reach a selected durable state.
    Wait {
        run_ref: String,
        /// Wait only for semantic completion instead of any current non-running state.
        #[arg(long, value_enum)]
        until: Option<AgentRunWaitUntilArg>,
        #[arg(
            long = "timeout-ms",
            default_value_t = crate::api::DEFAULT_WAIT_TIMEOUT.as_millis() as u64
        )]
        timeout_ms: u64,
    },
    /// Read the stored UTF-8 response body for one exact run.
    Response { run_ref: String },
    /// Observe a run twice and issue a short-lived recovery precondition when safe.
    Check { run_ref: String },
    /// Mark an unresolved historical run complete using a checked precondition.
    Resolve {
        run_ref: String,
        #[arg(long, value_enum)]
        outcome: AgentRunOutcomeArg,
        #[arg(long = "precondition-file")]
        precondition_file: PathBuf,
        #[arg(long = "resolution-id")]
        resolution_id: String,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum AgentRunWaitUntilArg {
    Completed,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum AgentRunOutcomeArg {
    Completed,
}

impl AgentRunOutcomeArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
        }
    }
}

#[derive(Debug, Subcommand)]
pub(super) enum AgentOperationCommand {
    /// Get one operation by exact operation_ref.
    Get { operation_ref: String },
    /// Wait until one exact operation reaches the requested dispatch state.
    Wait {
        operation_ref: String,
        #[arg(long, value_enum)]
        until: Option<AgentOperationWaitUntilArg>,
        /// Keep following a delivery_unknown operation for late hook confirmation.
        #[arg(long = "follow-unknown")]
        follow_unknown: bool,
        #[arg(
            long = "timeout-ms",
            default_value_t = crate::api::DEFAULT_WAIT_TIMEOUT.as_millis() as u64
        )]
        timeout_ms: u64,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum AgentOperationWaitUntilArg {
    PromptConfirmed,
}

#[derive(Debug, Subcommand)]
pub(super) enum AgentStorageCommand {
    /// Report bounded durable-state usage and limits.
    Status,
    /// Destructively reset durable agent state while the daemon and supported agents are stopped.
    Reset {
        #[arg(
            long = "expected-generation",
            required_unless_present = "recover_uninitialized",
            conflicts_with = "recover_uninitialized"
        )]
        expected_generation: Option<String>,
        /// Destructively replace missing, corrupt, or unsupported state metadata.
        #[arg(
            long = "recover-uninitialized",
            conflicts_with = "expected_generation",
            action = clap::ArgAction::SetTrue
        )]
        recover_uninitialized: bool,
        #[arg(long = "confirm-reset", required = true, action = clap::ArgAction::SetTrue)]
        confirm_reset: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum ApiAgentStatusArg {
    Blocked,
    Limited,
    Working,
    Done,
    Idle,
}

impl From<ApiAgentStatusArg> for crate::api::AgentStatus {
    fn from(value: ApiAgentStatusArg) -> Self {
        match value {
            ApiAgentStatusArg::Blocked => Self::Blocked,
            ApiAgentStatusArg::Limited => Self::Limited,
            ApiAgentStatusArg::Working => Self::Working,
            ApiAgentStatusArg::Done => Self::Done,
            ApiAgentStatusArg::Idle => Self::Idle,
        }
    }
}

pub(super) fn body_requires_stdin(args: &[OsString]) -> bool {
    args.get(1).and_then(|arg| arg.to_str()) == Some("agent")
        && matches!(
            args.get(2).and_then(|arg| arg.to_str()),
            Some("prompt" | "request" | "send" | "steer")
        )
        && args
            .iter()
            .skip(3)
            .any(|arg| arg.to_str() == Some("--stdin"))
        && !args
            .iter()
            .skip(3)
            .any(|arg| arg.to_str() == Some("--prompt-file"))
        && !args
            .iter()
            .skip(2)
            .any(|arg| matches!(arg.to_str(), Some("-h" | "--help")))
}

pub(super) fn read_prompt_input(mut input: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take((crate::api::MAX_PROMPT_BYTES + 3) as u64)
        .read_to_end(&mut bytes)?;
    finish_prompt_input(bytes)
}

pub(super) fn read_prompt_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path).map_err(|error| {
        crate::api::ApiError::new(
            crate::api::ApiErrorCode::InvalidArguments,
            format!("could not open prompt file {}: {error}", path.display()),
        )
    })?;
    read_prompt_input(file)
}

fn read_recovery_precondition_file(
    path: &Path,
) -> Result<crate::agent_state::RecoveryPrecondition> {
    const MAX_PRECONDITION_FILE_BYTES: usize = 64 * 1024;
    let mut file = std::fs::File::open(path).map_err(|error| {
        crate::api::ApiError::new(
            crate::api::ApiErrorCode::InvalidArguments,
            format!(
                "could not open recovery precondition file {}: {error}",
                path.display()
            ),
        )
    })?;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take((MAX_PRECONDITION_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PRECONDITION_FILE_BYTES {
        return Err(crate::api::ApiError::new(
            crate::api::ApiErrorCode::InvalidArguments,
            "recovery precondition file exceeds 65,536 bytes",
        )
        .into());
    }
    let envelope: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        crate::api::ApiError::new(
            crate::api::ApiErrorCode::InvalidArguments,
            format!("recovery precondition file is not valid JSON: {error}"),
        )
    })?;
    let value = envelope
        .pointer("/result/recovery_precondition")
        .filter(|value| !value.is_null())
        .ok_or_else(|| {
            crate::api::ApiError::new(
                crate::api::ApiErrorCode::InvalidArguments,
                "check output does not contain result.recovery_precondition",
            )
        })?;
    serde_json::from_value(value.clone()).map_err(|error| {
        crate::api::ApiError::new(
            crate::api::ApiErrorCode::InvalidArguments,
            format!("recovery precondition is invalid: {error}"),
        )
        .into()
    })
}

fn finish_prompt_input(mut bytes: Vec<u8>) -> Result<String> {
    if bytes.ends_with(b"\r\n") {
        bytes.truncate(bytes.len() - 2);
    } else if bytes.ends_with(b"\n") {
        bytes.pop();
    }
    if bytes.len() > crate::api::MAX_PROMPT_BYTES {
        return Err(crate::api::ApiError::new(
            crate::api::ApiErrorCode::InvalidArguments,
            format!(
                "prompt exceeds the {} byte limit",
                crate::api::MAX_PROMPT_BYTES
            ),
        )
        .into());
    }
    String::from_utf8(bytes).map_err(|_| {
        crate::api::ApiError::new(
            crate::api::ApiErrorCode::InvalidArguments,
            "prompt input must be valid UTF-8",
        )
        .into()
    })
}

pub(super) fn dispatch(
    command: AgentCommand,
    input: &str,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
) -> Result<String> {
    match command {
        AgentCommand::List {
            session,
            agent,
            status,
            cwd_prefix,
            unread,
            needs_action,
        } => crate::api::agent_list(
            runner,
            env,
            observed_at,
            &crate::api::AgentListFilter {
                session,
                agent,
                status: status.map(Into::into),
                cwd_prefix,
                unread_only: unread,
                needs_action_only: needs_action,
            },
        ),
        AgentCommand::Get { target } => crate::api::agent_get(runner, env, observed_at, &target),
        AgentCommand::Prompt {
            target,
            operation_id,
            stdin: _,
            prompt_file,
            confirm_timeout_ms,
        } => {
            let prompt = match prompt_file {
                Some(path) => read_prompt_file(&path)?,
                None => input.to_string(),
            };
            crate::api::agent_prompt(
                runner,
                env,
                &target,
                &operation_id,
                &prompt,
                Duration::from_millis(confirm_timeout_ms),
            )
        }
        AgentCommand::Request {
            target,
            state_file,
            stdin,
            prompt_file,
            confirm_timeout_ms,
        } => {
            let prompt = if let Some(path) = prompt_file {
                Some(read_prompt_file(&path)?)
            } else if stdin {
                Some(input.to_string())
            } else {
                None
            };
            super::agent_request::execute(
                runner,
                env,
                observed_at,
                &target,
                &state_file,
                prompt.as_deref(),
                Duration::from_millis(confirm_timeout_ms),
            )
        }
        AgentCommand::Send {
            target,
            stdin: _,
            prompt_file,
        } => {
            let prompt = match prompt_file {
                Some(path) => read_prompt_file(&path)?,
                None => input.to_string(),
            };
            crate::api::agent_send(runner, env, observed_at, &target, &prompt)
        }
        AgentCommand::Steer {
            target,
            stdin: _,
            prompt_file,
        } => {
            let prompt = match prompt_file {
                Some(path) => read_prompt_file(&path)?,
                None => input.to_string(),
            };
            crate::api::agent_steer(runner, env, observed_at, &target, &prompt)
        }
        AgentCommand::SendKeys { target, keys } => {
            crate::api::agent_send_keys(runner, env, observed_at, &target, &keys)
        }
        AgentCommand::Start {
            target,
            agent,
            args,
            timeout_ms,
        } => crate::api::agent_start(
            runner,
            env,
            observed_at,
            &target,
            &agent,
            &args,
            Duration::from_millis(timeout_ms),
        ),
        AgentCommand::Wait {
            target,
            until,
            timeout_ms,
            after_completed_seq,
        } => {
            let until = if until.is_empty() {
                [
                    crate::api::AgentStatus::Done,
                    crate::api::AgentStatus::Blocked,
                    crate::api::AgentStatus::Limited,
                ]
                .into_iter()
                .collect()
            } else {
                until.into_iter().map(Into::into).collect()
            };
            crate::api::agent_wait(
                runner,
                env,
                &target,
                &until,
                Duration::from_millis(timeout_ms),
                after_completed_seq,
            )
        }
        AgentCommand::Read { target, read } => crate::api::agent_read(
            runner,
            env,
            observed_at,
            &target,
            crate::api::ReadOptions {
                source: read.source.into(),
                lines: read.lines,
                ansi: read.ansi,
            },
        ),
        AgentCommand::Run { command } => match command {
            AgentRunCommand::Get { run_ref } => {
                crate::api::agent_run_get(runner, env, observed_at, &run_ref)
            }
            AgentRunCommand::Wait {
                run_ref,
                until,
                timeout_ms,
            } => crate::api::agent_run_wait(
                runner,
                env,
                observed_at,
                &run_ref,
                Duration::from_millis(timeout_ms),
                matches!(until, Some(AgentRunWaitUntilArg::Completed)),
            ),
            AgentRunCommand::Response { run_ref } => {
                crate::api::agent_run_response(runner, env, observed_at, &run_ref)
            }
            AgentRunCommand::Check { run_ref } => {
                crate::api::agent_run_check(runner, env, observed_at, &run_ref)
            }
            AgentRunCommand::Resolve {
                run_ref,
                outcome,
                precondition_file,
                resolution_id,
                reason,
            } => {
                let precondition = read_recovery_precondition_file(&precondition_file)?;
                crate::api::agent_run_resolve(
                    runner,
                    env,
                    observed_at,
                    &run_ref,
                    outcome.as_str(),
                    precondition,
                    &resolution_id,
                    &reason,
                )
            }
        },
        AgentCommand::Operation { command } => match command {
            AgentOperationCommand::Get { operation_ref } => {
                crate::api::agent_operation_get(runner, env, observed_at, &operation_ref)
            }
            AgentOperationCommand::Wait {
                operation_ref,
                until,
                follow_unknown,
                timeout_ms,
            } => crate::api::agent_operation_wait(
                runner,
                env,
                observed_at,
                &operation_ref,
                Duration::from_millis(timeout_ms),
                matches!(until, Some(AgentOperationWaitUntilArg::PromptConfirmed)),
                follow_unknown,
            ),
        },
        AgentCommand::Storage { command } => match command {
            AgentStorageCommand::Status => {
                crate::api::agent_storage_status(runner, env, observed_at)
            }
            AgentStorageCommand::Reset {
                expected_generation,
                recover_uninitialized,
                confirm_reset,
            } => reset_storage_offline(
                runner,
                env,
                observed_at,
                expected_generation.as_deref(),
                recover_uninitialized,
                confirm_reset,
            ),
        },
    }
}

fn reset_storage_offline(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    expected_generation: Option<&str>,
    recover_uninitialized: bool,
    confirm_reset: bool,
) -> Result<String> {
    if !confirm_reset {
        return Err(crate::api::ApiError::new(
            crate::api::ApiErrorCode::InvalidArguments,
            "offline storage reset requires --confirm-reset",
        )
        .into());
    }
    let expected_generation = expected_generation
        .map(crate::agent_state::StateGeneration::parse)
        .transpose()
        .map_err(|error| {
            crate::api::ApiError::new(
                crate::api::ApiErrorCode::InvalidArguments,
                error.to_string(),
            )
        })?;
    if expected_generation.is_some() == recover_uninitialized {
        return Err(crate::api::ApiError::new(
            crate::api::ApiErrorCode::InvalidArguments,
            "offline storage reset requires exactly one of --expected-generation or --recover-uninitialized",
        )
        .into());
    }
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)
        .map_err(|error| {
            crate::api::ApiError::new(
                crate::api::ApiErrorCode::TmuxServerUnavailable,
                format!("could not resolve tmux server for offline reset: {error:#}"),
            )
        })?;
    incarnation.verify(runner, env).map_err(|error| {
        crate::api::ApiError::new(
            crate::api::ApiErrorCode::StaleReference,
            format!("tmux server changed before offline reset: {error:#}"),
        )
    })?;

    let socket = crate::daemon::daemon_socket_path_for_incarnation(env, None, &incarnation.hash);
    if let Some(parent) = socket.parent().filter(|path| !path.as_os_str().is_empty()) {
        crate::daemon::lifecycle::ensure_secure_socket_dir(parent).map_err(|error| {
            crate::api::ApiError::new(
                crate::api::ApiErrorCode::RecoveryNotAllowed,
                format!("could not secure the daemon lock directory: {error:#}"),
            )
        })?;
    }
    let _daemon_guard = crate::daemon::lifecycle::try_acquire_daemon_instance_lock(&socket)
        .map_err(|error| {
            crate::api::ApiError::new(
                crate::api::ApiErrorCode::RecoveryNotAllowed,
                format!("could not prove daemon quiescence: {error:#}"),
            )
        })?
        .ok_or_else(|| {
            crate::api::ApiError::new(
                crate::api::ApiErrorCode::RecoveryNotAllowed,
                "offline storage reset is forbidden while the daemon is running",
            )
        })?;
    let lifecycle = crate::daemon::lifecycle::read_lifecycle_record(env, &incarnation.hash)
        .map_err(|error| {
            crate::api::ApiError::new(
                crate::api::ApiErrorCode::RecoveryNotAllowed,
                format!("could not inspect daemon lifecycle state: {error:#}"),
            )
        })?;
    if lifecycle.process.as_ref().is_some_and(|process| {
        crate::daemon::lifecycle::process_start_token(process.pid)
            .is_ok_and(|token| token == process.start_token)
    }) {
        return Err(crate::api::ApiError::new(
            crate::api::ApiErrorCode::RecoveryNotAllowed,
            "offline storage reset is forbidden while the recorded daemon process is alive",
        )
        .into());
    }

    let framing = crate::daemon::topology::QueryFraming::generate().map_err(|error| {
        crate::api::ApiError::new(
            crate::api::ApiErrorCode::IdentityVerificationFailed,
            error.to_string(),
        )
    })?;
    let args = crate::daemon::topology::guarded_poll_query_args(&framing);
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = runner.run(&refs).map_err(|error| {
        crate::api::ApiError::new(
            crate::api::ApiErrorCode::IdentityVerificationFailed,
            format!("could not inspect live tmux panes before reset: {error:#}"),
        )
    })?;
    let topology =
        crate::daemon::topology::parse_topology(&output, &framing, &incarnation.identity).map_err(
            |error| {
                crate::api::ApiError::new(
                    crate::api::ApiErrorCode::IdentityVerificationFailed,
                    error.to_string(),
                )
            },
        )?;
    let processes =
        crate::daemon::workers::read_agent_process_snapshot(Duration::from_secs(2), false);
    let codex = crate::pane_state::AgentKind::parse("codex").expect("valid provider kind");
    for pane in topology.panes {
        let detection = processes.detect_from_pid_tree(pane.pane_instance.pane_pid);
        if !detection.complete || !detection.process_identities_complete {
            return Err(crate::api::ApiError::new(
                crate::api::ApiErrorCode::IdentityVerificationFailed,
                format!(
                    "process scan for pane {} was incomplete; reset refused",
                    pane.pane_instance.pane_id
                ),
            )
            .into());
        }
        if detection.exact_agent_process(&codex).is_some() {
            return Err(crate::api::ApiError::new(
                crate::api::ApiErrorCode::RecoveryNotAllowed,
                format!(
                    "offline storage reset is forbidden while a supported Codex occupant is live in pane {}",
                    pane.pane_instance.pane_id
                ),
            )
            .into());
        }
    }

    let root = crate::agent_state::state_root(env, &incarnation.hash);
    let (previous_generation, generation) = if recover_uninitialized {
        (
            "uninitialized_or_unsupported".to_string(),
            crate::agent_state::AgentStateStore::recover_uninitialized_offline(root)
                .map_err(storage_reset_error)?,
        )
    } else {
        let expected_generation = expected_generation.expect("reset mode checked above");
        let generation =
            crate::agent_state::AgentStateStore::reset_offline(root, &expected_generation)
                .map_err(storage_reset_error)?;
        (expected_generation.as_str().to_string(), generation)
    };
    crate::api::agent_storage_reset_result(
        observed_at,
        incarnation.hash,
        previous_generation,
        generation.as_str().to_string(),
    )
}

fn storage_reset_error(error: crate::agent_state::StoreError) -> crate::api::ApiError {
    let code = match error {
        crate::agent_state::StoreError::StalePrecondition(_) => {
            crate::api::ApiErrorCode::StalePrecondition
        }
        crate::agent_state::StoreError::RecoveryNotAllowed(_) => {
            crate::api::ApiErrorCode::RecoveryNotAllowed
        }
        crate::agent_state::StoreError::StateUninitialized => {
            crate::api::ApiErrorCode::StateUninitialized
        }
        _ => crate::api::ApiErrorCode::DaemonError,
    };
    crate::api::ApiError::new(code, error.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn prompt_input_reader_enforces_utf8_and_the_byte_limit() {
        let accepted =
            super::read_prompt_input(std::io::Cursor::new(b"review\nthis".to_vec())).unwrap();
        assert_eq!(accepted, "review\nthis");
        assert_eq!(
            super::read_prompt_input(std::io::Cursor::new(b"review\nthis\n".to_vec())).unwrap(),
            "review\nthis"
        );
        assert_eq!(
            super::read_prompt_input(std::io::Cursor::new(b"review\r\n".to_vec())).unwrap(),
            "review"
        );
        assert_eq!(
            super::read_prompt_input(std::io::Cursor::new(b"review\n\n".to_vec())).unwrap(),
            "review\n"
        );

        let mut maximum_with_terminator = vec![b'x'; crate::api::MAX_PROMPT_BYTES];
        maximum_with_terminator.push(b'\n');
        assert_eq!(
            super::read_prompt_input(std::io::Cursor::new(maximum_with_terminator))
                .unwrap()
                .len(),
            crate::api::MAX_PROMPT_BYTES
        );

        let too_large = vec![b'x'; crate::api::MAX_PROMPT_BYTES + 1];
        let error = super::read_prompt_input(std::io::Cursor::new(too_large)).unwrap_err();
        assert!(error.to_string().contains("byte limit"));

        let error = super::read_prompt_input(std::io::Cursor::new(vec![0xff])).unwrap_err();
        assert!(error.to_string().contains("valid UTF-8"));
    }

    #[test]
    fn request_stdin_is_pre_read_only_when_explicitly_selected() {
        let with_stdin = [
            "vt",
            "agent",
            "request",
            "vta1:test",
            "--state-file",
            "/private/request.json",
            "--stdin",
        ]
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
        assert!(super::body_requires_stdin(&with_stdin));

        let resume = [
            "vt",
            "agent",
            "request",
            "vta1:test",
            "--state-file",
            "/private/request.json",
        ]
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
        assert!(!super::body_requires_stdin(&resume));
    }
}
