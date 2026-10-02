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

pub fn classify(
    profile: CodexProfile,
    viewport: &str,
    width: u16,
    height: u16,
    questions: &super::text::QuestionEvidence,
) -> CaptureClass {
    if profile == CodexProfile::Unknown || viewport.len() > STDOUT_LIMIT || viewport.is_empty() {
        return CaptureClass::Ambiguous;
    }
    let lines: Vec<_> = viewport.lines().collect();
    if matches!(profile, CodexProfile::V01593 | CodexProfile::V01600)
        && width >= 80
        && height >= 24
        && width <= 1000
        && height <= 1000
        && lines.len() <= usize::from(height)
        && !viewport.chars().any(|ch| ch.is_control() && ch != '\n')
        && matched_question(&lines, questions)
    {
        return CaptureClass::MatchedQuestion;
    }
    if lines
        .iter()
        .any(|line| crate::detect::codex::asynchronous_question_marker(line))
    {
        return CaptureClass::ActiveQuestion;
    }
    let evidence = crate::detect::codex::classify(viewport);
    if evidence.modal.is_some() || evidence.working || evidence.transcript_viewer {
        return CaptureClass::Ambiguous;
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
    if matches!(questions, super::text::QuestionEvidence::Unavailable) {
        return CaptureClass::Ambiguous;
    }
    CaptureClass::NormalComposer
}

fn matched_question(lines: &[&str], questions: &super::text::QuestionEvidence) -> bool {
    let Some(last) = lines.iter().rposition(|line| !line.trim().is_empty()) else {
        return false;
    };
    // The card's final block is its hints or flash message. Wording is not
    // evidence: matching also works when a keymap/flash omits generic markers.
    let mut footer = last;
    while footer > 0 && !lines[footer - 1].trim().is_empty() {
        footer -= 1;
    }
    let mut input_end = footer;
    while input_end > 0 && lines[input_end - 1].trim().is_empty() {
        input_end -= 1;
    }
    if input_end == 0 {
        return false;
    }
    static CHOICE: OnceLock<Regex> = OnceLock::new();
    let choice = pattern(&CHOICE, r"^\s*(?:›\s*)?([0-9]+)\.(?:\s+(.*))?$");
    // Full ordered named choices plus exactly one generated/draft Other row.
    // Try only complete sequences starting at 1; clipped choices are unknown.
    for input_start in (0..input_end).rev().filter(|&row| {
        choice
            .captures(lines[row])
            .is_some_and(|value| &value[1] == "1")
    }) {
        if normal_composer_below(lines, input_start, choice) {
            continue;
        }
        let mut options: Vec<String> = Vec::new();
        let mut complete = true;
        for line in &lines[input_start..input_end] {
            if let Some(value) = choice.captures(line) {
                if value[1].parse::<usize>().ok() != Some(options.len() + 1) {
                    complete = false;
                    break;
                }
                options.push(value.get(2).map_or("", |value| value.as_str()).to_owned());
            } else if !line.trim().is_empty() {
                let Some(option) = options.last_mut() else {
                    complete = false;
                    break;
                };
                option.push('\n');
                option.push_str(line.trim());
            }
        }
        if complete && options.len() >= 2 {
            options.pop(); // Upstream always appends Other; its label/draft is arbitrary.
            if title_matches(lines, input_start, questions, &options) {
                return true;
            }
        }
    }
    // Freeform input is unprefixed and can be an arbitrary multi-line draft.
    // Upstream caps it at eight physical rows. The preceding spacer separates
    // it from the complete title, which may itself contain paragraphs.
    for input_start in input_end.saturating_sub(8)..input_end {
        if input_start > 0
            && lines[input_start - 1].trim().is_empty()
            && !lines[input_start].trim().is_empty()
            && !normal_composer_below(lines, input_start, choice)
            && title_matches(lines, input_start, questions, &[])
        {
            return true;
        }
    }
    false
}

fn normal_composer_below(lines: &[&str], start: usize, choice: &Regex) -> bool {
    lines[start..]
        .iter()
        .any(|line| line.trim_start().starts_with('›') && !choice.is_match(line))
}

fn title_matches(
    lines: &[&str],
    input_start: usize,
    questions: &super::text::QuestionEvidence,
    options: &[String],
) -> bool {
    let mut end = input_start;
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let mut bytes = 0;
    for start in (0..end).rev() {
        let line = lines[start].trim();
        // History cells have a bullet; progress is outside the question title.
        if line.starts_with('•') || line.starts_with("› ") {
            break;
        }
        bytes += line.len() + 1;
        if bytes > 16 * 1024 {
            break;
        }
        let title = lines[start..end].join("\n");
        if questions.matches(&title, options) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(profile: CodexProfile, viewport: &str, width: u16, height: u16) -> CaptureClass {
        let questions = super::super::text::QuestionEvidence::from_tool_input(Some(
            &serde_json::json!({"questions":[{"title":"Synthetic question"}]}),
        ));
        super::classify(profile, viewport, width, height, &questions)
    }

    fn viewport(body: &str, footer: &str) -> String {
        let mut rows = vec![String::new(); 24];
        rows[4] = body.into();
        rows[20] = "› Ask Codex to do anything".into();
        rows[22] = footer.into();
        rows.join("\n") + "\n"
    }

    fn question_viewport(body: &[&str]) -> String {
        let mut rows = vec![String::new(); 24];
        for (row, text) in rows[10..].iter_mut().zip(body) {
            *row = (*text).into();
        }
        rows.join("\n") + "\n"
    }

    #[test]
    fn current_editor_matches_wrapped_title_choices_and_generated_other() {
        let questions =
            super::super::text::QuestionEvidence::from_tool_input(Some(&serde_json::json!({
                "questions":[{"title":"作業を続けますか？", "options":["Continue now", "Other"]}]
            })));
        let body = [
            "  2 of 2",
            "  作業を",
            "  続けますか？",
            "",
            "  › 1. Continue",
            "       now",
            "    2. Other",
            "    3. Other (write",
            "       an answer)",
            "",
            "  enter submit   ctrl+] skip",
            "  shift+→ main prompt",
        ];
        let screen = question_viewport(&body);
        assert_eq!(
            super::classify(CodexProfile::V01593, &screen, 100, 24, &questions),
            CaptureClass::MatchedQuestion
        );
        let unrelated = screen.replace("続けますか？", "止めますか？");
        assert_eq!(
            super::classify(CodexProfile::V01593, &unrelated, 100, 24, &questions),
            CaptureClass::ActiveQuestion
        );
        let clipped = screen
            .replace("  › 1. Continue", "")
            .replace("       now", "");
        assert_eq!(
            super::classify(CodexProfile::V01593, &clipped, 100, 24, &questions),
            CaptureClass::ActiveQuestion
        );
        let mut history: Vec<String> = screen.lines().map(str::to_owned).collect();
        history[20] = "› Ask Codex".into();
        history[21].clear();
        history[22] = "  ? for shortcuts".into();
        let history = history.join("\n") + "\n";
        assert_eq!(
            super::classify(CodexProfile::V01593, &history, 100, 24, &questions),
            CaptureClass::ActiveQuestion
        );
        let mut earlier_history: Vec<String> = screen.lines().map(str::to_owned).collect();
        earlier_history[2] = "1. Historical numbered output".into();
        assert_eq!(
            super::classify(
                CodexProfile::V01593,
                &(earlier_history.join("\n") + "\n"),
                100,
                24,
                &questions
            ),
            CaptureClass::MatchedQuestion
        );
    }

    #[test]
    fn free_text_editor_and_history_are_separate_and_missing_text_retains() {
        use super::super::text::QuestionEvidence;
        let questions = QuestionEvidence::from_tool_input(Some(
            &serde_json::json!({"questions":[{"title":"Next step?"}]}),
        ));
        let screen = question_viewport(&[
            "  Next step?",
            "",
            "  Type your answer",
            "",
            "  enter submit   ctrl+] skip",
            "  shift+→ main prompt",
        ]);
        assert_eq!(
            super::classify(CodexProfile::V01593, &screen, 100, 24, &questions),
            CaptureClass::MatchedQuestion
        );
        let normal = viewport("Next step?", "  ? for shortcuts");
        assert_eq!(
            super::classify(CodexProfile::V01593, &normal, 100, 24, &questions),
            CaptureClass::NormalComposer
        );
        assert_eq!(
            super::classify(
                CodexProfile::V01593,
                &normal,
                100,
                24,
                &QuestionEvidence::Unavailable
            ),
            CaptureClass::Ambiguous
        );
    }

    #[test]
    fn upstream_snapshots_match_complete_options_and_retain_clipped_cards() {
        for profile in [CodexProfile::V01593, CodexProfile::V01600] {
            use super::super::text::QuestionEvidence;
            fn body(snapshot: &str) -> &str {
                snapshot.splitn(3, "---\n").nth(2).unwrap()
            }
            let suggested = "A suggested answer that is long enough to wrap across multiple rows";
            let cases = [
                (
                    include_str!(
                        "../../scripts/fixtures/codex-0.159.3-questions/question_wrapped_named_option.snap"
                    ),
                    vec![suggested],
                ),
                (
                    include_str!(
                        "../../scripts/fixtures/codex-0.159.3-questions/question_wrapped_other.snap"
                    ),
                    vec![suggested],
                ),
                (
                    include_str!(
                        "../../scripts/fixtures/codex-0.159.3-questions/question_capped_other.snap"
                    ),
                    vec![suggested],
                ),
                (
                    include_str!(
                        "../../scripts/fixtures/codex-0.159.3-questions/question_named_other.snap"
                    ),
                    vec!["Other"],
                ),
            ];
            for (snapshot, options) in cases {
                let evidence = QuestionEvidence::from_tool_input(Some(&serde_json::json!({
                    "questions":[{"title":"Second", "options":options}]
                })));
                assert_eq!(
                    super::classify(profile, body(snapshot), 100, 24, &evidence),
                    CaptureClass::MatchedQuestion
                );
                let mut rows = vec!["• Second", "  1. Prior numbered output", ""];
                rows.extend(body(snapshot).lines());
                rows.resize(24, "");
                assert_eq!(
                    super::classify(profile, &rows.join("\n"), 100, 24, &evidence),
                    CaptureClass::MatchedQuestion
                );
            }
            let boundary = body(include_str!(
                "../../scripts/fixtures/codex-0.159.3-questions/question_options_width_boundary.snap"
            ));
            let (full, clipped) = boundary.split_once("\n\nClipped:\n").unwrap();
            let evidence = QuestionEvidence::from_tool_input(Some(&serde_json::json!({
                "questions":[{"title":"Second", "options":["abcdefgh ijklmno", "pqrstuvw xyzabcd"]}]
            })));
            assert_eq!(
                super::classify(
                    profile,
                    full.strip_prefix("Full height:\n").unwrap(),
                    100,
                    24,
                    &evidence
                ),
                CaptureClass::MatchedQuestion
            );
            assert_eq!(
                super::classify(profile, clipped, 100, 24, &evidence),
                CaptureClass::ActiveQuestion
            );
            let clipped_choice = body(include_str!(
                "../../scripts/fixtures/codex-0.159.3-questions/question_clipped_choice_rejected.snap"
            ));
            assert_eq!(
                super::classify(profile, clipped_choice, 100, 24, &evidence),
                CaptureClass::ActiveQuestion
            );
            for (snapshot, title, options, expected) in [
                (
                    include_str!(
                        "../../scripts/fixtures/codex-0.159.3-questions/long_prompt_active_input.snap"
                    ),
                    "A lengthy prompt. ".repeat(30),
                    vec![],
                    CaptureClass::Ambiguous,
                ),
                (
                    include_str!(
                        "../../scripts/fixtures/codex-0.159.3-questions/selected_other_placeholder.snap"
                    ),
                    "Long question ".repeat(40),
                    vec!["Named"],
                    CaptureClass::ActiveQuestion,
                ),
                (
                    include_str!(
                        "../../scripts/fixtures/codex-0.159.3-questions/question_clipped_prompt.snap"
                    ),
                    "Long question ".repeat(3 * 65_536),
                    vec!["Named"],
                    CaptureClass::Ambiguous,
                ),
            ] {
                let mut question = serde_json::json!({"title":title});
                if !options.is_empty() {
                    question["options"] = serde_json::json!(options);
                }
                let evidence = QuestionEvidence::from_tool_input(Some(
                    &serde_json::json!({"questions":[question]}),
                ));
                assert_eq!(
                    super::classify(profile, body(snapshot), 100, 24, &evidence),
                    expected
                );
            }
        }
    }

    #[test]
    fn freeform_draft_text_veto_works_without_generic_footer_markers() {
        for profile in [CodexProfile::V01593, CodexProfile::V01600] {
            use super::super::text::QuestionEvidence;
            let evidence = QuestionEvidence::from_tool_input(Some(&serde_json::json!({
                "questions":[{"title":"First paragraph.\n\nSecond paragraph?"}]
            })));
            let screen = question_viewport(&[
                "  First paragraph.",
                "",
                "  Second paragraph?",
                "",
                "  My draft",
                "  continues here",
                "",
                "  Custom key binding",
            ]);
            assert!(
                !screen
                    .lines()
                    .any(crate::detect::codex::asynchronous_question_marker)
            );
            assert_eq!(
                super::classify(profile, &screen, 100, 24, &evidence),
                CaptureClass::MatchedQuestion
            );
            for profile in [CodexProfile::V01551, CodexProfile::V01561] {
                assert_eq!(
                    super::classify(profile, &screen, 100, 24, &evidence),
                    CaptureClass::Ambiguous
                );
            }
            let changed = screen.replace("Second paragraph?", "Unrelated paragraph?");
            assert_eq!(
                super::classify(profile, &changed, 100, 24, &evidence),
                CaptureClass::Ambiguous
            );
            let history = screen.replace("  First paragraph.", "• First paragraph.");
            assert_eq!(
                super::classify(profile, &history, 100, 24, &evidence),
                CaptureClass::Ambiguous
            );
        }
    }

    #[test]
    fn joined_soft_wrap_cannot_become_normal_composer_evidence() {
        let screen = viewport("Synthetic output", "  ? for shortcuts");
        let joined = screen.replacen("\n\n", "\n", 1);
        assert_eq!(
            classify(CodexProfile::V01593, &joined, 100, 24),
            CaptureClass::Ambiguous
        );
        let joined_wide = screen.replace("Synthetic output", &"x".repeat(101));
        assert_eq!(
            classify(CodexProfile::V01593, &joined_wide, 100, 24),
            CaptureClass::Ambiguous
        );
    }

    #[test]
    fn source_defined_question_structures_veto_both_version_profiles_and_keymaps() {
        for profile in [
            CodexProfile::V01551,
            CodexProfile::V01561,
            CodexProfile::V01593,
            CodexProfile::V01600,
        ] {
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
        for profile in [
            CodexProfile::V01551,
            CodexProfile::V01561,
            CodexProfile::V01593,
            CodexProfile::V01600,
        ] {
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
