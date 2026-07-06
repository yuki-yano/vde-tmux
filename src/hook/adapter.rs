//! Claude/Codex hook payload を AgentEvent に変換する。

use anyhow::Result;
use serde::Deserialize;

use crate::hook::{AgentEvent, AgentStatus, OptionUpdate};

#[derive(Debug, Deserialize, Default)]
struct ClaudeHookPayload {
    notification_type: Option<String>,
    prompt: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct CodexHookPayload {
    prompt: Option<String>,
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
        "PreToolUse" | "PostToolUse" => {
            agent_event.status = Some(AgentStatus::Running);
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
    fn codex_notify_turn_complete_builds_idle_event() {
        let event = codex_notify_event_from_arg(r#"{"type":"agent-turn-complete"}"#, 456).unwrap();
        assert_eq!(event.agent, "codex");
        assert_eq!(event.status, Some(AgentStatus::Idle));
        assert_eq!(event.completed_at, Some(456));
        assert_eq!(event.attention, Some(true));
    }
}
