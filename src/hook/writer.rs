use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::hook::{
    PaneOptionValue, PaneOptionWrite, SubagentEntry, TaskItem, TaskProgress, WorktreeActivity,
    encode_subagents, encode_task_items, encode_worktree_activity,
};
use crate::options::{
    KEY_SUBAGENTS, KEY_TASK_ITEM_IDS, KEY_TASK_ITEMS, KEY_TASKS, KEY_WORKTREE_ACTIVITY,
    set_pane_option, unset_pane_option,
};
use crate::pane_state::{
    BODY_MAX_BYTES, IDENTIFIER_MAX_BYTES, PATH_MAX_BYTES, PaneInstance, ProgressOperation,
    SubagentState, TaskItemState, TaskItemStatus as CanonicalTaskItemStatus,
    TaskProgress as CanonicalTaskProgress, WorktreeActivity as CanonicalWorktreeActivity,
    WorktreeActivityKind as CanonicalWorktreeActivityKind, normalize_text, validate_required_text,
};
use crate::tmux::TmuxRunner;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProgressState {
    pub tasks: TaskProgress,
    pub task_items: Vec<TaskItem>,
    pub task_item_ids: Vec<String>,
    pub subagents: Vec<SubagentEntry>,
    pub worktree_activity: Option<WorktreeActivity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    TaskCreated,
    TaskCompleted,
    TaskSnapshot {
        progress: TaskProgress,
        items: Vec<TaskItem>,
    },
    TaskItemCreated {
        id: String,
        step: String,
    },
    TaskItemUpdated {
        id: String,
        status: crate::hook::TaskItemStatus,
    },
    WorktreeActivity(WorktreeActivity),
    SubagentStart(SubagentEntry),
    SubagentStop {
        agent_id: String,
    },
}

pub fn typed_progress_operations(event: ProgressEvent) -> Result<Vec<ProgressOperation>> {
    let operation = match event {
        ProgressEvent::TaskCreated => ProgressOperation::TaskCreated,
        ProgressEvent::TaskCompleted => ProgressOperation::TaskCompleted,
        ProgressEvent::TaskSnapshot { progress, items } => {
            let progress = canonical_progress(progress)?;
            if progress.total == 0 {
                if !items.is_empty() {
                    bail!("InvalidProgressOperation: zero task total has task items");
                }
                ProgressOperation::ClearTasks
            } else {
                let items = items
                    .into_iter()
                    .map(|item| {
                        let step = normalize_text(&item.step);
                        validate_required_text(&step, "task step", BODY_MAX_BYTES)?;
                        Ok(TaskItemState {
                            id: None,
                            step,
                            status: canonical_task_status(item.status),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                ProgressOperation::ReplaceTasks { progress, items }
            }
        }
        ProgressEvent::TaskItemCreated { id, step } => {
            let id = normalize_text(&id);
            let step = normalize_text(&step);
            validate_required_text(&id, "task item ID", IDENTIFIER_MAX_BYTES)?;
            validate_required_text(&step, "task step", BODY_MAX_BYTES)?;
            ProgressOperation::UpsertTaskItem { id, step }
        }
        ProgressEvent::TaskItemUpdated { id, status } => {
            let id = normalize_text(&id);
            validate_required_text(&id, "task item ID", IDENTIFIER_MAX_BYTES)?;
            ProgressOperation::UpdateTaskItemStatus {
                id,
                status: canonical_task_status(status),
            }
        }
        ProgressEvent::WorktreeActivity(activity) => {
            let activity = CanonicalWorktreeActivity {
                kind: match activity.kind {
                    crate::hook::WorktreeActivityKind::VwExec => {
                        CanonicalWorktreeActivityKind::VwExec
                    }
                },
                name: normalize_text(&activity.name),
                path: normalize_text(&activity.path),
                command: normalize_text(&activity.command),
                observed_at: activity.observed_at,
            };
            validate_required_text(&activity.name, "worktree name", BODY_MAX_BYTES)?;
            validate_required_text(&activity.path, "worktree path", PATH_MAX_BYTES)?;
            validate_required_text(&activity.command, "worktree command", BODY_MAX_BYTES)?;
            ProgressOperation::SetWorktreeActivity(activity)
        }
        ProgressEvent::SubagentStart(entry) => {
            let entry = SubagentState {
                agent_id: normalize_text(&entry.agent_id),
                agent_type: normalize_text(&entry.agent_type),
                display_name: entry
                    .display_name
                    .as_deref()
                    .map(normalize_text)
                    .filter(|name| !name.is_empty()),
            };
            validate_required_text(&entry.agent_id, "subagent ID", IDENTIFIER_MAX_BYTES)?;
            validate_required_text(&entry.agent_type, "subagent type", IDENTIFIER_MAX_BYTES)?;
            if let Some(name) = &entry.display_name {
                validate_required_text(name, "subagent name", IDENTIFIER_MAX_BYTES)?;
            }
            ProgressOperation::UpsertSubagent(entry)
        }
        ProgressEvent::SubagentStop { agent_id } => {
            let agent_id = normalize_text(&agent_id);
            validate_required_text(&agent_id, "subagent ID", IDENTIFIER_MAX_BYTES)?;
            ProgressOperation::RemoveSubagent { agent_id }
        }
    };
    Ok(vec![operation])
}

fn canonical_progress(progress: TaskProgress) -> Result<CanonicalTaskProgress> {
    if progress.done < 0 || progress.total < 0 {
        bail!("InvalidProgressOperation: task progress cannot be negative");
    }
    let progress = CanonicalTaskProgress {
        done: progress.done as u64,
        total: progress.total as u64,
    };
    if progress.done > progress.total {
        bail!("InvalidProgressOperation: task progress exceeds total");
    }
    Ok(progress)
}

fn canonical_task_status(status: crate::hook::TaskItemStatus) -> CanonicalTaskItemStatus {
    match status {
        crate::hook::TaskItemStatus::Pending => CanonicalTaskItemStatus::Pending,
        crate::hook::TaskItemStatus::InProgress => CanonicalTaskItemStatus::InProgress,
        crate::hook::TaskItemStatus::Completed => CanonicalTaskItemStatus::Completed,
    }
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

pub fn resolve_pane_instance(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<Option<PaneInstance>> {
    let Some(target) = resolve_pane(runner, env)? else {
        return Ok(None);
    };
    let output = runner.run(&[
        "display-message",
        "-p",
        "-t",
        &target,
        "#{pane_id}\t#{pane_pid}",
    ])?;
    let Some((pane_id, pane_pid)) = output.trim().split_once('\t') else {
        bail!("InvalidPaneInstance: tmux returned an invalid pane identity");
    };
    let pane = PaneInstance {
        pane_id: pane_id.to_string(),
        pane_pid: pane_pid
            .parse()
            .map_err(|_| anyhow::anyhow!("InvalidPaneInstance: invalid pane PID {pane_pid}"))?,
    };
    pane.validate()?;
    Ok(Some(pane))
}

pub fn write_pane_options(
    runner: &dyn TmuxRunner,
    pane: &str,
    writes: &[PaneOptionWrite],
) -> Result<()> {
    if writes.is_empty() {
        return Ok(());
    }
    let output = runner.run(&["show-options", "-p", "-t", pane])?;
    let mut current = parse_pane_options(&output);
    write_pane_options_with_current(runner, pane, writes, &mut current)
}

fn write_pane_options_with_current(
    runner: &dyn TmuxRunner,
    pane: &str,
    writes: &[PaneOptionWrite],
    current: &mut BTreeMap<String, String>,
) -> Result<()> {
    for write in writes {
        match &write.value {
            PaneOptionValue::Set(value) => {
                if current.get(write.key) == Some(value) {
                    continue;
                }
                set_pane_option(runner, pane, write.key, value)?;
                current.insert(write.key.to_string(), value.clone());
            }
            PaneOptionValue::Unset => {
                if !current.contains_key(write.key) {
                    continue;
                }
                unset_pane_option(runner, pane, write.key)?;
                current.remove(write.key);
            }
        }
    }
    Ok(())
}

fn parse_pane_options(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|line| parse_pane_option_line(line.trim()))
        .collect()
}

fn parse_pane_option_line(line: &str) -> Option<(String, String)> {
    if line.is_empty() {
        return None;
    }
    let (name, value) = match line.split_once(char::is_whitespace) {
        Some((name, value)) => (name.trim(), unquote_option_value(value.trim())),
        None => (line, String::new()),
    };
    Some((name.to_string(), value))
}

pub fn parse_progress_state(output: &str) -> ProgressState {
    let mut state = ProgressState::default();
    for (name, value) in parse_pane_options(output) {
        match name.as_str() {
            KEY_TASKS => state.tasks = parse_tasks(&value),
            KEY_TASK_ITEMS => {
                state.task_items = serde_json::from_str(&value).unwrap_or_default();
            }
            KEY_TASK_ITEM_IDS => {
                state.task_item_ids = decode_task_item_ids(&value);
            }
            KEY_SUBAGENTS => state.subagents = decode_subagents(&value),
            KEY_WORKTREE_ACTIVITY => {
                state.worktree_activity = serde_json::from_str(&value).ok();
            }
            _ => {}
        }
    }
    state
}

pub fn apply_progress_event(
    runner: &dyn TmuxRunner,
    pane: &str,
    event: ProgressEvent,
) -> Result<()> {
    let output = runner.run(&["show-options", "-p", "-t", pane])?;
    let mut current = parse_pane_options(&output);
    let mut state = parse_progress_state(&output);
    let writes = match event {
        ProgressEvent::TaskCreated => {
            state.tasks.total += 1;
            vec![PaneOptionWrite::set(KEY_TASKS, state.tasks.encode())]
        }
        ProgressEvent::TaskCompleted => {
            state.tasks.done += 1;
            vec![PaneOptionWrite::set(KEY_TASKS, state.tasks.encode())]
        }
        ProgressEvent::TaskSnapshot { progress, items } if progress.total > 0 => vec![
            PaneOptionWrite::set(KEY_TASKS, progress.encode()),
            PaneOptionWrite::set(KEY_TASK_ITEMS, encode_task_items(&items)),
            PaneOptionWrite::unset(KEY_TASK_ITEM_IDS),
        ],
        ProgressEvent::TaskSnapshot { .. } => vec![
            PaneOptionWrite::unset(KEY_TASKS),
            PaneOptionWrite::unset(KEY_TASK_ITEMS),
            PaneOptionWrite::unset(KEY_TASK_ITEM_IDS),
        ],
        ProgressEvent::TaskItemCreated { id, step } => {
            if let Some(index) = state
                .task_item_ids
                .iter()
                .position(|existing| existing == &id)
            {
                state.task_items[index].step = step;
                state.task_items[index].status = crate::hook::TaskItemStatus::Pending;
            } else {
                state.task_item_ids.push(id);
                state.task_items.push(TaskItem {
                    step,
                    status: crate::hook::TaskItemStatus::Pending,
                });
            }
            let progress = progress_from_task_items(&state.task_items);
            vec![
                PaneOptionWrite::set(KEY_TASKS, progress.encode()),
                PaneOptionWrite::set(KEY_TASK_ITEMS, encode_task_items(&state.task_items)),
                PaneOptionWrite::set(
                    KEY_TASK_ITEM_IDS,
                    encode_task_item_ids(&state.task_item_ids),
                ),
            ]
        }
        ProgressEvent::TaskItemUpdated { id, status } => {
            let Some(index) = state
                .task_item_ids
                .iter()
                .position(|existing| existing == &id)
            else {
                return Ok(());
            };
            state.task_items[index].status = status;
            let progress = progress_from_task_items(&state.task_items);
            vec![
                PaneOptionWrite::set(KEY_TASKS, progress.encode()),
                PaneOptionWrite::set(KEY_TASK_ITEMS, encode_task_items(&state.task_items)),
            ]
        }
        ProgressEvent::WorktreeActivity(activity) => vec![PaneOptionWrite::set(
            KEY_WORKTREE_ACTIVITY,
            encode_worktree_activity(&activity),
        )],
        ProgressEvent::SubagentStart(entry) => {
            state
                .subagents
                .retain(|existing| existing.agent_id != entry.agent_id);
            state.subagents.push(entry);
            vec![PaneOptionWrite::set(
                KEY_SUBAGENTS,
                encode_subagents(&state.subagents),
            )]
        }
        ProgressEvent::SubagentStop { agent_id } => {
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
    write_pane_options_with_current(runner, pane, &writes, &mut current)
}

fn trim_option_value(raw: &str) -> &str {
    raw.trim_matches(|ch| ch == '"' || ch == '\'')
}

fn unquote_option_value(raw: &str) -> String {
    serde_json::from_str::<String>(raw).unwrap_or_else(|_| trim_option_value(raw).to_string())
}

fn progress_from_task_items(items: &[TaskItem]) -> TaskProgress {
    TaskProgress {
        done: items
            .iter()
            .filter(|item| item.status == crate::hook::TaskItemStatus::Completed)
            .count() as i64,
        total: items.len() as i64,
    }
}

fn encode_task_item_ids(ids: &[String]) -> String {
    serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_string())
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
            entry.split_once(':').map(|(agent_id, rest)| {
                let (agent_type, display_name) = match rest.split_once(':') {
                    Some((agent_type, display_name)) => (
                        agent_type.to_string(),
                        (!display_name.is_empty()).then(|| display_name.to_string()),
                    ),
                    None => (rest.to_string(), None),
                };
                SubagentEntry {
                    agent_id: agent_id.to_string(),
                    agent_type,
                    display_name,
                }
            })
        })
        .collect()
}

fn decode_task_item_ids(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::{
        PaneOptionWrite, SubagentEntry, TaskItem, TaskItemStatus, WorktreeActivity,
        WorktreeActivityKind, encode_task_items, encode_worktree_activity,
    };
    use crate::options::{
        KEY_AGENT, KEY_STATUS, KEY_SUBAGENTS, KEY_TASK_ITEM_IDS, KEY_TASK_ITEMS, KEY_TASKS,
        KEY_WORKTREE_ACTIVITY,
    };
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
    fn resolve_pane_instance_queries_id_and_pid_together() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "%7",
                "#{pane_id}\t#{pane_pid}",
            ],
            "%7\t4242\n",
        );

        assert_eq!(
            resolve_pane_instance(&mock, &env(&[("TMUX_PANE", "%7")])).unwrap(),
            Some(PaneInstance {
                pane_id: "%7".to_string(),
                pane_pid: 4242,
            })
        );
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn progress_events_map_to_closed_canonical_operations() {
        assert_eq!(
            typed_progress_operations(ProgressEvent::TaskCreated).unwrap(),
            vec![ProgressOperation::TaskCreated]
        );
        assert_eq!(
            typed_progress_operations(ProgressEvent::TaskItemCreated {
                id: " task\n1 ".to_string(),
                step: " implement\ttyped adapter ".to_string(),
            })
            .unwrap(),
            vec![ProgressOperation::UpsertTaskItem {
                id: "task 1".to_string(),
                step: "implement typed adapter".to_string(),
            }]
        );
        assert_eq!(
            typed_progress_operations(ProgressEvent::SubagentStop {
                agent_id: " worker\t1 ".to_string(),
            })
            .unwrap(),
            vec![ProgressOperation::RemoveSubagent {
                agent_id: "worker 1".to_string(),
            }]
        );
    }

    #[test]
    fn task_snapshot_maps_empty_to_clear_and_nonempty_to_replace() {
        assert_eq!(
            typed_progress_operations(ProgressEvent::TaskSnapshot {
                progress: TaskProgress { done: 0, total: 0 },
                items: Vec::new(),
            })
            .unwrap(),
            vec![ProgressOperation::ClearTasks]
        );
        let operations = typed_progress_operations(ProgressEvent::TaskSnapshot {
            progress: TaskProgress { done: 1, total: 2 },
            items: vec![
                TaskItem {
                    step: "Explore".to_string(),
                    status: TaskItemStatus::Completed,
                },
                TaskItem {
                    step: "Implement".to_string(),
                    status: TaskItemStatus::InProgress,
                },
            ],
        })
        .unwrap();
        let ProgressOperation::ReplaceTasks { progress, items } = &operations[0] else {
            panic!("expected task replacement");
        };
        assert_eq!(*progress, CanonicalTaskProgress { done: 1, total: 2 });
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|item| item.id.is_none()));
    }

    #[test]
    fn write_pane_options_sets_and_unsets() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["show-options", "-p", "-t", "%1"],
            "@vde_status \"running\"\n",
        );
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
        assert_eq!(mock.calls().len(), 3);
    }

    #[test]
    fn write_pane_options_skips_unchanged_sets_and_absent_unsets() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["show-options", "-p", "-t", "%1"],
            "@vde_agent \"codex\"\n",
        );

        write_pane_options(
            &mock,
            "%1",
            &[
                PaneOptionWrite::set(KEY_AGENT, "codex"),
                PaneOptionWrite::unset(KEY_STATUS),
            ],
        )
        .unwrap();

        assert_eq!(
            mock.calls(),
            vec![vec![
                "show-options".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                "%1".to_string(),
            ]]
        );
    }

    #[test]
    fn write_pane_options_tracks_projected_state_for_clear_then_set() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["show-options", "-p", "-t", "%1"],
            "@vde_status \"idle\"\n",
        );
        mock.stub(&["set-option", "-p", "-u", "-t", "%1", KEY_STATUS], "");
        mock.stub(&["set-option", "-p", "-t", "%1", KEY_STATUS, "idle"], "");

        write_pane_options(
            &mock,
            "%1",
            &[
                PaneOptionWrite::unset(KEY_STATUS),
                PaneOptionWrite::set(KEY_STATUS, "idle"),
            ],
        )
        .unwrap();

        assert_eq!(mock.calls().len(), 3);
    }

    #[test]
    fn parse_progress_state_reads_tasks_task_items_subagents_and_worktree_activity() {
        let task_items = serde_json::json!([
            {"step": "Explore", "status": "completed"},
            {"step": "Implement", "status": "in_progress"}
        ])
        .to_string();
        let activity = serde_json::json!({
            "kind": "vw_exec",
            "name": "feature",
            "path": "/tmp/worktrees/feature",
            "command": "vw exec feature -- cargo test",
            "observed_at": 42
        })
        .to_string();
        let state = parse_progress_state(&format!(
            "@vde_tasks \"2/5\"\n@vde_task_items '{}'\n@vde_subagents \"a:Explore|b:Plan\"\n@vde_worktree_activity '{}'\n",
            task_items, activity
        ));
        assert_eq!(state.tasks.done, 2);
        assert_eq!(state.tasks.total, 5);
        assert_eq!(state.task_items.len(), 2);
        assert_eq!(state.task_items[0].status, TaskItemStatus::Completed);
        assert_eq!(state.subagents.len(), 2);
        assert_eq!(state.worktree_activity.as_ref().unwrap().name, "feature");
    }

    #[test]
    fn parse_progress_state_reads_subagent_display_names() {
        let state = parse_progress_state(
            "@vde_subagents \"019f3c28:default:Ramanujan|019f3c86:worker:Fermat\"\n",
        );

        assert_eq!(state.subagents.len(), 2);
        assert_eq!(state.subagents[0].agent_type, "default");
        assert_eq!(
            state.subagents[0].display_name.as_deref(),
            Some("Ramanujan")
        );
        assert_eq!(state.subagents[1].display_name.as_deref(), Some("Fermat"));
    }

    #[test]
    fn task_created_increments_total_without_touching_task_items_or_worktree_activity() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["show-options", "-p", "-t", "%1"],
            "@vde_tasks \"2/5\"\n@vde_task_items \"[]\"\n@vde_worktree_activity \"{}\"\n",
        );
        mock.stub(&["set-option", "-p", "-t", "%1", KEY_TASKS, "2/6"], "");
        apply_progress_event(&mock, "%1", ProgressEvent::TaskCreated).unwrap();
        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn task_snapshot_sets_absolute_progress_and_items() {
        let mock = MockTmuxRunner::new();
        let items = vec![
            TaskItem {
                step: "Explore".to_string(),
                status: TaskItemStatus::Completed,
            },
            TaskItem {
                step: "Implement".to_string(),
                status: TaskItemStatus::InProgress,
            },
        ];
        mock.stub(&["show-options", "-p", "-t", "%1"], "");
        mock.stub(&["set-option", "-p", "-t", "%1", KEY_TASKS, "1/2"], "");
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                KEY_TASK_ITEMS,
                &encode_task_items(&items),
            ],
            "",
        );
        mock.stub(
            &["set-option", "-p", "-u", "-t", "%1", KEY_TASK_ITEM_IDS],
            "",
        );

        apply_progress_event(
            &mock,
            "%1",
            ProgressEvent::TaskSnapshot {
                progress: TaskProgress { done: 1, total: 2 },
                items,
            },
        )
        .unwrap();

        assert_eq!(mock.calls().len(), 3);
    }

    #[test]
    fn task_snapshot_unsets_empty_progress_and_items() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["show-options", "-p", "-t", "%1"],
            "@vde_tasks \"1/1\"\n@vde_task_items \"[]\"\n@vde_task_item_ids '[\"1\"]'\n",
        );
        mock.stub(&["set-option", "-p", "-u", "-t", "%1", KEY_TASKS], "");
        mock.stub(&["set-option", "-p", "-u", "-t", "%1", KEY_TASK_ITEMS], "");
        mock.stub(
            &["set-option", "-p", "-u", "-t", "%1", KEY_TASK_ITEM_IDS],
            "",
        );

        apply_progress_event(
            &mock,
            "%1",
            ProgressEvent::TaskSnapshot {
                progress: TaskProgress { done: 0, total: 0 },
                items: Vec::new(),
            },
        )
        .unwrap();

        assert_eq!(mock.calls().len(), 4);
    }

    #[test]
    fn worktree_activity_sets_activity_option() {
        let mock = MockTmuxRunner::new();
        let activity = WorktreeActivity {
            kind: WorktreeActivityKind::VwExec,
            name: "feature".to_string(),
            path: "/tmp/worktrees/feature".to_string(),
            command: "vw exec feature -- cargo test".to_string(),
            observed_at: 42,
        };
        mock.stub(&["show-options", "-p", "-t", "%1"], "");
        mock.stub(
            &[
                "set-option",
                "-p",
                "-t",
                "%1",
                KEY_WORKTREE_ACTIVITY,
                &encode_worktree_activity(&activity),
            ],
            "",
        );

        apply_progress_event(&mock, "%1", ProgressEvent::WorktreeActivity(activity)).unwrap();

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
        apply_progress_event(
            &mock,
            "%1",
            ProgressEvent::SubagentStop {
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
        apply_progress_event(
            &mock,
            "%1",
            ProgressEvent::SubagentStart(SubagentEntry {
                agent_id: "a".to_string(),
                agent_type: "Plan".to_string(),
                display_name: None,
            }),
        )
        .unwrap();
        assert_eq!(mock.calls().len(), 2);
    }
}
