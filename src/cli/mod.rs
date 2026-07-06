use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        #[command(subcommand)]
        command: Option<StatuslineCategoryCommand>,
    },
    #[command(name = "statusline-summary")]
    StatuslineSummary,
    #[command(name = "statusline-attention")]
    StatuslineAttention,
    #[command(name = "statusline-sessions")]
    StatuslineSessions {
        #[arg(long = "show-index")]
        show_index: bool,
        #[command(subcommand)]
        command: Option<StatuslineSessionsCommand>,
    },
    Daemon {
        #[arg(long)]
        socket: Option<String>,
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
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    #[command(name = "session-manager")]
    SessionManager {
        #[arg(long)]
        popup: bool,
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
enum ConfigCommand {
    Schema,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    Stop {
        #[arg(long)]
        socket: Option<String>,
    },
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
    #[command(name = "set-category")]
    SetCategory { session: String, category: String },
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    #[command(name = "refresh-category")]
    RefreshCategory,
}

#[derive(Debug, Subcommand)]
enum HooksCommand {
    #[command(name = "on-client-session-changed")]
    OnClientSessionChanged {
        client_name: Option<String>,
        session_name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    Switch { path: String },
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
    let is_hook = args.get(1).and_then(|arg| arg.to_str()) == Some("hook");
    let timeout = if is_hook {
        Duration::from_millis(300)
    } else {
        Duration::from_secs(3)
    };
    let mut input = String::new();
    if is_hook {
        let _ = std::io::stdin().read_to_string(&mut input);
    }
    let runner = SystemTmuxRunner::from_env(timeout);
    match run_with_input_at(args, &input, &runner, &current_env(), now_epoch()) {
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
    let cli = Cli::try_parse_from(args)?;
    let loaded = load_config(env);
    emit_config_warnings(&loaded.warnings, warning_writer)?;
    let config = loaded.config;
    match cli.command {
        Command::StatuslineCategory { command } => match command {
            Some(StatuslineCategoryCommand::Switch { index }) => {
                crate::statusline::switch_statusline_category(runner, &config, cli_index(index))?;
                Ok(None)
            }
            None => Ok(Some(crate::statusline::statusline_category(
                runner, &config,
            )?)),
        },
        Command::StatuslineSessions {
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
                Ok(Some(crate::statusline::statusline_sessions(
                    runner, &config,
                )?))
            }
        },
        Command::StatuslineSummary => Ok(Some(daemon::statusline_summary(runner, env, &config)?)),
        Command::StatuslineAttention => {
            Ok(Some(daemon::statusline_attention(runner, env, &config)?))
        }
        Command::Daemon { socket, command } => match command {
            Some(DaemonCommand::Stop {
                socket: stop_socket,
            }) => daemon::stop_daemon(env, stop_socket.as_deref().or(socket.as_deref())),
            None => daemon::run_daemon(runner, env, socket.as_deref()),
        },
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
                }
            }
            Ok(None)
        }
        Command::Project { command } => {
            match command {
                ProjectCommand::Switch { path } => {
                    crate::project::switch_project(runner, &config, &path)?;
                }
            }
            Ok(None)
        }
        Command::SessionManager { popup, command } => {
            match (popup, command) {
                (true, _) => crate::session_manager::open_popup(runner)?,
                (false, Some(SessionManagerCommand::KillWindow { target })) => {
                    crate::session_manager::kill_window(runner, &target)?;
                }
                (false, Some(SessionManagerCommand::KillPane { target })) => {
                    crate::session_manager::kill_pane(runner, &target)?;
                }
                (false, None) => crate::session_manager::open_tree(runner)?,
            }
            Ok(None)
        }
        Command::Hook { command } => {
            hook::run_hook_command(command, input, runner, env, now_epoch)?;
            Ok(None)
        }
    }
}

fn emit_config_warnings<W: Write>(warnings: &[String], writer: &mut W) -> Result<()> {
    for warning in warnings {
        writeln!(writer, "vde-tmux config warning: {warning}")?;
    }
    Ok(())
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
