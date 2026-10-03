//! Bounded evidence and policy for the opt-in stock Codex recovery path.
use crate::question_notice::ingress::TranscriptLocator;
use serde::Deserialize;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

pub const ORIGIN: &str = "capacity_auto_resume";
pub const REASON: &str = "provider_overloaded";
pub const ERROR: &str = "Selected model is at capacity. Please try a different model.";
pub const DEFAULT_PROMPT: &str = "The previous request was interrupted by a model capacity error. Resume only the unfinished work after checking the current state and completed actions. Preserve the original objective, scope, constraints, approvals, and response language. Do not repeat completed operations. Follow the existing requirements for clarification and approval. This message is not a new approval.";
pub const MAX_TAIL: u64 = 256 * 1024;

pub fn failure_hint(screen: &str) -> bool {
    if screen.len() > 512 * 1024 {
        return false;
    }
    let rows: Vec<_> = screen.lines().collect();
    let recent = &rows[rows.len().saturating_sub(30)..];
    let hint = recent.iter().enumerate().any(|(i, row)| {
        row.trim_start().starts_with("■ Selected")
            && recent[i..]
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .starts_with(&format!("■ {ERROR}"))
    });
    hint && !crate::detect::codex::classify(screen).working
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacitySample {
    pub pane: crate::pane_state::PaneInstance,
    pub failure_hint: bool,
    pub working: bool,
    pub frame: Option<[u8; 32]>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutcome {
    Pending,
    Capacity,
    OtherError,
    Success,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodexConfig {
    pub capacity_auto_resume: CapacityConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityConfig {
    pub enabled: bool,
    pub prompt: String,
}
impl Default for CapacityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            prompt: DEFAULT_PROMPT.into(),
        }
    }
}
pub fn validate_prompt(value: &str) -> Result<(), String> {
    let value = value.trim_end_matches('\n');
    if value.trim().is_empty()
        || value.len() > 65536
        || value.trim() != value
        || value
            .chars()
            .any(|c| (c.is_control() && c != '\n') || ('\u{80}'..='\u{9f}').contains(&c))
        || value.starts_with(['/', '!'])
        || value
            .split_whitespace()
            .last()
            .is_some_and(|v| v.starts_with(['@', '$']))
    {
        return Err("codex.capacity_auto_resume.prompt must be 1..65536 UTF-8 bytes, without surrounding whitespace, unsafe controls, command prefixes, or a trailing @/$ completion token".into());
    }
    Ok(())
}
pub fn read_terminal(
    locator: &TranscriptLocator,
    turn: &str,
    size: &mut u64,
) -> anyhow::Result<TerminalOutcome> {
    anyhow::ensure!(
        locator.matches_current_file(),
        "transcript identity changed"
    );
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&locator.transcript)?;
    let meta = file.metadata()?;
    anyhow::ensure!(
        meta.is_file()
            && meta.dev() == locator.dev
            && meta.ino() == locator.ino
            && meta.len() >= *size,
        "transcript replaced or truncated"
    );
    *size = meta.len();
    let offset = meta.len().saturating_sub(MAX_TAIL);
    file.seek(SeekFrom::Start(offset.saturating_sub(1)))?;
    let mut data = Vec::new();
    file.take(meta.len() - offset.saturating_sub(1))
        .read_to_end(&mut data)?;
    let start = if offset == 0 {
        0
    } else if data.first() == Some(&b'\n') {
        1
    } else {
        data.iter()
            .position(|b| *b == b'\n')
            .map_or(data.len(), |n| n + 1)
    };
    parse_terminal(&data[start..], turn)
}
pub fn read_failure(
    locator: &TranscriptLocator,
    turn: &str,
    size: &mut u64,
) -> anyhow::Result<bool> {
    Ok(read_terminal(locator, turn, size)? == TerminalOutcome::Capacity)
}
fn parse_terminal(data: &[u8], turn: &str) -> anyhow::Result<TerminalOutcome> {
    let mut outcome = TerminalOutcome::Pending;
    let mut newer_turn = false;
    for line in data.split_inclusive(|b| *b == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        }
        let event: serde_json::Value = serde_json::from_slice(line)?;
        if event["type"] != "event_msg" {
            continue;
        }
        let e = &event["payload"];
        if e["type"] == "task_started" {
            outcome = TerminalOutcome::Pending;
            newer_turn = e["turn_id"] != turn;
        }
        if !newer_turn && e["type"] == "task_complete" && e["turn_id"] == turn {
            outcome = if e["error"]["codex_error_info"] == "server_overloaded" {
                TerminalOutcome::Capacity
            } else if !e["error"].is_null() {
                TerminalOutcome::OtherError
            } else {
                TerminalOutcome::Success
            };
        }
    }
    Ok(outcome)
}
#[cfg(test)]
fn parse_tail(data: &[u8], turn: &str) -> anyhow::Result<bool> {
    Ok(parse_terminal(data, turn)? == TerminalOutcome::Capacity)
}

/// Plain viewport predicate; ANSI styling is checked separately for the live composer row.
pub fn frame(screen: &str) -> Option<(usize, [u8; 32])> {
    if screen.len() > 512 * 1024 || screen.chars().any(|c| c.is_control() && c != '\n') {
        return None;
    }
    let evidence = crate::detect::codex::classify(screen);
    if evidence.working
        || evidence.modal.is_some()
        || evidence.asynchronous_question
        || evidence.transcript_viewer
    {
        return None;
    }
    let rows: Vec<_> = screen.lines().collect();
    let row = rows
        .iter()
        .rposition(|v| v.trim_start().starts_with(['›', '»']))?;
    if !matches!(
        rows[row].trim(),
        "› Ask Codex to do anything" | "» Ask Codex to do anything"
    ) {
        return None;
    }
    let error = rows[..row]
        .iter()
        .rposition(|v| v.trim_start().starts_with('■'))?;
    let cell = rows[error..row]
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if cell != format!("■ {ERROR}") {
        return None;
    }
    Some((
        row,
        // tmux retains background-colored padding only in ANSI captures.
        crate::daemon::workers::capture_sha256(
            &rows[..row]
                .iter()
                .map(|line| line.trim_end())
                .collect::<Vec<_>>()
                .join("\n"),
        )?,
    ))
}
/// OSC 8 hyperlink wrappers have zero cell width. Reject other or incomplete OSC.
pub fn strip_terminal_links(screen: &str) -> Option<String> {
    let mut output = String::with_capacity(screen.len());
    let mut rest = screen;
    while let Some(start) = rest.find("\x1b]") {
        output.push_str(&rest[..start]);
        let osc = &rest[start + 2..];
        let end = osc.find(['\x07', '\x1b'])?;
        let payload = &osc[..end];
        let (_, uri) = payload.strip_prefix("8;")?.split_once(';')?;
        if payload.chars().any(char::is_control) || uri.contains('\n') {
            return None;
        }
        let terminator = &osc[end..];
        rest = if let Some(next) = terminator.strip_prefix('\x07') {
            next
        } else {
            terminator.strip_prefix("\x1b\\")?
        };
    }
    output.push_str(rest);
    Some(output)
}
pub fn styled_placeholder(row: &str) -> bool {
    styled_placeholder_at(row, 0)
}
pub fn styled_placeholder_at(screen: &str, row_index: usize) -> bool {
    // Preserve inherited SGR while ignoring unrelated attributes after the composer.
    let through_composer = screen
        .split_inclusive('\n')
        .take(row_index + 1)
        .collect::<String>();
    let Some(screen) = strip_terminal_links(&through_composer) else {
        return false;
    };
    fn parse(row: &str) -> Option<Vec<(char, bool)>> {
        let mut result = Vec::new();
        let mut rest = row;
        let mut dim = false;
        while !rest.is_empty() {
            if let Some(sgr) = rest.strip_prefix("\x1b[") {
                let end = sgr.find('m')?;
                let params = if end == 0 {
                    vec![0]
                } else {
                    sgr[..end]
                        .split(';')
                        .map(str::parse::<u16>)
                        .collect::<Result<Vec<_>, _>>()
                        .ok()?
                };
                let mut i = 0;
                while i < params.len() {
                    match params[i] {
                        0 | 22 => dim = false,
                        2 => dim = true,
                        38 | 48 | 58 => {
                            i += match params.get(i + 1) {
                                Some(5) => 2,
                                Some(2) => 4,
                                _ => return None,
                            };
                            if i >= params.len() {
                                return None;
                            }
                        }
                        1 | 3..=9 | 21 | 23..=29 | 30..=37 | 39..=47 | 49 | 59 | 90..=107 => {}
                        _ => return None,
                    }
                    i += 1;
                }
                rest = &sgr[end + 1..];
            } else {
                let c = rest.chars().next()?;
                if c.is_control() && c != '\n' {
                    return None;
                }
                result.push((c, dim));
                rest = &rest[c.len_utf8()..];
            }
        }
        Some(result)
    }
    let Some(all) = parse(&screen) else {
        return false;
    };
    let Some(selected) = all.split(|v| v.0 == '\n').nth(row_index) else {
        return false;
    };
    let mut chars = selected.to_vec();
    while chars.first().is_some_and(|v| v.0 == ' ') {
        chars.remove(0);
    }
    while chars.last().is_some_and(|v| v.0 == ' ') {
        chars.pop();
    }
    chars.len() == "Ask Codex to do anything".chars().count() + 2
        && matches!(chars[0], ('›' | '»', false))
        && chars[1].0 == ' '
        && chars[2..].iter().all(|v| v.1)
        && chars[2..].iter().map(|v| v.0).collect::<String>() == "Ask Codex to do anything"
}
pub fn delay(attempt: usize, jitter: u64) -> u64 {
    let seconds = [60, 120, 300][attempt];
    seconds + jitter % (seconds / 5 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_tail_long_history_partial_lines_replacement_and_truncation() {
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Scratch(std::env::temp_dir().join(format!(
            "vde-capacity-tail-{}",
            crate::pane_state::EventId::generate().unwrap().as_str()
        )));
        std::fs::create_dir_all(&root.0).unwrap();
        let home = root.0.as_path();
        std::fs::create_dir(home.join("sessions")).unwrap();
        let path = home.join("sessions/rollout-test.jsonl");
        let head = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"test\"}}\n";
        let fail = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"t\",\"error\":{\"codex_error_info\":\"server_overloaded\"}}}\n";
        std::fs::write(&path, format!("{head}{}\n{fail}",serde_json::json!({"type":"response_item","payload":"x".repeat(MAX_TAIL as usize+1000)}))).unwrap();
        let locator = TranscriptLocator::capture(home, &path).unwrap();
        let mut size = 0;
        assert!(read_failure(&locator, "t", &mut size).unwrap());
        assert!(!read_failure(&locator, "old", &mut size).unwrap());
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{incomplete").unwrap();
        assert!(read_failure(&locator, "t", &mut size).unwrap());
        file.write_all(b"}\n").unwrap();
        assert!(read_failure(&locator, "t", &mut size).is_err());
        drop(file);
        std::fs::write(&path, fail).unwrap();
        assert!(read_failure(&locator, "t", &mut size).is_err());
        std::fs::rename(&path, home.join("old")).unwrap();
        std::fs::write(&path, fail).unwrap();
        assert!(read_failure(&locator, "t", &mut 0).is_err());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(home.join("old"), &path).unwrap();
        assert!(TranscriptLocator::capture(home, &path).is_none());
    }
    #[test]
    fn terminal_outcomes_and_old_completion_after_new_start() {
        let line = |p| format!("{}\n", serde_json::json!({"type":"event_msg","payload":p}));
        for (error, result) in [
            (serde_json::Value::Null, TerminalOutcome::Success),
            (
                serde_json::json!({"codex_error_info":"unauthorized"}),
                TerminalOutcome::OtherError,
            ),
            (
                serde_json::json!({"codex_error_info":"server_overloaded"}),
                TerminalOutcome::Capacity,
            ),
        ] {
            let complete =
                line(serde_json::json!({"type":"task_complete","turn_id":"t","error":error}));
            assert_eq!(parse_terminal(complete.as_bytes(), "t").unwrap(), result);
            let later =
                line(serde_json::json!({"type":"task_started","turn_id":"new"})) + &complete;
            assert_eq!(
                parse_terminal(later.as_bytes(), "t").unwrap(),
                TerminalOutcome::Pending
            );
        }
        assert!(failure_hint(
            "■ Selected model is\nat capacity. Please try a different model.\n\n› Ask Codex to do anything"
        ));
        assert!(!failure_hint("ordinary output"));
        assert!(styled_placeholder_at(
            "\x1b[1mheader\n›\x1b[22m \x1b[2mAsk Codex to do anything",
            1
        ));
        assert!(!styled_placeholder_at(
            "\x1b[2mheader\n› Ask Codex to do anything",
            1
        ));
    }

    #[test]
    fn config_validation() {
        for prompt in [
            DEFAULT_PROMPT,
            "続行してください。",
            "Continue.\nPreserve scope.\n\n",
        ] {
            assert!(validate_prompt(prompt).is_ok());
        }
        for prompt in [
            "",
            " ",
            "/model",
            "!cmd",
            "Continue @file",
            "Continue $skill",
            " x",
            "x ",
            "x\t",
            "x\r",
            "x\u{85}",
        ] {
            assert!(validate_prompt(prompt).is_err(), "{prompt:?}");
        }
        assert!(validate_prompt(&"a".repeat(65536)).is_ok());
        assert!(validate_prompt(&"あ".repeat(21846)).is_err());
    }
    #[test]
    fn terminal_only_and_late_items() {
        let fail = b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"t\",\"error\":{\"codex_error_info\":\"server_overloaded\"}}}\n";
        assert!(parse_tail(fail, "t").unwrap());
        assert!(!parse_tail(fail, "old").unwrap());
        let mut data = fail.to_vec();
        data.extend_from_slice(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"turn_id\":\"t\"}}\n");
        assert!(!parse_tail(&data, "t").unwrap());
        assert!(!parse_tail(&fail[..fail.len() - 1], "t").unwrap());
        assert!(parse_tail(b"invalid\n", "t").is_err());
    }
    #[test]
    fn empty_composer_guards() {
        let good = format!("■ {ERROR}\n\n› Ask Codex to do anything\n  ? for shortcuts");
        assert!(frame(&good).is_some());
        assert!(frame(&good.replace('›', "»")).is_some());
        for extra in ["[Image #1]", "• Queued follow-up inputs", "? 1 question"] {
            assert!(frame(&good.replace("\n\n›", &format!("\n{extra}\n›"))).is_none());
        }
        for glyph in [
            "!",
            "Viewing sub-agent — direct input is disabled",
            "Ask a follow-up question",
        ] {
            assert!(frame(&good.replace("› Ask Codex to do anything", glyph)).is_none());
        }
        assert!(!styled_placeholder("› Ask Codex to do anything"));
        assert!(styled_placeholder(
            "\x1b[1m›\x1b[22m \x1b[2mAsk Codex to do anything\x1b[0m"
        ));
        assert!(styled_placeholder(
            "\x1b[1m»\x1b[22m \x1b[2mAsk Codex to do anything"
        ));
        assert!(!styled_placeholder("\x1b[2m› Ask Codex to do anything"));
    }
    #[test]
    fn retry_bounds() {
        for (a, s) in [60, 120, 300].into_iter().enumerate() {
            for j in 0..1000 {
                assert!((s..=s + s / 5).contains(&delay(a, j)));
            }
        }
    }
}
