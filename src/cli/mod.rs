use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::load::load_config;
use crate::session::Direction;
use crate::tmux::{SystemTmuxRunner, TmuxRunner};

mod category;
mod daemon;
mod hook;
pub mod pane_switch;
mod sidebar;

const MAX_AGENT_HOOK_STDIN_BYTES: usize = 16 * 1024 * 1024;

/// vde-tmux CLI。
#[derive(Debug, Parser)]
#[command(version, about = "tmux state & UI manager", subcommand_required = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect the versioned agent JSON API.
    Api {
        /// Accepted for explicit agent invocation; API output is always JSON.
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        command: ApiCommand,
    },
    /// Read pane topology and bounded terminal output as JSON.
    Pane {
        /// Accepted for explicit agent invocation; API output is always JSON.
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        command: PaneCommand,
    },
    /// Inspect and wait for agent occupants as JSON.
    Agent {
        /// Accepted for explicit agent invocation; API output is always JSON.
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        command: AgentCommand,
    },
    #[command(name = "statusline-category")]
    StatuslineCategory {
        #[arg(long = "session-id")]
        session_id: Option<String>,
        #[arg(long = "client-name")]
        client_name: Option<String>,
        #[command(subcommand)]
        command: Option<StatuslineCategoryCommand>,
    },
    #[command(name = "statusline-summary")]
    StatuslineSummary,
    #[command(name = "statusline-attention")]
    StatuslineAttention {
        #[arg(long = "session-id")]
        session_id: String,
    },
    #[command(name = "statusline-sessions")]
    StatuslineSessions {
        #[arg(long = "session-id")]
        session_id: Option<String>,
        #[arg(long = "client-name")]
        client_name: Option<String>,
        #[arg(long = "show-index")]
        show_index: bool,
        #[command(subcommand)]
        command: Option<StatuslineSessionsCommand>,
    },
    #[command(name = "statusline-windows")]
    StatuslineWindows {
        #[arg(long = "session-id")]
        session_id: Option<String>,
        #[command(subcommand)]
        command: Option<StatuslineWindowsCommand>,
    },
    #[command(name = "statusline-pane")]
    StatuslinePane {
        #[arg(long)]
        target: String,
    },
    #[command(name = "statusline-click")]
    StatuslineClick {
        #[command(flatten)]
        scope: ClientActionScope,
        range: Option<String>,
    },
    /// Run or control the pane-state daemon for the current tmux server.
    #[command(
        long_about = "Run or control the pane-state daemon for the current tmux server.\n\nWith no subcommand this is the internal foreground server entry point. For normal use, use `daemon ensure`, `daemon stop`, or `daemon reload` (`daemon restart` is an alias for `reload`).",
        after_help = "Examples:\n  vt daemon ensure\n  vt daemon status\n  vt daemon stop\n  vt daemon reload"
    )]
    Daemon {
        #[arg(long, hide = true)]
        socket: Option<String>,
        #[arg(long, hide = true)]
        server_identity: Option<String>,
        #[arg(long, hide = true)]
        server_pid: Option<u32>,
        #[arg(long, hide = true)]
        server_start_time: Option<i64>,
        #[arg(long, hide = true)]
        tmux_server_socket: Option<String>,
        #[command(subcommand)]
        command: Option<DaemonCommand>,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Sidebar {
        #[command(subcommand)]
        command: sidebar::SidebarCommand,
    },
    #[command(name = "pane-switch")]
    PaneSwitch {
        direction: pane_switch::PaneSwitchDirection,
        #[arg(long = "pane-id")]
        pane_id: String,
        #[arg(long = "pane-pid")]
        pane_pid: u32,
        #[arg(long = "daemon-socket", hide = true)]
        daemon_socket: String,
        #[arg(long = "server-identity", hide = true)]
        server_identity: String,
    },
    Category {
        #[command(subcommand)]
        command: CategoryCommand,
    },
    #[command(name = "session-cycle")]
    SessionCycle {
        #[command(subcommand)]
        command: SessionCycleCommand,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Hooks {
        #[command(subcommand)]
        command: HooksCommand,
    },
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    #[command(name = "session-manager")]
    SessionManager {
        #[arg(long)]
        popup: bool,
        #[arg(long, hide = true)]
        render_preview: Option<String>,
        #[arg(long, hide = true)]
        preview_name: Option<String>,
        #[command(subcommand)]
        command: Option<SessionManagerCommand>,
    },
    /// Accept typed lifecycle and progress events from agent integrations.
    #[command(
        after_help = "Examples:\n  vt hook emit --agent myagent --session-id run-42 --status running\n  vt hook claude Stop\n  vt hook codex Stop"
    )]
    Hook {
        #[command(subcommand)]
        command: hook::HookCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ApiCommand {
    /// Emit the public JSON schemas without connecting to tmux or the daemon.
    Schema,
    /// Emit one compact, canonical pane/agent snapshot.
    Snapshot,
}

#[derive(Debug, Subcommand)]
enum PaneCommand {
    /// List panes from the daemon's cached canonical snapshot.
    List,
    /// Get one pane by %pane_id or pane_ref.
    Get { target: String },
    /// Get the pane identified by TMUX_PANE, never by client focus.
    Current,
    /// Capture bounded terminal output after pinning the pane process identity.
    Read {
        /// %pane_id or pane_ref; defaults to TMUX_PANE.
        target: Option<String>,
        #[command(flatten)]
        read: ApiReadArgs,
    },
    /// Split an exact pane without changing client focus by default.
    Split {
        /// Exact pane_ref returned by pane list/get/current.
        target: String,
        #[arg(long, value_enum)]
        direction: ApiPaneSplitDirectionArg,
        #[arg(long = "size-percent")]
        size_percent: Option<u8>,
        /// Absolute cwd for the new pane; defaults to the target pane cwd.
        #[arg(long)]
        cwd: Option<String>,
        /// Select the created pane. Omitted by default so agent automation does not steal focus.
        #[arg(long)]
        focus: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
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
enum AgentRunCommand {
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
enum AgentRunWaitUntilArg {
    Completed,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AgentRunOutcomeArg {
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
enum AgentOperationCommand {
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
enum AgentOperationWaitUntilArg {
    PromptConfirmed,
}

#[derive(Debug, Subcommand)]
enum AgentStorageCommand {
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
enum ApiAgentStatusArg {
    Blocked,
    Working,
    Done,
    Idle,
}

impl From<ApiAgentStatusArg> for crate::api::AgentStatus {
    fn from(value: ApiAgentStatusArg) -> Self {
        match value {
            ApiAgentStatusArg::Blocked => Self::Blocked,
            ApiAgentStatusArg::Working => Self::Working,
            ApiAgentStatusArg::Done => Self::Done,
            ApiAgentStatusArg::Idle => Self::Idle,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ApiReadSourceArg {
    Visible,
    Latest,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ApiPaneSplitDirectionArg {
    Right,
    Down,
}

impl From<ApiPaneSplitDirectionArg> for crate::api::PaneSplitDirection {
    fn from(value: ApiPaneSplitDirectionArg) -> Self {
        match value {
            ApiPaneSplitDirectionArg::Right => Self::Right,
            ApiPaneSplitDirectionArg::Down => Self::Down,
        }
    }
}

impl From<ApiReadSourceArg> for crate::api::ReadSource {
    fn from(value: ApiReadSourceArg) -> Self {
        match value {
            ApiReadSourceArg::Visible => Self::Visible,
            ApiReadSourceArg::Latest => Self::Latest,
        }
    }
}

#[derive(Debug, Clone, Copy, Args)]
struct ApiReadArgs {
    #[arg(long, default_value_t = crate::api::DEFAULT_READ_LINES)]
    lines: usize,
    #[arg(long, value_enum, default_value_t = ApiReadSourceArg::Latest)]
    source: ApiReadSourceArg,
    #[arg(long)]
    ansi: bool,
}

#[derive(Debug, Subcommand)]
enum StatuslineCategoryCommand {
    Switch { index: usize },
}

#[derive(Debug, Subcommand)]
enum StatuslineSessionsCommand {
    Switch { index: usize },
}

#[derive(Debug, Subcommand)]
enum StatuslineWindowsCommand {
    Switch { target: String },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Schema,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Start if enabled; when disabled, succeed without changing state.
    Ensure,
    /// Explicitly start the daemon; disabled mode is an error.
    Start,
    /// Stop temporarily while leaving automatic startup enabled.
    Stop {
        /// Signal only after revalidating the recorded process and socket identity.
        #[arg(long)]
        force: bool,
    },
    /// Remove owned hooks, stop the daemon, and suppress automatic startup.
    Disable,
    /// Install owned hooks, reach Serving, then re-enable automatic startup.
    Enable,
    /// Alias for reload: strictly validate config, stop, and start without rollback.
    Restart,
    /// Strictly validate config and restart without rollback on startup failure.
    Reload,
    /// Report lifecycle and runtime health without changing daemon state.
    Status,
}

#[derive(Debug, Subcommand)]
enum CategoryCommand {
    Next {
        #[command(flatten)]
        scope: ClientActionScope,
    },
    Prev {
        #[command(flatten)]
        scope: ClientActionScope,
    },
    Use {
        name: String,
        #[command(flatten)]
        scope: ClientActionScope,
    },
    List,
    Create {
        name: String,
    },
    Rename {
        name: String,
        replacement: String,
    },
    Delete {
        name: String,
        #[arg(long = "move-to")]
        move_to: Option<String>,
        #[arg(long)]
        automatic: bool,
    },
    Assign {
        category: String,
        #[arg(long)]
        repo: String,
    },
    Automatic {
        #[arg(long)]
        repo: String,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCycleCommand {
    Next {
        #[command(flatten)]
        scope: ClientActionScope,
    },
    Prev {
        #[command(flatten)]
        scope: ClientActionScope,
    },
}

#[derive(Debug, Default, Args)]
struct ClientActionScope {
    /// tmux client captured by the invoking binding; required when clients share a pane.
    #[arg(long = "client-name")]
    client_name: Option<String>,
    /// Source tmux session captured by the invoking binding.
    #[arg(long = "session-id")]
    session_id: Option<String>,
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    New {
        #[arg(short = 'c', long = "cwd")]
        cwd: Option<String>,
        #[command(flatten)]
        scope: ClientActionScope,
    },
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)] // Hidden view hook mirrors its bounded inline snapshot frame.
enum HooksCommand {
    #[command(name = "on-client-session-changed")]
    OnClientSessionChanged {
        client_pid: Option<u32>,
        session_name: Option<String>,
    },
    #[command(name = "pane-state-view", hide = true)]
    PaneStateView {
        event_kind: String,
        #[arg(long)]
        owner: String,
        #[arg(long)]
        protocol: u16,
        #[arg(long = "hook-session")]
        hook_session: Option<String>,
        #[arg(long = "hook-window")]
        hook_window: Option<String>,
        #[arg(long = "snapshot-session", hide = true, default_value = "")]
        snapshot_session: String,
        #[arg(long = "snapshot-window", hide = true, default_value = "")]
        snapshot_window: String,
        #[arg(long = "snapshot-pane", hide = true, default_value = "")]
        snapshot_pane: String,
        #[arg(long = "snapshot-pane-pid", hide = true, default_value = "")]
        snapshot_pane_pid: String,
        #[arg(long = "snapshot-panes", hide = true, default_value = "")]
        snapshot_panes: String,
        #[arg(long = "snapshot-clients", hide = true, default_value = "")]
        snapshot_clients: String,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    Switch {
        path: String,
    },
    Selector {
        #[arg(long)]
        popup: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SessionManagerCommand {
    #[command(name = "kill-window")]
    KillWindow { target: String },
    #[command(name = "kill-pane")]
    KillPane { target: String },
}

fn current_env() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

pub fn run() -> ExitCode {
    let args = std::env::args_os().collect::<Vec<_>>();
    let is_json_api = is_json_api_args(&args);
    let observed_at = now_epoch();
    let is_agent_hook = args.get(1).and_then(|arg| arg.to_str()) == Some("hook");
    let is_view_hook = args.get(1).and_then(|arg| arg.to_str()) == Some("hooks")
        && args.get(2).and_then(|arg| arg.to_str()) == Some("pane-state-view");
    let timeout = if is_view_hook {
        Duration::from_millis(100)
    } else if is_agent_hook {
        Duration::from_millis(300)
    } else {
        Duration::from_secs(3)
    };
    let reads_agent_hook_stdin = agent_hook_requires_stdin(&args);
    let agent_hook_deadline =
        reads_agent_hook_stdin.then(|| Instant::now() + Duration::from_secs(2));
    let input = match agent_hook_deadline {
        Some(deadline) => match read_agent_hook_input_until(deadline) {
            Ok(input) => input,
            Err(error) => {
                eprintln!("{error:#}");
                return ExitCode::FAILURE;
            }
        },
        None if agent_body_requires_stdin(&args) => match read_prompt_input(std::io::stdin()) {
            Ok(input) => input,
            Err(error) => {
                eprintln!("{}", crate::api::render_error(&error, observed_at));
                return ExitCode::FAILURE;
            }
        },
        None => String::new(),
    };
    let runner = SystemTmuxRunner::from_env(timeout);
    match run_with_input_at_with_hook_deadline(
        args,
        &input,
        &runner,
        &current_env(),
        observed_at,
        agent_hook_deadline,
    ) {
        Ok(Some(output)) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            if is_json_api {
                eprintln!("{}", crate::api::render_error(&error, observed_at));
            } else {
                eprintln!("{error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

fn agent_body_requires_stdin(args: &[OsString]) -> bool {
    args.get(1).and_then(|arg| arg.to_str()) == Some("agent")
        && matches!(
            args.get(2).and_then(|arg| arg.to_str()),
            Some("prompt" | "send" | "steer")
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

fn read_prompt_input(mut input: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take((crate::api::MAX_PROMPT_BYTES + 3) as u64)
        .read_to_end(&mut bytes)?;
    finish_prompt_input(bytes)
}

fn read_prompt_file(path: &Path) -> Result<String> {
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

fn is_json_api_args(args: &[OsString]) -> bool {
    !args
        .iter()
        .skip(2)
        .any(|arg| matches!(arg.to_str(), Some("-h" | "--help")))
        && matches!(
            args.get(1).and_then(|arg| arg.to_str()),
            Some("api" | "pane" | "agent")
        )
}

fn agent_hook_requires_stdin(args: &[OsString]) -> bool {
    if args.get(1).and_then(|arg| arg.to_str()) != Some("hook")
        || args
            .iter()
            .skip(2)
            .any(|arg| matches!(arg.to_str(), Some("-h" | "--help")))
    {
        return false;
    }
    match args.get(2).and_then(|arg| arg.to_str()) {
        Some("claude") => args.get(3).is_some(),
        Some("codex") => args
            .get(3)
            .and_then(|arg| arg.to_str())
            .is_some_and(|event| !event.trim_start().starts_with('{')),
        _ => false,
    }
}

fn read_agent_hook_input_until(deadline: Instant) -> Result<String> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    read_agent_hook_input_from_until(&mut input, deadline)
}

pub(crate) fn read_agent_hook_input_from_until<R>(
    input: &mut R,
    deadline: Instant,
) -> Result<String>
where
    R: Read + AsRawFd,
{
    let mut bytes = Vec::new();
    loop {
        match wait_for_input(input.as_raw_fd(), deadline)? {
            PollOutcome::Ready => {}
            PollOutcome::Closed => return finish_agent_hook_input(bytes),
            PollOutcome::TimedOut => {
                if bytes.is_empty() {
                    bail!("agent hook 2s deadline exceeded while reading stdin");
                }
                // Keep whatever fully arrived; a slow sender that never closes
                // stdin must not discard an already-complete payload.
                return finish_agent_hook_input(bytes);
            }
        }
        let mut chunk = [0_u8; 8192];
        let read = input.read(&mut chunk)?;
        if read == 0 {
            return finish_agent_hook_input(bytes);
        }
        if bytes.len().saturating_add(read) > MAX_AGENT_HOOK_STDIN_BYTES {
            bail!(
                "agent hook stdin exceeds the {} byte limit",
                MAX_AGENT_HOOK_STDIN_BYTES
            );
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

fn finish_agent_hook_input(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes)
        .map_err(|error| anyhow::anyhow!("agent hook stdin is not UTF-8: {error}"))
}

enum PollOutcome {
    Ready,
    Closed,
    TimedOut,
}

fn wait_for_input(fd: RawFd, deadline: Instant) -> Result<PollOutcome> {
    if fd < 0 {
        return Ok(PollOutcome::Closed);
    }
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(PollOutcome::TimedOut);
        };
        let timeout_ms = remaining
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll_fd points to one initialized descriptor for the duration of the call.
        let result = unsafe { libc::poll(&raw mut poll_fd, 1, timeout_ms) };
        if result == 0 {
            return Ok(PollOutcome::TimedOut);
        }
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if poll_fd.revents & libc::POLLNVAL != 0 {
            return Ok(PollOutcome::Closed);
        }
        return Ok(PollOutcome::Ready);
    }
}

pub fn run_with<I, T>(
    args: I,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<Option<String>>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    run_with_input_at(args, "", runner, env, now_epoch())
}

pub fn run_with_input_at<I, T>(
    args: I,
    input: &str,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    now_epoch: i64,
) -> Result<Option<String>>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut stderr = std::io::stderr();
    run_with_input_at_writing_warnings(args, input, runner, env, now_epoch, &mut stderr)
}

fn run_with_input_at_with_hook_deadline<I, T>(
    args: I,
    input: &str,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    now_epoch: i64,
    hook_deadline: Option<Instant>,
) -> Result<Option<String>>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut stderr = std::io::stderr();
    run_with_input_at_writing_warnings_and_hook_deadline(
        args,
        input,
        runner,
        env,
        now_epoch,
        &mut stderr,
        hook_deadline,
    )
}

pub(crate) fn run_with_input_at_writing_warnings<I, T, W>(
    args: I,
    input: &str,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    now_epoch: i64,
    warning_writer: &mut W,
) -> Result<Option<String>>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
    W: Write,
{
    run_with_input_at_writing_warnings_and_hook_deadline(
        args,
        input,
        runner,
        env,
        now_epoch,
        warning_writer,
        None,
    )
}

fn run_with_input_at_writing_warnings_and_hook_deadline<I, T, W>(
    args: I,
    input: &str,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    now_epoch: i64,
    warning_writer: &mut W,
    hook_deadline: Option<Instant>,
) -> Result<Option<String>>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
    W: Write,
{
    let cli = Cli::try_parse_from(args)?;
    let loaded = load_config(env);
    let is_json_api = matches!(
        &cli.command,
        Command::Api { .. }
            | Command::Pane { .. }
            | Command::Agent { .. }
            | Command::Sidebar {
                command: sidebar::SidebarCommand::PrepareLayout { .. }
            }
    );
    if !is_json_api {
        emit_config_warnings(&loaded.warnings, warning_writer)?;
    }
    let config = loaded.config;
    match cli.command {
        Command::Api { command, json: _ } => match command {
            ApiCommand::Schema => Ok(Some(crate::api::schema_json(now_epoch)?)),
            ApiCommand::Snapshot => Ok(Some(crate::api::snapshot(runner, env, now_epoch)?)),
        },
        Command::Pane { command, json: _ } => match command {
            PaneCommand::List => Ok(Some(crate::api::pane_list(runner, env, now_epoch)?)),
            PaneCommand::Get { target } => {
                Ok(Some(crate::api::pane_get(runner, env, now_epoch, &target)?))
            }
            PaneCommand::Current => Ok(Some(crate::api::pane_current(runner, env, now_epoch)?)),
            PaneCommand::Read { target, read } => {
                let target = target
                    .or_else(|| env.get("TMUX_PANE").cloned())
                    .ok_or_else(|| {
                        crate::api::ApiError::new(
                            crate::api::ApiErrorCode::NoCurrentPane,
                            "target is required when TMUX_PANE is not set",
                        )
                    })?;
                Ok(Some(crate::api::pane_read(
                    runner,
                    env,
                    now_epoch,
                    &target,
                    crate::api::ReadOptions {
                        source: read.source.into(),
                        lines: read.lines,
                        ansi: read.ansi,
                    },
                )?))
            }
            PaneCommand::Split {
                target,
                direction,
                size_percent,
                cwd,
                focus,
            } => Ok(Some(crate::api::pane_split(
                runner,
                env,
                now_epoch,
                &target,
                crate::api::PaneSplitOptions {
                    direction: direction.into(),
                    size_percent,
                    cwd: cwd.as_deref(),
                    focus,
                },
            )?)),
        },
        Command::Agent { command, json: _ } => match command {
            AgentCommand::List {
                session,
                agent,
                status,
                cwd_prefix,
                unread,
                needs_action,
            } => Ok(Some(crate::api::agent_list(
                runner,
                env,
                now_epoch,
                &crate::api::AgentListFilter {
                    session,
                    agent,
                    status: status.map(Into::into),
                    cwd_prefix,
                    unread_only: unread,
                    needs_action_only: needs_action,
                },
            )?)),
            AgentCommand::Get { target } => Ok(Some(crate::api::agent_get(
                runner, env, now_epoch, &target,
            )?)),
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
                Ok(Some(crate::api::agent_prompt(
                    runner,
                    env,
                    &target,
                    &operation_id,
                    &prompt,
                    Duration::from_millis(confirm_timeout_ms),
                )?))
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
                Ok(Some(crate::api::agent_send(
                    runner, env, now_epoch, &target, &prompt,
                )?))
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
                Ok(Some(crate::api::agent_steer(
                    runner, env, now_epoch, &target, &prompt,
                )?))
            }
            AgentCommand::SendKeys { target, keys } => Ok(Some(crate::api::agent_send_keys(
                runner, env, now_epoch, &target, &keys,
            )?)),
            AgentCommand::Start {
                target,
                agent,
                args,
                timeout_ms,
            } => Ok(Some(crate::api::agent_start(
                runner,
                env,
                now_epoch,
                &target,
                &agent,
                &args,
                Duration::from_millis(timeout_ms),
            )?)),
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
                    ]
                    .into_iter()
                    .collect()
                } else {
                    until.into_iter().map(Into::into).collect()
                };
                Ok(Some(crate::api::agent_wait(
                    runner,
                    env,
                    &target,
                    &until,
                    Duration::from_millis(timeout_ms),
                    after_completed_seq,
                )?))
            }
            AgentCommand::Read { target, read } => Ok(Some(crate::api::agent_read(
                runner,
                env,
                now_epoch,
                &target,
                crate::api::ReadOptions {
                    source: read.source.into(),
                    lines: read.lines,
                    ansi: read.ansi,
                },
            )?)),
            AgentCommand::Run { command } => match command {
                AgentRunCommand::Get { run_ref } => Ok(Some(crate::api::agent_run_get(
                    runner, env, now_epoch, &run_ref,
                )?)),
                AgentRunCommand::Wait {
                    run_ref,
                    until,
                    timeout_ms,
                } => Ok(Some(crate::api::agent_run_wait(
                    runner,
                    env,
                    now_epoch,
                    &run_ref,
                    Duration::from_millis(timeout_ms),
                    matches!(until, Some(AgentRunWaitUntilArg::Completed)),
                )?)),
                AgentRunCommand::Response { run_ref } => Ok(Some(crate::api::agent_run_response(
                    runner, env, now_epoch, &run_ref,
                )?)),
                AgentRunCommand::Check { run_ref } => Ok(Some(crate::api::agent_run_check(
                    runner, env, now_epoch, &run_ref,
                )?)),
                AgentRunCommand::Resolve {
                    run_ref,
                    outcome,
                    precondition_file,
                    resolution_id,
                    reason,
                } => {
                    let precondition = read_recovery_precondition_file(&precondition_file)?;
                    Ok(Some(crate::api::agent_run_resolve(
                        runner,
                        env,
                        now_epoch,
                        &run_ref,
                        outcome.as_str(),
                        precondition,
                        &resolution_id,
                        &reason,
                    )?))
                }
            },
            AgentCommand::Operation { command } => match command {
                AgentOperationCommand::Get { operation_ref } => Ok(Some(
                    crate::api::agent_operation_get(runner, env, now_epoch, &operation_ref)?,
                )),
                AgentOperationCommand::Wait {
                    operation_ref,
                    until,
                    follow_unknown,
                    timeout_ms,
                } => Ok(Some(crate::api::agent_operation_wait(
                    runner,
                    env,
                    now_epoch,
                    &operation_ref,
                    Duration::from_millis(timeout_ms),
                    matches!(until, Some(AgentOperationWaitUntilArg::PromptConfirmed)),
                    follow_unknown,
                )?)),
            },
            AgentCommand::Storage { command } => match command {
                AgentStorageCommand::Status => Ok(Some(crate::api::agent_storage_status(
                    runner, env, now_epoch,
                )?)),
                AgentStorageCommand::Reset {
                    expected_generation,
                    recover_uninitialized,
                    confirm_reset,
                } => Ok(Some(reset_agent_storage_offline(
                    runner,
                    env,
                    now_epoch,
                    expected_generation.as_deref(),
                    recover_uninitialized,
                    confirm_reset,
                )?)),
            },
        },
        Command::StatuslineCategory {
            session_id,
            client_name,
            command,
        } => match command {
            Some(StatuslineCategoryCommand::Switch { index }) => {
                let context = action_session_context(
                    runner,
                    env,
                    client_name.as_deref(),
                    session_id.as_deref(),
                )?;
                let snapshot = daemon::active_status_snapshot(
                    runner,
                    env,
                    crate::daemon::protocol::v2::StatusContext::Global,
                )?;
                crate::statusline::switch_statusline_category(
                    runner,
                    &snapshot,
                    &context.client_name,
                    &context.session_id,
                    cli_index(index)?,
                )?;
                Ok(None)
            }
            None => Ok(Some(
                daemon::statusline_session_segments(
                    runner,
                    env,
                    &config,
                    session_id
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("--session-id is required"))?,
                )?
                .category,
            )),
        },
        Command::StatuslineSessions {
            session_id,
            client_name,
            show_index,
            command,
        } => match command {
            Some(StatuslineSessionsCommand::Switch { index }) => {
                let context = action_session_context(
                    runner,
                    env,
                    client_name.as_deref(),
                    session_id.as_deref(),
                )?;
                crate::statusline::switch_statusline_session(
                    runner,
                    &context.client_name,
                    &context.session_id,
                    cli_index(index)?,
                )?;
                Ok(None)
            }
            None => {
                let mut config = config.clone();
                if show_index {
                    config.statusline.sessions.show_index = true;
                }
                Ok(Some(
                    daemon::statusline_session_segments(
                        runner,
                        env,
                        &config,
                        session_id
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("--session-id is required"))?,
                    )?
                    .sessions,
                ))
            }
        },
        Command::StatuslineWindows {
            session_id,
            command,
        } => match command {
            Some(StatuslineWindowsCommand::Switch { target }) => {
                crate::statusline::switch_statusline_window(runner, &target)?;
                Ok(None)
            }
            None => Ok(Some(
                daemon::statusline_session_segments(
                    runner,
                    env,
                    &config,
                    session_id
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("--session-id is required"))?,
                )?
                .windows,
            )),
        },
        Command::StatuslinePane { target } => Ok(Some(daemon::statusline_pane(
            runner, env, &config, &target,
        )?)),
        Command::StatuslineClick { scope, range } => {
            let needs_client = range.as_deref().map(str::trim).is_some_and(|range| {
                range.starts_with("session:")
                    || range.starts_with("c:")
                    || range.starts_with("C:")
                    || range.starts_with(crate::statusline::ATTENTION_RANGE_PREFIX)
                    || range.starts_with('$')
            });
            let context = needs_client
                .then(|| {
                    action_session_context(
                        runner,
                        env,
                        scope.client_name.as_deref(),
                        scope.session_id.as_deref(),
                    )
                })
                .transpose()?;
            let attention_click = range
                .as_deref()
                .map(str::trim)
                .is_some_and(|range| range.starts_with(crate::statusline::ATTENTION_RANGE_PREFIX));
            if attention_click {
                let context = context.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("attention click is missing an invoking tmux client")
                })?;
                let range = range.as_deref().map(str::trim).unwrap_or_default();
                let pane =
                    daemon::statusline_attention_target(runner, env, &context.session_id, range)?;
                crate::sidebar::layout::jump_to_pane_for_named_client(
                    runner,
                    &pane,
                    &context.client_name,
                )?;
                return Ok(None);
            }
            let category_click = range
                .as_deref()
                .map(str::trim)
                .is_some_and(|range| range.starts_with("c:") || range.starts_with("C:"));
            let category_snapshot = category_click
                .then(|| {
                    daemon::active_status_snapshot(
                        runner,
                        env,
                        crate::daemon::protocol::v2::StatusContext::Global,
                    )
                })
                .transpose()?;
            crate::statusline::handle_statusline_click(
                runner,
                category_snapshot.as_ref(),
                context.as_ref().map(|context| context.client_name.as_str()),
                range.as_deref(),
            )?;
            Ok(None)
        }
        Command::StatuslineSummary => Ok(Some(daemon::statusline_summary(runner, env, &config)?)),
        Command::StatuslineAttention { session_id } => Ok(Some(daemon::statusline_attention(
            runner,
            env,
            &config,
            &session_id,
        )?)),
        Command::Daemon {
            socket,
            server_identity,
            server_pid,
            server_start_time,
            tmux_server_socket,
            command,
        } => {
            if command.is_some()
                && (server_identity.is_some()
                    || server_pid.is_some()
                    || server_start_time.is_some()
                    || tmux_server_socket.is_some())
            {
                bail!(
                    "explicit server incarnation arguments are valid only for daemon foreground startup"
                );
            }
            if command.is_some() && socket.is_some() {
                bail!(
                    "InvalidRequest: --socket is internal to spawned daemon startup and cannot override the v2 incarnation namespace"
                );
            }
            match command {
                Some(DaemonCommand::Ensure) => {
                    daemon::ensure_daemon(runner, env, socket.as_deref())
                }
                Some(DaemonCommand::Start) => daemon::start_daemon(runner, env, None),
                Some(DaemonCommand::Stop { force }) => {
                    daemon::stop_daemon(runner, env, None, force)
                }
                Some(DaemonCommand::Disable) => daemon::disable_daemon(runner, env, None),
                Some(DaemonCommand::Enable) => daemon::enable_daemon(runner, env, None),
                Some(DaemonCommand::Restart) => {
                    let result = daemon::restart_daemon(runner, env, None)?;
                    let config =
                        crate::config::load::load_config_strict(env).map_err(anyhow::Error::msg)?;
                    crate::session::sync_session_category_mirrors(runner, &config, env)?;
                    Ok(result)
                }
                Some(DaemonCommand::Reload) => {
                    let result = daemon::reload_daemon(runner, env, None)?;
                    let config =
                        crate::config::load::load_config_strict(env).map_err(anyhow::Error::msg)?;
                    crate::session::sync_session_category_mirrors(runner, &config, env)?;
                    Ok(result)
                }
                Some(DaemonCommand::Status) => daemon::status_daemon(runner, env, None),
                None => daemon::run_daemon(
                    runner,
                    env,
                    socket.as_deref(),
                    server_identity.as_deref(),
                    server_pid,
                    server_start_time,
                    tmux_server_socket.as_deref(),
                ),
            }
        }
        Command::Config { command } => match command {
            ConfigCommand::Schema => daemon::config_schema(),
        },
        Command::Sidebar { command } => {
            let guarded_config = command
                .requires_active_config()
                .then(|| require_active_config(runner, env))
                .transpose()?;
            sidebar::run_sidebar_command(
                command,
                runner,
                env,
                guarded_config.as_ref().unwrap_or(&config),
            )
        }
        Command::Category { command } => {
            match command {
                CategoryCommand::Next { scope } => {
                    let context = action_session_context(
                        runner,
                        env,
                        scope.client_name.as_deref(),
                        scope.session_id.as_deref(),
                    )?;
                    let snapshot = daemon::active_status_snapshot(
                        runner,
                        env,
                        crate::daemon::protocol::v2::StatusContext::Global,
                    )?;
                    crate::statusline::cycle_statusline_category(
                        runner,
                        &snapshot,
                        &context.client_name,
                        &context.session_id,
                        Direction::Next,
                    )?;
                }
                CategoryCommand::Prev { scope } => {
                    let context = action_session_context(
                        runner,
                        env,
                        scope.client_name.as_deref(),
                        scope.session_id.as_deref(),
                    )?;
                    let snapshot = daemon::active_status_snapshot(
                        runner,
                        env,
                        crate::daemon::protocol::v2::StatusContext::Global,
                    )?;
                    crate::statusline::cycle_statusline_category(
                        runner,
                        &snapshot,
                        &context.client_name,
                        &context.session_id,
                        Direction::Previous,
                    )?;
                }
                CategoryCommand::Use { name, scope } => {
                    let context = action_session_context(
                        runner,
                        env,
                        scope.client_name.as_deref(),
                        scope.session_id.as_deref(),
                    )?;
                    let snapshot = daemon::active_status_snapshot(
                        runner,
                        env,
                        crate::daemon::protocol::v2::StatusContext::Global,
                    )?;
                    crate::session::use_category_for_client_from_status_snapshot(
                        runner,
                        &snapshot,
                        &name,
                        &context.client_name,
                    )?;
                }
                CategoryCommand::List => {
                    let config = require_active_config(runner, env)?;
                    return category::list(runner, env, &config).map(Some);
                }
                CategoryCommand::Create { name } => {
                    category::send_intent(
                        runner,
                        env,
                        crate::category::CategoryIntent::CreateCategory {
                            name: category::parse_dynamic_category(&name)?,
                        },
                    )?;
                }
                CategoryCommand::Rename { name, replacement } => {
                    category::send_intent(
                        runner,
                        env,
                        crate::category::CategoryIntent::RenameCategory {
                            current: category::parse_dynamic_category(&name)?,
                            replacement: category::parse_dynamic_category(&replacement)?,
                        },
                    )?;
                }
                CategoryCommand::Delete {
                    name,
                    move_to,
                    automatic,
                } => {
                    category::send_intent(
                        runner,
                        env,
                        crate::category::CategoryIntent::DeleteCategory {
                            category: category::parse_dynamic_category(&name)?,
                            replacement: category::delete_target(move_to.as_deref(), automatic)?,
                        },
                    )?;
                }
                CategoryCommand::Assign {
                    category: target,
                    repo,
                } => {
                    let repo = category::repo_identity(&repo)?;
                    category::send_intent(
                        runner,
                        env,
                        crate::category::CategoryIntent::AssignRepo {
                            repo: repo.key,
                            category: category::parse_category(&target)?,
                        },
                    )?;
                }
                CategoryCommand::Automatic { repo } => {
                    let repo = category::repo_identity(&repo)?;
                    category::send_intent(
                        runner,
                        env,
                        crate::category::CategoryIntent::SetRepoAutomatic { repo: repo.key },
                    )?;
                }
            }
            Ok(None)
        }
        Command::SessionCycle { command } => {
            match command {
                SessionCycleCommand::Next { scope } => {
                    let context = action_session_context(
                        runner,
                        env,
                        scope.client_name.as_deref(),
                        scope.session_id.as_deref(),
                    )?;
                    crate::statusline::cycle_statusline_session(
                        runner,
                        &context.client_name,
                        &context.session_id,
                        Direction::Next,
                    )?;
                }
                SessionCycleCommand::Prev { scope } => {
                    let context = action_session_context(
                        runner,
                        env,
                        scope.client_name.as_deref(),
                        scope.session_id.as_deref(),
                    )?;
                    crate::statusline::cycle_statusline_session(
                        runner,
                        &context.client_name,
                        &context.session_id,
                        Direction::Previous,
                    )?;
                }
            }
            Ok(None)
        }
        Command::PaneSwitch {
            direction,
            pane_id,
            pane_pid,
            daemon_socket,
            server_identity,
        } => {
            pane_switch::request(
                std::path::Path::new(&daemon_socket),
                &server_identity,
                direction,
                pane_id,
                pane_pid,
            )?;
            Ok(None)
        }
        Command::Session { command } => {
            match command {
                SessionCommand::New { cwd, scope } => {
                    let context = action_session_context(
                        runner,
                        env,
                        scope.client_name.as_deref(),
                        scope.session_id.as_deref(),
                    )?;
                    let config = require_active_config(runner, env)?;
                    crate::session::create_session_for_client(
                        runner,
                        &config,
                        env,
                        cwd.as_deref(),
                        &context.client_name,
                    )?;
                    let _ = request_canonical_topology_refresh(runner, env);
                }
            }
            Ok(None)
        }
        Command::Hooks { command } => {
            match command {
                HooksCommand::OnClientSessionChanged {
                    client_pid,
                    session_name,
                } => {
                    if client_pid.is_some() != session_name.is_some() {
                        bail!("client_pid and session_name must be provided together");
                    }
                    let config = require_active_config(runner, env)?;
                    crate::session::on_client_session_changed(
                        runner,
                        &config,
                        env,
                        client_pid,
                        session_name.as_deref(),
                    )?;
                }
                HooksCommand::PaneStateView {
                    event_kind,
                    owner,
                    protocol,
                    hook_session,
                    hook_window,
                    snapshot_session,
                    snapshot_window,
                    snapshot_pane,
                    snapshot_pane_pid,
                    snapshot_panes,
                    snapshot_clients,
                } => hook::run_view_hook_command(
                    &event_kind,
                    &owner,
                    protocol,
                    hook_session.as_deref(),
                    hook_window.as_deref(),
                    &snapshot_session,
                    &snapshot_window,
                    &snapshot_pane,
                    &snapshot_pane_pid,
                    &snapshot_panes,
                    &snapshot_clients,
                    runner,
                    env,
                )?,
            }
            Ok(None)
        }
        Command::Project { command } => {
            match command {
                ProjectCommand::Switch { path } => {
                    let config = require_active_config(runner, env)?;
                    crate::project::switch_project(runner, &config, env, &path)?;
                }
                ProjectCommand::Selector { popup } => {
                    if popup {
                        crate::project::open_project_selector_popup(
                            runner,
                            &config.popup,
                            &std::env::current_exe()?.display().to_string(),
                        )?;
                    } else {
                        let config = require_active_config(runner, env)?;
                        crate::project::run_project_selector(runner, &config, env)?;
                    }
                }
            }
            Ok(None)
        }
        Command::SessionManager {
            popup,
            render_preview,
            preview_name,
            command,
        } => {
            if render_preview.is_some() != preview_name.is_some() {
                bail!("--render-preview and --preview-name must be provided together");
            }
            if let (Some(action), Some(name)) = (render_preview.as_deref(), preview_name.as_deref())
            {
                return Ok(Some(crate::session_manager::render_preview(
                    runner, action, name, env,
                )?));
            }
            let outcome = match (popup, command) {
                (true, None) => crate::session_manager::run_interactive(runner, env)?,
                (false, Some(SessionManagerCommand::KillWindow { target })) => {
                    crate::session_manager::kill_window(runner, &target)?;
                    crate::session_manager::SessionManagerOutcome::Done
                }
                (false, Some(SessionManagerCommand::KillPane { target })) => {
                    crate::session_manager::kill_pane(runner, &target)?;
                    crate::session_manager::SessionManagerOutcome::Done
                }
                (true, Some(SessionManagerCommand::KillWindow { target })) => {
                    crate::session_manager::kill_window(runner, &target)?;
                    crate::session_manager::SessionManagerOutcome::Done
                }
                (true, Some(SessionManagerCommand::KillPane { target })) => {
                    crate::session_manager::kill_pane(runner, &target)?;
                    crate::session_manager::SessionManagerOutcome::Done
                }
                (false, None) => {
                    if should_wrap_session_manager_in_popup(env) {
                        crate::session_manager::open_popup(
                            runner,
                            &config.popup,
                            &std::env::current_exe()?.display().to_string(),
                        )?;
                        crate::session_manager::SessionManagerOutcome::Done
                    } else {
                        crate::session_manager::run_interactive_outside_tmux(runner, env)?
                    }
                }
            };
            if outcome == crate::session_manager::SessionManagerOutcome::KillServer {
                let mut ops = crate::session_manager::kill_server::SystemKillServerOps::new(
                    runner,
                    |incarnation| daemon::disable_daemon_for_server(runner, env, incarnation),
                );
                crate::session_manager::kill_server::clean_kill_server(
                    &mut ops,
                    &config.session_manager.kill,
                )?;
            }
            Ok(None)
        }
        Command::Hook { command } => {
            hook::run_hook_command(
                command,
                input,
                runner,
                env,
                now_epoch,
                hook_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(2)),
            )?;
            Ok(None)
        }
    }
}

fn require_active_config(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<crate::config::Config> {
    let config = crate::config::load::load_config_strict(env).map_err(|error| {
        anyhow::anyhow!(
            "config-dependent operation refused: {error}; fix the config and run `vt daemon reload`"
        )
    })?;
    let disk_hash = crate::daemon::lifecycle::config_hash(&config);
    let active_hash = query_active_config_hash(runner, env).map_err(|error| {
        anyhow::anyhow!(
            "config-dependent operation refused: daemon active config is unavailable ({error:#}); run `vt daemon reload`"
        )
    })?;
    verify_active_config_hash(&disk_hash, &active_hash)?;
    Ok(config)
}

fn verify_active_config_hash(disk_hash: &str, active_hash: &str) -> Result<()> {
    if active_hash.trim().is_empty() {
        bail!(
            "config-dependent operation refused: daemon active config hash is empty; run `vt daemon reload`"
        );
    }
    if active_hash != disk_hash {
        bail!(
            "config-dependent operation refused: disk config does not match the daemon active config; run `vt daemon reload`"
        );
    }
    Ok(())
}

fn query_active_config_hash(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    let socket = crate::daemon::daemon_socket_path_for_incarnation(env, None, &incarnation.hash);
    let mut client = crate::daemon::protocol::v2::V2Client::connect_with_timeout(
        &socket,
        &incarnation.hash,
        Duration::from_millis(500),
    )?;
    if client.phase() != crate::daemon::protocol::v2::DaemonPhase::Serving {
        bail!("daemon is not serving active configuration queries");
    }
    match client.request(
        &crate::daemon::protocol::v2::ClientMessage::QueryRuntimeInfo {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
        },
    )? {
        crate::daemon::protocol::v2::ServerMessage::RuntimeInfoResult { info } => {
            Ok(info.config_hash)
        }
        crate::daemon::protocol::v2::ServerMessage::Error { code, message, .. } => {
            bail!("daemon query failed ({code:?}): {message}")
        }
        other => bail!("unexpected daemon health response: {other:?}"),
    }
}

fn reset_agent_storage_offline(
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
                .map_err(agent_storage_reset_error)?,
        )
    } else {
        let expected_generation = expected_generation.expect("reset mode checked above");
        let generation =
            crate::agent_state::AgentStateStore::reset_offline(root, &expected_generation)
                .map_err(agent_storage_reset_error)?;
        (expected_generation.as_str().to_string(), generation)
    };
    crate::api::agent_storage_reset_result(
        observed_at,
        incarnation.hash,
        previous_generation,
        generation.as_str().to_string(),
    )
}

fn agent_storage_reset_error(error: crate::agent_state::StoreError) -> crate::api::ApiError {
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

fn should_wrap_session_manager_in_popup(env: &BTreeMap<String, String>) -> bool {
    env.get("TMUX")
        .or_else(|| env.get("TMUX_PANE"))
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

fn emit_config_warnings<W: Write>(warnings: &[String], writer: &mut W) -> Result<()> {
    for warning in warnings {
        writeln!(writer, "vde-tmux config warning: {warning}")?;
    }
    Ok(())
}

fn request_canonical_topology_refresh(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    let (incarnation, socket) =
        crate::daemon::lifecycle::ensure_daemon_serving_v2(runner, env, None)?;
    crate::sidebar::client::request_topology_refresh_v2(&socket, &incarnation.hash)
}

fn action_session_context(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    requested_client_name: Option<&str>,
    requested_session_id: Option<&str>,
) -> Result<crate::session::ClientSessionContext> {
    let context = match requested_client_name {
        Some(client_name) => {
            crate::session::client_session_context_for_client(runner, client_name)?
        }
        None => {
            let pane_id = env
                .get("TMUX_PANE")
                .map(String::as_str)
                .filter(|pane_id| !pane_id.trim().is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("TMUX_PANE is required when --client-name is omitted")
                })?;
            crate::session::client_session_context_for_pane(runner, pane_id, None)?
        }
    };
    if let Some(requested) = requested_session_id
        && requested != context.session_id
    {
        anyhow::bail!(
            "requested source session {requested} does not match invoking client {} session {}",
            context.client_name,
            context.session_id
        );
    }
    Ok(context)
}

fn cli_index(index: usize) -> Result<usize> {
    index
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("statusline index must be 1 or greater"))
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
