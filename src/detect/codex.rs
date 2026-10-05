//! Bounded, body-free evidence from the Codex TUI, not lifecycle/completion events.
//!
//! The region/timer approach is informed by herdr's Codex manifest at
//! 81ddfc65b4661c52569f8b8d1ee41d5a6d68f85e. This is a provider-specific
//! implementation, with no manifest interpreter, remote rules or title storage.
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

const MAX_SCREEN_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modal {
    Approval,
    SynchronousQuestion,
    TrustDirectory,
    StartupUpdate,
}

/// A question may coexist with work. Absence of all evidence means unknown,
/// never idle, answered, completed, or safe to dispatch a prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub working: bool,
    pub modal: Option<Modal>,
    pub asynchronous_question: bool,
    pub transcript_viewer: bool,
}

pub fn classify(screen: &str) -> Evidence {
    if screen.is_empty()
        || screen.len() > MAX_SCREEN_BYTES
        || screen.chars().any(|ch| ch.is_control() && ch != '\n')
    {
        return Evidence::default();
    }
    let lines: Vec<_> = screen.trim_end().lines().collect();
    let prompt = lines.iter().rposition(|line| is_prompt(line));
    let current_prompt =
        prompt.filter(|index| !lines[index + 1..].iter().any(|line| is_response(line)));
    let after_prompt = &lines[prompt.map_or(0, |index| index + 1)..];
    if transcript_viewer(after_prompt) {
        return Evidence {
            transcript_viewer: true,
            ..Evidence::default()
        };
    }
    // A modal below the last prompt supersedes that historical input. A new
    // composer below the modal excludes historical approval/question text.
    let modal = if current_prompt.is_some()
        && lines
            .iter()
            .rev()
            .take(5)
            .any(|line| line.contains(" to submit answer") || line.contains(" to submit all"))
        && detect_modal(&lines) == Some(Modal::SynchronousQuestion)
    {
        Some(Modal::SynchronousQuestion)
    } else {
        startup_modal(&lines).or_else(|| detect_modal(after_prompt))
    };
    let before_composer = &lines[..current_prompt.unwrap_or(lines.len())];
    let working = live_timer(before_composer);
    // Async controls can be above or below the composer while a turn continues.
    // Restrict to the bottom UI; this is positive presence only, never absence proof.
    let recent = &lines[lines.len().saturating_sub(30)..];
    Evidence {
        working,
        modal,
        asynchronous_question: modal.is_none()
            && recent.iter().any(|line| asynchronous_question_marker(line)),
        transcript_viewer: false,
    }
}

fn startup_modal(lines: &[&str]) -> Option<Modal> {
    // A later composer or response makes startup text historical. Numbered
    // choices are deliberately excluded by is_prompt.
    if lines
        .iter()
        .any(|line| is_prompt(line) || is_response(line))
    {
        return None;
    }
    let non_empty: Vec<_> = lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    let top = &non_empty[..non_empty.len().min(20)];
    if top
        .first()
        .is_some_and(|line| line.starts_with("> You are in "))
        && top
            .join(" ")
            .contains("Do you trust the contents of this directory?")
        && top
            .iter()
            .any(|line| yes_choice(&line.to_ascii_lowercase()))
    {
        return Some(Modal::TrustDirectory);
    }
    let bottom = &non_empty[non_empty.len().saturating_sub(20)..];
    if bottom.iter().any(|line| line.contains("Update available!"))
        && bottom.iter().any(|line| line.contains("Update now"))
        && bottom.join(" ").contains("Skip until next version")
        && bottom.last() == Some(&"Press enter to continue")
    {
        return Some(Modal::StartupUpdate);
    }
    None
}

fn is_prompt(line: &str) -> bool {
    // Numbered choices must not become composer boundaries, including narrow captures.
    let Some(rest) = line.strip_prefix("› ") else {
        return line == "›";
    };
    !rest.starts_with("Type your answer")
        && !rest
            .trim_start_matches(|ch: char| ch.is_ascii_digit())
            .starts_with(". ")
}

fn is_response(line: &str) -> bool {
    line.starts_with(['•', '◦', '■', '✗', '✓'])
}

fn transcript_viewer(lines: &[&str]) -> bool {
    lines.iter().any(|line| {
        line.contains("↑/↓ to scroll")
            || line.contains("pgup/pgdn to")
            || line.contains("home/end to jump")
            || line.contains("esc to edit prev")
            || line.contains("esc/← to edit prev")
    })
}

fn regex(slot: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    slot.get_or_init(|| Regex::new(pattern).expect("constant Codex screen pattern"))
}

fn live_timer(lines: &[&str]) -> bool {
    static TIMER: OnceLock<Regex> = OnceLock::new();
    let timer = regex(
        &TIMER,
        r"^(?:[•◦] +)?[^\s›•◦■✗✓─][^\r\n]* \((?:[0-9]+[hm] )*[0-9]+s(?: • [^\r\n]+ to interrupt)?\)(?: · [^\r\n]*)?$",
    );
    for (index, line) in lines.iter().enumerate().rev() {
        if line.contains("Reconnect failed — check the endpoint, then relaunch") {
            return false;
        }
        if timer.is_match(line) {
            return lines[index + 1..]
                .iter()
                .all(|later| !is_response(later) || queued_input_header(later));
        }
        if is_prompt(line) || (is_response(line) && !queued_input_header(line)) {
            return false;
        }
    }
    false
}

pub(crate) fn queued_input_header(line: &str) -> bool {
    [
        "• Queued follow-up inputs",
        "• Messages to be submitted after next tool call",
        "• Messages to be submitted at end of turn",
    ]
    .iter()
    .any(|header| line.starts_with(header))
}

fn detect_modal(lines: &[&str]) -> Option<Modal> {
    let lines: Vec<_> = lines
        .iter()
        .rev()
        .take(30)
        .rev()
        .map(|line| line.trim().to_ascii_lowercase())
        .filter(|line| !line.is_empty())
        .collect();
    for (index, line) in lines.iter().enumerate() {
        let permission = line.contains("would you like to run the following command?")
            || line.contains("would you like to make the following edits?")
            || line.contains("do you want to proceed?")
            || ((line.contains("allow")
                || line.contains("approve")
                || line.contains("permission"))
                && line.contains('?')
                && [
                    "command", "edit", "write", "tool", "bash", "use", "run", "execute",
                ]
                .iter()
                .any(|action| line.contains(action)));
        if permission && lines[index + 1..].iter().any(|choice| yes_choice(choice)) {
            return Some(Modal::Approval);
        }
    }
    static QUESTION: OnceLock<Regex> = OnceLock::new();
    static ANSWERED: OnceLock<Regex> = OnceLock::new();
    let question = regex(
        &QUESTION,
        r"^question\s*[0-9]+\s*/\s*[0-9]+\s*\([1-9][0-9]* unanswered\)$",
    );
    let answered = regex(&ANSWERED, r"^questions\s*[0-9]+\s*/\s*[0-9]+\s*answered$");
    for line in lines.iter().rev() {
        let line = line.trim_start_matches(['•', '*', '-']).trim();
        if answered.is_match(line) {
            return None;
        }
        if question.is_match(line) {
            return Some(Modal::SynchronousQuestion);
        }
    }
    None
}

fn yes_choice(line: &str) -> bool {
    let text = line.trim_start_matches(|ch: char| {
        ch.is_whitespace()
            || ch.is_ascii_digit()
            || matches!(ch, '-' | '*' | '>' | '❯' | '›' | '.' | ')')
    });
    text == "yes"
        || text.starts_with("yes ")
        || text.starts_with("yes,")
        || text.starts_with("y) yes")
        || text.starts_with("y - yes")
        || text.starts_with("[y] yes")
}

/// Shared structural markers. The notice veto deliberately scans the entire
/// viewport, whereas activity detection limits its current-UI region.
pub fn asynchronous_question_marker(line: &str) -> bool {
    static SUMMARY: OnceLock<Regex> = OnceLock::new();
    static PROGRESS: OnceLock<Regex> = OnceLock::new();
    static CHOICE: OnceLock<Regex> = OnceLock::new();
    regex(&SUMMARY, r"^\s*\?\s+\d+ questions?(?:\s*·.*)?\s*$").is_match(line)
        || regex(&PROGRESS, r"^\s*\d+ of \d+\s*$").is_match(line)
        || regex(&CHOICE, r"^\s*›\s+\d+\.\s").is_match(line)
        || (line.contains(" submit") && line.contains(" skip"))
        || line.contains(" to answer")
        || line.contains("next question")
        || line.contains("prev question")
        || line.trim() == "Type your answer"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_modals_require_current_structured_controls() {
        let trust = "> You are in /synthetic/project\n\nDo you trust the contents of this directory?\n› 1. Yes, continue\n  2. No, exit\n";
        let update = "Update available! 0.1 -> 0.2\n› 1. Update now\n  2. Skip\n  3. Skip until next version\n\nPress enter to continue\n";
        for (screen, modal) in [
            (trust, Modal::TrustDirectory),
            (update, Modal::StartupUpdate),
        ] {
            assert_eq!(classify(screen).modal, Some(modal));
            for historical in [
                format!("{screen}\n› Ask Codex\n"),
                format!("• Earlier output\n{screen}"),
                format!("{screen}\n↑/↓ to scroll · q to quit\n"),
            ] {
                assert_eq!(classify(&historical).modal, None, "{historical:?}");
            }
        }
        for unknown in [
            "Do you trust the contents of this directory?\n",
            "Update available!\n",
            "[y/n]\n",
            "would you like to\nyes\n",
            "Update now\nSkip until next version\nPress enter to continue\n",
        ] {
            assert_eq!(classify(unknown).modal, None);
        }
        assert_eq!(
            classify(&trust.replace("Yes, continue", "unrecognized option")).modal,
            None
        );
        assert_eq!(
            classify(&format!("{update}{}", "later output\n".repeat(25))).modal,
            None
        );
    }

    #[test]
    fn dynamic_activity_keymaps_reduced_motion_and_queued_inputs() {
        for activity in [
            "• Working (3s • esc to interrupt)",
            "• Inspecting source (1m 12s • ctrl+x to interrupt)",
            "Inspecting source (2h 3m 4s)",
            "◦ Building (12s)",
            "• Working (1s) · 2 background terminals",
        ] {
            for queue in [
                "",
                "• Queued follow-up inputs\n  synthetic input\n",
                "• Messages to be submitted after next tool call (press f9 to interrupt and send immediately)\n  synthetic input\n",
                "• Messages to be submitted at end of turn\n  synthetic input\n",
            ] {
                assert!(
                    classify(&format!(
                        "{activity}\n{queue}\n› Ask Codex\n\n  ? for shortcuts\n"
                    ))
                    .working
                );
            }
        }
    }

    #[test]
    fn unknown_and_historical_content_never_imply_idle_or_completion() {
        for screen in [
            "",
            "unknown UI",
            "› Ask Codex\n\n  ? for shortcuts\n",
            "• Working (9s)\n• Completed output\n› Ask Codex\n",
            "• Reconnect failed — check the endpoint, then relaunch (30s)\n› Ask Codex\n",
            "› • Working (3s)\n  ? for shortcuts\n",
            "• Working (9s)\n› previous input\n• latest response\n",
        ] {
            assert_eq!(classify(screen), Evidence::default(), "{screen:?}");
        }
        assert!(
            !classify("• Working (2s)\n› old input\n↑/↓ to scroll · esc to edit prev\n").working
        );
    }

    #[test]
    fn modal_and_async_questions_are_distinct_and_work_can_continue() {
        let approval = "Would you like to run the following command?\n\n  $ synthetic-command\n\n  › 1. Yes, proceed (y)\n  2. No (esc)\n";
        assert_eq!(classify(approval).modal, Some(Modal::Approval));
        assert!(!classify(approval).asynchronous_question);
        assert_eq!(classify("Question 1/1 (1 unanswered)\n› synthetic draft\n ctrl+j to submit answer | esc to interrupt\n").modal, Some(Modal::SynchronousQuestion));
        assert_eq!(classify(&format!("{approval}\n› Ask Codex\n")).modal, None);
        assert_eq!(
            classify("Question 1/1 (1 unanswered)\n").modal,
            Some(Modal::SynchronousQuestion)
        );
        assert_eq!(
            classify("Question 1/1 (1 unanswered)\nQuestions 1/1 answered\n").modal,
            None
        );
        for question in [
            "  ? 1 question · 3s",
            "  1 of 2\n  Type your answer\n  enter submit   ctrl+] skip",
        ] {
            let evidence = classify(&format!("• Reviewing (3s)\n{question}\n› Ask Codex\n"));
            assert!(evidence.working && evidence.asynchronous_question);
            assert_eq!(evidence.modal, None);
        }
    }
}
