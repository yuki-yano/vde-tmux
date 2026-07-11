use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::hook::origin::{HookOrigin, claude_hook_origin, codex_hook_origin_from_payload};
use crate::pane_state::{
    AgentKind, AgentSessionId, AgentSessionSource, DaemonInstanceId, EventId, ExplicitStateReport,
    FieldUpdate, PaneEvent, PaneEventEnvelope, PaneInstance, ProgressOperation, PromptState,
    ReportedLifecycle, SubagentState, TaskProgress as CanonicalTaskProgress, WaitReason,
    normalize_text, validate_subagents,
};

#[derive(Debug, Clone)]
pub struct TypedAdapterContext {
    pub daemon_instance_id: DaemonInstanceId,
    pub event_id: EventId,
    pub pane_instance: PaneInstance,
    pub observed_at: i64,
}

impl TypedAdapterContext {
    pub fn envelope(
        &self,
        agent: AgentKind,
        agent_session_id: AgentSessionId,
        event: PaneEvent,
    ) -> PaneEventEnvelope {
        PaneEventEnvelope {
            daemon_instance_id: self.daemon_instance_id.clone(),
            event_id: self.event_id.clone(),
            pane_instance: self.pane_instance.clone(),
            agent: Some(agent),
            agent_session_id: Some(agent_session_id),
            event,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GenericEmitInput {
    pub agent: String,
    pub session_id: String,
    pub status: Option<String>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub prompt: Option<String>,
    pub prompt_source: Option<String>,
    pub clear_prompt: bool,
    pub wait_reason: Option<String>,
    pub tasks: Option<CanonicalTaskProgress>,
    pub clear_tasks: bool,
    pub subagents: Option<Vec<SubagentState>>,
    pub clear_subagents: bool,
    pub attention: bool,
}

pub fn claude_typed_event_from_json(
    event: &str,
    raw_json: &str,
    context: &TypedAdapterContext,
) -> Result<Option<PaneEventEnvelope>> {
    let payload: ClaudeHookPayload = serde_json::from_str(raw_json.trim())?;
    let origin = claude_hook_origin(
        payload.transcript_path.as_deref(),
        payload.agent_transcript_path.as_deref(),
    );
    let event = payload.hook_event_name.as_deref().unwrap_or(event);
    if origin == HookOrigin::Subagent && is_guarded_claude_lifecycle_event(event) {
        return Ok(None);
    }
    let event = match event {
        "SessionStart" => PaneEvent::AgentSessionStarted {
            observed_at: context.observed_at,
            source: parse_session_source(payload.source.as_deref())?,
            resumed_prompt: if payload.source.as_deref() == Some("resume") {
                payload
                    .transcript_path
                    .as_deref()
                    .and_then(latest_user_prompt_from_transcript)
                    .map(|text| prompt_state(text, "resume"))
                    .transpose()?
            } else {
                None
            },
        },
        "UserPromptSubmit" => PaneEvent::BeginRun {
            started_at: context.observed_at,
            prompt: payload
                .prompt
                .as_deref()
                .and_then(build_prompt_preview)
                .map(|text| prompt_state(text, "user"))
                .transpose()?,
        },
        "PreToolUse" | "PostToolUse" => PaneEvent::ActivityObserved {
            observed_at: context.observed_at,
        },
        "Notification" if payload.notification_type.as_deref() == Some("permission_prompt") => {
            PaneEvent::WaitRequested {
                observed_at: context.observed_at,
                reason: WaitReason::PermissionPrompt,
            }
        }
        "Notification" => return Ok(None),
        "Stop" => PaneEvent::CompleteRun {
            completed_at: context.observed_at,
        },
        _ => return Ok(None),
    };
    Ok(Some(context.envelope(
        AgentKind::parse("claude")?,
        required_session_id(payload.session_id)?,
        event,
    )))
}

pub fn codex_typed_event_from_json(
    event: &str,
    raw_json: &str,
    context: &TypedAdapterContext,
) -> Result<Option<PaneEventEnvelope>> {
    codex_typed_event_from_json_with_home(event, raw_json, context, codex_home().as_deref())
}

pub fn codex_typed_event_from_json_with_home(
    event: &str,
    raw_json: &str,
    context: &TypedAdapterContext,
    codex_home: Option<&Path>,
) -> Result<Option<PaneEventEnvelope>> {
    let payload: CodexHookPayload = serde_json::from_str(raw_json.trim())?;
    let origin = codex_hook_origin_from_payload(
        payload.session_id.as_deref(),
        payload.agent_id.as_deref(),
        payload.transcript_path.as_deref(),
        codex_home,
    );
    if origin == HookOrigin::Subagent && is_guarded_codex_lifecycle_event(event) {
        return Ok(None);
    }
    let event = match event {
        "SessionStart" => PaneEvent::AgentSessionStarted {
            observed_at: context.observed_at,
            source: parse_session_source(payload.source.as_deref())?,
            resumed_prompt: if payload.source.as_deref() == Some("resume") {
                payload
                    .transcript_path
                    .as_deref()
                    .and_then(latest_user_prompt_from_transcript)
                    .map(|text| prompt_state(text, "resume"))
                    .transpose()?
            } else {
                None
            },
        },
        "UserPromptSubmit" => PaneEvent::BeginRun {
            started_at: context.observed_at,
            prompt: payload
                .prompt
                .as_deref()
                .and_then(build_prompt_preview)
                .map(|text| prompt_state(text, "user"))
                .transpose()?,
        },
        "PermissionRequest" => PaneEvent::WaitRequested {
            observed_at: context.observed_at,
            reason: WaitReason::PermissionPrompt,
        },
        "Stop" => PaneEvent::CompleteRun {
            completed_at: context.observed_at,
        },
        _ => return Ok(None),
    };
    Ok(Some(context.envelope(
        AgentKind::parse("codex")?,
        required_session_id(payload.session_id)?,
        event,
    )))
}

pub fn generic_typed_event(
    input: GenericEmitInput,
    context: &TypedAdapterContext,
) -> Result<Option<PaneEventEnvelope>> {
    if input.prompt.is_some() && input.clear_prompt {
        bail!("InvalidRequest: --prompt and --clear-prompt are mutually exclusive");
    }
    if input.tasks.is_some() && input.clear_tasks {
        bail!("InvalidRequest: --tasks and --clear-tasks are mutually exclusive");
    }
    if input.subagents.is_some() && input.clear_subagents {
        bail!("InvalidRequest: --subagents and --clear-subagents are mutually exclusive");
    }
    if input.prompt.is_some() != input.prompt_source.is_some() {
        bail!("InvalidRequest: --prompt requires exactly one non-empty --prompt-source");
    }
    let lifecycle = match input.status.as_deref() {
        Some("running") => Some(ReportedLifecycle::Running),
        Some("waiting") => Some(ReportedLifecycle::Waiting {
            reason: parse_wait_reason(input.wait_reason.as_deref())?,
        }),
        Some("idle") => Some(ReportedLifecycle::Idle),
        Some("error") => Some(ReportedLifecycle::Error { reason: None }),
        Some(status) => bail!("InvalidRequest: unknown hook status {status}"),
        None => None,
    };
    if input.started_at.is_some() && !matches!(lifecycle, Some(ReportedLifecycle::Running)) {
        bail!("InvalidRequest: --started-at requires --status running");
    }
    if input.completed_at.is_some() && !matches!(lifecycle, Some(ReportedLifecycle::Idle)) {
        bail!("InvalidRequest: --completed-at requires --status idle");
    }
    if input.wait_reason.is_some() && !matches!(lifecycle, Some(ReportedLifecycle::Waiting { .. }))
    {
        bail!("InvalidRequest: --wait-reason requires --status waiting");
    }
    if input.attention && !matches!(lifecycle, Some(ReportedLifecycle::Idle)) {
        bail!("InvalidRequest: --attention requires --status idle");
    }
    let prompt = match (input.prompt, input.prompt_source, input.clear_prompt) {
        (Some(text), Some(source), false) => Some(FieldUpdate::Set(prompt_state(text, source)?)),
        (None, None, true) => Some(FieldUpdate::Clear),
        (None, None, false) => None,
        _ => unreachable!("prompt combinations were validated above"),
    };
    let tasks = match (input.tasks, input.clear_tasks) {
        (Some(progress), false) => {
            if progress.done > progress.total {
                bail!("InvalidRequest: task progress exceeds total");
            }
            Some(FieldUpdate::Set(progress))
        }
        (None, true) => Some(FieldUpdate::Clear),
        (None, false) => None,
        _ => unreachable!("task combinations were validated above"),
    };
    let subagents = match (input.subagents, input.clear_subagents) {
        (Some(mut subagents), false) => {
            normalize_subagents(&mut subagents);
            validate_subagents(&subagents)?;
            Some(FieldUpdate::Set(subagents))
        }
        (None, true) => Some(FieldUpdate::Clear),
        (None, false) => None,
        _ => unreachable!("subagent combinations were validated above"),
    };
    let event = PaneEvent::ExplicitStateReported {
        report: ExplicitStateReport {
            observed_at: context.observed_at,
            lifecycle,
            started_at: input.started_at,
            completed_at: input.completed_at,
            prompt,
            tasks,
            subagents,
            attention: input.attention,
        },
    };
    if event.is_semantically_empty() {
        return Ok(None);
    }
    let agent = AgentKind::parse(input.agent)?;
    let agent_session_id = AgentSessionId::parse(input.session_id)?;
    Ok(Some(context.envelope(agent, agent_session_id, event)))
}

pub fn typed_progress_event(
    agent: impl AsRef<str>,
    session_id: impl Into<String>,
    operations: Vec<ProgressOperation>,
    context: &TypedAdapterContext,
) -> Result<PaneEventEnvelope> {
    Ok(context.envelope(
        AgentKind::parse(agent)?,
        AgentSessionId::parse(session_id)?,
        PaneEvent::ProgressUpdated {
            observed_at: context.observed_at,
            operations,
        },
    ))
}

fn required_session_id(session_id: Option<String>) -> Result<AgentSessionId> {
    let session_id = session_id
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("InvalidRequest: hook payload requires session_id"))?;
    Ok(AgentSessionId::parse(session_id)?)
}

fn parse_session_source(source: Option<&str>) -> Result<AgentSessionSource> {
    match source {
        Some("startup") => Ok(AgentSessionSource::Startup),
        Some("resume") => Ok(AgentSessionSource::Resume),
        Some("clear") => Ok(AgentSessionSource::Clear),
        _ => bail!("InvalidRequest: SessionStart requires startup, resume, or clear source"),
    }
}

fn parse_wait_reason(reason: Option<&str>) -> Result<WaitReason> {
    match reason {
        Some("permission_prompt") => Ok(WaitReason::PermissionPrompt),
        Some(reason) if reason.starts_with("other:") => {
            let reason = normalize_text(&reason["other:".len()..]);
            let parsed = WaitReason::Other(reason);
            parsed.validate()?;
            Ok(parsed)
        }
        _ => bail!(
            "InvalidRequest: waiting status requires permission_prompt or other:<text> wait reason"
        ),
    }
}

fn prompt_state(text: impl AsRef<str>, source: impl AsRef<str>) -> Result<PromptState> {
    let prompt = PromptState {
        text: normalize_text(text.as_ref()),
        source: normalize_text(source.as_ref()),
    };
    prompt.validate()?;
    Ok(prompt)
}

fn normalize_subagents(subagents: &mut [SubagentState]) {
    for subagent in subagents {
        subagent.agent_id = normalize_text(&subagent.agent_id);
        subagent.agent_type = normalize_text(&subagent.agent_type);
        subagent.display_name = subagent
            .display_name
            .as_deref()
            .map(normalize_text)
            .filter(|name| !name.is_empty());
    }
}

#[derive(Debug, Deserialize, Default)]
struct ClaudeHookPayload {
    agent_transcript_path: Option<String>,
    hook_event_name: Option<String>,
    notification_type: Option<String>,
    prompt: Option<String>,
    #[allow(dead_code)]
    session_id: Option<String>,
    source: Option<String>,
    transcript_path: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct CodexHookPayload {
    agent_id: Option<String>,
    prompt: Option<String>,
    session_id: Option<String>,
    source: Option<String>,
    transcript_path: Option<String>,
}

fn is_guarded_claude_lifecycle_event(event: &str) -> bool {
    matches!(
        event,
        "UserPromptSubmit"
            | "SessionStart"
            | "Stop"
            | "Notification"
            | "PreToolUse"
            | "PostToolUse"
    )
}

fn is_guarded_codex_lifecycle_event(event: &str) -> bool {
    matches!(
        event,
        "UserPromptSubmit" | "SessionStart" | "Stop" | "PermissionRequest"
    )
}

fn codex_home() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CODEX_HOME").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".codex"))
}

pub fn build_prompt_preview(raw: &str) -> Option<String> {
    let normalized = raw
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>();
    let preview = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    if preview.is_empty() {
        None
    } else {
        Some(preview)
    }
}

fn latest_user_prompt_from_transcript(path: &str) -> Option<String> {
    let file = std::fs::File::open(Path::new(path)).ok()?;
    let reader = std::io::BufReader::new(file);
    reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter_map(|value| user_prompt_from_transcript_value(&value))
        .filter_map(|prompt| build_prompt_preview(&prompt))
        .last()
}

fn user_prompt_from_transcript_value(value: &Value) -> Option<String> {
    let payload = value.get("payload").unwrap_or(value);
    if role_of(payload) == Some("user") {
        return text_from_content(payload.get("content")?);
    }
    let message = payload.get("message").or_else(|| value.get("message"))?;
    if role_of(message) == Some("user") {
        return text_from_content(message.get("content")?);
    }
    None
}

fn role_of(value: &Value) -> Option<&str> {
    value.get("role").and_then(Value::as_str)
}

fn text_from_content(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some(text)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed_context() -> TypedAdapterContext {
        TypedAdapterContext {
            daemon_instance_id: DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100")
                .unwrap(),
            event_id: EventId::parse("102132435465768798a9bacbdcedfe0f").unwrap(),
            pane_instance: PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 42,
            },
            observed_at: 123,
        }
    }

    #[test]
    fn claude_typed_fixture_maps_supported_lifecycle_events() {
        let fixtures = [
            (
                "UserPromptSubmit",
                r#"{"session_id":"session-1","prompt":"hello\nworld"}"#,
                PaneEvent::BeginRun {
                    started_at: 123,
                    prompt: Some(PromptState {
                        text: "hello world".to_string(),
                        source: "user".to_string(),
                    }),
                },
            ),
            (
                "Notification",
                r#"{"session_id":"session-1","notification_type":"permission_prompt"}"#,
                PaneEvent::WaitRequested {
                    observed_at: 123,
                    reason: WaitReason::PermissionPrompt,
                },
            ),
            (
                "Stop",
                r#"{"session_id":"session-1"}"#,
                PaneEvent::CompleteRun { completed_at: 123 },
            ),
        ];
        for (hook, payload, expected) in fixtures {
            let envelope = claude_typed_event_from_json(hook, payload, &typed_context())
                .unwrap()
                .unwrap();
            assert_eq!(envelope.agent.unwrap().as_str(), "claude");
            assert_eq!(envelope.agent_session_id.unwrap().as_str(), "session-1");
            assert_eq!(envelope.event, expected);
        }
    }

    #[test]
    fn codex_typed_fixture_maps_supported_lifecycle_events() {
        let fixtures = [
            (
                "UserPromptSubmit",
                r#"{"session_id":"session-2","prompt":"do it"}"#,
                PaneEvent::BeginRun {
                    started_at: 123,
                    prompt: Some(PromptState {
                        text: "do it".to_string(),
                        source: "user".to_string(),
                    }),
                },
            ),
            (
                "PermissionRequest",
                r#"{"session_id":"session-2"}"#,
                PaneEvent::WaitRequested {
                    observed_at: 123,
                    reason: WaitReason::PermissionPrompt,
                },
            ),
            (
                "Stop",
                r#"{"session_id":"session-2"}"#,
                PaneEvent::CompleteRun { completed_at: 123 },
            ),
        ];
        for (hook, payload, expected) in fixtures {
            let envelope =
                codex_typed_event_from_json_with_home(hook, payload, &typed_context(), None)
                    .unwrap()
                    .unwrap();
            assert_eq!(envelope.agent.unwrap().as_str(), "codex");
            assert_eq!(envelope.agent_session_id.unwrap().as_str(), "session-2");
            assert_eq!(envelope.event, expected);
        }
    }

    #[test]
    fn typed_session_start_requires_source_and_session_id() {
        let error = claude_typed_event_from_json(
            "SessionStart",
            r#"{"session_id":"session-1"}"#,
            &typed_context(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("requires startup"));

        let error = codex_typed_event_from_json_with_home(
            "Stop",
            r#"{"agent_id":"not-a-session"}"#,
            &typed_context(),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("requires session_id"));
    }

    #[test]
    fn generic_typed_event_normalizes_fields_and_validates_combinations() {
        let envelope = generic_typed_event(
            GenericEmitInput {
                agent: " Custom.Agent ".to_string(),
                session_id: " session\n1 ".to_string(),
                status: Some("waiting".to_string()),
                wait_reason: Some("other: needs\tinput ".to_string()),
                prompt: Some(" explain\nthis ".to_string()),
                prompt_source: Some(" user\tinput ".to_string()),
                ..GenericEmitInput::default()
            },
            &typed_context(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(envelope.agent.unwrap().as_str(), "custom.agent");
        assert_eq!(envelope.agent_session_id.unwrap().as_str(), "session 1");
        let PaneEvent::ExplicitStateReported { report } = envelope.event else {
            panic!("expected explicit state report");
        };
        assert_eq!(
            report.lifecycle,
            Some(ReportedLifecycle::Waiting {
                reason: WaitReason::Other("needs input".to_string())
            })
        );
        assert_eq!(
            report.prompt,
            Some(FieldUpdate::Set(PromptState {
                text: "explain this".to_string(),
                source: "user input".to_string(),
            }))
        );

        let error = generic_typed_event(
            GenericEmitInput {
                agent: "custom".to_string(),
                session_id: "session".to_string(),
                status: Some("running".to_string()),
                completed_at: Some(123),
                ..GenericEmitInput::default()
            },
            &typed_context(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("--completed-at requires --status idle")
        );
    }

    #[test]
    fn semantic_empty_generic_report_skips_identity_validation() {
        let event = generic_typed_event(GenericEmitInput::default(), &typed_context()).unwrap();
        assert!(event.is_none());
    }
}
