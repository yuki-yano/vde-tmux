use std::io::BufRead;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::hook::{AgentEvent, AgentStatus, OptionUpdate};

#[derive(Debug, Deserialize, Default)]
struct ClaudeHookPayload {
    notification_type: Option<String>,
    prompt: Option<String>,
    source: Option<String>,
    transcript_path: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct CodexHookPayload {
    prompt: Option<String>,
    source: Option<String>,
    transcript_path: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct CodexNotifyPayload {
    #[serde(rename = "type")]
    kind: Option<String>,
}

pub fn claude_event_from_json(event: &str, raw_json: &str, now_epoch: i64) -> Result<AgentEvent> {
    let payload: ClaudeHookPayload = serde_json::from_str(raw_json.trim()).unwrap_or_default();
    let mut agent_event = AgentEvent {
        agent: "claude".to_string(),
        ..AgentEvent::default()
    };
    match event {
        "Notification" if payload.notification_type.as_deref() == Some("permission_prompt") => {
            agent_event.status = Some(AgentStatus::Waiting);
            agent_event.wait_reason = Some(OptionUpdate::Set("permission_prompt".to_string()));
        }
        "Notification" => {}
        "Stop" => {
            agent_event.status = Some(AgentStatus::Idle);
            agent_event.attention = Some(true);
            agent_event.completed_at = Some(now_epoch);
            agent_event.subagents = Some(OptionUpdate::Unset);
        }
        "UserPromptSubmit" => {
            agent_event.status = Some(AgentStatus::Running);
            agent_event.started_at = Some(now_epoch);
            if let Some(prompt) = payload
                .prompt
                .and_then(|prompt| build_prompt_preview(&prompt))
            {
                agent_event.prompt = Some(OptionUpdate::Set(prompt));
                agent_event.prompt_source = Some(OptionUpdate::Set("user".to_string()));
            }
            agent_event.tasks = Some(OptionUpdate::Unset);
            agent_event.subagents = Some(OptionUpdate::Unset);
        }
        "PreToolUse" | "PostToolUse" => {
            agent_event.status = Some(AgentStatus::Running);
        }
        "SessionStart" => {
            apply_session_start(&mut agent_event, payload.source, payload.transcript_path);
        }
        _ => {
            agent_event.agent.clear();
        }
    }
    Ok(agent_event)
}

pub fn codex_event_from_json(event: &str, raw_json: &str, now_epoch: i64) -> Result<AgentEvent> {
    let payload: CodexHookPayload = serde_json::from_str(raw_json.trim()).unwrap_or_default();
    let mut agent_event = AgentEvent {
        agent: "codex".to_string(),
        ..AgentEvent::default()
    };
    match event {
        "PermissionRequest" => {
            agent_event.status = Some(AgentStatus::Waiting);
            agent_event.wait_reason = Some(OptionUpdate::Set("permission_prompt".to_string()));
        }
        "Stop" => {
            agent_event.status = Some(AgentStatus::Idle);
            agent_event.attention = Some(true);
            agent_event.completed_at = Some(now_epoch);
        }
        "UserPromptSubmit" => {
            agent_event.status = Some(AgentStatus::Running);
            agent_event.started_at = Some(now_epoch);
            if let Some(prompt) = payload
                .prompt
                .and_then(|prompt| build_prompt_preview(&prompt))
            {
                agent_event.prompt = Some(OptionUpdate::Set(prompt));
                agent_event.prompt_source = Some(OptionUpdate::Set("user".to_string()));
            }
        }
        "SessionStart" => {
            apply_session_start(&mut agent_event, payload.source, payload.transcript_path);
        }
        _ => {
            agent_event.agent.clear();
        }
    }
    Ok(agent_event)
}

pub fn codex_notify_event_from_arg(raw_json: &str, now_epoch: i64) -> Result<AgentEvent> {
    let payload: CodexNotifyPayload = serde_json::from_str(raw_json.trim()).unwrap_or_default();
    let mut agent_event = AgentEvent {
        agent: "codex".to_string(),
        ..AgentEvent::default()
    };
    match payload.kind.as_deref() {
        Some("agent-turn-complete") => {
            agent_event.status = Some(AgentStatus::Idle);
            agent_event.attention = Some(true);
            agent_event.completed_at = Some(now_epoch);
        }
        _ => {
            agent_event.agent.clear();
        }
    }
    Ok(agent_event)
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

fn apply_session_start(
    agent_event: &mut AgentEvent,
    source: Option<String>,
    transcript_path: Option<String>,
) {
    match source.as_deref() {
        Some("startup" | "resume" | "clear") => {
            agent_event.clear_state = true;
            agent_event.status = Some(AgentStatus::Idle);
            agent_event.attention = Some(false);
            if source.as_deref() == Some("resume")
                && let Some(prompt) = transcript_path
                    .as_deref()
                    .and_then(latest_user_prompt_from_transcript)
            {
                agent_event.prompt = Some(OptionUpdate::Set(prompt));
                agent_event.prompt_source = Some(OptionUpdate::Set("resume".to_string()));
            }
        }
        _ => {
            agent_event.agent.clear();
        }
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
    use crate::hook::{AgentStatus, OptionUpdate};

    #[test]
    fn claude_user_prompt_submit_builds_running_event() {
        let event =
            claude_event_from_json("UserPromptSubmit", r#"{"prompt":"hello\nworld\t!"}"#, 123)
                .unwrap();
        assert_eq!(event.agent, "claude");
        assert_eq!(event.status, Some(AgentStatus::Running));
        assert_eq!(event.started_at, Some(123));
        assert_eq!(
            event.prompt,
            Some(OptionUpdate::Set("hello world !".into()))
        );
        assert_eq!(event.prompt_source, Some(OptionUpdate::Set("user".into())));
    }

    #[test]
    fn claude_notification_permission_builds_waiting_event() {
        let event = claude_event_from_json(
            "Notification",
            r#"{"notification_type":"permission_prompt"}"#,
            123,
        )
        .unwrap();
        assert_eq!(event.agent, "claude");
        assert_eq!(event.status, Some(AgentStatus::Waiting));
        assert_eq!(
            event.wait_reason,
            Some(OptionUpdate::Set("permission_prompt".into()))
        );
    }

    #[test]
    fn codex_permission_request_builds_waiting_event() {
        let event = codex_event_from_json("PermissionRequest", "{}", 123).unwrap();
        assert_eq!(event.agent, "codex");
        assert_eq!(event.status, Some(AgentStatus::Waiting));
        assert_eq!(
            event.wait_reason,
            Some(OptionUpdate::Set("permission_prompt".into()))
        );
    }

    #[test]
    fn codex_tool_use_events_do_not_start_running_state() {
        for hook in ["PreToolUse", "PostToolUse"] {
            let event = codex_event_from_json(hook, "{}", 123).unwrap();
            assert_eq!(event, AgentEvent::default());
        }
    }

    #[test]
    fn codex_notify_turn_complete_builds_idle_event() {
        let event = codex_notify_event_from_arg(r#"{"type":"agent-turn-complete"}"#, 456).unwrap();
        assert_eq!(event.agent, "codex");
        assert_eq!(event.status, Some(AgentStatus::Idle));
        assert_eq!(event.completed_at, Some(456));
        assert_eq!(event.attention, Some(true));
    }

    #[test]
    fn codex_session_start_resume_clears_state_and_reads_latest_prompt_from_transcript() {
        let path = write_temp_transcript(
            "codex-session-start",
            &[
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"old prompt"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"latest\nprompt"}]}}"#,
            ],
        );
        let raw = format!(
            r#"{{"source":"resume","transcript_path":{}}}"#,
            serde_json::to_string(path.to_str().unwrap()).unwrap()
        );

        let event = codex_event_from_json("SessionStart", &raw, 123).unwrap();

        assert!(event.clear_state);
        assert_eq!(event.agent, "codex");
        assert_eq!(event.status, Some(AgentStatus::Idle));
        assert_eq!(event.attention, Some(false));
        assert_eq!(
            event.prompt,
            Some(OptionUpdate::Set("latest prompt".to_string()))
        );
        assert_eq!(
            event.prompt_source,
            Some(OptionUpdate::Set("resume".to_string()))
        );
    }

    #[test]
    fn claude_session_start_resume_reads_message_content_from_transcript() {
        let path = write_temp_transcript(
            "claude-session-start",
            &[
                r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"claude prompt"}]}}"#,
            ],
        );
        let raw = format!(
            r#"{{"source":"resume","transcript_path":{}}}"#,
            serde_json::to_string(path.to_str().unwrap()).unwrap()
        );

        let event = claude_event_from_json("SessionStart", &raw, 123).unwrap();

        assert!(event.clear_state);
        assert_eq!(event.agent, "claude");
        assert_eq!(
            event.prompt,
            Some(OptionUpdate::Set("claude prompt".to_string()))
        );
        assert_eq!(
            event.prompt_source,
            Some(OptionUpdate::Set("resume".to_string()))
        );
    }

    fn write_temp_transcript(name: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "vde-tmux-{name}-{}.jsonl",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }
}
