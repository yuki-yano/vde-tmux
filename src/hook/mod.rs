pub mod adapter;
pub mod writer;

use serde::{Deserialize, Serialize};

use crate::options::{
    KEY_AGENT, KEY_ATTENTION, KEY_COMPLETED_AT, KEY_PROMPT, KEY_PROMPT_SOURCE, KEY_STARTED_AT,
    KEY_STATUS, KEY_SUBAGENTS, KEY_TASKS, KEY_WAIT_REASON, PANE_STATE_KEYS,
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneOptionValue {
    Set(String),
    Unset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneOptionWrite {
    pub key: &'static str,
    pub value: PaneOptionValue,
}

impl PaneOptionWrite {
    pub fn set(key: &'static str, value: impl Into<String>) -> Self {
        Self {
            key,
            value: PaneOptionValue::Set(value.into()),
        }
    }

    pub fn unset(key: &'static str) -> Self {
        Self {
            key,
            value: PaneOptionValue::Unset,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskProgress {
    pub done: i64,
    pub total: i64,
}

impl TaskProgress {
    pub fn encode(&self) -> String {
        format!("{}/{}", self.done, self.total)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentEntry {
    pub agent_id: String,
    pub agent_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentEvent {
    pub clear_state: bool,
    pub agent: String,
    pub status: Option<AgentStatus>,
    pub prompt: Option<OptionUpdate<String>>,
    pub prompt_source: Option<OptionUpdate<String>>,
    pub wait_reason: Option<OptionUpdate<String>>,
    pub attention: Option<bool>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub tasks: Option<OptionUpdate<TaskProgress>>,
    pub subagents: Option<OptionUpdate<Vec<SubagentEntry>>>,
}

pub fn derive_event_writes(event: &AgentEvent) -> Vec<PaneOptionWrite> {
    let mut writes = Vec::new();
    if event.clear_state {
        writes.extend(
            PANE_STATE_KEYS
                .iter()
                .map(|key| PaneOptionWrite::unset(key)),
        );
    }
    if let Some(status) = event.status {
        writes.push(PaneOptionWrite::set(KEY_STATUS, status.as_str()));
        if !event.clear_state
            && matches!(status, AgentStatus::Running | AgentStatus::Idle)
            && event.wait_reason.is_none()
        {
            writes.push(PaneOptionWrite::unset(KEY_WAIT_REASON));
        }
    }
    if let Some(update) = &event.wait_reason {
        push_string_update(&mut writes, KEY_WAIT_REASON, update);
    }
    if !event.agent.is_empty() {
        writes.push(PaneOptionWrite::set(KEY_AGENT, &event.agent));
    }
    if let Some(attention) = event.attention {
        writes.push(PaneOptionWrite::set(
            KEY_ATTENTION,
            if attention { "1" } else { "0" },
        ));
    }
    if let Some(started_at) = event.started_at {
        writes.push(PaneOptionWrite::set(KEY_STARTED_AT, started_at.to_string()));
    }
    if let Some(completed_at) = event.completed_at {
        writes.push(PaneOptionWrite::set(
            KEY_COMPLETED_AT,
            completed_at.to_string(),
        ));
    }
    if let Some(update) = &event.prompt {
        push_string_update(&mut writes, KEY_PROMPT, update);
    }
    if let Some(update) = &event.prompt_source {
        push_string_update(&mut writes, KEY_PROMPT_SOURCE, update);
    }
    if let Some(update) = &event.tasks {
        match update {
            OptionUpdate::Set(progress) => {
                writes.push(PaneOptionWrite::set(KEY_TASKS, progress.encode()));
            }
            OptionUpdate::Unset => writes.push(PaneOptionWrite::unset(KEY_TASKS)),
        }
    }
    if let Some(update) = &event.subagents {
        match update {
            OptionUpdate::Set(subagents) => {
                writes.push(PaneOptionWrite::set(
                    KEY_SUBAGENTS,
                    encode_subagents(subagents),
                ));
            }
            OptionUpdate::Unset => writes.push(PaneOptionWrite::unset(KEY_SUBAGENTS)),
        }
    }
    writes
}

fn push_string_update(
    writes: &mut Vec<PaneOptionWrite>,
    key: &'static str,
    update: &OptionUpdate<String>,
) {
    match update {
        OptionUpdate::Set(value) => writes.push(PaneOptionWrite::set(key, value)),
        OptionUpdate::Unset => writes.push(PaneOptionWrite::unset(key)),
    }
}

pub fn encode_subagents(entries: &[SubagentEntry]) -> String {
    entries
        .iter()
        .map(|entry| {
            format!(
                "{}:{}",
                sanitize_subagent_field(&entry.agent_id),
                sanitize_subagent_field(&entry.agent_type)
            )
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn sanitize_subagent_field(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_control() || ch == ':' || ch == '|' {
                ' '
            } else {
                ch
            }
        })
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{
        KEY_AGENT, KEY_PROMPT, KEY_PROMPT_SOURCE, KEY_STARTED_AT, KEY_STATUS, KEY_SUBAGENTS,
        KEY_TASKS, KEY_WAIT_REASON,
    };

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

    #[test]
    fn derive_event_writes_sets_agent_status_and_prompt() {
        let event = AgentEvent {
            agent: "codex".to_string(),
            status: Some(AgentStatus::Running),
            prompt: Some(OptionUpdate::Set("hello".to_string())),
            prompt_source: Some(OptionUpdate::Set("user".to_string())),
            started_at: Some(42),
            ..AgentEvent::default()
        };
        assert_eq!(
            derive_event_writes(&event),
            vec![
                PaneOptionWrite::set(KEY_STATUS, "running"),
                PaneOptionWrite::unset(KEY_WAIT_REASON),
                PaneOptionWrite::set(KEY_AGENT, "codex"),
                PaneOptionWrite::set(KEY_STARTED_AT, "42"),
                PaneOptionWrite::set(KEY_PROMPT, "hello"),
                PaneOptionWrite::set(KEY_PROMPT_SOURCE, "user"),
            ]
        );
    }

    #[test]
    fn derive_event_writes_encodes_tasks_and_subagents() {
        let event = AgentEvent {
            agent: "claude".to_string(),
            tasks: Some(OptionUpdate::Set(TaskProgress { done: 2, total: 5 })),
            subagents: Some(OptionUpdate::Set(vec![SubagentEntry {
                agent_id: "abc".to_string(),
                agent_type: "Explore".to_string(),
            }])),
            ..AgentEvent::default()
        };
        assert_eq!(
            derive_event_writes(&event),
            vec![
                PaneOptionWrite::set(KEY_AGENT, "claude"),
                PaneOptionWrite::set(KEY_TASKS, "2/5"),
                PaneOptionWrite::set(KEY_SUBAGENTS, "abc:Explore"),
            ]
        );
    }

    #[test]
    fn derive_event_writes_clears_existing_pane_state_before_session_start_values() {
        let event = AgentEvent {
            clear_state: true,
            agent: "codex".to_string(),
            status: Some(AgentStatus::Idle),
            attention: Some(false),
            prompt: Some(OptionUpdate::Set("latest prompt".to_string())),
            prompt_source: Some(OptionUpdate::Set("resume".to_string())),
            ..AgentEvent::default()
        };

        let writes = derive_event_writes(&event);

        for key in crate::options::PANE_STATE_KEYS {
            assert!(writes.contains(&PaneOptionWrite::unset(key)), "{key}");
        }
        assert!(writes.contains(&PaneOptionWrite::set(KEY_STATUS, "idle")));
        assert!(writes.contains(&PaneOptionWrite::set(KEY_AGENT, "codex")));
        assert!(writes.contains(&PaneOptionWrite::set(KEY_PROMPT, "latest prompt")));
        assert!(writes.contains(&PaneOptionWrite::set(KEY_PROMPT_SOURCE, "resume")));
    }
}
