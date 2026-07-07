use std::collections::BTreeMap;

use anyhow::{Result, bail};
use clap::Subcommand;

use crate::hook::adapter::{
    claude_event_from_json, codex_event_from_json, codex_notify_event_from_arg,
};
use crate::hook::writer::{
    ClaudeProgressEvent, apply_claude_progress_event, resolve_pane, write_pane_options,
};
use crate::hook::{
    AgentEvent, AgentStatus, OptionUpdate, SubagentEntry, TaskProgress, derive_event_writes,
};
use crate::tmux::TmuxRunner;

#[derive(Debug, Subcommand)]
pub(crate) enum HookCommand {
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

pub(crate) fn run_hook_command(
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
                clear_state: false,
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
        hook_event_name: Option<String>,
        agent_id: Option<String>,
        agent_type: Option<String>,
    }

    let payload: Payload = serde_json::from_str(input.trim()).unwrap_or_default();
    let event = payload.hook_event_name.as_deref().unwrap_or(event);
    let progress = match event {
        "TaskCreated" => ClaudeProgressEvent::TaskCreated,
        "TaskCompleted" => ClaudeProgressEvent::TaskCompleted,
        "SubagentStart" => {
            let Some(agent_id) = payload.agent_id else {
                return Ok(None);
            };
            ClaudeProgressEvent::SubagentStart(SubagentEntry {
                agent_id,
                agent_type: payload.agent_type.unwrap_or_else(|| "agent".to_string()),
            })
        }
        "SubagentStop" => {
            let Some(agent_id) = payload.agent_id else {
                return Ok(None);
            };
            ClaudeProgressEvent::SubagentStop { agent_id }
        }
        _ => return Ok(None),
    };
    Ok(Some(progress))
}
