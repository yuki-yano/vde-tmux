use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::Value;

use crate::hook::adapter::{
    GenericEmitInput, TypedAdapterContext, build_prompt_preview, claude_typed_event_from_json,
    codex_typed_event_from_json_with_home, generic_typed_event, typed_progress_event,
};
use crate::hook::origin::{
    HookOrigin, claude_hook_origin, codex_hook_origin_from_payload, find_codex_session_file,
};
use crate::hook::writer::{ProgressEvent, typed_progress_operations};
use crate::hook::{
    AgentEvent, AgentStatus, OptionUpdate, SubagentEntry, TaskItem, TaskItemStatus, TaskProgress,
    WorktreeActivity, WorktreeActivityKind,
};
use crate::pane_state::{
    AgentKind, AgentSessionId, PaneEvent, PaneEventEnvelope, PromptState, normalize_text,
};
use crate::tmux::TmuxRunner;

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)] // `Emit` mirrors the intentionally flat public CLI schema.
pub(crate) enum HookCommand {
    /// Emit a typed state update without reading stdin.
    #[command(
        long_about = "Emit a typed state update for the agent running in the current tmux pane. This command does not read stdin.",
        after_help = "Examples:\n  vt hook emit --agent myagent --session-id run-42 --status running\n  vt hook emit --agent myagent --session-id run-42 --status waiting --wait-reason permission_prompt\n  vt hook emit --agent myagent --session-id run-42 --status idle --completed-at 1700000000"
    )]
    Emit {
        /// Stable agent kind or integration name.
        #[arg(long, value_name = "NAME")]
        agent: String,
        /// Stable ID for this agent session or run.
        #[arg(long = "session-id", value_name = "ID")]
        session_id: String,
        /// Lifecycle state: running, waiting, idle, or error.
        #[arg(long, value_name = "STATE")]
        status: Option<String>,
        /// Current prompt text; requires --prompt-source.
        #[arg(long, value_name = "TEXT")]
        prompt: Option<String>,
        /// Source label for --prompt.
        #[arg(long = "prompt-source", value_name = "SOURCE")]
        prompt_source: Option<String>,
        /// Clear the current prompt instead of setting one.
        #[arg(long = "clear-prompt")]
        clear_prompt: bool,
        /// Waiting reason: permission_prompt or other:TEXT.
        #[arg(long = "wait-reason", value_name = "REASON")]
        wait_reason: Option<String>,
        /// Mark an idle completion as requiring attention.
        #[arg(long)]
        attention: bool,
        /// Run start time as Unix epoch seconds; valid with running.
        #[arg(long = "started-at", value_name = "EPOCH")]
        started_at: Option<i64>,
        /// Completion time as Unix epoch seconds; valid with idle.
        #[arg(long = "completed-at", value_name = "EPOCH")]
        completed_at: Option<i64>,
        /// Replace task progress using DONE/TOTAL.
        #[arg(long, value_name = "DONE/TOTAL")]
        tasks: Option<String>,
        /// Clear task progress and task rows.
        #[arg(long = "clear-tasks")]
        clear_tasks: bool,
        /// Replace subagents using ID:TYPE|ID:TYPE.
        #[arg(long, value_name = "ENTRIES")]
        subagents: Option<String>,
        /// Clear the current subagent set.
        #[arg(long = "clear-subagents")]
        clear_subagents: bool,
    },
    /// Read a Claude Code hook payload from stdin and submit its typed event.
    Claude {
        /// Claude Code hook event name, for example Stop.
        event: String,
    },
    /// Read a Codex hook payload from stdin and submit its typed event.
    Codex {
        /// Codex hook event name, for example Stop.
        arg: Option<String>,
    },
}

pub(crate) fn run_hook_command(
    command: HookCommand,
    input: &str,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    now_epoch: i64,
    deadline: Instant,
) -> Result<()> {
    match command {
        HookCommand::Emit {
            agent,
            session_id,
            status,
            prompt,
            prompt_source,
            clear_prompt,
            wait_reason,
            attention,
            started_at,
            completed_at,
            tasks,
            clear_tasks,
            subagents,
            clear_subagents,
        } => {
            let (mut client, context, server_hash) =
                typed_hook_context(runner, env, deadline, now_epoch)?;
            let event = generic_typed_event(
                GenericEmitInput {
                    agent,
                    session_id,
                    status,
                    prompt,
                    prompt_source,
                    clear_prompt,
                    wait_reason,
                    tasks: tasks
                        .as_deref()
                        .map(parse_task_progress)
                        .transpose()?
                        .map(canonical_task_progress)
                        .transpose()?,
                    clear_tasks,
                    subagents: subagents
                        .as_deref()
                        .map(parse_subagents_arg)
                        .transpose()?
                        .map(canonical_subagents)
                        .transpose()?,
                    clear_subagents,
                    attention,
                    started_at,
                    completed_at,
                },
                &context,
            )?;
            send_typed_hook_event_observed(&mut client, event, env, &server_hash)
        }
        HookCommand::Claude { event } => {
            let (mut client, context, server_hash) =
                typed_hook_context(runner, env, deadline, now_epoch)?;
            let event = claude_typed_event_from_input(&event, input, &context)?;
            send_typed_hook_event_observed(&mut client, event, env, &server_hash)
        }
        HookCommand::Codex { arg } => {
            let Some(arg) = arg else {
                bail!("InvalidRequest: Codex hook event is required");
            };
            if arg.trim_start().starts_with('{') {
                bail!("UnsupportedLegacyNotify: Codex agent-turn-complete notify is not supported");
            }
            let codex_home = codex_home_from_env(env);
            let (mut client, context, server_hash) =
                typed_hook_context(runner, env, deadline, now_epoch)?;
            let event = codex_typed_event_from_input(&arg, input, &context, codex_home.as_deref())?;
            send_typed_hook_event_observed(&mut client, event, env, &server_hash)
        }
    }
}

fn typed_hook_context(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    deadline: Instant,
    observed_at: i64,
) -> Result<(
    crate::daemon::protocol::v2::V2Client,
    TypedAdapterContext,
    String,
)> {
    let pane_instance = crate::hook::writer::resolve_pane_instance(runner, env)?
        .ok_or_else(|| anyhow::anyhow!("InvalidPaneInstance: hook has no target pane"))?;
    if crate::daemon::lifecycle::tmux_desired_mode(runner, env)?
        == crate::daemon::lifecycle::DesiredMode::Disabled
    {
        bail!("daemon is disabled for the current tmux server");
    }
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    let server_hash = incarnation.hash.clone();
    let delivery = (|| -> Result<_> {
        let (incarnation, socket) =
            crate::daemon::lifecycle::ensure_daemon_live_v2_for_incarnation_until(
                incarnation,
                env,
                None,
                deadline,
            )?;
        let client = crate::daemon::protocol::v2::V2Client::connect_with_deadline(
            &socket,
            &incarnation.hash,
            deadline,
        )?;
        Ok(client)
    })();
    let client = delivery.inspect_err(|error| {
        record_agent_hook_delivery(env, &server_hash, error);
    })?;
    let context = TypedAdapterContext {
        daemon_instance_id: client.daemon_instance_id().clone(),
        event_id: crate::pane_state::EventId::generate()?,
        pane_instance,
        observed_at,
    };
    Ok((client, context, server_hash))
}

fn send_typed_hook_event_observed(
    client: &mut crate::daemon::protocol::v2::V2Client,
    event: Option<PaneEventEnvelope>,
    env: &BTreeMap<String, String>,
    server_hash: &str,
) -> Result<()> {
    match send_typed_hook_event(client, event) {
        Ok(()) => {
            let _ = crate::daemon::lifecycle::record_hook_delivery_recovered(env, server_hash);
            Ok(())
        }
        Err(error) => {
            record_agent_hook_delivery(env, server_hash, &error);
            Err(error)
        }
    }
}

fn record_agent_hook_delivery(
    env: &BTreeMap<String, String>,
    server_hash: &str,
    error: &anyhow::Error,
) {
    let message = error.to_string();
    let code = if message.contains("deadline") || message.contains("timed out") {
        "agent_hook_timeout"
    } else if message.contains("QueueFull") || message.contains("queue") {
        "agent_hook_queue_full"
    } else {
        "agent_hook_delivery_failed"
    };
    let _ = crate::daemon::lifecycle::record_hook_delivery_failure(env, server_hash, code);
    let _ = crate::daemon::lifecycle::append_incarnation_log(
        env,
        server_hash,
        "pane-state-hook.log",
        &format!("agent hook delivery failed: {code}"),
    );
}

fn send_typed_hook_event(
    client: &mut crate::daemon::protocol::v2::V2Client,
    event: Option<PaneEventEnvelope>,
) -> Result<()> {
    let Some(envelope) = event else {
        return Ok(());
    };
    let event_id = envelope.event_id.clone();
    let response = client.request_with_stage(
        &crate::daemon::protocol::v2::ClientMessage::SubmitPaneEvent {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            envelope,
        },
    )?;
    match response {
        crate::daemon::protocol::v2::ServerMessage::PaneEventResult {
            event_id: response_id,
            ..
        } if response_id == event_id => Ok(()),
        crate::daemon::protocol::v2::ServerMessage::Error { code, message, .. } => {
            bail!("daemon returned {code:?}: {message}")
        }
        response => bail!("unexpected pane event response: {response:?}"),
    }
}

fn canonical_task_progress(progress: TaskProgress) -> Result<crate::pane_state::TaskProgress> {
    if progress.done < 0 || progress.total < 0 {
        bail!("InvalidRequest: task progress cannot be negative");
    }
    let progress = crate::pane_state::TaskProgress {
        done: progress.done as u64,
        total: progress.total as u64,
    };
    if progress.done > progress.total {
        bail!("InvalidRequest: task progress exceeds total");
    }
    Ok(progress)
}

fn canonical_subagents(
    entries: Vec<SubagentEntry>,
) -> Result<Vec<crate::pane_state::SubagentState>> {
    let states = entries
        .into_iter()
        .map(|entry| {
            let state = crate::pane_state::SubagentState {
                agent_id: normalize_text(&entry.agent_id),
                agent_type: normalize_text(&entry.agent_type),
                display_name: entry
                    .display_name
                    .as_deref()
                    .map(normalize_text)
                    .filter(|value| !value.is_empty()),
            };
            Ok(state)
        })
        .collect::<Result<Vec<_>>>()?;
    crate::pane_state::validate_subagents(&states)?;
    Ok(states)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_view_hook_command(
    event_kind: &str,
    owner: &str,
    protocol: u16,
    hook_session: Option<&str>,
    hook_window: Option<&str>,
    snapshot_session: &str,
    snapshot_window: &str,
    snapshot_pane: &str,
    snapshot_pane_pid: &str,
    snapshot_panes: &str,
    snapshot_clients: &str,
    hook_client: Option<&str>,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
) -> Result<()> {
    use crate::pane_state::ViewHookKind;

    let deadline = Instant::now() + Duration::from_millis(500);
    if owner != crate::daemon::view_hooks::HOOK_OWNER
        || protocol != crate::daemon::view_hooks::HOOK_PROTOCOL
    {
        bail!("InvalidRequest: pane-state view hook ownership marker mismatch");
    }
    let hook_kind = match event_kind {
        "window-pane-changed" => ViewHookKind::WindowPaneChanged,
        "session-window-changed" => ViewHookKind::SessionWindowChanged,
        "client-session-changed" => ViewHookKind::ClientSessionChanged,
        "client-attached" => ViewHookKind::ClientAttached,
        "client-detached" => ViewHookKind::ClientDetached,
        _ => bail!("InvalidRequest: unknown pane-state view hook kind {event_kind}"),
    };
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    let server_hash = incarnation.hash.clone();
    let result = (|| -> Result<()> {
        ensure_view_hook_deadline(deadline, "resolving tmux server identity")?;
        let snapshot = crate::daemon::view_hooks::parse_hook_view_snapshot(
            hook_kind,
            crate::daemon::view_hooks::HookViewSnapshotFrame {
                hook_session: hook_session.unwrap_or_default(),
                hook_window: hook_window.unwrap_or_default(),
                session_id: snapshot_session,
                window_id: snapshot_window,
                pane_id: snapshot_pane,
                pane_pid: snapshot_pane_pid,
                panes: snapshot_panes,
                clients: snapshot_clients,
                hook_client: hook_client.unwrap_or_default(),
            },
        )?;
        ensure_view_hook_deadline(deadline, "parsing immutable hook snapshot")?;
        ensure_view_hook_deadline(deadline, "ensuring pane-state daemon")?;
        let (incarnation, socket) =
            crate::daemon::lifecycle::ensure_daemon_live_v2_for_incarnation_until(
                incarnation,
                env,
                None,
                deadline,
            )?;
        let event_id = crate::pane_state::EventId::generate()?;
        let mut delivery = crate::daemon::view_hooks::ViewDeliveryContract::default();
        let probe = loop {
            if Instant::now() >= deadline {
                bail!("view hook 500ms deadline exceeded while connecting to pane-state daemon");
            }
            if !delivery.begin_attempt() {
                bail!(
                    "view event {} failed before full write: connection attempts exhausted",
                    event_id.as_str()
                );
            }
            match crate::daemon::protocol::v2::V2Client::connect_with_deadline(
                &socket,
                &incarnation.hash,
                deadline,
            ) {
                Ok(client) => break client,
                Err(_)
                    if delivery.may_retry(
                        crate::daemon::view_hooks::DeliveryFailureStage::BeforeFullWrite,
                    ) => {}
                Err(error) => return Err(error),
            }
        };
        // View events carry only the immutable tmux snapshot. The daemon applies
        // its active `done_clear_on` policy while processing the event, so disk
        // config drift must not stop pane/window/client registry synchronization.
        let built = crate::daemon::view_hooks::build_foreground_view_event(
            probe.daemon_instance_id().clone(),
            event_id,
            hook_kind,
            snapshot.occurrence,
            snapshot.source_client,
            snapshot.witnesses,
            config.daemon.done_clear_on,
        )?;
        crate::daemon::view_hooks::deliver_view_event_with_active_attempt(
            &mut crate::daemon::view_hooks::SocketViewEventSender {
                socket: &socket,
                server_identity: &incarnation.hash,
                initial_client: Some(probe),
            },
            &built.event,
            deadline,
            delivery,
        )?;
        Ok(())
    })();
    if let Err(error) = &result {
        eprintln!("{error:#}");
        log_view_hook_failure(env, &server_hash, &format!("{error:#}"));
    } else {
        let _ = crate::daemon::lifecycle::record_hook_delivery_recovered(env, &server_hash);
    }
    result
}

fn ensure_view_hook_deadline(deadline: Instant, stage: &str) -> Result<()> {
    if Instant::now() >= deadline {
        bail!("pane-state view hook deadline exceeded while {stage}");
    }
    Ok(())
}

fn log_view_hook_failure(env: &BTreeMap<String, String>, server_hash: &str, message: &str) {
    let code = if message.contains("deadline") || message.contains("timed out") {
        "hook_delivery_timeout"
    } else {
        "hook_delivery_failed"
    };
    let _ = crate::daemon::lifecycle::record_hook_delivery_failure(env, server_hash, code);
    let _ = crate::daemon::lifecycle::append_incarnation_log(
        env,
        server_hash,
        "pane-state-hook.log",
        message,
    );
}

pub(crate) fn claude_typed_event_from_input(
    event: &str,
    input: &str,
    context: &TypedAdapterContext,
) -> Result<Option<PaneEventEnvelope>> {
    if let Some(progress_event) = claude_progress_event_from_input(event, input)? {
        let session_id = required_payload_session(input)?;
        return Ok(Some(typed_progress_event(
            "claude",
            session_id,
            typed_progress_operations(progress_event)?,
            context,
        )?));
    }
    claude_typed_event_from_json(event, input, context)
}

pub(crate) fn codex_typed_event_from_input(
    event: &str,
    input: &str,
    context: &TypedAdapterContext,
    codex_home: Option<&Path>,
) -> Result<Option<PaneEventEnvelope>> {
    if event.trim_start().starts_with('{') {
        bail!("UnsupportedLegacyNotify: Codex agent-turn-complete notify is not supported");
    }
    if let Some(aux_event) =
        codex_aux_event_from_input(event, input, context.observed_at, codex_home)?
    {
        let session_id = required_payload_session(input)?;
        return match aux_event {
            CodexAuxEvent::ActivityAndProgress(progress_event) => Ok(Some(context.envelope(
                AgentKind::parse("codex")?,
                AgentSessionId::parse(session_id)?,
                PaneEvent::ActivityAndProgressObserved {
                    observed_at: context.observed_at,
                    operations: typed_progress_operations(progress_event)?,
                },
            ))),
            CodexAuxEvent::Progress(progress_event) => Ok(Some(typed_progress_event(
                "codex",
                session_id,
                typed_progress_operations(progress_event)?,
                context,
            )?)),
            CodexAuxEvent::Agent(event) => {
                let Some(OptionUpdate::Set(text)) = event.prompt else {
                    return Ok(None);
                };
                let Some(OptionUpdate::Set(source)) = event.prompt_source else {
                    return Ok(None);
                };
                let prompt = PromptState {
                    text: normalize_text(&text),
                    source: normalize_text(&source),
                };
                prompt.validate()?;
                Ok(Some(context.envelope(
                    AgentKind::parse("codex")?,
                    AgentSessionId::parse(session_id)?,
                    PaneEvent::BeginRun {
                        started_at: event.started_at.unwrap_or(context.observed_at),
                        prompt: Some(prompt),
                    },
                )))
            }
        };
    }
    codex_typed_event_from_json_with_home(event, input, context, codex_home)
}

fn required_payload_session(input: &str) -> Result<String> {
    let payload: Value = serde_json::from_str(input.trim())?;
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|session_id| !session_id.trim().is_empty());
    session_id.ok_or_else(|| anyhow::anyhow!("InvalidRequest: hook payload requires session_id"))
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

#[allow(clippy::large_enum_variant)]
enum CodexAuxEvent {
    ActivityAndProgress(ProgressEvent),
    Progress(ProgressEvent),
    Agent(AgentEvent),
}

fn codex_aux_event_from_input(
    event: &str,
    input: &str,
    now_epoch: i64,
    codex_home: Option<&Path>,
) -> Result<Option<CodexAuxEvent>> {
    let payload: Value = match serde_json::from_str(input.trim()) {
        Ok(payload) => payload,
        Err(_) => return Ok(None),
    };
    match event {
        "PostToolUse" => {
            if is_guarded_codex_post_tool_use(&payload)
                && codex_hook_origin_from_payload(
                    payload.get("session_id").and_then(Value::as_str),
                    payload.get("agent_id").and_then(Value::as_str),
                    payload.get("transcript_path").and_then(Value::as_str),
                    codex_home,
                ) == HookOrigin::Subagent
            {
                return Ok(None);
            }
            Ok(
                codex_post_tool_use_event(&payload, now_epoch)?.map(|event| match event {
                    CodexAuxEvent::Progress(event) => CodexAuxEvent::ActivityAndProgress(event),
                    other => other,
                }),
            )
        }
        "SubagentStart" => Ok(codex_subagent_start_event_with_home(&payload, codex_home)?
            .map(CodexAuxEvent::Progress)),
        "SubagentStop" => Ok(codex_subagent_stop_event(&payload)?.map(CodexAuxEvent::Progress)),
        _ => Ok(None),
    }
}

fn is_guarded_codex_post_tool_use(payload: &Value) -> bool {
    matches!(
        payload.get("tool_name").and_then(Value::as_str),
        Some("update_plan" | "Bash" | "create_goal")
    )
}

fn codex_post_tool_use_event(payload: &Value, now_epoch: i64) -> Result<Option<CodexAuxEvent>> {
    let Some(tool_name) = payload.get("tool_name").and_then(Value::as_str) else {
        return Ok(None);
    };
    match tool_name {
        "update_plan" => Ok(codex_update_plan_event(payload)?.map(CodexAuxEvent::Progress)),
        "Bash" => Ok(codex_bash_event(payload, now_epoch)?.map(CodexAuxEvent::Progress)),
        "create_goal" => Ok(codex_create_goal_event(payload, now_epoch)?.map(CodexAuxEvent::Agent)),
        _ => Ok(None),
    }
}

fn codex_create_goal_event(payload: &Value, now_epoch: i64) -> Result<Option<AgentEvent>> {
    let Some(objective) = payload
        .get("tool_input")
        .and_then(|tool_input| tool_input.get("objective"))
        .and_then(Value::as_str)
        .and_then(build_prompt_preview)
    else {
        return Ok(None);
    };
    Ok(Some(AgentEvent {
        agent: "codex".to_string(),
        status: Some(AgentStatus::Running),
        prompt: Some(OptionUpdate::Set(objective)),
        prompt_source: Some(OptionUpdate::Set("goal".to_string())),
        started_at: Some(now_epoch),
        tasks: Some(OptionUpdate::Unset),
        task_items: Some(OptionUpdate::Unset),
        worktree_activity: Some(OptionUpdate::Unset),
        ..AgentEvent::default()
    }))
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

/// Bound on the `vw path` probe. Worktree activity is a best-effort display
/// detail, so a stalled `vw` must never block the hook indefinitely.
const VW_PATH_TIMEOUT: Duration = Duration::from_millis(300);

fn resolve_vw_target_path(binary: &str, target: &str) -> Option<String> {
    let mut command = Command::new(binary);
    command.args(["path", target, "--json"]);
    let stdout = run_command_with_timeout(command, VW_PATH_TIMEOUT)?;
    let stdout = String::from_utf8_lossy(&stdout);
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

fn run_command_with_timeout(mut command: Command, timeout: Duration) -> Option<Vec<u8>> {
    // Run in a fresh process group so the whole tree can be killed; a descendant
    // that inherits stdout would otherwise keep the pipe open and block the
    // reader join. The group is killed before the leader is reaped, so the pgid
    // cannot be reused (see crate::proc).
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let status = crate::proc::await_exit_then_kill_group(&mut child, timeout);
    let stdout = reader.join().ok()?;
    match status {
        // Worktree activity is best-effort: a timeout or a wait error just skips it.
        Ok(Some(status)) if status.success() => Some(stdout),
        _ => None,
    }
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
    use crate::pane_state::{DaemonInstanceId, EventId, ProgressOperation};
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

    #[test]
    fn claude_todo_write_maps_to_task_snapshot() {
        let payload = serde_json::json!({
            "tool_name": "TodoWrite",
            "tool_input": {
                "todos": [
                    {"content": "a", "status": "completed"},
                    {"content": "b", "status": "in_progress"},
                    {"content": "c", "status": "pending"},
                ]
            }
        });
        let event = claude_post_tool_use_event(&payload).unwrap().unwrap();
        let ProgressEvent::TaskSnapshot { progress, items } = event else {
            panic!("expected task snapshot");
        };
        assert_eq!(progress.done, 1);
        assert_eq!(progress.total, 3);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].step, "a");
        assert_eq!(items[0].status, TaskItemStatus::Completed);
    }

    #[test]
    fn claude_todo_write_without_todos_is_none() {
        let payload = serde_json::json!({"tool_name": "TodoWrite", "tool_input": {}});
        assert!(claude_post_tool_use_event(&payload).unwrap().is_none());
    }

    #[test]
    fn claude_task_create_prefers_output_subject_over_input() {
        let payload = serde_json::json!({
            "tool_name": "TaskCreate",
            "tool_input": {"subject": "fallback subject"},
            "tool_response": {"task": {"id": "task-1", "subject": "real subject"}}
        });
        let ProgressEvent::TaskItemCreated { id, step } =
            claude_post_tool_use_event(&payload).unwrap().unwrap()
        else {
            panic!("expected task item created");
        };
        assert_eq!(id, "task-1");
        assert_eq!(step, "real subject");
    }

    #[test]
    fn claude_task_create_falls_back_to_input_subject() {
        let payload = serde_json::json!({
            "tool_name": "TaskCreate",
            "tool_input": {"subject": "fallback subject"},
            "tool_response": {"task": {"id": "task-2"}}
        });
        let ProgressEvent::TaskItemCreated { id, step } =
            claude_post_tool_use_event(&payload).unwrap().unwrap()
        else {
            panic!("expected task item created");
        };
        assert_eq!(id, "task-2");
        assert_eq!(step, "fallback subject");
    }

    #[test]
    fn claude_task_update_maps_status() {
        let payload = serde_json::json!({
            "tool_name": "TaskUpdate",
            "tool_input": {"taskId": "task-3", "status": "completed"}
        });
        let ProgressEvent::TaskItemUpdated { id, status } =
            claude_post_tool_use_event(&payload).unwrap().unwrap()
        else {
            panic!("expected task item updated");
        };
        assert_eq!(id, "task-3");
        assert_eq!(status, TaskItemStatus::Completed);
    }

    #[test]
    fn claude_post_tool_use_ignores_unknown_tool() {
        let payload = serde_json::json!({"tool_name": "Bash", "tool_input": {}});
        assert!(claude_post_tool_use_event(&payload).unwrap().is_none());
    }

    #[test]
    fn claude_task_create_without_output_is_none() {
        let payload = serde_json::json!({
            "tool_name": "TaskCreate",
            "tool_input": {"subject": "s"}
        });
        assert!(claude_post_tool_use_event(&payload).unwrap().is_none());
    }

    #[test]
    fn claude_task_create_without_id_is_none() {
        let payload = serde_json::json!({
            "tool_name": "TaskCreate",
            "tool_input": {"subject": "s"},
            "tool_response": {"task": {"subject": "x"}}
        });
        assert!(claude_post_tool_use_event(&payload).unwrap().is_none());
    }

    #[test]
    fn claude_task_create_without_any_subject_is_none() {
        let payload = serde_json::json!({
            "tool_name": "TaskCreate",
            "tool_input": {},
            "tool_response": {"task": {"id": "t1"}}
        });
        assert!(claude_post_tool_use_event(&payload).unwrap().is_none());
    }

    #[test]
    fn claude_task_update_without_task_id_is_none() {
        let payload = serde_json::json!({
            "tool_name": "TaskUpdate",
            "tool_input": {"status": "completed"}
        });
        assert!(claude_post_tool_use_event(&payload).unwrap().is_none());
    }

    #[test]
    fn claude_task_update_with_invalid_status_is_none() {
        let payload = serde_json::json!({
            "tool_name": "TaskUpdate",
            "tool_input": {"taskId": "t1", "status": "bogus"}
        });
        assert!(claude_post_tool_use_event(&payload).unwrap().is_none());
    }

    #[test]
    fn claude_task_update_without_status_is_none() {
        let payload = serde_json::json!({
            "tool_name": "TaskUpdate",
            "tool_input": {"taskId": "t1"}
        });
        assert!(claude_post_tool_use_event(&payload).unwrap().is_none());
    }

    #[test]
    fn parse_vw_exec_command_extracts_binary_and_target() {
        assert_eq!(
            parse_vw_exec_command("vw exec /abs/path -- cargo test"),
            Some(("vw", "/abs/path"))
        );
        assert_eq!(
            parse_vw_exec_command("vde-worktree exec feature -- ls"),
            Some(("vde-worktree", "feature"))
        );
    }

    #[test]
    fn parse_vw_exec_command_rejects_non_matching_forms() {
        assert_eq!(parse_vw_exec_command("vw exec target"), None);
        assert_eq!(parse_vw_exec_command("git exec target -- ls"), None);
        assert_eq!(parse_vw_exec_command("vw run target -- ls"), None);
        assert_eq!(parse_vw_exec_command("vw exec target xx ls"), None);
    }

    #[test]
    fn resolve_vw_exec_target_uses_absolute_path_without_subprocess() {
        let (name, path) = resolve_vw_exec_target("vw", "/abs/work/repo").unwrap();
        assert_eq!(name, "repo");
        assert_eq!(path, "/abs/work/repo");
    }

    #[test]
    fn run_command_with_timeout_returns_stdout_on_success() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf hello"]);
        let out = run_command_with_timeout(command, Duration::from_secs(5)).unwrap();
        assert_eq!(out, b"hello");
    }

    #[test]
    fn run_command_with_timeout_kills_slow_command() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        let start = Instant::now();
        let out = run_command_with_timeout(command, Duration::from_millis(100));
        assert!(out.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must not block for the full sleep"
        );
    }

    #[test]
    fn run_command_with_timeout_preserves_output_despite_lingering_grandchild() {
        // The parent writes its output, then backgrounds a descendant that
        // inherits stdout and outlives it. The parent's bytes must still be
        // returned, and the call must not block on the descendant.
        let mut command = Command::new("sh");
        command.args(["-c", "printf PARENT; sleep 5 &"]);
        let start = Instant::now();
        let out = run_command_with_timeout(command, Duration::from_secs(10));
        assert_eq!(out.as_deref(), Some(b"PARENT".as_slice()));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must not block on a descendant that inherited stdout"
        );
    }

    fn typed_context() -> TypedAdapterContext {
        TypedAdapterContext {
            daemon_instance_id: DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100")
                .unwrap(),
            event_id: EventId::parse("102132435465768798a9bacbdcedfe0f").unwrap(),
            pane_instance: crate::pane_state::PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 42,
            },
            observed_at: 123,
        }
    }

    #[test]
    fn claude_progress_fixture_maps_to_typed_operation_with_session() {
        let envelope = claude_typed_event_from_input(
            "TaskCreated",
            r#"{"session_id":"claude-session"}"#,
            &typed_context(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            envelope.agent_session_id.unwrap().as_str(),
            "claude-session"
        );
        assert_eq!(
            envelope.event,
            PaneEvent::ProgressUpdated {
                observed_at: 123,
                operations: vec![ProgressOperation::TaskCreated],
            }
        );
    }

    #[test]
    fn codex_goal_fixture_maps_to_begin_run_and_legacy_notify_is_rejected() {
        let envelope = codex_typed_event_from_input(
            "PostToolUse",
            r#"{"session_id":"codex-session","tool_name":"create_goal","tool_input":{"objective":"ship\nthe change"}}"#,
            &typed_context(),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            envelope.event,
            PaneEvent::BeginRun {
                started_at: 123,
                prompt: Some(PromptState {
                    text: "ship the change".to_string(),
                    source: "goal".to_string(),
                }),
            }
        );

        let error = codex_typed_event_from_input(
            r#"{"type":"agent-turn-complete"}"#,
            "",
            &typed_context(),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("UnsupportedLegacyNotify"));
    }

    #[test]
    fn codex_post_tool_progress_also_reports_lifecycle_activity() {
        let envelope = codex_typed_event_from_input(
            "PostToolUse",
            r#"{"session_id":"codex-session","tool_name":"update_plan","tool_input":{"plan":[{"step":"ship it","status":"in_progress"}]}}"#,
            &typed_context(),
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            envelope.event,
            PaneEvent::ActivityAndProgressObserved {
                observed_at: 123,
                operations: vec![ProgressOperation::ReplaceTasks {
                    progress: crate::pane_state::TaskProgress { done: 0, total: 1 },
                    items: vec![crate::pane_state::TaskItemState {
                        id: None,
                        step: "ship it".to_string(),
                        status: crate::pane_state::TaskItemStatus::InProgress,
                    }],
                }],
            }
        );
    }

    #[test]
    fn codex_subagent_tool_activity_does_not_update_parent_lifecycle() {
        let root = unique_temp_dir("codex-subagent-tool-activity");
        let sessions = root.join("sessions").join("2026").join("07").join("16");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("rollout-parent-session.jsonl"),
            r#"{"type":"session_meta","payload":{"id":"parent-session","thread_source":"root"}}"#,
        )
        .unwrap();
        fs::write(
            sessions.join("rollout-subagent-session.jsonl"),
            r#"{"type":"session_meta","payload":{"id":"subagent-session","thread_source":"subagent","parent_thread_id":"parent-session"}}"#,
        )
        .unwrap();
        let payload =
            r#"{"session_id":"parent-session","agent_id":"subagent-session","tool_name":"exec"}"#;

        for hook in ["PreToolUse", "PostToolUse"] {
            assert!(
                codex_typed_event_from_input(hook, payload, &typed_context(), Some(&root))
                    .unwrap()
                    .is_none()
            );
        }

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
