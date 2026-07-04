use crate::hook::AgentStatus;

pub fn detect_codex_wait_reason(screen_tail: &str) -> Option<&'static str> {
    let lower = screen_tail.to_ascii_lowercase();
    let asks_permission = lower.contains("allow")
        && (lower.contains("command") || lower.contains("edit") || lower.contains("write"));
    let has_choices = lower.contains("yes") || lower.contains("y)");
    (asks_permission && has_choices).then_some("permission_prompt")
}

pub fn demote_stale_running(
    status: Option<AgentStatus>,
    last_activity_epoch: i64,
    now_epoch: i64,
    threshold_seconds: i64,
) -> Option<AgentStatus> {
    if status == Some(AgentStatus::Running)
        && threshold_seconds >= 0
        && now_epoch.saturating_sub(last_activity_epoch) > threshold_seconds
    {
        Some(AgentStatus::Idle)
    } else {
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::AgentStatus;

    #[test]
    fn detects_codex_permission_prompt_from_screen_tail() {
        let text = "some output\n? Allow command to run?\n  y) yes\n  n) no\n";
        assert_eq!(detect_codex_wait_reason(text), Some("permission_prompt"));
    }

    #[test]
    fn stale_running_demotes_to_idle_for_display() {
        let status = demote_stale_running(Some(AgentStatus::Running), 100, 200, 30);
        assert_eq!(status, Some(AgentStatus::Idle));
        let fresh = demote_stale_running(Some(AgentStatus::Running), 190, 200, 30);
        assert_eq!(fresh, Some(AgentStatus::Running));
    }
}
