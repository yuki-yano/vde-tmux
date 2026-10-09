use std::collections::BTreeMap;

use serde_json::Value;

use crate::pane_state::{
    ClaudeTaskNotification, ClaudeToolResult, MAX_BACKGROUND_TASKS, valid_background_id,
};

/// All envelopes must consume the entire prompt; surrounding prose is not a receipt.
pub fn task_notifications(prompt: &str) -> Option<Vec<ClaudeTaskNotification>> {
    let mut rest = prompt.trim();
    let mut notifications = Vec::new();
    while !rest.is_empty() {
        rest.strip_prefix("<task-notification>")?;
        let end = rest.find("</task-notification>")? + "</task-notification>".len();
        notifications.push(task_notification(&rest[..end])?);
        if notifications.len() > MAX_BACKGROUND_TASKS {
            return None;
        }
        rest = rest[end..].trim();
    }
    (!notifications.is_empty()).then_some(notifications)
}

/// Parse one complete envelope, never an ID quoted in ordinary prose.
pub fn task_notification(prompt: &str) -> Option<ClaudeTaskNotification> {
    let mut inner = prompt
        .trim()
        .strip_prefix("<task-notification>")?
        .strip_suffix("</task-notification>")?
        .trim();
    let mut fields = BTreeMap::new();
    while !inner.is_empty() {
        let rest = inner.strip_prefix('<')?;
        let end = rest.find('>')?;
        let name = &rest[..end];
        if !matches!(
            name,
            "task-id" | "tool-use-id" | "status" | "summary" | "output-file"
        ) {
            return None;
        }
        let rest = &rest[end + 1..];
        let closing = format!("</{name}>");
        let end = rest.find(&closing)?;
        let content = &rest[..end];
        if matches!(name, "task-id" | "tool-use-id" | "status") && content.contains('<')
            || fields.insert(name, content).is_some()
        {
            return None;
        }
        inner = rest[end + closing.len()..].trim();
    }
    let task_id = fields.get("task-id")?.trim();
    let tool_use_id = fields.get("tool-use-id")?.trim();
    if !valid_background_id(task_id) || !valid_background_id(tool_use_id) {
        return None;
    }
    Some(ClaudeTaskNotification {
        task_id: task_id.into(),
        tool_use_id: tool_use_id.into(),
        status: fields.get("status").map(|s| s.trim().to_string()),
    })
}

pub fn tool_result(payload: &Value) -> Option<ClaudeToolResult> {
    let response = payload.get("tool_response")?;
    match payload.get("tool_name")?.as_str()? {
        "Bash" => {
            let command = payload.get("tool_input")?.get("command")?.as_str()?;
            if !crate::cli::is_literal_agent_wait(command) {
                return None;
            }
            let tool_use_id = payload.get("tool_use_id")?.as_str()?;
            if !valid_background_id(tool_use_id) {
                return None;
            }
            let id = response
                .get("backgroundTaskId")
                .filter(|id| !id.is_null())?;
            Some(match id.as_str().filter(|id| valid_background_id(id)) {
                Some(task_id) => ClaudeToolResult::Registered {
                    task_id: task_id.into(),
                    tool_use_id: tool_use_id.into(),
                    command: command.into(),
                },
                None => ClaudeToolResult::LaunchUnconfirmed {
                    tool_use_id: tool_use_id.into(),
                },
            })
        }
        "TaskStop" if response.get("task_type")?.as_str()? == "local_bash" => {
            let task_id = response.get("task_id")?.as_str()?;
            if !valid_background_id(task_id) {
                return None;
            }
            Some(ClaudeToolResult::TaskStopped {
                task_id: task_id.into(),
                command: response.get("command")?.as_str()?.into(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_envelope_and_required_identity() {
        let s = "<task-notification>\n<task-id>b123</task-id><tool-use-id>tool123</tool-use-id><status>killed</status><summary>Stopped</summary></task-notification>";
        assert_eq!(
            task_notification(s).unwrap().status.as_deref(),
            Some("killed")
        );
        assert!(task_notification(&format!("quote {s}")).is_none());
        assert!(task_notification(&format!("{s} ordinary text")).is_none());
        assert!(task_notification(&s.replace("<task-id>b123</task-id>", "")).is_none());
        assert!(
            task_notification(&s.replace("<status>killed</status>", ""))
                .unwrap()
                .status
                .is_none()
        );
        assert!(
            task_notification(&s.replace(
                "<status>killed</status>",
                "<status>killed</status><status>failed</status>"
            ))
            .is_none()
        );
    }

    #[test]
    fn summaries_allow_raw_angles_and_notifications_can_arrive_together() {
        let one = "<task-notification><task-id>a</task-id><tool-use-id>t-a</tool-use-id><status>completed</status><summary>description <a> with markup</summary></task-notification>";
        let two = one
            .replace("<task-id>a", "<task-id>b")
            .replace("t-a", "t-b");
        assert_eq!(
            task_notifications(&format!("{one}\n{two}")).unwrap().len(),
            2
        );
        assert!(task_notifications(&format!("quoted {one}")).is_none());
        assert!(task_notifications(&format!("{one} ordinary text {two}")).is_none());
        assert!(task_notifications(&one.replace("t-a", "<t-a>")).is_none());
    }
}
