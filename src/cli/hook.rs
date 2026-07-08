use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::Value;

use crate::hook::adapter::{
    claude_event_from_json, codex_event_from_json_with_home, codex_notify_event_from_arg,
};
use crate::hook::origin::{
    HookOrigin, claude_hook_origin, codex_hook_origin, find_codex_session_file,
};
use crate::hook::writer::{ProgressEvent, apply_progress_event, resolve_pane, write_pane_options};
use crate::hook::{
    AgentEvent, AgentStatus, OptionUpdate, SubagentEntry, TaskItem, TaskItemStatus, TaskProgress,
    WorktreeActivity, WorktreeActivityKind, derive_event_writes,
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
                task_items: None,
                subagents: subagents
                    .as_deref()
                    .map(parse_subagents_arg)
                    .transpose()?
                    .map(OptionUpdate::Set),
                worktree_activity: None,
            };
            write_agent_event(runner, env, &event)
        }
        HookCommand::Claude { event } => {
            if let Some(progress_event) = claude_progress_event_from_input(&event, input)? {
                if let Some(pane) = resolve_pane(runner, env)? {
                    apply_progress_event(runner, &pane, progress_event)?;
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
            let codex_home = codex_home_from_env(env);
            if arg.trim_start().starts_with('{') {
                let event = codex_notify_event_from_arg(&arg, now_epoch)?;
                return write_agent_event(runner, env, &event);
            }
            if let Some(progress_event) =
                codex_aux_event_from_input(&arg, input, now_epoch, codex_home.as_deref())?
            {
                if let Some(pane) = resolve_pane(runner, env)? {
                    apply_progress_event(runner, &pane, progress_event)?;
                }
                return Ok(());
            }
            let event =
                codex_event_from_json_with_home(&arg, input, now_epoch, codex_home.as_deref())?;
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
                display_name: None,
            })
        })
        .collect()
}

fn claude_progress_event_from_input(event: &str, input: &str) -> Result<Option<ProgressEvent>> {
    #[derive(serde::Deserialize, Default)]
    struct Payload {
        agent_transcript_path: Option<String>,
        hook_event_name: Option<String>,
        agent_id: Option<String>,
        agent_type: Option<String>,
        #[allow(dead_code)]
        session_id: Option<String>,
        transcript_path: Option<String>,
    }

    let payload_value: Value = serde_json::from_str(input.trim()).unwrap_or(Value::Null);
    let payload: Payload = serde_json::from_value(payload_value.clone()).unwrap_or_default();
    let event = payload.hook_event_name.as_deref().unwrap_or(event);
    let progress = match event {
        "SubagentStart" => {
            let Some(agent_id) = payload.agent_id else {
                return Ok(None);
            };
            ProgressEvent::SubagentStart(SubagentEntry {
                agent_id,
                agent_type: payload.agent_type.unwrap_or_else(|| "agent".to_string()),
                display_name: None,
            })
        }
        "SubagentStop" => {
            let Some(agent_id) = payload.agent_id else {
                return Ok(None);
            };
            ProgressEvent::SubagentStop { agent_id }
        }
        "TaskCreated" | "TaskCompleted" | "PostToolUse"
            if claude_hook_origin(
                payload.transcript_path.as_deref(),
                payload.agent_transcript_path.as_deref(),
            ) == HookOrigin::Subagent =>
        {
            return Ok(None);
        }
        "TaskCreated" => ProgressEvent::TaskCreated,
        "TaskCompleted" => ProgressEvent::TaskCompleted,
        "PostToolUse" => return claude_post_tool_use_event(&payload_value),
        _ => return Ok(None),
    };
    Ok(Some(progress))
}

fn claude_post_tool_use_event(payload: &Value) -> Result<Option<ProgressEvent>> {
    let Some(tool_name) = payload.get("tool_name").and_then(Value::as_str) else {
        return Ok(None);
    };
    match tool_name {
        "TodoWrite" => claude_todo_write_event(payload),
        "TaskCreate" => claude_task_create_event(payload),
        "TaskUpdate" => claude_task_update_event(payload),
        _ => Ok(None),
    }
}

fn claude_todo_write_event(payload: &Value) -> Result<Option<ProgressEvent>> {
    let Some(todos) = payload
        .get("tool_input")
        .and_then(|tool_input| tool_input.get("todos"))
        .and_then(Value::as_array)
    else {
        return Ok(None);
    };
    let Some(items) = todos
        .iter()
        .map(claude_todo_item_from_value)
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(None);
    };
    let done = items
        .iter()
        .filter(|item| item.status == TaskItemStatus::Completed)
        .count() as i64;
    Ok(Some(ProgressEvent::TaskSnapshot {
        progress: TaskProgress {
            done,
            total: items.len() as i64,
        },
        items,
    }))
}

fn claude_todo_item_from_value(value: &Value) -> Option<TaskItem> {
    let content = value.get("content").and_then(Value::as_str)?;
    let status = claude_task_status_from_str(value.get("status").and_then(Value::as_str)?)?;
    Some(TaskItem {
        step: content.to_string(),
        status,
    })
}

fn claude_task_create_event(payload: &Value) -> Result<Option<ProgressEvent>> {
    let Some(tool_input) = payload.get("tool_input") else {
        return Ok(None);
    };
    let Some(tool_output) = claude_tool_output(payload) else {
        return Ok(None);
    };
    let Some(id) = tool_output
        .get("task")
        .and_then(|task| task.get("id"))
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let Some(step) = tool_output
        .get("task")
        .and_then(|task| task.get("subject"))
        .and_then(Value::as_str)
        .or_else(|| tool_input.get("subject").and_then(Value::as_str))
    else {
        return Ok(None);
    };
    Ok(Some(ProgressEvent::TaskItemCreated {
        id: id.to_string(),
        step: step.to_string(),
    }))
}

fn claude_task_update_event(payload: &Value) -> Result<Option<ProgressEvent>> {
    let Some(tool_input) = payload.get("tool_input") else {
        return Ok(None);
    };
    let Some(id) = tool_input.get("taskId").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(status) = tool_input
        .get("status")
        .and_then(Value::as_str)
        .and_then(claude_task_status_from_str)
    else {
        return Ok(None);
    };
    Ok(Some(ProgressEvent::TaskItemUpdated {
        id: id.to_string(),
        status,
    }))
}

fn claude_tool_output(payload: &Value) -> Option<&Value> {
    payload
        .get("tool_response")
        .filter(|value| !value.is_null())
        .or_else(|| payload.get("tool_result").filter(|value| !value.is_null()))
}

fn claude_task_status_from_str(raw: &str) -> Option<TaskItemStatus> {
    match raw {
        "pending" => Some(TaskItemStatus::Pending),
        "in_progress" => Some(TaskItemStatus::InProgress),
        "completed" => Some(TaskItemStatus::Completed),
        _ => None,
    }
}

fn codex_aux_event_from_input(
    event: &str,
    input: &str,
    now_epoch: i64,
    codex_home: Option<&Path>,
) -> Result<Option<ProgressEvent>> {
    let payload: Value = match serde_json::from_str(input.trim()) {
        Ok(payload) => payload,
        Err(_) => return Ok(None),
    };
    match event {
        "PostToolUse" => {
            if is_guarded_codex_post_tool_use(&payload)
                && codex_hook_origin(
                    payload.get("session_id").and_then(Value::as_str),
                    codex_home,
                ) == HookOrigin::Subagent
            {
                return Ok(None);
            }
            codex_post_tool_use_event(&payload, now_epoch)
        }
        "SubagentStart" => codex_subagent_start_event_with_home(&payload, codex_home),
        "SubagentStop" => codex_subagent_stop_event(&payload),
        _ => Ok(None),
    }
}

fn is_guarded_codex_post_tool_use(payload: &Value) -> bool {
    matches!(
        payload.get("tool_name").and_then(Value::as_str),
        Some("update_plan" | "Bash")
    )
}

fn codex_post_tool_use_event(payload: &Value, now_epoch: i64) -> Result<Option<ProgressEvent>> {
    let Some(tool_name) = payload.get("tool_name").and_then(Value::as_str) else {
        return Ok(None);
    };
    match tool_name {
        "update_plan" => codex_update_plan_event(payload),
        "Bash" => codex_bash_event(payload, now_epoch),
        _ => Ok(None),
    }
}

fn codex_update_plan_event(payload: &Value) -> Result<Option<ProgressEvent>> {
    let Some(plan) = payload
        .get("tool_input")
        .and_then(|tool_input| tool_input.get("plan"))
    else {
        return Ok(None);
    };
    let Ok(items) = serde_json::from_value::<Vec<TaskItem>>(plan.clone()) else {
        return Ok(None);
    };
    let done = items
        .iter()
        .filter(|item| item.status == TaskItemStatus::Completed)
        .count() as i64;
    Ok(Some(ProgressEvent::TaskSnapshot {
        progress: TaskProgress {
            done,
            total: items.len() as i64,
        },
        items,
    }))
}

fn codex_bash_event(payload: &Value, now_epoch: i64) -> Result<Option<ProgressEvent>> {
    let Some(command) = payload
        .get("tool_input")
        .and_then(|tool_input| tool_input.get("command"))
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let Some((binary, target)) = parse_vw_exec_command(command) else {
        return Ok(None);
    };
    let Some((name, path)) = resolve_vw_exec_target(binary, target) else {
        return Ok(None);
    };
    Ok(Some(ProgressEvent::WorktreeActivity(WorktreeActivity {
        kind: WorktreeActivityKind::VwExec,
        name,
        path,
        command: command.to_string(),
        observed_at: now_epoch,
    })))
}

fn parse_vw_exec_command(command: &str) -> Option<(&str, &str)> {
    let fields = command.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 5 {
        return None;
    }
    let binary = fields[0];
    if binary != "vw" && binary != "vde-worktree" {
        return None;
    }
    if fields[1] != "exec" || fields[3] != "--" {
        return None;
    }
    Some((binary, fields[2]))
}

fn resolve_vw_exec_target(binary: &str, target: &str) -> Option<(String, String)> {
    let target_path = Path::new(target);
    if target_path.is_absolute() {
        return Some((
            path_basename(target).unwrap_or_else(|| target.to_string()),
            target.to_string(),
        ));
    }
    resolve_vw_target_path(binary, target).map(|path| (target.to_string(), path))
}

fn resolve_vw_target_path(binary: &str, target: &str) -> Option<String> {
    let output = Command::new(binary)
        .args(["path", target, "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Ok(value) = serde_json::from_str::<Value>(&stdout) {
        if let Some(path) = value.get("path").and_then(Value::as_str) {
            return Some(path.to_string());
        }
        if let Some(path) = value.as_str() {
            return Some(path.to_string());
        }
    }
    let path = stdout.trim();
    (!path.is_empty()).then(|| path.to_string())
}

fn path_basename(raw: &str) -> Option<String> {
    Path::new(raw.trim_end_matches('/'))
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn codex_subagent_start_event_with_home(
    payload: &Value,
    codex_home: Option<&Path>,
) -> Result<Option<ProgressEvent>> {
    let Some(agent_id) = payload.get("agent_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(agent_type) = payload.get("agent_type").and_then(Value::as_str) else {
        return Ok(None);
    };
    let display_name = codex_home.and_then(|home| codex_subagent_display_name(home, agent_id));
    Ok(Some(ProgressEvent::SubagentStart(SubagentEntry {
        agent_id: agent_id.to_string(),
        agent_type: agent_type.to_string(),
        display_name,
    })))
}

fn codex_home_from_env(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(path) = env.get("CODEX_HOME").filter(|path| !path.trim().is_empty()) {
        return Some(PathBuf::from(path));
    }
    env.get("HOME")
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".codex"))
}

fn codex_subagent_display_name(codex_home: &Path, agent_id: &str) -> Option<String> {
    let path = find_codex_session_file(&codex_home.join("sessions"), agent_id)?;
    read_codex_session_display_name(&path)
}

fn read_codex_session_display_name(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    let value = serde_json::from_str::<Value>(line.trim()).ok()?;
    let payload = value.get("payload")?;
    payload
        .get("agent_nickname")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .pointer("/source/subagent/thread_spawn/agent_nickname")
                .and_then(Value::as_str)
        })
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
}

fn codex_subagent_stop_event(payload: &Value) -> Result<Option<ProgressEvent>> {
    let Some(agent_id) = payload.get("agent_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    Ok(Some(ProgressEvent::SubagentStop {
        agent_id: agent_id.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn codex_subagent_start_uses_agent_nickname_from_session_meta() {
        let root = unique_temp_dir("codex-session-meta");
        let sessions = root.join("sessions").join("2026").join("07").join("07");
        fs::create_dir_all(&sessions).unwrap();
        let agent_id = "019f3c28-629a-7bc2-864a-3232c84499c3";
        fs::write(
            sessions.join(format!("rollout-2026-07-07T19-38-27-{agent_id}.jsonl")),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{agent_id}","agent_nickname":"Ramanujan","agent_role":"explorer"}}}}"#
            ),
        )
        .unwrap();

        let payload = serde_json::json!({
            "agent_id": agent_id,
            "agent_type": "default"
        });

        let event = codex_subagent_start_event_with_home(&payload, Some(&root))
            .unwrap()
            .unwrap();
        let ProgressEvent::SubagentStart(entry) = event else {
            panic!("expected subagent start event");
        };
        assert_eq!(entry.agent_type, "default");
        assert_eq!(entry.display_name.as_deref(), Some("Ramanujan"));

        fs::remove_dir_all(root).unwrap();
    }

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vde-tmux-{name}-{}-{nanos}", std::process::id()))
    }
}
