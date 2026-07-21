use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::config::load::load_config;
use crate::session::Direction;
use crate::tmux::{SystemTmuxRunner, TmuxRunner};

mod daemon;
mod hook;
mod sidebar;

/// vde-tmux CLI。
#[derive(Debug, Parser)]
#[command(version, about = "tmux state & UI manager", subcommand_required = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
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
        None => String::new(),
    };
    let runner = SystemTmuxRunner::from_env(timeout);
    match run_with_input_at_with_hook_deadline(
        args,
        &input,
        &runner,
        &current_env(),
        now_epoch(),
        agent_hook_deadline,
    ) {
        Ok(Some(output)) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
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
    emit_config_warnings(&loaded.warnings, warning_writer)?;
    let config = loaded.config;
    match cli.command {
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
                let config = require_active_config(runner, env)?;
                crate::statusline::switch_statusline_category(
                    runner,
                    &config,
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
            let guarded_config = category_click
                .then(|| require_active_config(runner, env))
                .transpose()?;
            crate::statusline::handle_statusline_click(
                runner,
                guarded_config.as_ref().unwrap_or(&config),
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
                    crate::session::sync_session_category_mirrors(runner, &config)?;
                    Ok(result)
                }
                Some(DaemonCommand::Reload) => {
                    let result = daemon::reload_daemon(runner, env, None)?;
                    let config =
                        crate::config::load::load_config_strict(env).map_err(anyhow::Error::msg)?;
                    crate::session::sync_session_category_mirrors(runner, &config)?;
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
                    let config = require_active_config(runner, env)?;
                    crate::statusline::cycle_statusline_category(
                        runner,
                        &config,
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
                    let config = require_active_config(runner, env)?;
                    crate::statusline::cycle_statusline_category(
                        runner,
                        &config,
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
                    let config = require_active_config(runner, env)?;
                    crate::session::use_category_for_client(
                        runner,
                        &config,
                        &name,
                        &context.client_name,
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
                    crate::project::switch_project(runner, &config, &path)?;
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
