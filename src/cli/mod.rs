use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

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
    StatuslineClick { range: Option<String> },
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
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    Hooks {
        #[command(subcommand)]
        command: HooksCommand,
    },
    #[command(name = "pane-state")]
    PaneState {
        #[command(subcommand)]
        command: PaneStateCommand,
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
    Ensure,
    Stop,
    Restart,
}

#[derive(Debug, Subcommand)]
enum CategoryCommand {
    Next,
    Prev,
    Use { name: String },
}

#[derive(Debug, Subcommand)]
enum SessionCycleCommand {
    Next,
    Prev,
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    New {
        #[arg(short = 'c', long = "cwd")]
        cwd: Option<String>,
    },
    #[command(name = "set-category")]
    SetCategory { session: String, category: String },
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    #[command(name = "refresh-category")]
    RefreshCategory,
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)] // Hidden view hook mirrors its bounded inline snapshot frame.
enum HooksCommand {
    #[command(name = "on-client-session-changed")]
    OnClientSessionChanged {
        client_name: Option<String>,
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
        #[arg(long = "hook-client")]
        hook_client: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum PaneStateCommand {
    #[command(name = "cleanup-legacy")]
    CleanupLegacy {
        #[arg(long, required = true)]
        all: bool,
    },
    Reset {
        #[arg(long)]
        target: String,
    },
    Hooks {
        #[command(subcommand)]
        command: PaneStateHooksCommand,
    },
}

#[derive(Debug, Subcommand)]
enum PaneStateHooksCommand {
    Uninstall,
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
    let agent_hook_deadline = is_agent_hook.then(|| Instant::now() + Duration::from_secs(2));
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
        if !wait_for_input(input.as_raw_fd(), deadline)? {
            return String::from_utf8(bytes)
                .map_err(|error| anyhow::anyhow!("agent hook stdin is not UTF-8: {error}"));
        }
        let mut chunk = [0_u8; 8192];
        let read = input.read(&mut chunk)?;
        if read == 0 {
            return String::from_utf8(bytes)
                .map_err(|error| anyhow::anyhow!("agent hook stdin is not UTF-8: {error}"));
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

fn wait_for_input(fd: RawFd, deadline: Instant) -> Result<bool> {
    if fd < 0 {
        return Ok(false);
    }
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            bail!("agent hook 2s deadline exceeded while reading stdin");
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
            bail!("agent hook 2s deadline exceeded while reading stdin");
        }
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if poll_fd.revents & libc::POLLNVAL != 0 {
            return Ok(false);
        }
        return Ok(true);
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
            command,
        } => match command {
            Some(StatuslineCategoryCommand::Switch { index }) => {
                crate::statusline::switch_statusline_category(runner, &config, cli_index(index))?;
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
            show_index,
            command,
        } => match command {
            Some(StatuslineSessionsCommand::Switch { index }) => {
                crate::statusline::switch_statusline_session(runner, &config, cli_index(index))?;
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
        Command::StatuslineClick { range } => {
            crate::statusline::handle_statusline_click(runner, &config, range.as_deref())?;
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
                Some(DaemonCommand::Stop) => daemon::stop_daemon(runner, env, None),
                Some(DaemonCommand::Restart) => daemon::restart_daemon(runner, env, None),
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
        Command::Sidebar { command } => sidebar::run_sidebar_command(command, runner, env, &config),
        Command::Category { command } => {
            match command {
                CategoryCommand::Next => {
                    crate::session::use_adjacent_category(runner, &config, Direction::Next)?;
                }
                CategoryCommand::Prev => {
                    crate::session::use_adjacent_category(runner, &config, Direction::Previous)?;
                }
                CategoryCommand::Use { name } => {
                    crate::session::use_category(runner, &config, &name)?;
                }
            }
            Ok(None)
        }
        Command::SessionCycle { command } => {
            match command {
                SessionCycleCommand::Next => {
                    crate::session::cycle_session(runner, &config, Direction::Next)?;
                }
                SessionCycleCommand::Prev => {
                    crate::session::cycle_session(runner, &config, Direction::Previous)?;
                }
            }
            Ok(None)
        }
        Command::Session { command } => {
            match command {
                SessionCommand::New { cwd } => {
                    crate::session::create_session(runner, &config, env, cwd.as_deref())?;
                    let _ = request_canonical_topology_refresh(runner, env);
                }
                SessionCommand::SetCategory { session, category } => {
                    crate::session::set_session_category_override(runner, &session, &category)?;
                }
            }
            Ok(None)
        }
        Command::Sessions { command } => {
            match command {
                SessionsCommand::RefreshCategory => {
                    crate::session::refresh_session_categories(runner, &config)?;
                }
            }
            Ok(None)
        }
        Command::Hooks { command } => {
            match command {
                HooksCommand::OnClientSessionChanged {
                    client_name,
                    session_name,
                } => {
                    if client_name.is_some() != session_name.is_some() {
                        bail!("client_name and session_name must be provided together");
                    }
                    crate::session::on_client_session_changed(
                        runner,
                        &config,
                        client_name.as_deref(),
                        session_name.as_deref(),
                    )?;
                    let _ = request_canonical_topology_refresh(runner, env);
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
                    hook_client,
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
                    hook_client.as_deref(),
                    runner,
                    env,
                    &config,
                )?,
            }
            Ok(None)
        }
        Command::PaneState { command } => match command {
            PaneStateCommand::CleanupLegacy { all } => {
                if !all {
                    bail!("--all is required");
                }
                daemon::cleanup_legacy_state(runner, env)
            }
            PaneStateCommand::Reset { target } => daemon::reset_pane_state(runner, env, &target),
            PaneStateCommand::Hooks {
                command: PaneStateHooksCommand::Uninstall,
            } => daemon::uninstall_pane_state_hooks(runner, env),
        },
        Command::Project { command } => {
            match command {
                ProjectCommand::Switch { path } => {
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
            match (popup, command) {
                (true, None) => crate::session_manager::run_interactive(runner)?,
                (false, Some(SessionManagerCommand::KillWindow { target })) => {
                    crate::session_manager::kill_window(runner, &target)?;
                }
                (false, Some(SessionManagerCommand::KillPane { target })) => {
                    crate::session_manager::kill_pane(runner, &target)?;
                }
                (true, Some(SessionManagerCommand::KillWindow { target })) => {
                    crate::session_manager::kill_window(runner, &target)?;
                }
                (true, Some(SessionManagerCommand::KillPane { target })) => {
                    crate::session_manager::kill_pane(runner, &target)?;
                }
                (false, None) => {
                    if should_wrap_session_manager_in_popup(env) {
                        crate::session_manager::open_popup(
                            runner,
                            &config.popup,
                            &std::env::current_exe()?.display().to_string(),
                        )?;
                    } else {
                        crate::session_manager::run_interactive_outside_tmux(runner, env)?;
                    }
                }
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

fn cli_index(index: usize) -> usize {
    index.saturating_sub(1)
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
