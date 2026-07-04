use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use crate::config::load::load_config;
use crate::hook::adapter::{
    claude_event_from_json, codex_event_from_json, codex_notify_event_from_arg,
};
use crate::hook::writer::{
    ClaudeProgressEvent, apply_claude_progress_event, resolve_pane, write_pane_options,
};
use crate::hook::{
    AgentEvent, AgentStatus, OptionUpdate, SubagentEntry, TaskProgress, derive_event_writes,
};
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
    #[command(name = "statusline-agent-badge")]
    StatuslineAgentBadge,
    #[command(name = "statusline-sessions")]
    StatuslineSessions {
        #[command(subcommand)]
        command: Option<StatuslineSessionsCommand>,
    },
    Daemon {
        #[arg(long)]
        socket: Option<String>,
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
        command: HookCommand,
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

#[derive(Debug, Subcommand)]
enum HookCommand {
    Emit {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long = "prompt-source")]
        prompt_source: Option<String>,
        #[arg(long = "wait-reason")]
        wait_reason: Option<String>,
        #[arg(long)]
        attention: bool,
        #[arg(long = "started-at")]
        started_at: Option<i64>,
        #[arg(long = "completed-at")]
        completed_at: Option<i64>,
        #[arg(long)]
        tasks: Option<String>,
        #[arg(long)]
        subagents: Option<String>,
    },
    Claude {
        event: String,
    },
    Codex {
        arg: Option<String>,
    },
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
        Command::StatuslineAgentBadge => {
            Ok(Some(crate::daemon::statusline_agent_badge(runner, env)?))
        }
        Command::Daemon { socket } => {
            let socket_path = crate::daemon::daemon_socket_path(env, socket.as_deref());
            crate::daemon::server::run_daemon_server(runner, &socket_path)?;
            Ok(None)
        }
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
            run_hook_command(command, input, runner, env, now_epoch)?;
            Ok(None)
        }
    }
}

fn cli_index(index: usize) -> usize {
    index.saturating_sub(1)
}

fn run_hook_command(
    command: HookCommand,
    input: &str,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    now_epoch: i64,
) -> Result<()> {
    match command {
        HookCommand::Emit {
            agent,
            status,
            prompt,
            prompt_source,
            wait_reason,
            attention,
            started_at,
            completed_at,
            tasks,
            subagents,
        } => {
            let event = AgentEvent {
                agent,
                status: status.as_deref().map(parse_agent_status).transpose()?,
                prompt: prompt.map(OptionUpdate::Set),
                prompt_source: prompt_source.map(OptionUpdate::Set),
                wait_reason: wait_reason.map(OptionUpdate::Set),
                attention: attention.then_some(true),
                started_at,
                completed_at,
                tasks: tasks
                    .as_deref()
                    .map(parse_task_progress)
                    .transpose()?
                    .map(OptionUpdate::Set),
                subagents: subagents
                    .as_deref()
                    .map(parse_subagents_arg)
                    .transpose()?
                    .map(OptionUpdate::Set),
            };
            write_agent_event(runner, env, &event)
        }
        HookCommand::Claude { event } => {
            if let Some(progress_event) = claude_progress_event_from_input(&event, input)? {
                if let Some(pane) = resolve_pane(runner, env)? {
                    apply_claude_progress_event(runner, &pane, progress_event)?;
                }
                return Ok(());
            }
            let event = claude_event_from_json(&event, input, now_epoch)?;
            write_agent_event(runner, env, &event)
        }
        HookCommand::Codex { arg } => {
            let Some(arg) = arg else {
                return Ok(());
            };
            let event = if arg.trim_start().starts_with('{') {
                codex_notify_event_from_arg(&arg, now_epoch)?
            } else {
                codex_event_from_json(&arg, input, now_epoch)?
            };
            write_agent_event(runner, env, &event)
        }
    }
}

fn write_agent_event(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    event: &AgentEvent,
) -> Result<()> {
    let writes = derive_event_writes(event);
    if writes.is_empty() {
        return Ok(());
    }
    if let Some(pane) = resolve_pane(runner, env)? {
        write_pane_options(runner, &pane, &writes)?;
    }
    Ok(())
}

fn parse_agent_status(raw: &str) -> Result<AgentStatus> {
    match raw {
        "running" => Ok(AgentStatus::Running),
        "waiting" => Ok(AgentStatus::Waiting),
        "idle" => Ok(AgentStatus::Idle),
        "error" => Ok(AgentStatus::Error),
        _ => bail!("unknown hook status: {raw}"),
    }
}

fn parse_task_progress(raw: &str) -> Result<TaskProgress> {
    let Some((done, total)) = raw.split_once('/') else {
        bail!("tasks must be done/total: {raw}");
    };
    Ok(TaskProgress {
        done: done.parse()?,
        total: total.parse()?,
    })
}

fn parse_subagents_arg(raw: &str) -> Result<Vec<SubagentEntry>> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    raw.split('|')
        .map(|entry| {
            let Some((agent_id, agent_type)) = entry.split_once(':') else {
                bail!("subagents must be id:type entries: {raw}");
            };
            Ok(SubagentEntry {
                agent_id: agent_id.to_string(),
                agent_type: agent_type.to_string(),
            })
        })
        .collect()
}

fn claude_progress_event_from_input(
    event: &str,
    input: &str,
) -> Result<Option<ClaudeProgressEvent>> {
    #[derive(serde::Deserialize, Default)]
    struct Payload {
        agent_id: Option<String>,
        agent_type: Option<String>,
    }

    let progress = match event {
        "TaskCreated" => ClaudeProgressEvent::TaskCreated,
        "TaskCompleted" => ClaudeProgressEvent::TaskCompleted,
        "SubagentStart" => {
            let payload: Payload = serde_json::from_str(input.trim()).unwrap_or_default();
            let Some(agent_id) = payload.agent_id else {
                return Ok(None);
            };
            ClaudeProgressEvent::SubagentStart(SubagentEntry {
                agent_id,
                agent_type: payload.agent_type.unwrap_or_else(|| "agent".to_string()),
            })
        }
        "SubagentStop" => {
            let payload: Payload = serde_json::from_str(input.trim()).unwrap_or_default();
            let Some(agent_id) = payload.agent_id else {
                return Ok(None);
            };
            ClaudeProgressEvent::SubagentStop { agent_id }
        }
        _ => return Ok(None),
    };
    Ok(Some(progress))
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
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

    #[test]
    fn dispatch_hook_emit_writes_pane_options() {
        let mock = MockTmuxRunner::new();
        let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_STATUS,
                "running",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-u",
                "-t",
                "%1",
                crate::options::KEY_WAIT_REASON,
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_AGENT,
                "codex",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_PROMPT,
                "hello",
            ],
            "",
        );
        run_with_input_at(
            [
                "vt", "hook", "emit", "--agent", "codex", "--status", "running", "--prompt",
                "hello",
            ],
            "",
            &mock,
            &env,
            123,
        )
        .unwrap();
        assert_eq!(mock.calls().len(), 4);
    }

    #[test]
    fn dispatch_hook_claude_reads_stdin_json() {
        let mock = MockTmuxRunner::new();
        let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_STATUS,
                "running",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-u",
                "-t",
                "%1",
                crate::options::KEY_WAIT_REASON,
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_AGENT,
                "claude",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_STARTED_AT,
                "123",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_PROMPT,
                "hello",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_PROMPT_SOURCE,
                "user",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-u",
                "-t",
                "%1",
                crate::options::KEY_TASKS,
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-u",
                "-t",
                "%1",
                crate::options::KEY_SUBAGENTS,
            ],
            "",
        );
        run_with_input_at(
            ["vt", "hook", "claude", "UserPromptSubmit"],
            r#"{"prompt":"hello"}"#,
            &mock,
            &env,
            123,
        )
        .unwrap();
        assert_eq!(mock.calls().len(), 8);
    }

    #[test]
    fn dispatch_hook_codex_event_reads_stdin_json() {
        let mock = MockTmuxRunner::new();
        let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_STATUS,
                "waiting",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_WAIT_REASON,
                "permission_prompt",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_AGENT,
                "codex",
            ],
            "",
        );
        run_with_input_at(
            ["vt", "hook", "codex", "PermissionRequest"],
            "{}",
            &mock,
            &env,
            123,
        )
        .unwrap();
        assert_eq!(mock.calls().len(), 3);
    }

    #[test]
    fn dispatch_hook_codex_notify_reads_argv_json() {
        let mock = MockTmuxRunner::new();
        let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_STATUS,
                "idle",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-u",
                "-t",
                "%1",
                crate::options::KEY_WAIT_REASON,
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_AGENT,
                "codex",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_ATTENTION,
                "1",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_COMPLETED_AT,
                "456",
            ],
            "",
        );
        run_with_input_at(
            ["vt", "hook", "codex", r#"{"type":"agent-turn-complete"}"#],
            "",
            &mock,
            &env,
            456,
        )
        .unwrap();
        assert_eq!(mock.calls().len(), 5);
    }

    #[test]
    fn dispatch_hook_claude_task_created_updates_progress() {
        let mock = MockTmuxRunner::new();
        let env = BTreeMap::from([("TMUX_PANE".to_string(), "%1".to_string())]);
        mock.stub(&["show-options", "-p", "-t", "%1"], "@vde_tasks \"0/0\"\n");
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                crate::options::KEY_TASKS,
                "0/1",
            ],
            "",
        );
        run_with_input_at(
            ["vt", "hook", "claude", "TaskCreated"],
            "{}",
            &mock,
            &env,
            456,
        )
        .unwrap();
        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn dispatch_statusline_agent_badge_falls_back_to_tmux_snapshot() {
        let mock = MockTmuxRunner::new();
        let format = crate::options::snapshot::snapshot_format();
        let line = [
            "main", "@1", "%1", "/tmp", "zsh", "", "codex", "running", "", "", "", "", "", "", "",
            "",
        ]
        .join("\u{1f}");
        mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));
        let output = run_with(["vt", "statusline-agent-badge"], &mock, &env()).unwrap();
        assert_eq!(output, Some("running:1".to_string()));
    }
}
