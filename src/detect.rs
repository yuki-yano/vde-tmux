// capture-pane may include deeper scrollback; only recent screen lines should drive waiting state.
const WAIT_REASON_SCAN_TAIL_LINES: usize = 30;

pub fn detect_codex_wait_reason(screen_tail: &str) -> Option<&'static str> {
    let lines = recent_wait_reason_lines(screen_tail);

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
    if codex_question_prompt_active(&lines) {
        return Some("codex_question_prompt");
    }
    None
}

fn recent_wait_reason_lines(screen_tail: &str) -> Vec<String> {
    let raw_lines = screen_tail.lines().collect::<Vec<_>>();
    let scan_start = raw_lines.len().saturating_sub(WAIT_REASON_SCAN_TAIL_LINES);
    raw_lines[scan_start..]
        .iter()
        .map(|line| line.trim().to_ascii_lowercase())
        .filter(|line| !line.is_empty())
        .collect()
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

fn codex_question_prompt_active(lines: &[String]) -> bool {
    let mut latest_status = None;
    for line in lines {
        if looks_like_codex_question_unanswered(line) {
            latest_status = Some(true);
        } else if looks_like_codex_questions_answered(line) {
            latest_status = Some(false);
        }
    }
    latest_status == Some(true)
}

fn looks_like_codex_question_unanswered(line: &str) -> bool {
    let line = normalize_question_status_line(line);
    let Some(rest) = line.strip_prefix("question") else {
        return false;
    };
    let Some(rest) = parse_question_index(rest) else {
        return false;
    };
    let rest = rest.trim();
    if !rest.starts_with('(') || !rest.ends_with(')') {
        return false;
    }
    let inner = rest.trim_start_matches('(').trim_end_matches(')').trim();
    let Some(rest) = consume_ascii_digits(inner) else {
        return false;
    };
    rest.trim() == "unanswered"
}

fn looks_like_codex_questions_answered(line: &str) -> bool {
    let line = normalize_question_status_line(line);
    let Some(rest) = line.strip_prefix("questions") else {
        return false;
    };
    parse_question_index(rest)
        .map(str::trim)
        .is_some_and(|rest| rest == "answered")
}

fn normalize_question_status_line(line: &str) -> &str {
    line.trim_start_matches(['•', '*', '-']).trim()
}

fn parse_question_index(input: &str) -> Option<&str> {
    let rest = consume_ascii_digits(input.trim_start())?;
    let rest = rest.trim_start().strip_prefix('/')?;
    consume_ascii_digits(rest.trim_start())
}

fn consume_ascii_digits(input: &str) -> Option<&str> {
    let digit_count = input.bytes().take_while(u8::is_ascii_digit).count();
    (digit_count > 0).then_some(&input[digit_count..])
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn detects_codex_question_prompt_from_unanswered_status() {
        let text = "Question 1/1 (1 unanswered)\nRun this commit plan?\n› 1. y (Recommended)\n  2. e\n  3. n\n  4. None of the above\n";
        assert_eq!(
            detect_codex_wait_reason(text),
            Some("codex_question_prompt")
        );
    }

    #[test]
    fn detects_permission_prompt_within_recent_30_lines() {
        let mut text = String::from("? Allow command to run?\n  y) yes\n");
        for index in 0..28 {
            text.push_str(&format!("new output {index}\n"));
        }

        assert_eq!(detect_codex_wait_reason(&text), Some("permission_prompt"));
    }

    #[test]
    fn does_not_detect_codex_question_prompt_after_answered_status() {
        let text = "Question 1/1 (1 unanswered)\nRun this commit plan?\nQuestions 1/1 answered\n";
        assert_eq!(detect_codex_wait_reason(text), None);
    }

    #[test]
    fn does_not_detect_stale_question_prompt_outside_recent_tail() {
        let mut text = String::from(
            "Question 1/1 (1 unanswered)\nRun this commit plan?\n› 1. y (Recommended)\n  2. n\n",
        );
        for index in 0..30 {
            text.push_str(&format!("new output {index}\n"));
        }

        assert_eq!(detect_codex_wait_reason(&text), None);
    }

    #[test]
    fn does_not_detect_stale_claude_permission_prompt_outside_recent_tail() {
        let mut text = String::from(
            "Claude needs your permission to use Bash\nDo you want to proceed?\n❯ 1. Yes\n  2. No\n",
        );
        for index in 0..30 {
            text.push_str(&format!("new output {index}\n"));
        }

        assert_eq!(detect_codex_wait_reason(&text), None);
    }
}
