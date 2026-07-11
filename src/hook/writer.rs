use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::hook::{SubagentEntry, TaskItem, TaskProgress, WorktreeActivity};
use crate::pane_state::{
    BODY_MAX_BYTES, IDENTIFIER_MAX_BYTES, PATH_MAX_BYTES, PaneInstance, ProgressOperation,
    SubagentState, TaskItemState, TaskItemStatus as CanonicalTaskItemStatus,
    TaskProgress as CanonicalTaskProgress, WorktreeActivity as CanonicalWorktreeActivity,
    WorktreeActivityKind as CanonicalWorktreeActivityKind, normalize_text, validate_required_text,
};
use crate::tmux::TmuxRunner;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::{TaskItemStatus, TaskProgress};
    use crate::tmux::mock::MockTmuxRunner;

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
}
