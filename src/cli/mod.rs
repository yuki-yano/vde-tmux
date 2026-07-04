use std::collections::BTreeMap;
use std::ffi::OsString;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use crate::config::load::load_config;
use crate::session::Direction;
use crate::tmux::{SystemTmuxRunner, TmuxRunner};

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
    #[command(name = "statusline-sessions")]
    StatuslineSessions {
        #[command(subcommand)]
        command: Option<StatuslineSessionsCommand>,
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
    let runner = SystemTmuxRunner::from_env(Duration::from_secs(3));
    match run_with(std::env::args_os(), &runner, &current_env()) {
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
    let cli = Cli::try_parse_from(args)?;
    let loaded = load_config(env);
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
        Command::StatuslineSessions { command } => match command {
            Some(StatuslineSessionsCommand::Switch { index }) => {
                crate::statusline::switch_statusline_session(runner, &config, cli_index(index))?;
                Ok(None)
            }
            None => Ok(Some(crate::statusline::statusline_sessions(
                runner, &config,
            )?)),
        },
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
    }
}

fn cli_index(index: usize) -> usize {
    index.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::mock::MockTmuxRunner;
    use std::collections::BTreeMap;

    fn env() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn dispatch_statusline_sessions_prints_output() {
        let mock = MockTmuxRunner::new();
        let format = crate::session::session_list_format();
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\n",
        );
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        let output = run_with(["vt", "statusline-sessions"], &mock, &env()).unwrap();
        assert!(output.unwrap().contains("main"));
    }

    #[test]
    fn dispatch_category_use_switches_category() {
        let mock = MockTmuxRunner::new();
        let format = crate::session::session_list_format();
        mock.stub(&["display-message", "-p", "#{client_name}"], "abc\n");
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\n",
        );
        mock.stub(&["show-option", "-gqv", "@vde_client_616263_work"], "");
        mock.stub(&["switch-client", "-t", "main"], "");
        mock.stub(&["set-option", "-g", "@vde_client_616263_work", "main"], "");
        run_with(["vt", "category", "use", "work"], &mock, &env()).unwrap();
        assert_eq!(mock.calls().len(), 5);
    }
}
