pub mod adapter;
pub mod origin;
pub mod writer;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Running,
    Waiting,
    Idle,
    Error,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Running => "running",
            AgentStatus::Waiting => "waiting",
            AgentStatus::Idle => "idle",
            AgentStatus::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollupLevel {
    Error,
    Running,
    Permission,
    Background,
    Waiting,
    Idle,
}

pub fn pane_rollup_level(status: Option<AgentStatus>, wait_reason: Option<&str>) -> RollupLevel {
    match status {
        Some(AgentStatus::Error) => RollupLevel::Error,
        Some(AgentStatus::Running) => RollupLevel::Running,
        Some(AgentStatus::Waiting) if is_permission_wait(wait_reason) => RollupLevel::Permission,
        Some(AgentStatus::Waiting) => RollupLevel::Waiting,
        Some(AgentStatus::Idle) => RollupLevel::Idle,
        None => RollupLevel::Background,
    }
}

fn is_permission_wait(wait_reason: Option<&str>) -> bool {
    matches!(
        wait_reason,
        Some(reason)
            if reason.eq_ignore_ascii_case("permission_prompt")
                || reason.eq_ignore_ascii_case("permission")
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionUpdate<T> {
    Set(T),
    Unset,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskProgress {
    pub done: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskItemStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskItem {
    pub step: String,
    pub status: TaskItemStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeActivityKind {
    VwExec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeActivity {
    pub kind: WorktreeActivityKind,
    pub name: String,
    pub path: String,
    pub command: String,
    pub observed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentEntry {
    pub agent_id: String,
    pub agent_type: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentEvent {
    pub clear_state: bool,
    pub agent: String,
    pub status: Option<AgentStatus>,
    pub prompt: Option<OptionUpdate<String>>,
    pub prompt_source: Option<OptionUpdate<String>>,
    pub wait_reason: Option<OptionUpdate<String>>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub tasks: Option<OptionUpdate<TaskProgress>>,
    pub task_items: Option<OptionUpdate<Vec<TaskItem>>>,
    pub subagents: Option<OptionUpdate<Vec<SubagentEntry>>>,
    pub worktree_activity: Option<OptionUpdate<WorktreeActivity>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollup_level_order_matches_attention_priority() {
        assert!(RollupLevel::Error < RollupLevel::Running);
        assert!(RollupLevel::Running < RollupLevel::Permission);
        assert!(RollupLevel::Permission < RollupLevel::Background);
        assert!(RollupLevel::Background < RollupLevel::Waiting);
        assert!(RollupLevel::Waiting < RollupLevel::Idle);
    }

    #[test]
    fn pane_rollup_level_maps_waiting_permission() {
        assert_eq!(
            pane_rollup_level(Some(AgentStatus::Waiting), Some("permission_prompt")),
            RollupLevel::Permission
        );
        assert_eq!(
            pane_rollup_level(Some(AgentStatus::Waiting), None),
            RollupLevel::Waiting
        );
        assert_eq!(pane_rollup_level(None, None), RollupLevel::Background);
    }
}
