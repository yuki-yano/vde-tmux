use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::config::DoneClearOn;
use crate::daemon::session_badge::BadgeState;

pub const PANE_STATE_SCHEMA_VERSION: u16 = 1;
pub const IDENTIFIER_MAX_BYTES: usize = 256;
pub const BODY_MAX_BYTES: usize = 4096;
pub const PATH_MAX_BYTES: usize = 8192;
pub const MAX_TASK_ITEMS: usize = 256;
pub const MAX_SUBAGENTS: usize = 256;
pub const MAX_VIEW_PANES: usize = 512;
pub const MAX_VIEW_WITNESSES: usize = 64;
pub const MAX_STORED_RECORD_BYTES: usize = 256 * 1024;
pub const MAX_REQUEST_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelError(pub String);

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModelError {}

fn validate_random_id(value: &str, name: &str) -> Result<(), ModelError> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ModelError(format!(
            "{name} must be exactly 32 lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

fn random_hex_128() -> Result<String, ModelError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| ModelError(format!("failed to obtain OS randomness: {error}")))?;
    let mut value = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(value)
}

macro_rules! random_id_type {
    ($name:ident, $label:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn generate() -> Result<Self, ModelError> {
                Ok(Self(random_hex_128()?))
            }

            pub fn parse(value: impl Into<String>) -> Result<Self, ModelError> {
                let value = value.into();
                validate_random_id(&value, $label)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(de::Error::custom)
            }
        }
    };
}

random_id_type!(StateId, "state ID");
random_id_type!(ResetTombstoneId, "reset tombstone ID");
random_id_type!(EventId, "event ID");
random_id_type!(DaemonInstanceId, "daemon instance ID");

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentKind(String);

impl AgentKind {
    pub fn parse(value: impl AsRef<str>) -> Result<Self, ModelError> {
        let value = value.as_ref().trim().to_ascii_lowercase();
        let valid = !value.is_empty()
            && value.len() <= 64
            && value.bytes().enumerate().all(|(index, byte)| match byte {
                b'a'..=b'z' | b'0'..=b'9' => true,
                b'.' | b'_' | b'-' => index > 0,
                _ => false,
            });
        if !valid {
            return Err(ModelError(
                "agent kind must match [a-z0-9][a-z0-9._-]{0,63}".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn deserialize_strict(value: String) -> Result<Self, ModelError> {
        let parsed = Self::parse(&value)?;
        if parsed.as_str() != value {
            return Err(ModelError(
                "agent kind must already be normalized at deserialize boundary".to_string(),
            ));
        }
        Ok(parsed)
    }
}

impl Serialize for AgentKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AgentKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::deserialize_strict(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentSessionId(String);

impl AgentSessionId {
    pub fn parse(value: impl Into<String>) -> Result<Self, ModelError> {
        let value = normalize_text(&value.into());
        validate_required_text(&value, "agent session ID", IDENTIFIER_MAX_BYTES)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn deserialize_strict(value: String) -> Result<Self, ModelError> {
        validate_required_text(&value, "agent session ID", IDENTIFIER_MAX_BYTES)?;
        if normalize_text(&value) != value {
            return Err(ModelError(
                "agent session ID must already be normalized at deserialize boundary".to_string(),
            ));
        }
        Ok(Self(value))
    }
}

impl Serialize for AgentSessionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AgentSessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::deserialize_strict(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

pub fn normalize_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character == '\0' || character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn validate_required_text(
    value: &str,
    field: &str,
    max_bytes: usize,
) -> Result<(), ModelError> {
    if value.trim().is_empty() {
        return Err(ModelError(format!("{field} must not be empty")));
    }
    validate_optional_text(value, field, max_bytes)
}

pub fn validate_optional_text(
    value: &str,
    field: &str,
    max_bytes: usize,
) -> Result<(), ModelError> {
    if value.len() > max_bytes {
        return Err(ModelError(format!(
            "{field} exceeds the {max_bytes}-byte limit"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(ModelError(format!("{field} contains a control character")));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneInstance {
    pub pane_id: String,
    pub pane_pid: u32,
}

impl PaneInstance {
    pub fn validate(&self) -> Result<(), ModelError> {
        let valid_pane_id = self.pane_id.strip_prefix('%').is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        });
        if !valid_pane_id || self.pane_pid == 0 {
            return Err(ModelError("invalid pane instance".to_string()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateVersion {
    pub state_id: StateId,
    pub agent_epoch: u64,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitReason {
    PermissionPrompt,
    Other(String),
}

impl WaitReason {
    pub fn validate(&self) -> Result<(), ModelError> {
        if let Self::Other(reason) = self {
            validate_required_text(reason, "wait reason", IDENTIFIER_MAX_BYTES)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleState {
    Idle,
    Running,
    Waiting { reason: WaitReason },
    Error { reason: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptState {
    pub text: String,
    pub source: String,
}

impl PromptState {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_required_text(&self.text, "prompt", BODY_MAX_BYTES)?;
        validate_required_text(&self.source, "prompt source", IDENTIFIER_MAX_BYTES)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskProgress {
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskItemStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskItemState {
    pub id: Option<String>,
    pub step: String,
    pub status: TaskItemStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskState {
    pub progress: TaskProgress,
    pub items: Vec<TaskItemState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentState {
    pub agent_id: String,
    pub agent_type: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WorktreeActivityKind {
    VwExec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeActivity {
    pub kind: WorktreeActivityKind,
    pub name: String,
    pub path: String,
    pub command: String,
    pub observed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneState {
    pub schema_version: u16,
    pub state_id: StateId,
    pub revision: u64,
    pub pane_instance: PaneInstance,
    pub agent: AgentKind,
    pub agent_session_id: Option<AgentSessionId>,
    pub agent_epoch: u64,
    pub agent_present: bool,
    pub scan_verified: bool,
    pub synthetic_completion_armed: bool,
    pub lifecycle: LifecycleState,
    pub run_seq: u64,
    pub completed_seq: u64,
    pub acknowledged_seq: u64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub prompt: Option<PromptState>,
    pub tasks: TaskState,
    pub subagents: Vec<SubagentState>,
    pub worktree_activity: Option<WorktreeActivity>,
}

impl PaneState {
    pub fn version(&self) -> StateVersion {
        StateVersion {
            state_id: self.state_id.clone(),
            agent_epoch: self.agent_epoch,
            revision: self.revision,
        }
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        if self.schema_version != PANE_STATE_SCHEMA_VERSION {
            return Err(ModelError(format!(
                "unsupported pane state schema version {}",
                self.schema_version
            )));
        }
        self.pane_instance.validate()?;
        if self.revision == 0 || self.agent_epoch == 0 {
            return Err(ModelError(
                "revision and agent epoch must be positive".to_string(),
            ));
        }
        if self.acknowledged_seq > self.completed_seq || self.completed_seq > self.run_seq {
            return Err(ModelError(
                "pane state sequence order is invalid".to_string(),
            ));
        }
        if self
            .run_seq
            .checked_sub(self.completed_seq)
            .is_none_or(|open| open > 1)
        {
            return Err(ModelError(
                "pane state has more than one open run".to_string(),
            ));
        }
        let idle = matches!(self.lifecycle, LifecycleState::Idle);
        if idle != (self.run_seq == self.completed_seq) {
            return Err(ModelError(
                "lifecycle and run sequence disagree".to_string(),
            ));
        }
        if !self.agent_present && (!idle || !self.scan_verified) {
            return Err(ModelError(
                "absent agent must be scan-verified and idle".to_string(),
            ));
        }
        if self.synthetic_completion_armed
            && (!idle
                || self.run_seq != 0
                || !self.scan_verified
                || self.agent_session_id.is_some())
        {
            return Err(ModelError("invalid synthetic completion state".to_string()));
        }
        if self.run_seq == 0
            && (self.completed_seq != 0 || self.acknowledged_seq != 0 || self.started_at.is_some())
        {
            return Err(ModelError("zero run sequence has run metadata".to_string()));
        }
        if (self.completed_seq == 0) != self.completed_at.is_none() {
            return Err(ModelError(
                "completed sequence and timestamp disagree".to_string(),
            ));
        }
        if self.run_seq > 0 && self.started_at.is_none() {
            return Err(ModelError(
                "run sequence requires a start timestamp".to_string(),
            ));
        }
        if let LifecycleState::Waiting { reason } = &self.lifecycle {
            reason.validate()?;
        }
        if let LifecycleState::Error {
            reason: Some(reason),
        } = &self.lifecycle
        {
            validate_optional_text(reason, "error reason", IDENTIFIER_MAX_BYTES)?;
        }
        if let Some(prompt) = &self.prompt {
            prompt.validate()?;
        }
        validate_tasks(&self.tasks)?;
        validate_subagents(&self.subagents)?;
        if let Some(activity) = &self.worktree_activity {
            validate_required_text(&activity.name, "worktree name", BODY_MAX_BYTES)?;
            validate_required_text(&activity.path, "worktree path", PATH_MAX_BYTES)?;
            validate_required_text(&activity.command, "worktree command", BODY_MAX_BYTES)?;
        }
        Ok(())
    }
}

pub fn validate_tasks(tasks: &TaskState) -> Result<(), ModelError> {
    if tasks.progress.done > tasks.progress.total {
        return Err(ModelError("task progress exceeds total".to_string()));
    }
    if tasks.items.len() > MAX_TASK_ITEMS {
        return Err(ModelError("too many task items".to_string()));
    }
    let mut ids = BTreeSet::new();
    for item in &tasks.items {
        validate_required_text(&item.step, "task step", BODY_MAX_BYTES)?;
        if let Some(id) = &item.id {
            validate_required_text(id, "task item ID", IDENTIFIER_MAX_BYTES)?;
            if !ids.insert(id) {
                return Err(ModelError("duplicate task item ID".to_string()));
            }
        }
    }
    if !tasks.items.is_empty() {
        let done = tasks
            .items
            .iter()
            .filter(|item| item.status == TaskItemStatus::Completed)
            .count() as u64;
        if tasks.progress.done != done || tasks.progress.total != tasks.items.len() as u64 {
            return Err(ModelError(
                "task progress does not match task items".to_string(),
            ));
        }
    }
    Ok(())
}

pub fn validate_subagents(subagents: &[SubagentState]) -> Result<(), ModelError> {
    if subagents.len() > MAX_SUBAGENTS {
        return Err(ModelError("too many subagents".to_string()));
    }
    let mut ids = BTreeSet::new();
    for subagent in subagents {
        validate_required_text(&subagent.agent_id, "subagent ID", IDENTIFIER_MAX_BYTES)?;
        validate_required_text(&subagent.agent_type, "subagent type", IDENTIFIER_MAX_BYTES)?;
        if let Some(name) = &subagent.display_name {
            validate_required_text(name, "subagent name", IDENTIFIER_MAX_BYTES)?;
        }
        if !ids.insert(&subagent.agent_id) {
            return Err(ModelError("duplicate subagent ID".to_string()));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetTombstone {
    pub schema_version: u16,
    pub tombstone_id: ResetTombstoneId,
    pub pane_instance: PaneInstance,
    pub reset_at: i64,
}

impl ResetTombstone {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.schema_version != PANE_STATE_SCHEMA_VERSION || self.reset_at < 0 {
            return Err(ModelError("invalid reset tombstone".to_string()));
        }
        self.pane_instance.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "record_type",
    content = "record",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[allow(clippy::large_enum_variant)]
pub enum StoredPaneRecord {
    Active(PaneState),
    Reset(ResetTombstone),
}

impl StoredPaneRecord {
    pub fn descriptor(&self) -> StoredStateDescriptor {
        match self {
            Self::Active(state) => StoredStateDescriptor::Canonical {
                version: state.version(),
            },
            Self::Reset(tombstone) => StoredStateDescriptor::Reset {
                tombstone_id: tombstone.tombstone_id.clone(),
            },
        }
    }

    pub fn pane_instance(&self) -> &PaneInstance {
        match self {
            Self::Active(state) => &state.pane_instance,
            Self::Reset(tombstone) => &tombstone.pane_instance,
        }
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        match self {
            Self::Active(state) => state.validate(),
            Self::Reset(tombstone) => tombstone.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StoredStateDescriptor {
    Canonical { version: StateVersion },
    Quarantined { quarantine_id: String },
    Reset { tombstone_id: ResetTombstoneId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneStateLoadError {
    pub pane_instance: PaneInstance,
    pub quarantine_id: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedPaneState {
    pub canonical: PaneState,
    pub window_id: String,
    pub pane_id: String,
    pub current_path: String,
    pub badge: BadgeState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentSessionSource {
    Startup,
    Resume,
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentPresenceObservation {
    Present(AgentKind),
    Absent,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureObservation {
    pub inference: CaptureInference,
    pub observed_fingerprint: Option<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureInference {
    PermissionWait { reason: WaitReason },
    ActivityObserved,
    StaleRunCompleted,
    NoChange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgressOperation {
    SetPrompt(PromptState),
    ClearPrompt,
    TaskCreated,
    TaskCompleted,
    ReplaceTasks {
        progress: TaskProgress,
        items: Vec<TaskItemState>,
    },
    UpsertTaskItem {
        id: String,
        step: String,
    },
    UpdateTaskItemStatus {
        id: String,
        status: TaskItemStatus,
    },
    ClearTasks,
    UpsertSubagent(SubagentState),
    RemoveSubagent {
        agent_id: String,
    },
    ReplaceSubagents(Vec<SubagentState>),
    ClearSubagents,
    SetWorktreeActivity(WorktreeActivity),
    ClearWorktreeActivity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ReportedLifecycle {
    Running,
    Waiting { reason: WaitReason },
    Idle,
    Error { reason: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum FieldUpdate<T> {
    Set(T),
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplicitStateReport {
    pub observed_at: i64,
    pub lifecycle: Option<ReportedLifecycle>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub prompt: Option<FieldUpdate<PromptState>>,
    pub tasks: Option<FieldUpdate<TaskProgress>>,
    pub subagents: Option<FieldUpdate<Vec<SubagentState>>>,
    pub attention: bool,
}

impl ExplicitStateReport {
    pub fn is_semantically_empty(&self) -> bool {
        self.lifecycle.is_none()
            && self.started_at.is_none()
            && self.completed_at.is_none()
            && self.prompt.is_none()
            && self.tasks.is_none()
            && self.subagents.is_none()
            && !self.attention
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PaneEvent {
    AgentSessionStarted {
        observed_at: i64,
        source: AgentSessionSource,
        resumed_prompt: Option<PromptState>,
    },
    BeginRun {
        started_at: i64,
        prompt: Option<PromptState>,
    },
    ActivityObserved {
        observed_at: i64,
    },
    WaitRequested {
        observed_at: i64,
        reason: WaitReason,
    },
    CompleteRun {
        completed_at: i64,
    },
    FailRun {
        observed_at: i64,
        reason: Option<String>,
    },
    AcknowledgeView {
        expected_state_id: StateId,
        expected_agent_epoch: u64,
        through_seq: u64,
    },
    MarkDone {
        expected: StateVersion,
        completed_at: i64,
    },
    ProgressUpdated {
        observed_at: i64,
        operations: Vec<ProgressOperation>,
    },
    ExplicitStateReported {
        report: ExplicitStateReport,
    },
    ObservationBatch {
        base: Option<StoredStateDescriptor>,
        tracker_generation: u64,
        observed_at: i64,
        presence: AgentPresenceObservation,
        capture: Option<CaptureObservation>,
    },
    PaneRemoved {
        expected: Option<StoredStateDescriptor>,
    },
}

impl PaneEvent {
    pub fn is_external(&self) -> bool {
        matches!(
            self,
            Self::AgentSessionStarted { .. }
                | Self::BeginRun { .. }
                | Self::ActivityObserved { .. }
                | Self::WaitRequested { .. }
                | Self::CompleteRun { .. }
                | Self::FailRun { .. }
                | Self::ProgressUpdated { .. }
                | Self::ExplicitStateReported { .. }
        )
    }

    pub fn is_semantically_empty(&self) -> bool {
        matches!(self, Self::ProgressUpdated { operations, .. } if operations.is_empty())
            || matches!(self, Self::ExplicitStateReported { report } if report.is_semantically_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneEventEnvelope {
    pub daemon_instance_id: DaemonInstanceId,
    pub event_id: EventId,
    pub pane_instance: PaneInstance,
    pub agent: Option<AgentKind>,
    pub agent_session_id: Option<AgentSessionId>,
    pub event: PaneEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ViewHookKind {
    WindowPaneChanged,
    SessionWindowChanged,
    ClientSessionChanged,
    ClientAttached,
    ClientDetached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewOccurrence {
    pub session_id: String,
    pub window_id: String,
    pub active_pane: PaneInstance,
    pub observed_panes: Vec<PaneInstance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceClientHint {
    pub client_pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientWitness {
    pub client_pid: u32,
    pub session_id: String,
    pub window_id: String,
    pub active_pane: PaneInstance,
    pub control_mode: bool,
    pub active_pane_flag: bool,
}

impl ClientWitness {
    pub fn is_eligible(&self) -> bool {
        !self.control_mode && !self.active_pane_flag
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewEvent {
    pub daemon_instance_id: DaemonInstanceId,
    pub event_id: EventId,
    pub hook_kind: ViewHookKind,
    pub occurrence: Option<ViewOccurrence>,
    pub source_client: Option<SourceClientHint>,
    pub witnesses: Vec<ClientWitness>,
}

impl ViewEvent {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.hook_kind == ViewHookKind::ClientDetached && self.occurrence.is_some() {
            return Err(ModelError(
                "client-detached event cannot contain a view occurrence".to_string(),
            ));
        }
        if matches!(
            self.hook_kind,
            ViewHookKind::ClientSessionChanged
                | ViewHookKind::ClientAttached
                | ViewHookKind::ClientDetached
        ) && self.source_client.is_none()
        {
            return Err(ModelError(
                "client view hook requires source_client".to_string(),
            ));
        }
        if self.witnesses.len() > MAX_VIEW_WITNESSES {
            return Err(ModelError("too many client witnesses".to_string()));
        }
        let mut witness_pids = BTreeSet::new();
        for witness in &self.witnesses {
            if witness.client_pid == 0 || !witness_pids.insert(witness.client_pid) {
                return Err(ModelError("duplicate client witness".to_string()));
            }
            validate_tmux_entity_id(&witness.session_id, '$', "witness session")?;
            validate_tmux_entity_id(&witness.window_id, '@', "witness window")?;
            witness.active_pane.validate()?;
        }
        if self
            .source_client
            .as_ref()
            .is_some_and(|source| source.client_pid == 0)
        {
            return Err(ModelError("invalid source client PID".to_string()));
        }
        if let Some(occurrence) = &self.occurrence {
            validate_tmux_entity_id(&occurrence.session_id, '$', "occurrence session")?;
            validate_tmux_entity_id(&occurrence.window_id, '@', "occurrence window")?;
            if occurrence.observed_panes.len() > MAX_VIEW_PANES {
                return Err(ModelError("too many panes in view occurrence".to_string()));
            }
            for pane in &occurrence.observed_panes {
                pane.validate()?;
            }
            let panes = occurrence.observed_panes.iter().collect::<BTreeSet<_>>();
            if panes.len() != occurrence.observed_panes.len()
                || !panes.contains(&occurrence.active_pane)
            {
                return Err(ModelError("invalid panes in view occurrence".to_string()));
            }
            if self.witnesses.iter().any(|witness| {
                witness.window_id == occurrence.window_id && !panes.contains(&witness.active_pane)
            }) {
                return Err(ModelError(
                    "client witness active pane is not in the declared window".to_string(),
                ));
            }
        }
        Ok(())
    }
}

fn validate_tmux_entity_id(value: &str, prefix: char, field: &str) -> Result<(), ModelError> {
    if value.strip_prefix(prefix).is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        Ok(())
    } else {
        Err(ModelError(format!("invalid {field} ID")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VisibilitySnapshot {
    pub pane_visible_to_eligible_client: bool,
    pub window_visible_to_eligible_client: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaptureTrackerSnapshot {
    pub generation: u64,
    pub epoch: Option<(StateId, u64)>,
    pub absence_count: u8,
    pub replacement_kind: Option<AgentKind>,
    pub replacement_streak: u8,
    pub fingerprint: Option<[u8; 32]>,
    pub last_change_at: Option<i64>,
    pub rebaseline_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureTrackerDelta {
    pub next: CaptureTrackerSnapshot,
}

#[derive(Debug, Clone)]
pub struct ReductionContext<'a> {
    pub done_clear_on: DoneClearOn,
    pub visibility: &'a VisibilitySnapshot,
    pub tracker: &'a CaptureTrackerSnapshot,
    pub new_state_id: Option<StateId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_state() -> PaneState {
        PaneState {
            schema_version: PANE_STATE_SCHEMA_VERSION,
            state_id: StateId::parse("00112233445566778899aabbccddeeff").unwrap(),
            revision: 1,
            pane_instance: PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 42,
            },
            agent: AgentKind::parse("codex").unwrap(),
            agent_session_id: Some(AgentSessionId::parse("session-1").unwrap()),
            agent_epoch: 1,
            agent_present: true,
            scan_verified: false,
            synthetic_completion_armed: false,
            lifecycle: LifecycleState::Idle,
            run_seq: 0,
            completed_seq: 0,
            acknowledged_seq: 0,
            started_at: None,
            completed_at: None,
            prompt: None,
            tasks: TaskState::default(),
            subagents: Vec::new(),
            worktree_activity: None,
        }
    }

    #[test]
    fn random_identifier_serialization_is_transparent_and_strict() {
        let id = StateId::parse("00112233445566778899aabbccddeeff").unwrap();
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            r#""00112233445566778899aabbccddeeff""#
        );
        assert!(serde_json::from_str::<StateId>(r#""ABC""#).is_err());
        assert!(serde_json::from_str::<StateId>(r#""00112233445566778899AABBCCDDEEFF""#).is_err());
        assert_eq!(StateId::generate().unwrap().as_str().len(), 32);
    }

    #[test]
    fn agent_kind_is_normalized_and_validated() {
        assert_eq!(AgentKind::parse(" CoDeX ").unwrap().as_str(), "codex");
        assert!(AgentKind::parse("-codex").is_err());
        assert!(AgentKind::parse("codex/unsafe").is_err());
        assert!(AgentKind::parse("").is_err());
        assert!(serde_json::from_str::<AgentKind>(r#"" CoDeX ""#).is_err());
        assert!(serde_json::from_str::<AgentSessionId>(r#"" session ""#).is_err());
    }

    #[test]
    fn unknown_storage_fields_are_rejected() {
        let record = StoredPaneRecord::Active(valid_state());
        let mut value = serde_json::to_value(record).unwrap();
        value["record"]["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<StoredPaneRecord>(value).is_err());

        let mut value = serde_json::to_value(StoredPaneRecord::Active(valid_state())).unwrap();
        value["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<StoredPaneRecord>(value).is_err());

        let descriptor = StoredStateDescriptor::Canonical {
            version: valid_state().version(),
        };
        let mut value = serde_json::to_value(descriptor).unwrap();
        value["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<StoredStateDescriptor>(value).is_err());

        let event = r#"{"type":"begin_run","data":{"started_at":1,"prompt":null,"extra":true}}"#;
        assert!(serde_json::from_str::<PaneEvent>(event).is_err());
    }

    #[test]
    fn invariant_validation_rejects_multiple_open_runs() {
        let mut state = valid_state();
        state.run_seq = 2;
        state.completed_seq = 0;
        state.started_at = Some(1);
        state.lifecycle = LifecycleState::Running;
        assert!(state.validate().is_err());
    }

    #[test]
    fn task_and_subagent_identifiers_must_be_unique() {
        let mut state = valid_state();
        state.tasks = TaskState {
            progress: TaskProgress { done: 0, total: 2 },
            items: vec![
                TaskItemState {
                    id: Some("1".to_string()),
                    step: "one".to_string(),
                    status: TaskItemStatus::Pending,
                },
                TaskItemState {
                    id: Some("1".to_string()),
                    step: "two".to_string(),
                    status: TaskItemStatus::Pending,
                },
            ],
        };
        assert!(state.validate().is_err());

        state.tasks = TaskState::default();
        let subagent = SubagentState {
            agent_id: "same".to_string(),
            agent_type: "worker".to_string(),
            display_name: None,
        };
        state.subagents = vec![subagent.clone(), subagent];
        assert!(state.validate().is_err());
    }

    #[test]
    fn view_occurrence_rejects_duplicates_and_ineligible_flags_are_explicit() {
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 42,
        };
        let event = ViewEvent {
            daemon_instance_id: DaemonInstanceId::parse("00112233445566778899aabbccddeeff")
                .unwrap(),
            event_id: EventId::parse("ffeeddccbbaa99887766554433221100").unwrap(),
            hook_kind: ViewHookKind::WindowPaneChanged,
            occurrence: Some(ViewOccurrence {
                session_id: "$1".to_string(),
                window_id: "@1".to_string(),
                active_pane: pane.clone(),
                observed_panes: vec![pane.clone(), pane.clone()],
            }),
            source_client: None,
            witnesses: vec![ClientWitness {
                client_pid: 10,
                session_id: "$1".to_string(),
                window_id: "@1".to_string(),
                active_pane: pane,
                control_mode: true,
                active_pane_flag: false,
            }],
        };
        assert!(!event.witnesses[0].is_eligible());
        assert!(event.validate().is_err());
    }
}
