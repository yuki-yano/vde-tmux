use crate::hook::AgentStatus;

pub fn detect_codex_wait_reason(screen_tail: &str) -> Option<&'static str> {
    let lines = screen_tail
        .lines()
        .map(|line| line.trim().to_ascii_lowercase())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    for (index, line) in lines.iter().enumerate() {
        if !looks_like_permission_question(line) {
            continue;
        }
        if lines
            .iter()
            .skip(index + 1)
            .take(3)
            .any(|candidate| looks_like_yes_choice(candidate))
        {
            return Some("permission_prompt");
        }
    }
    None
}

fn looks_like_permission_question(line: &str) -> bool {
    let asks_for_permission =
        line.contains("allow") || line.contains("approve") || line.contains("permission");
    let mentions_action = line.contains("command")
        || line.contains("edit")
        || line.contains("write")
        || line.contains("tool")
        || line.contains("bash")
        || line.contains("use")
        || line.contains("run")
        || line.contains("execute");
    (asks_for_permission && mentions_action && line.contains('?'))
        || line.contains("do you want to proceed?")
}

fn looks_like_yes_choice(line: &str) -> bool {
    let normalized = line
        .trim_start_matches(|ch: char| {
            ch.is_whitespace() || ch == '-' || ch == '*' || ch == '>' || ch == '❯'
        })
        .trim_start_matches(|ch: char| ch.is_ascii_digit() || ch == '.' || ch == ')' || ch == '-')
        .trim();
    normalized == "yes"
        || normalized.starts_with("yes ")
        || normalized.starts_with("y) yes")
        || normalized.starts_with("y - yes")
        || normalized.starts_with("[y] yes")
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
    fn does_not_detect_yes_when_permission_question_is_not_adjacent() {
        let text = "Allow command to run?\nnoise\nmore noise\nunrelated summary: yes\n";
        assert_eq!(detect_codex_wait_reason(text), None);
    }

    #[test]
    fn detects_codex_permission_prompt_with_adjacent_choice() {
        let text = "? Allow command to run?\n  y) yes\n  n) no\n";
        assert_eq!(detect_codex_wait_reason(text), Some("permission_prompt"));
    }

    #[test]
    fn detects_claude_permission_prompt_with_numbered_yes_choice() {
        let text = "Claude needs your permission to use Bash\nDo you want to proceed?\n❯ 1. Yes\n  2. No\n";
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
