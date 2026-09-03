use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::detect::{PROVIDER_OVERLOADED_REASON, is_provider_overloaded_error};
use crate::hook::origin::{HookOrigin, claude_hook_origin, codex_hook_origin_from_payload};
use crate::pane_state::{
    AgentKind, AgentSessionId, AgentSessionSource, BODY_MAX_BYTES, DaemonInstanceId, EventId,
    ExplicitStateReport, FieldUpdate, PaneEvent, PaneEventEnvelope, PaneInstance,
    ProgressOperation, PromptState, RESPONSE_PREVIEW_MAX_BYTES, ReportedLifecycle, ResponseState,
    SubagentState, TaskProgress as CanonicalTaskProgress, WaitReason, normalize_text,
    validate_subagents,
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
    if origin == HookOrigin::NonParent && is_guarded_claude_lifecycle_event(event) {
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
                    .map(|text| prompt_preview_state(&text, "resume"))
                    .transpose()?
                    .flatten()
            } else {
                None
            },
        },
        "UserPromptSubmit" => PaneEvent::BeginRun {
            started_at: context.observed_at,
            prompt: payload
                .prompt
                .as_deref()
                .map(|text| prompt_preview_state(text, "user"))
                .transpose()?
                .flatten(),
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
        "Stop" => stop_event(
            payload.last_assistant_message.as_deref(),
            context.observed_at,
        ),
        "StopFailure" => claude_stop_failure_event(&payload, context.observed_at)?,
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
    if !origin.is_parent() && is_guarded_codex_lifecycle_event(event) {
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
                    .map(|text| prompt_preview_state(&text, "resume"))
                    .transpose()?
                    .flatten()
            } else {
                None
            },
        },
        "UserPromptSubmit" => PaneEvent::BeginRun {
            started_at: context.observed_at,
            prompt: payload
                .prompt
                .as_deref()
                .map(|text| prompt_preview_state(text, "user"))
                .transpose()?
                .flatten(),
        },
        "PreToolUse" | "PostToolUse" => PaneEvent::ActivityObserved {
            observed_at: context.observed_at,
        },
        "PermissionRequest" => PaneEvent::WaitRequested {
            observed_at: context.observed_at,
            reason: WaitReason::PermissionPrompt,
        },
        "Stop" => stop_event(
            payload.last_assistant_message.as_deref(),
            context.observed_at,
        ),
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
        Some("usage_limit") => Ok(WaitReason::usage_limit()),
        Some(reason) if reason.starts_with("other:") => {
            let reason = normalize_text(&reason["other:".len()..]);
            let parsed = WaitReason::Other(reason);
            parsed.validate()?;
            Ok(parsed)
        }
        _ => bail!(
            "InvalidRequest: waiting status requires permission_prompt, usage_limit, or other:<text> wait reason"
        ),
    }
}

fn prompt_state(text: impl AsRef<str>, source: impl AsRef<str>) -> Result<PromptState> {
    let raw = text.as_ref();
    let prompt = PromptState {
        text: normalize_text(raw),
        source: normalize_text(source.as_ref()),
        digest: Some(PromptState::digest_decoded_prompt(raw)),
    };
    prompt.validate()?;
    Ok(prompt)
}

fn prompt_preview_state(raw: &str, source: &str) -> Result<Option<PromptState>> {
    let Some(text) = build_prompt_preview(raw) else {
        return Ok(None);
    };
    let prompt = PromptState {
        text,
        source: normalize_text(source),
        digest: Some(PromptState::digest_decoded_prompt(raw)),
    };
    prompt.validate()?;
    Ok(Some(prompt))
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
    error: Option<String>,
    error_details: Option<String>,
    hook_event_name: Option<String>,
    last_assistant_message: Option<String>,
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
    last_assistant_message: Option<String>,
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
            | "StopFailure"
            | "Notification"
            | "PreToolUse"
            | "PostToolUse"
    )
}

fn claude_stop_failure_event(payload: &ClaudeHookPayload, observed_at: i64) -> Result<PaneEvent> {
    let error = payload
        .error
        .as_deref()
        .filter(|error| !error.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("InvalidRequest: StopFailure requires error"))?;
    if error == "rate_limit" {
        return Ok(PaneEvent::WaitRequested {
            observed_at,
            reason: WaitReason::usage_limit(),
        });
    }
    let rendered_as_overload = matches!(error, "server_error" | "unknown")
        && [
            payload.error_details.as_deref(),
            payload.last_assistant_message.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(is_provider_overloaded_error);
    let reason = if error == "overloaded" || rendered_as_overload {
        PROVIDER_OVERLOADED_REASON
    } else {
        match error {
            "authentication_failed"
            | "oauth_org_not_allowed"
            | "account_on_hold"
            | "billing_error"
            | "invalid_request"
            | "model_not_found"
            | "server_error"
            | "max_output_tokens"
            | "unknown" => error,
            _ => bail!("InvalidRequest: unsupported Claude StopFailure error {error}"),
        }
    };
    Ok(PaneEvent::FailRun {
        observed_at,
        reason: Some(reason.to_string()),
    })
}

fn is_guarded_codex_lifecycle_event(event: &str) -> bool {
    matches!(
        event,
        "UserPromptSubmit"
            | "SessionStart"
            | "Stop"
            | "PermissionRequest"
            | "PreToolUse"
            | "PostToolUse"
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
    let mut preview = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    if preview.len() > BODY_MAX_BYTES {
        let mut end = BODY_MAX_BYTES;
        while !preview.is_char_boundary(end) {
            end -= 1;
        }
        preview.truncate(end);
        preview.truncate(preview.trim_end().len());
    }
    if preview.is_empty() {
        None
    } else {
        Some(preview)
    }
}

fn stop_event(last_assistant_message: Option<&str>, completed_at: i64) -> PaneEvent {
    match last_assistant_message.and_then(build_response_preview) {
        Some(text) => PaneEvent::ResponseAndCompleteRun {
            completed_at,
            response: ResponseState {
                text,
                observed_at: completed_at,
            },
        },
        None => PaneEvent::CompleteRun { completed_at },
    }
}

fn build_response_preview(raw: &str) -> Option<String> {
    let normalized = raw
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>();
    let mut preview = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    if preview.len() > RESPONSE_PREVIEW_MAX_BYTES {
        let mut end = RESPONSE_PREVIEW_MAX_BYTES;
        while !preview.is_char_boundary(end) {
            end -= 1;
        }
        preview.truncate(end);
        preview.truncate(preview.trim_end().len());
    }
    (!preview.is_empty()).then_some(preview)
}

fn latest_user_prompt_from_transcript(path: &str) -> Option<String> {
    let file = std::fs::File::open(Path::new(path)).ok()?;
    let reader = std::io::BufReader::new(file);
    reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter_map(|value| user_prompt_from_transcript_value(&value))
        .filter(|prompt| build_prompt_preview(prompt).is_some())
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
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

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
                        digest: Some(
                            "3cf479a04899c793e4faf30c5b150c0c6e0aca73f52780ec274168e795a9634b"
                                .to_string(),
                        ),
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
    fn claude_stop_failure_maps_rate_limit_to_usage_limit_wait() {
        let payload = r#"{"session_id":"session-1","error":"rate_limit","error_details":"429 Too Many Requests","last_assistant_message":"API Error: Rate limit reached"}"#;
        let envelope = claude_typed_event_from_json("StopFailure", payload, &typed_context())
            .unwrap()
            .unwrap();

        assert_eq!(
            envelope.event,
            PaneEvent::WaitRequested {
                observed_at: 123,
                reason: WaitReason::usage_limit(),
            }
        );
    }

    #[test]
    fn claude_stop_failure_maps_overloads_to_provider_overloaded() {
        for payload in [
            r#"{"session_id":"session-1","error":"overloaded"}"#,
            r#"{"session_id":"session-1","error":"server_error","error_details":"529 Overloaded","last_assistant_message":"API Error: 529 Overloaded"}"#,
        ] {
            let envelope = claude_typed_event_from_json("StopFailure", payload, &typed_context())
                .unwrap()
                .unwrap();

            assert_eq!(
                envelope.event,
                PaneEvent::FailRun {
                    observed_at: 123,
                    reason: Some(PROVIDER_OVERLOADED_REASON.to_string()),
                }
            );
        }
    }

    #[test]
    fn claude_stop_failure_maps_other_official_errors_to_failed_runs() {
        for error in [
            "authentication_failed",
            "oauth_org_not_allowed",
            "account_on_hold",
            "billing_error",
            "invalid_request",
            "model_not_found",
            "server_error",
            "max_output_tokens",
            "unknown",
        ] {
            let payload = serde_json::json!({
                "session_id": "session-1",
                "error": error,
            })
            .to_string();
            let envelope = claude_typed_event_from_json("StopFailure", &payload, &typed_context())
                .unwrap()
                .unwrap();

            assert_eq!(
                envelope.event,
                PaneEvent::FailRun {
                    observed_at: 123,
                    reason: Some(error.to_string()),
                }
            );
        }
    }

    #[test]
    fn claude_stop_failure_does_not_override_specific_errors_from_rendered_text() {
        let payload = r#"{"session_id":"session-1","error":"billing_error","last_assistant_message":"API Error: 529 Overloaded"}"#;
        let envelope = claude_typed_event_from_json("StopFailure", payload, &typed_context())
            .unwrap()
            .unwrap();

        assert_eq!(
            envelope.event,
            PaneEvent::FailRun {
                observed_at: 123,
                reason: Some("billing_error".to_string()),
            }
        );
    }

    #[test]
    fn generic_wait_reason_accepts_usage_limit_as_a_first_class_value() {
        let reason = parse_wait_reason(Some("usage_limit")).unwrap();
        assert!(reason.is_usage_limit());
    }

    #[test]
    fn claude_user_prompt_submit_truncates_large_task_notification() {
        let prompt = format!(
            "<task-notification>{}</task-notification>",
            "x".repeat(BODY_MAX_BYTES)
        );
        let payload = serde_json::json!({
            "session_id": "session-1",
            "prompt": prompt,
        })
        .to_string();

        let envelope = claude_typed_event_from_json("UserPromptSubmit", &payload, &typed_context())
            .unwrap()
            .unwrap();
        let PaneEvent::BeginRun {
            prompt: Some(prompt),
            ..
        } = envelope.event
        else {
            panic!("expected begin-run prompt");
        };

        assert_eq!(prompt.text.len(), BODY_MAX_BYTES);
        prompt.validate().unwrap();
    }

    #[test]
    fn prompt_preview_truncates_at_utf8_boundary() {
        let preview = build_prompt_preview(&"界".repeat(BODY_MAX_BYTES)).unwrap();

        assert!(preview.len() <= BODY_MAX_BYTES);
        assert_eq!(preview.len(), BODY_MAX_BYTES - 1);
        assert!(preview.chars().all(|character| character == '界'));
    }

    #[test]
    fn prompt_digest_uses_decoded_bytes_before_preview_normalization() {
        let normalized = prompt_preview_state("hello world", "user")
            .unwrap()
            .unwrap();
        let multiline = prompt_preview_state("hello\nworld", "user")
            .unwrap()
            .unwrap();

        assert_eq!(normalized.text, multiline.text);
        assert_ne!(normalized.digest, multiline.digest);
        assert_eq!(
            multiline.digest.as_deref(),
            Some("3cf479a04899c793e4faf30c5b150c0c6e0aca73f52780ec274168e795a9634b")
        );
    }

    #[test]
    fn codex_typed_fixture_maps_supported_lifecycle_events() {
        let root = codex_root_session("typed-fixtures", "session-2");
        let transcript = root
            .join("sessions")
            .join("2026")
            .join("08")
            .join("14")
            .join("rollout-session-2.jsonl");
        let fixtures = [
            (
                "PreToolUse",
                r#"{"session_id":"session-2","tool_name":"exec"}"#,
                PaneEvent::ActivityObserved { observed_at: 123 },
            ),
            (
                "PostToolUse",
                r#"{"session_id":"session-2","tool_name":"wait"}"#,
                PaneEvent::ActivityObserved { observed_at: 123 },
            ),
            (
                "UserPromptSubmit",
                r#"{"session_id":"session-2","prompt":"do it"}"#,
                PaneEvent::BeginRun {
                    started_at: 123,
                    prompt: Some(PromptState {
                        text: "do it".to_string(),
                        source: "user".to_string(),
                        digest: Some(
                            "e9de5641495b8879a8d6b829979d53ae024f7f236b82b3ae9b26e6587d2b7087"
                                .to_string(),
                        ),
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
            let mut payload = serde_json::from_str::<Value>(payload).unwrap();
            payload["transcript_path"] = Value::String(transcript.display().to_string());
            let envelope = codex_typed_event_from_json_with_home(
                hook,
                &payload.to_string(),
                &typed_context(),
                Some(&root),
            )
            .unwrap()
            .unwrap();
            assert_eq!(envelope.agent.unwrap().as_str(), "codex");
            assert_eq!(envelope.agent_session_id.unwrap().as_str(), "session-2");
            assert_eq!(envelope.event, expected);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn codex_unverified_lifecycle_hooks_are_ignored() {
        let root = unique_temp_dir("codex-unverified-lifecycle");
        let payload = r#"{"session_id":"internal-session","source":"startup","prompt":"hidden","tool_name":"exec"}"#;

        for hook in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PermissionRequest",
            "Stop",
        ] {
            assert!(
                codex_typed_event_from_json_with_home(
                    hook,
                    payload,
                    &typed_context(),
                    Some(&root),
                )
                .unwrap()
                .is_none(),
                "{hook} should not mutate lifecycle for an unverified session"
            );
        }
    }

    #[test]
    fn stop_payloads_store_one_line_response_previews() {
        let claude_payload =
            r#"{"session_id":"session-1","last_assistant_message":"done\nfor claude"}"#;
        let claude = claude_typed_event_from_json("Stop", claude_payload, &typed_context())
            .unwrap()
            .unwrap();
        assert_response_completion(claude, "claude");

        let root = codex_root_session("stop-preview", "session-1");
        let codex_payload = format!(
            r#"{{"session_id":"session-1","transcript_path":"{}","last_assistant_message":"done\nfor codex"}}"#,
            root.join("sessions/2026/08/14/rollout-session-1.jsonl")
                .display()
        );
        let codex = codex_typed_event_from_json_with_home(
            "Stop",
            &codex_payload,
            &typed_context(),
            Some(&root),
        )
        .unwrap()
        .unwrap();
        assert_response_completion(codex, "codex");
        fs::remove_dir_all(root).unwrap();
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

        let root = codex_root_session("missing-session-id", "session-1");
        let payload = format!(
            r#"{{"transcript_path":"{}","last_assistant_message":"done"}}"#,
            root.join("sessions/2026/08/14/rollout-session-1.jsonl")
                .display()
        );
        let error =
            codex_typed_event_from_json_with_home("Stop", &payload, &typed_context(), Some(&root))
                .unwrap_err();
        assert!(error.to_string().contains("requires session_id"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resumed_prompt_digest_uses_raw_transcript_text() {
        let root = unique_temp_dir("resume-prompt-digest");
        fs::create_dir_all(&root).unwrap();
        let transcript = root.join("transcript.jsonl");
        fs::write(
            &transcript,
            serde_json::json!({"role": "user", "content": "continue\nraw"}).to_string(),
        )
        .unwrap();
        let payload = serde_json::json!({
            "session_id": "session-1",
            "source": "resume",
            "transcript_path": transcript,
        })
        .to_string();

        let envelope = claude_typed_event_from_json("SessionStart", &payload, &typed_context())
            .unwrap()
            .unwrap();
        let PaneEvent::AgentSessionStarted {
            resumed_prompt: Some(prompt),
            ..
        } = envelope.event
        else {
            panic!("expected resumed prompt");
        };
        assert_eq!(prompt.text, "continue raw");
        assert_eq!(
            prompt.digest,
            Some(PromptState::digest_decoded_prompt("continue\nraw"))
        );

        fs::remove_dir_all(root).unwrap();
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
                digest: Some(PromptState::digest_decoded_prompt(" explain\nthis ")),
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
    fn generic_typed_event_rejects_invalid_field_lifecycle_combinations() {
        let cases = [
            (
                GenericEmitInput {
                    prompt: Some("prompt".to_string()),
                    prompt_source: Some("user".to_string()),
                    clear_prompt: true,
                    ..GenericEmitInput::default()
                },
                "--prompt and --clear-prompt are mutually exclusive",
            ),
            (
                GenericEmitInput {
                    status: Some("idle".to_string()),
                    started_at: Some(123),
                    ..GenericEmitInput::default()
                },
                "--started-at requires --status running",
            ),
            (
                GenericEmitInput {
                    status: Some("running".to_string()),
                    wait_reason: Some("permission".to_string()),
                    ..GenericEmitInput::default()
                },
                "--wait-reason requires --status waiting",
            ),
            (
                GenericEmitInput {
                    status: Some("running".to_string()),
                    attention: true,
                    ..GenericEmitInput::default()
                },
                "--attention requires --status idle",
            ),
            (
                GenericEmitInput {
                    status: Some("running".to_string()),
                    completed_at: Some(123),
                    ..GenericEmitInput::default()
                },
                "--completed-at requires --status idle",
            ),
            (
                GenericEmitInput {
                    prompt: Some("prompt".to_string()),
                    ..GenericEmitInput::default()
                },
                "--prompt requires exactly one non-empty --prompt-source",
            ),
            (
                GenericEmitInput {
                    prompt_source: Some("user".to_string()),
                    ..GenericEmitInput::default()
                },
                "--prompt requires exactly one non-empty --prompt-source",
            ),
            (
                GenericEmitInput {
                    tasks: Some(CanonicalTaskProgress { done: 0, total: 1 }),
                    clear_tasks: true,
                    ..GenericEmitInput::default()
                },
                "--tasks and --clear-tasks are mutually exclusive",
            ),
            (
                GenericEmitInput {
                    subagents: Some(Vec::new()),
                    clear_subagents: true,
                    ..GenericEmitInput::default()
                },
                "--subagents and --clear-subagents are mutually exclusive",
            ),
        ];

        for (input, expected) in cases {
            let error = generic_typed_event(input, &typed_context()).unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "expected {expected:?}, got {error:#}"
            );
        }
    }

    #[test]
    fn generic_typed_event_maps_task_and_subagent_field_updates() {
        let envelope = generic_typed_event(
            GenericEmitInput {
                agent: "custom".to_string(),
                session_id: "session".to_string(),
                tasks: Some(CanonicalTaskProgress { done: 2, total: 3 }),
                subagents: Some(vec![SubagentState {
                    agent_id: " worker\n1 ".to_string(),
                    agent_type: " reviewer\t".to_string(),
                    display_name: Some(" Review\nAgent ".to_string()),
                }]),
                ..GenericEmitInput::default()
            },
            &typed_context(),
        )
        .unwrap()
        .unwrap();
        let PaneEvent::ExplicitStateReported { report } = envelope.event else {
            panic!("expected explicit state report");
        };
        assert_eq!(
            report.tasks,
            Some(FieldUpdate::Set(CanonicalTaskProgress {
                done: 2,
                total: 3,
            }))
        );
        assert_eq!(
            report.subagents,
            Some(FieldUpdate::Set(vec![SubagentState {
                agent_id: "worker 1".to_string(),
                agent_type: "reviewer".to_string(),
                display_name: Some("Review Agent".to_string()),
            }]))
        );

        let envelope = generic_typed_event(
            GenericEmitInput {
                agent: "custom".to_string(),
                session_id: "session".to_string(),
                clear_tasks: true,
                clear_subagents: true,
                ..GenericEmitInput::default()
            },
            &typed_context(),
        )
        .unwrap()
        .unwrap();
        let PaneEvent::ExplicitStateReported { report } = envelope.event else {
            panic!("expected explicit state report");
        };
        assert_eq!(report.tasks, Some(FieldUpdate::Clear));
        assert_eq!(report.subagents, Some(FieldUpdate::Clear));
    }

    #[test]
    fn semantic_empty_generic_report_skips_identity_validation() {
        let event = generic_typed_event(GenericEmitInput::default(), &typed_context()).unwrap();
        assert!(event.is_none());
    }

    fn assert_response_completion(envelope: PaneEventEnvelope, agent: &str) {
        let PaneEvent::ResponseAndCompleteRun {
            completed_at,
            response,
        } = envelope.event
        else {
            panic!("expected response completion for {agent}");
        };
        assert_eq!(completed_at, 123);
        assert_eq!(response.text, format!("done for {agent}"));
        assert_eq!(response.observed_at, 123);
    }

    fn codex_root_session(name: &str, session_id: &str) -> PathBuf {
        let root = unique_temp_dir(name);
        let sessions = root.join("sessions").join("2026").join("08").join("14");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join(format!("rollout-{session_id}.jsonl")),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{session_id}","thread_source":"user"}}}}"#
            ),
        )
        .unwrap();
        root
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vde-tmux-{name}-{}-{nanos}", std::process::id()))
    }
}
