//! Claude's parent-shell result receipts, distinct from process liveness.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::model::{BODY_MAX_BYTES, IDENTIFIER_MAX_BYTES, ModelError};

pub const MAX_BACKGROUND_TASKS: usize = 256;

pub fn valid_background_id(id: &str) -> bool {
    !id.trim().is_empty()
        && id.trim() == id
        && id.len() <= IDENTIFIER_MAX_BYTES
        && !id.chars().any(char::is_control)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum RegistryPresence {
    Present,
    Absent,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundReceipt {
    Pending,
    Delivered,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundWaitTask {
    pub task_id: String,
    pub tool_use_id: String,
    pub owner_run_seq: u64,
    pub command: String,
    pub registered_at: i64,
    pub receipt: BackgroundReceipt,
    pub notification_status: Option<String>,
    pub last_registry_presence: RegistryPresence,
    pub last_checked_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundWaitFault {
    pub reason: String,
    pub key: String,
    pub reported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ClaudeBackgroundState {
    pub tasks: Vec<BackgroundWaitTask>,
    pub faults: Vec<BackgroundWaitFault>,
    pub paused: bool,
    pub paused_at: Option<i64>,
}

impl ClaudeBackgroundState {
    pub fn pending_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.receipt == BackgroundReceipt::Pending)
            .count()
    }

    pub fn blocks_completion(&self) -> bool {
        self.pending_count() > 0 || !self.faults.is_empty()
    }

    pub fn is_awaiting_result(&self) -> bool {
        self.paused && self.pending_count() > 0
    }

    pub fn activity(&mut self) {
        self.paused = false;
        self.paused_at = None;
    }

    pub fn fault(&mut self, reason: &str, key: &str) {
        if self
            .faults
            .iter()
            .any(|f| f.reason == reason && f.key == key)
        {
            return;
        }
        if self.faults.len() >= MAX_BACKGROUND_TASKS {
            if self
                .faults
                .iter()
                .any(|f| f.reason == "await_tracking_overflow")
            {
                return;
            }
            // A single sticky overflow owns the dropped fault; it needs manual resolution.
            self.faults[MAX_BACKGROUND_TASKS - 1] = BackgroundWaitFault {
                reason: "await_tracking_overflow".into(),
                key: "overflow".into(),
                reported: false,
            };
            return;
        }
        self.faults.push(BackgroundWaitFault {
            reason: reason.into(),
            key: key.into(),
            reported: false,
        });
    }

    pub fn clear_task_faults(&mut self, key: &str) {
        self.faults
            .retain(|f| f.key != key || f.reason == "await_tracking_overflow");
    }

    pub fn validate(&self, run_seq: u64) -> Result<(), ModelError> {
        if self.tasks.len() > MAX_BACKGROUND_TASKS || self.faults.len() > MAX_BACKGROUND_TASKS {
            return Err(ModelError("background wait limit exceeded".into()));
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut tool_ids = std::collections::BTreeSet::new();
        for task in &self.tasks {
            if !valid_background_id(&task.task_id)
                || !valid_background_id(&task.tool_use_id)
                || !ids.insert(&task.task_id)
                || !tool_ids.insert(&task.tool_use_id)
                || task.owner_run_seq != run_seq
                || task.command.is_empty()
                || task.command.len() > BODY_MAX_BYTES
                || task.registered_at < 0
                || task.last_checked_at.is_some_and(|at| at < 0)
                || task
                    .notification_status
                    .as_ref()
                    .is_some_and(|s| !valid_background_id(s))
                || matches!(
                    (task.receipt, task.notification_status.is_some()),
                    (BackgroundReceipt::Pending, true) | (BackgroundReceipt::Delivered, false)
                )
            {
                return Err(ModelError("invalid background wait task".into()));
            }
        }
        for fault in &self.faults {
            if !valid_background_id(&fault.reason) || !valid_background_id(&fault.key) {
                return Err(ModelError("invalid background wait fault".into()));
            }
        }
        if self.paused != self.paused_at.is_some() || self.paused_at.is_some_and(|at| at < 0) {
            return Err(ModelError("invalid background wait pause".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClaudeToolResult {
    Registered {
        task_id: String,
        tool_use_id: String,
        command: String,
    },
    LaunchUnconfirmed {
        tool_use_id: String,
    },
    TaskStopped {
        task_id: String,
        command: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeTaskNotification {
    pub task_id: String,
    pub tool_use_id: String,
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeRegistryEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub task_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClaudeCron {
    pub id: String,
    pub schedule: String,
    pub recurring: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaudeCronSnapshot {
    pub entries: Vec<ClaudeCron>,
    pub observed_at: Option<i64>,
}

impl ClaudeCronSnapshot {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.entries.len() > MAX_BACKGROUND_TASKS
            || self.observed_at.is_some_and(|at| at < 0)
            || (!self.entries.is_empty() && self.observed_at.is_none())
            || self.entries.iter().any(|e| {
                !valid_background_id(&e.id)
                    || e.schedule.is_empty()
                    || e.schedule.len() > IDENTIFIER_MAX_BYTES
                    || e.schedule.chars().any(char::is_control)
            })
        {
            return Err(ModelError("invalid cron snapshot".into()));
        }
        Ok(())
    }
}
