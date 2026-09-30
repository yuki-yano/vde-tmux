//! Input readiness for the first embedded prompt is separate from hook authority.
//! Nothing here changes presentation, lifecycle, or provider confirmation.

use crate::pane_state::{CaptureTrackerSnapshot, LifecycleState, PaneState};
use crate::tmux::TmuxRunner;
use ansi_to_tui::IntoText as _;
use ratatui::style::Modifier;

pub(super) fn candidate(record: &PaneState, tracker: &CaptureTrackerSnapshot) -> bool {
    record.agent.as_str() == "codex"
        && record.agent_present
        && record.scan_verified
        && record.agent_process.is_some()
        && !tracker.hook_authoritative
        && record.agent_session_id.is_none()
        && record.run_seq == 0
        && record.completed_seq == 0
        && record.current_run.is_none()
        && record.prompt.is_none()
        && matches!(record.lifecycle, LifecycleState::Idle)
}

pub(super) fn require_ready(runner: &dyn TmuxRunner, record: &PaneState) -> Result<(), String> {
    let process = record
        .agent_process
        .as_ref()
        .ok_or("agent process is unavailable")?;
    let args = runner
        .agent_process_arguments(process)
        .map_err(|error| error.to_string())?;
    if !crate::question_notice::profile::independent_arguments(&args) {
        return Err("first prompt requires an explicit --no-daemon interactive invocation without a queued initial prompt".into());
    }
    let pane = &record.pane_instance;
    let verify = || {
        if runner
            .resolve_agent_process(pane.pane_pid, &record.agent)
            .map_err(|error| error.to_string())?
            .as_ref()
            != Some(process)
        {
            return Err("agent process changed while verifying first prompt readiness".into());
        }
        runner
            .verify_agent_input_owner(pane.pane_pid, process.pid)
            .map_err(|error| error.to_string())
    };
    verify()?;
    // Read the live viewport and cursor in one tmux command queue. Matching
    // before/after frames exclude pane replacement, resize and cursor movement.
    let frame =
        "__vde_initial_prompt__#{pane_pid}:#{cursor_x}:#{cursor_y}:#{pane_width}:#{pane_height}";
    let output = runner
        .run_bounded(
            &[
                "display-message",
                "-p",
                "-t",
                &pane.pane_id,
                frame,
                ";",
                "capture-pane",
                "-p",
                "-e",
                "-t",
                &pane.pane_id,
                ";",
                "display-message",
                "-p",
                "-t",
                &pane.pane_id,
                frame,
            ],
            512 * 1024,
        )
        .map_err(|error| error.to_string())?;
    if output.truncated || !empty_composer(&output.text, pane.pane_pid) {
        return Err("Codex initial input field is not ready or is not empty".into());
    }
    verify()
}

fn empty_composer(output: &str, pane_pid: u32) -> bool {
    let lines: Vec<_> = output.lines().collect();
    let Some(header) = lines
        .first()
        .filter(|first| lines.len() >= 3 && Some(*first) == lines.last())
    else {
        return false;
    };
    let Some(shape) = header.strip_prefix("__vde_initial_prompt__") else {
        return false;
    };
    let numbers: Option<Vec<usize>> = shape.split(':').map(|part| part.parse().ok()).collect();
    let Some(numbers) = numbers.filter(|values| values.len() == 5) else {
        return false;
    };
    let [pid, x, y, width, height] = numbers[..] else {
        return false;
    };
    if pid != pane_pid as usize
        || width == 0
        || height != lines.len() - 2
        || y >= height
        || x >= width
    {
        return false;
    }
    let ansi_screen = lines[1..lines.len() - 1].join("\n");
    let Ok(styled) = ansi_screen.as_bytes().into_text() else {
        return false;
    };
    if styled.lines.len() > height || y >= styled.lines.len() {
        return false;
    }
    let screen = styled
        .lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    if crate::detect::codex::classify(&screen) != crate::detect::codex::Evidence::default() {
        return false;
    }
    let composer = styled.lines[y].to_string();
    let leading_spaces = composer.bytes().take_while(|byte| *byte == b' ').count();
    if composer.trim() != "› Ask Codex to do anything" || x != leading_spaces + 2 {
        return false;
    }
    // Stock Codex renders a placeholder dim, while typed drafts use normal
    // text. A draft containing the placeholder words with its cursor at Home
    // must not be mistaken for an empty composer.
    let start = leading_spaces + 2;
    let end = start + "Ask Codex to do anything".len();
    let mut offset = 0;
    styled.lines[y].spans.iter().all(|span| {
        let next = offset + span.content.chars().count();
        let overlaps_placeholder = offset < end && next > start;
        offset = next;
        !overlaps_placeholder || span.style.add_modifier.contains(Modifier::DIM)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_composer_requires_stable_shape_cursor_and_positive_empty_field() {
        let frame = "__vde_initial_prompt__42:2:1:80:4";
        let screen = format!(
            "{frame}\nCodex\n› \x1b[2mAsk Codex to do anything\x1b[0m\n\n? for shortcuts\n{frame}\n"
        );
        assert!(empty_composer(&screen, 42));
        let blank_bottom = screen.replace("? for shortcuts", "");
        assert!(empty_composer(&blank_bottom, 42));
        for rejected in [
            screen.replace("42:2:", "42:3:"),
            screen.replace("\x1b[2m", ""),
            screen.replace("42:2:", "43:2:"),
            screen.replacen(frame, "__vde_initial_prompt__42:2:2:80:4", 1),
            screen.replace("Ask Codex to do anything", "typed draft"),
            screen.replace("Ask Codex to do anything", "1. Yes, continue"),
            screen.replacen("Codex\n", "• Working (3s)\n", 1),
            screen.replace("? for shortcuts", "Question 1/1 (1 unanswered)"),
            screen.replace("? for shortcuts", "↑/↓ to scroll"),
            screen.replace("? for shortcuts", "  ? 1 question · 3s"),
        ] {
            assert!(!empty_composer(&rejected, 42), "{rejected}");
        }
    }
}
