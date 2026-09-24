//! A viewport veto, never evidence of question resolution. No capture is retained here.
use regex::Regex;
use std::sync::OnceLock;
use unicode_width::UnicodeWidthStr;

use super::profile::CodexProfile;
pub use super::resolver::Sample as CaptureClass;

pub const STDOUT_LIMIT: usize = 512 * 1024;
pub const STDERR_LIMIT: usize = 64 * 1024;
pub const GROUP_LIMIT: usize = 1024 * 1024;

fn pattern(slot: &'static OnceLock<Regex>, source: &str) -> &'static Regex {
    slot.get_or_init(|| Regex::new(source).expect("constant capture pattern"))
}

pub fn classify(profile: CodexProfile, viewport: &str, width: u16, height: u16) -> CaptureClass {
    if profile == CodexProfile::Unknown || viewport.len() > STDOUT_LIMIT || viewport.is_empty() {
        return CaptureClass::Ambiguous;
    }
    static SUMMARY: OnceLock<Regex> = OnceLock::new();
    static PROGRESS: OnceLock<Regex> = OnceLock::new();
    static CHOICE: OnceLock<Regex> = OnceLock::new();
    let summary = pattern(&SUMMARY, r"^\s*\?\s+\d+ questions?(?:\s*·.*)?\s*$");
    let progress = pattern(&PROGRESS, r"^\s*\d+ of \d+\s*$");
    let choice = pattern(&CHOICE, r"^\s*›\s+\d+\.\s");
    let lines: Vec<_> = viewport.lines().collect();
    for line in &lines {
        if summary.is_match(line)
            || progress.is_match(line)
            || choice.is_match(line)
            || (line.contains(" submit") && line.contains(" skip"))
            || line.contains(" to answer")
            || line.contains("next question")
            || line.contains("prev question")
            || line.trim() == "Type your answer"
        {
            return CaptureClass::ActiveQuestion;
        }
    }
    if width < 80
        || height < 24
        || width > 1000
        || height > 1000
        || lines.len() != usize::from(height)
        || lines.iter().any(|line| line.width() > usize::from(width))
        || viewport.chars().any(|ch| ch.is_control() && ch != '\n')
    {
        return CaptureClass::Ambiguous;
    }
    // Known overlays and transient controls may coexist with a composer underneath them.
    if [
        "Action Required",
        "Approval",
        "Press enter",
        "esc to cancel",
        "Esc to cancel",
        "tab to queue message",
        "Vim:",
        "waiting for chord",
        "Copied selection",
        "Connected to",
        "Reconnecting",
    ]
    .iter()
    .any(|text| viewport.contains(text))
        || viewport.chars().any(|ch| {
            matches!(
                ch,
                '⠋' | '⠙' | '⠹' | '⠸' | '⠼' | '⠴' | '⠦' | '⠧' | '⠇' | '⠏'
            )
        })
    {
        return CaptureClass::Ambiguous;
    }
    let Some(last) = lines.iter().rposition(|line| !line.trim().is_empty()) else {
        return CaptureClass::Ambiguous;
    };
    if lines.len() - last > 3 {
        return CaptureClass::Ambiguous;
    }
    static CONTEXT: OnceLock<Regex> = OnceLock::new();
    static STATUS: OnceLock<Regex> = OnceLock::new();
    let context = pattern(
        &CONTEXT,
        r"^(?:\? for shortcuts\s+)?(?:100|[1-9]?[0-9])% context left$",
    );
    let status = pattern(
        &STATUS,
        r"^(?:GPT|gpt)-[0-9A-Za-z.-]+ (?:default|minimal|low|medium|high|xhigh|max|ultra)(?: fast)? · [^\r\n]+$",
    );
    let footer = lines[last].trim();
    let footer_start = if footer == "? for shortcuts" {
        if last > 0 && status.is_match(lines[last - 1].trim()) {
            last - 1
        } else {
            last
        }
    } else if context.is_match(footer) || status.is_match(footer) {
        last
    } else {
        return CaptureClass::Ambiguous;
    };
    if footer_start < 2 || !lines[footer_start - 1].trim().is_empty() {
        return CaptureClass::Ambiguous;
    }
    let composer = lines[footer_start - 2].trim_end();
    // Only empty/stock placeholders are positive fixtures. Drafts, search, popups and
    // unrecognized keymap/layout variants remain ambiguous instead of being guessed.
    if !matches!(composer, "›" | "› Ask Codex" | "› Ask Codex to do anything") {
        return CaptureClass::Ambiguous;
    }
    if footer_start < 3 || !lines[footer_start - 3].trim().is_empty() {
        return CaptureClass::Ambiguous;
    }
    CaptureClass::NormalComposer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport(body: &str, footer: &str) -> String {
        let mut rows = vec![String::new(); 24];
        rows[4] = body.into();
        rows[20] = "› Ask Codex to do anything".into();
        rows[22] = footer.into();
        rows.join("\n") + "\n"
    }

    #[test]
    fn source_defined_question_structures_veto_both_version_profiles_and_keymaps() {
        for profile in [CodexProfile::V01551, CodexProfile::V01561] {
            for question in [
                "  ? 1 question",
                "  ? 4 questions · 30s",
                "  1 of 2",
                "  › 2. Other",
                "  Type your answer",
                "  enter submit   ctrl+] skip",
                "  f9 submit   ctrl+] skip   option 1/13",
                "  ⌥+↓ prev question",
                "    ⌥+↓ to answer",
            ] {
                assert_eq!(
                    classify(profile, &viewport(question, "  ? for shortcuts"), 100, 24),
                    CaptureClass::ActiveQuestion
                );
            }
        }
    }

    #[test]
    fn positive_geometry_composer_and_known_footer_are_all_required() {
        for profile in [CodexProfile::V01551, CodexProfile::V01561] {
            for footer in [
                "  ? for shortcuts",
                "  ? for shortcuts                                      85% context left",
                "  100% context left",
                "  GPT-5.6-Sol default · /synthetic/project",
            ] {
                let screen = viewport("Synthetic completed output", footer);
                assert_eq!(
                    classify(profile, &screen, 100, 24),
                    CaptureClass::NormalComposer
                );
                for (width, height) in [(79, 24), (100, 23), (100, 25)] {
                    assert_eq!(
                        classify(profile, &screen, width, height),
                        CaptureClass::Ambiguous
                    );
                }
                assert_eq!(
                    classify(CodexProfile::Unknown, &screen, 100, 24),
                    CaptureClass::Ambiguous
                );
                assert_eq!(
                    classify(
                        profile,
                        &screen.replace("› Ask Codex to do anything", "$ shell prompt"),
                        100,
                        24
                    ),
                    CaptureClass::Ambiguous
                );
            }
            for footer in [
                "  F9 for shortcuts",
                "  esc to close",
                "  unknown status",
                "  ? for shortcuts 999% context left",
            ] {
                assert_eq!(
                    classify(profile, &viewport("", footer), 100, 24),
                    CaptureClass::Ambiguous
                );
            }
            for overlay in [
                "Action Required",
                "Approval required",
                "Esc to cancel",
                "Ctrl+X waiting for chord",
                "⠋ Working",
                "\u{1b}[31m",
            ] {
                assert_eq!(
                    classify(profile, &viewport(overlay, "  ? for shortcuts"), 100, 24),
                    CaptureClass::Ambiguous
                );
            }
            assert_eq!(
                classify(
                    profile,
                    &viewport(&"x".repeat(101), "  ? for shortcuts"),
                    100,
                    24
                ),
                CaptureClass::Ambiguous
            );
            assert_eq!(
                classify(profile, &"x".repeat(STDOUT_LIMIT + 1), 100, 24),
                CaptureClass::Ambiguous
            );
        }
    }
}
