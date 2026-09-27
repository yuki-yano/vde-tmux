pub mod codex;

// capture-pane may include deeper scrollback; only recent screen lines should drive waiting state.
const WAIT_REASON_SCAN_TAIL_LINES: usize = 30;

pub const PROVIDER_API_ERROR_REASON: &str = "provider_api_error";
pub const PROVIDER_OVERLOADED_REASON: &str = "provider_overloaded";

pub fn detect_usage_limit(screen_tail: &str) -> bool {
    recent_wait_reason_lines(screen_tail).iter().any(|line| {
        let line = line
            .trim_start_matches(['•', '■', '*', '-', '!', '✕', '│', '╰', '─', '⎿'])
            .trim();
        line.starts_with("you've hit your session limit")
            || line.starts_with("you've hit your usage limit")
    })
}

pub fn detect_claude_terminal_error(screen_tail: &str) -> Option<&'static str> {
    let lines = recent_claude_screen_lines(screen_tail);
    let latest_prompt_index = lines.iter().rposition(|line| line.starts_with('❯'))?;
    if lines[latest_prompt_index] != "❯" {
        return None;
    }
    let turn_done_index = lines[..latest_prompt_index]
        .iter()
        .rposition(|line| !is_claude_prompt_separator(line))?;
    if !looks_like_claude_turn_done(&lines[turn_done_index]) {
        return None;
    }
    let latest_assistant_line = lines[..turn_done_index]
        .iter()
        .rev()
        .find(|line| line.starts_with('⏺'))?;
    let error = latest_assistant_line
        .strip_prefix('⏺')?
        .trim_start()
        .strip_prefix("api error:")?
        .trim_start();

    Some(if is_provider_overloaded_error(error) {
        PROVIDER_OVERLOADED_REASON
    } else {
        PROVIDER_API_ERROR_REASON
    })
}

pub fn is_provider_overloaded_error(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    normalized.contains("529") && normalized.contains("overloaded")
}

fn recent_wait_reason_lines(screen_tail: &str) -> Vec<String> {
    let raw_lines = screen_tail.lines().collect::<Vec<_>>();
    let scan_start = raw_lines.len().saturating_sub(WAIT_REASON_SCAN_TAIL_LINES);
    raw_lines[scan_start..]
        .iter()
        .map(|line| line.trim().to_ascii_lowercase().replace(['’', '‘'], "'"))
        .filter(|line| !line.is_empty())
        .collect()
}

fn recent_claude_screen_lines(screen_tail: &str) -> Vec<String> {
    let raw_lines = screen_tail.lines().collect::<Vec<_>>();
    let scan_start = raw_lines.len().saturating_sub(WAIT_REASON_SCAN_TAIL_LINES);
    raw_lines[scan_start..]
        .iter()
        .map(|line| line.trim_end().to_ascii_lowercase())
        .filter(|line| !line.is_empty())
        .collect()
}

fn is_claude_prompt_separator(line: &str) -> bool {
    !line.is_empty() && line.chars().all(|ch| ch == '─')
}

fn looks_like_claude_turn_done(line: &str) -> bool {
    line.starts_with(['✻', '✽', '✶', '✢', '✳']) && line.contains(" · done ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_usage_limit_variants() {
        for text in [
            "background agent failed\nYou've hit your session limit · resets 5:40pm\n",
            "  ⎿ \u{a0}You've hit your session limit · resets 12am (Asia/Tokyo)\n",
            "■ You’ve hit your usage limit. Try again at 6:15 PM.\n",
        ] {
            assert!(detect_usage_limit(text), "{text:?}");
        }
    }

    #[test]
    fn does_not_treat_generic_rate_limit_or_status_text_as_usage_limit() {
        let text = "rate limit exceeded\nusage remaining: 3%\ncontext window limit warning\n";
        assert!(!detect_usage_limit(text));
    }

    #[test]
    fn detects_claude_overload_after_the_turn_returns_to_an_idle_prompt() {
        let text = "\
⏺ API Error: 529 Overloaded. This is a server-side issue, usually temporary.\n\
✻ Brewed for 6m 27s · done 22:28\n\
────────\n\
❯ \n";

        assert_eq!(
            detect_claude_terminal_error(text),
            Some(PROVIDER_OVERLOADED_REASON)
        );
    }

    #[test]
    fn detects_other_claude_api_errors_without_exposing_provider_text() {
        let text = "⏺ API Error: authentication failed\n✻ Worked for 2s · done 22:28\n❯\n";

        assert_eq!(
            detect_claude_terminal_error(text),
            Some(PROVIDER_API_ERROR_REASON)
        );
    }

    #[test]
    fn does_not_treat_an_api_error_as_terminal_while_claude_is_still_working() {
        let no_idle_prompt = "⏺ API Error: 529 Overloaded\n✻ Working…\n❯\n";
        assert_eq!(detect_claude_terminal_error(no_idle_prompt), None);

        let later_output = "⏺ API Error: 529 Overloaded\n⏺ Retrying the request now.\n✻ Worked for 2s · done 22:28\n❯\n";
        assert_eq!(detect_claude_terminal_error(later_output), None);

        let later_user_prompt = "⏺ API Error: 529 Overloaded\n✻ Worked for 2s · done 22:28\n❯ retry this request\n✽ Waddling…\n❯\n";
        assert_eq!(detect_claude_terminal_error(later_user_prompt), None);

        let quoted_error = "  ⏺ API Error: 529 Overloaded\n✻ Worked for 2s · done 22:28\n❯\n";
        assert_eq!(detect_claude_terminal_error(quoted_error), None);

        let later_tool_output = "⏺ API Error: 529 Overloaded\n✻ Worked for 2s · done 22:28\n  ⎿ running a later tool\n✽ Waddling…\n❯\n";
        assert_eq!(detect_claude_terminal_error(later_tool_output), None);

        let provider_retry =
            "⏺ API Error (529 Overloaded) · Retrying in 2 seconds…\n✽ Waddling…\n❯\n";
        assert_eq!(detect_claude_terminal_error(provider_retry), None);
    }

    #[test]
    fn does_not_detect_a_stale_claude_api_error_outside_the_recent_tail() {
        let mut text = String::from("⏺ API Error: 529 Overloaded\n");
        for index in 0..30 {
            text.push_str(&format!("new output {index}\n"));
        }
        text.push_str("✻ Worked for 2s · done 22:28\n❯\n");

        assert_eq!(detect_claude_terminal_error(&text), None);
    }

    #[test]
    fn does_not_detect_stale_usage_limit_outside_recent_tail() {
        let mut text = String::from("You've hit your usage limit.\n");
        for index in 0..30 {
            text.push_str(&format!("new output {index}\n"));
        }
        assert!(!detect_usage_limit(&text));
    }

    #[test]
    fn does_not_detect_yes_when_permission_question_is_not_adjacent() {
        let text = "Allow command to run?\nnoise\nmore noise\nunrelated summary: yes\n";
        assert_eq!(codex::classify(text).modal, None);
    }

    #[test]
    fn detects_codex_permission_prompt_with_adjacent_choice() {
        let text = "? Allow command to run?\n  y) yes\n  n) no\n";
        assert_eq!(codex::classify(text).modal, Some(codex::Modal::Approval));
    }

    #[test]
    fn detects_claude_permission_prompt_with_numbered_yes_choice() {
        let text = "Claude needs your permission to use Bash\nDo you want to proceed?\n❯ 1. Yes\n  2. No\n";
        assert_eq!(codex::classify(text).modal, Some(codex::Modal::Approval));
    }

    #[test]
    fn detects_codex_question_prompt_from_unanswered_status() {
        let text = "Question 1/1 (1 unanswered)\nRun this commit plan?\n› 1. y (Recommended)\n  2. e\n  3. n\n  4. None of the above\n";
        assert_eq!(
            codex::classify(text).modal,
            Some(codex::Modal::SynchronousQuestion)
        );
    }

    #[test]
    fn permission_prompt_is_detected_only_within_the_recent_30_lines() {
        let mut within = String::from("? Allow command to run?\n  y) yes\n");
        for index in 0..28 {
            within.push_str(&format!("new output {index}\n"));
        }
        assert_eq!(codex::classify(&within).modal, Some(codex::Modal::Approval));

        let mut outside = String::from(
            "Claude needs your permission to use Bash\nDo you want to proceed?\n❯ 1. Yes\n  2. No\n",
        );
        for index in 0..30 {
            outside.push_str(&format!("new output {index}\n"));
        }
        assert_eq!(codex::classify(&outside).modal, None);
    }

    #[test]
    fn does_not_detect_codex_question_prompt_after_answered_status() {
        let text = "Question 1/1 (1 unanswered)\nRun this commit plan?\nQuestions 1/1 answered\n";
        assert_eq!(codex::classify(text).modal, None);
    }
}
