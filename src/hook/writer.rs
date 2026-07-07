use std::collections::BTreeMap;

use anyhow::Result;

use crate::hook::{
    PaneOptionValue, PaneOptionWrite, SubagentEntry, TaskProgress, encode_subagents,
};
use crate::options::{KEY_SUBAGENTS, KEY_TASKS, set_pane_option, unset_pane_option};
use crate::tmux::TmuxRunner;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProgressState {
    pub tasks: TaskProgress,
    pub subagents: Vec<SubagentEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeProgressEvent {
    TaskCreated,
    TaskCompleted,
    SubagentStart(SubagentEntry),
    SubagentStop { agent_id: String },
}

pub fn resolve_pane(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    if let Some(pane) = env.get("TMUX_PANE").filter(|pane| !pane.trim().is_empty()) {
        return Ok(Some(pane.clone()));
    }
    let pane = runner
        .run(&["display-message", "-p", "#{pane_id}"])?
        .trim()
        .to_string();
    if pane.is_empty() {
        Ok(None)
    } else {
        Ok(Some(pane))
    }
}

pub fn write_pane_options(
    runner: &dyn TmuxRunner,
    pane: &str,
    writes: &[PaneOptionWrite],
) -> Result<()> {
    for write in writes {
        match &write.value {
            PaneOptionValue::Set(value) => set_pane_option(runner, pane, write.key, value)?,
            PaneOptionValue::Unset => unset_pane_option(runner, pane, write.key)?,
        }
    }
    Ok(())
}

pub fn parse_progress_state(output: &str) -> ProgressState {
    let mut state = ProgressState::default();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (name, value) = match line.split_once(char::is_whitespace) {
            Some((name, value)) => (name.trim(), value.trim().trim_matches('"')),
            None => (line, ""),
        };
        match name {
            KEY_TASKS => state.tasks = parse_tasks(value),
            KEY_SUBAGENTS => state.subagents = decode_subagents(value),
            _ => {}
        }
    }
    state
}

pub fn apply_claude_progress_event(
    runner: &dyn TmuxRunner,
    pane: &str,
    event: ClaudeProgressEvent,
) -> Result<()> {
    let output = runner.run(&["show-options", "-p", "-t", pane])?;
    let mut state = parse_progress_state(&output);
    let writes = match event {
        ClaudeProgressEvent::TaskCreated => {
            state.tasks.total += 1;
            vec![PaneOptionWrite::set(KEY_TASKS, state.tasks.encode())]
        }
        ClaudeProgressEvent::TaskCompleted => {
            state.tasks.done += 1;
            vec![PaneOptionWrite::set(KEY_TASKS, state.tasks.encode())]
        }
        ClaudeProgressEvent::SubagentStart(entry) => {
            state
                .subagents
                .retain(|existing| existing.agent_id != entry.agent_id);
            state.subagents.push(entry);
            vec![PaneOptionWrite::set(
                KEY_SUBAGENTS,
                encode_subagents(&state.subagents),
            )]
        }
        ClaudeProgressEvent::SubagentStop { agent_id } => {
            state
                .subagents
                .retain(|existing| existing.agent_id != agent_id);
            if state.subagents.is_empty() {
                vec![PaneOptionWrite::unset(KEY_SUBAGENTS)]
            } else {
                vec![PaneOptionWrite::set(
                    KEY_SUBAGENTS,
                    encode_subagents(&state.subagents),
                )]
            }
        }
    };
    write_pane_options(runner, pane, &writes)
}

fn parse_tasks(raw: &str) -> TaskProgress {
    let Some((done, total)) = raw.split_once('/') else {
        return TaskProgress::default();
    };
    TaskProgress {
        done: done.parse().unwrap_or_default(),
        total: total.parse().unwrap_or_default(),
    }
}

fn decode_subagents(raw: &str) -> Vec<SubagentEntry> {
    raw.split('|')
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            entry
                .split_once(':')
                .map(|(agent_id, agent_type)| SubagentEntry {
                    agent_id: agent_id.to_string(),
                    agent_type: agent_type.to_string(),
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::{PaneOptionWrite, SubagentEntry};
    use crate::options::{KEY_AGENT, KEY_STATUS, KEY_SUBAGENTS, KEY_TASKS};
    use crate::tmux::mock::MockTmuxRunner;
    use std::collections::BTreeMap;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn resolve_pane_prefers_tmux_pane_env() {
        let mock = MockTmuxRunner::new();
        assert_eq!(
            resolve_pane(&mock, &env(&[("TMUX_PANE", "%1")])).unwrap(),
            Some("%1".to_string())
        );
        assert!(mock.calls().is_empty());
    }

    #[test]
    fn write_pane_options_sets_and_unsets() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["set-option", "-p", "-t", "%1", KEY_AGENT, "codex"], "");
        mock.stub(&["set-option", "-p", "-u", "-t", "%1", KEY_STATUS], "");
        write_pane_options(
            &mock,
            "%1",
            &[
                PaneOptionWrite::set(KEY_AGENT, "codex"),
                PaneOptionWrite::unset(KEY_STATUS),
            ],
        )
        .unwrap();
        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn parse_progress_state_reads_tasks_and_subagents() {
        let state =
            parse_progress_state("@vde_tasks \"2/5\"\n@vde_subagents \"a:Explore|b:Plan\"\n");
        assert_eq!(state.tasks.done, 2);
        assert_eq!(state.tasks.total, 5);
        assert_eq!(state.subagents.len(), 2);
    }

    #[test]
    fn task_created_increments_total() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["show-options", "-p", "-t", "%1"], "@vde_tasks \"2/5\"\n");
        mock.stub(&["set-option", "-p", "-t", "%1", KEY_TASKS, "2/6"], "");
        apply_claude_progress_event(&mock, "%1", ClaudeProgressEvent::TaskCreated).unwrap();
        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn subagent_stop_unsets_when_last_entry_is_removed() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["show-options", "-p", "-t", "%1"],
            "@vde_subagents \"a:Explore\"\n",
        );
        mock.stub(&["set-option", "-p", "-u", "-t", "%1", KEY_SUBAGENTS], "");
        apply_claude_progress_event(
            &mock,
            "%1",
            ClaudeProgressEvent::SubagentStop {
                agent_id: "a".to_string(),
            },
        )
        .unwrap();
        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn subagent_start_appends_or_replaces_entry() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["show-options", "-p", "-t", "%1"],
            "@vde_subagents \"a:Explore\"\n",
        );
        mock.stub(
            &["set-option", "-p", "-t", "%1", KEY_SUBAGENTS, "a:Plan"],
            "",
        );
        apply_claude_progress_event(
            &mock,
            "%1",
            ClaudeProgressEvent::SubagentStart(SubagentEntry {
                agent_id: "a".to_string(),
                agent_type: "Plan".to_string(),
            }),
        )
        .unwrap();
        assert_eq!(mock.calls().len(), 2);
    }
}
