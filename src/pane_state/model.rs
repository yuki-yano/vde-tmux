use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::daemon::session_badge::BadgeState;

pub const PANE_STATE_SCHEMA_VERSION: u16 = 10;
pub const IDENTIFIER_MAX_BYTES: usize = 256;
pub const BODY_MAX_BYTES: usize = 4096;
pub const PATH_MAX_BYTES: usize = 8192;
pub const MAX_TASK_ITEMS: usize = 256;
pub const MAX_SUBAGENTS: usize = 256;
pub const MAX_VIEW_PANES: usize = 512;
pub const MAX_VIEW_WITNESSES: usize = 64;
pub const MAX_TASK_CONTEXT_PROMPTS: usize = 4;
pub const TASK_CONTEXT_PROMPT_MAX_BYTES: usize = 1_200;
pub const TASK_SUMMARY_MAX_BYTES: usize = 256;
pub const RESPONSE_PREVIEW_MAX_BYTES: usize = 1_200;
pub const USAGE_LIMIT_WAIT_REASON: &str = "usage_limit";
pub const MAX_LISTENING_PORTS: usize = 64;
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
    pub fn usage_limit() -> Self {
        Self::Other(USAGE_LIMIT_WAIT_REASON.to_string())
    }

    pub fn is_usage_limit(&self) -> bool {
        matches!(self, Self::Other(reason) if reason == USAGE_LIMIT_WAIT_REASON)
    }

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

impl LifecycleState {
    pub fn is_usage_limited(&self) -> bool {
        matches!(self, Self::Waiting { reason } if reason.is_usage_limit())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum UnreadReason {
    Waiting,
    Error,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnreadOccurrence {
    pub seq: u64,
    pub order: u64,
    pub reason: UnreadReason,
    pub occurred_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnreadState {
    pub occurrence_seq: u64,
    pub read_seq: u64,
    pub latest: Option<UnreadOccurrence>,
}

impl UnreadState {
    pub fn is_unread(&self) -> bool {
        self.occurrence_seq > self.read_seq
    }

    pub fn latest_unread(&self) -> Option<&UnreadOccurrence> {
        self.is_unread().then_some(self.latest.as_ref()).flatten()
    }

    pub fn is_jump_eligible(&self, lifecycle: &LifecycleState) -> bool {
        self.latest_unread().is_some_and(|occurrence| {
            matches!(occurrence.reason, UnreadReason::Completed)
                || matches!(
                    (&occurrence.reason, lifecycle),
                    (UnreadReason::Waiting, LifecycleState::Waiting { .. })
                        | (UnreadReason::Error, LifecycleState::Error { .. })
                )
        })
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        if self.read_seq > self.occurrence_seq {
            return Err(ModelError(
                "unread read sequence exceeds occurrence sequence".to_string(),
            ));
        }
        match (&self.latest, self.occurrence_seq) {
            (None, 0) => Ok(()),
            (Some(latest), sequence)
                if sequence > 0 && latest.seq == sequence && latest.order > 0 =>
            {
                Ok(())
            }
            _ => Err(ModelError(
                "unread latest occurrence disagrees with occurrence sequence".to_string(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptState {
    pub text: String,
    pub source: String,
    pub digest: Option<String>,
}

impl PromptState {
    pub fn digest_decoded_prompt(prompt: &str) -> String {
        use sha2::{Digest, Sha256};

        let mut digest = Sha256::new();
        digest.update(b"vde-tmux:prompt:v1\0");
        digest.update(prompt.as_bytes());
        format!("{:x}", digest.finalize())
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        validate_required_text(&self.text, "prompt", BODY_MAX_BYTES)?;
        validate_required_text(&self.source, "prompt source", IDENTIFIER_MAX_BYTES)?;
        if self.digest.as_ref().is_some_and(|digest| {
            digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        }) {
            return Err(ModelError("prompt digest is invalid".to_string()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseState {
    pub text: String,
    pub observed_at: i64,
}

impl ResponseState {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_required_text(&self.text, "response preview", RESPONSE_PREVIEW_MAX_BYTES)?;
        if self.text.contains(['\r', '\n']) {
            return Err(ModelError("response preview must be one line".to_string()));
        }
        if self.observed_at < 0 {
            return Err(ModelError(
                "response preview timestamp must be non-negative".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundProcessState {
    pub command: String,
    pub observed_at: i64,
}

impl BackgroundProcessState {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_required_text(&self.command, "background process command", BODY_MAX_BYTES)?;
        if self.command.contains(['\r', '\n']) {
            return Err(ModelError(
                "background process command must be one line".to_string(),
            ));
        }
        if self.observed_at < 0 {
            return Err(ModelError(
                "background process timestamp must be non-negative".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProcessIdentity {
    pub pid: u32,
    pub start_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentDurableRunProjection {
    pub run_id: String,
    pub run_seq: u64,
    pub run_revision: u64,
}

impl CurrentDurableRunProjection {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_random_id(&self.run_id, "durable run ID")?;
        if self.run_seq == 0 || self.run_revision == 0 {
            return Err(ModelError(
                "durable run sequence and revision must be positive".to_string(),
            ));
        }
        Ok(())
    }
}

impl AgentProcessIdentity {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.pid == 0
            || self.start_token.trim().is_empty()
            || self.start_token.len() > IDENTIFIER_MAX_BYTES
            || self.start_token.contains(['\r', '\n'])
        {
            return Err(ModelError("invalid agent process identity".to_string()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessObservation {
    pub agent_process_checked: bool,
    pub agent_process: Option<AgentProcessIdentity>,
    pub background_process_alive: Option<bool>,
    pub listening_ports: Option<Vec<u16>>,
}

impl ProcessObservation {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.agent_process.is_some() && !self.agent_process_checked {
            return Err(ModelError(
                "agent process identity requires a completed check".to_string(),
            ));
        }
        if let Some(identity) = &self.agent_process {
            identity.validate()?;
        }
        if let Some(ports) = &self.listening_ports {
            if ports.len() > MAX_LISTENING_PORTS {
                return Err(ModelError("too many listening ports".to_string()));
            }
            if ports.windows(2).any(|pair| pair[0] >= pair[1]) {
                return Err(ModelError(
                    "listening ports must be sorted and unique".to_string(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSummaryState {
    pub text: Option<String>,
    pub context_fingerprint: String,
    pub generated_at: i64,
    pub outcome: TaskSummaryOutcome,
    pub failure_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSummaryOutcome {
    Generated,
    Failed,
}

impl TaskSummaryState {
    pub fn validate(&self) -> Result<(), ModelError> {
        if let Some(text) = &self.text {
            validate_required_text(text, "task summary", TASK_SUMMARY_MAX_BYTES)?;
            if text.contains(['\r', '\n']) {
                return Err(ModelError("task summary must be one line".to_string()));
            }
        }
        if self.context_fingerprint.len() != 64
            || !self
                .context_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ModelError(
                "task summary context fingerprint is invalid".to_string(),
            ));
        }
        if self.generated_at < 0 {
            return Err(ModelError(
                "task summary timestamp must be non-negative".to_string(),
            ));
        }
        match self.outcome {
            TaskSummaryOutcome::Generated if self.failure_code.is_none() => {}
            TaskSummaryOutcome::Failed
                if self.text.is_none()
                    && self.failure_code.as_ref().is_some_and(|code| {
                        !code.is_empty()
                            && code.len() <= IDENTIFIER_MAX_BYTES
                            && code
                                .bytes()
                                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
                    }) => {}
            _ => {
                return Err(ModelError(
                    "task summary outcome metadata is inconsistent".to_string(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskContextState {
    pub recent_prompts: Vec<String>,
    pub reference_response: Option<String>,
    pub summary: Option<TaskSummaryState>,
}

impl TaskContextState {
    pub fn observe_prompt(&mut self, prompt: &str) {
        let prompt = truncate_utf8(prompt.trim(), TASK_CONTEXT_PROMPT_MAX_BYTES);
        if prompt.is_empty() {
            return;
        }
        if self.recent_prompts.last() == Some(&prompt) {
            return;
        }
        self.recent_prompts.push(prompt);
        if self.recent_prompts.len() > MAX_TASK_CONTEXT_PROMPTS {
            self.recent_prompts.remove(0);
        }
    }

    pub fn observe_prompt_with_reference(&mut self, prompt: &str, response: Option<&str>) {
        self.reference_response = response
            .map(str::trim)
            .filter(|response| !response.is_empty())
            .map(|response| truncate_utf8(response, RESPONSE_PREVIEW_MAX_BYTES));
        self.observe_prompt(prompt);
    }

    pub fn context_fingerprint(&self) -> Option<String> {
        use sha2::{Digest, Sha256};

        if self.recent_prompts.is_empty() {
            return None;
        }
        let mut digest = Sha256::new();
        for prompt in &self.recent_prompts {
            digest.update(b"\0recent\0");
            digest.update(prompt.as_bytes());
        }
        if let Some(response) = &self.reference_response {
            digest.update(b"\0reference-response\0");
            digest.update(response.as_bytes());
        }
        Some(format!("{:x}", digest.finalize()))
    }

    pub fn current_summary(&self) -> Option<&TaskSummaryState> {
        let fingerprint = self.context_fingerprint()?;
        self.summary
            .as_ref()
            .filter(|summary| summary.context_fingerprint == fingerprint)
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        if self.recent_prompts.len() > MAX_TASK_CONTEXT_PROMPTS {
            return Err(ModelError("too many task context prompts".to_string()));
        }
        for prompt in &self.recent_prompts {
            validate_required_text(prompt, "task context prompt", TASK_CONTEXT_PROMPT_MAX_BYTES)?;
        }
        if let Some(response) = &self.reference_response {
            validate_required_text(
                response,
                "task context reference response",
                RESPONSE_PREVIEW_MAX_BYTES,
            )?;
        }
        if let Some(summary) = &self.summary {
            summary.validate()?;
        }
        Ok(())
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].trim_end().to_string()
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
    pub agent_process: Option<AgentProcessIdentity>,
    pub agent_epoch: u64,
    pub agent_present: bool,
    pub scan_verified: bool,
    pub synthetic_completion_armed: bool,
    pub lifecycle: LifecycleState,
    pub run_seq: u64,
    pub current_run: Option<CurrentDurableRunProjection>,
    pub completed_seq: u64,
    pub unread: UnreadState,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub prompt: Option<PromptState>,
    pub latest_response: Option<ResponseState>,
    pub task_context: TaskContextState,
    pub tasks: TaskState,
    pub subagents: Vec<SubagentState>,
    pub worktree_activity: Option<WorktreeActivity>,
    pub background_process: Option<BackgroundProcessState>,
    pub listening_ports: Vec<u16>,
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
        if let Some(identity) = &self.agent_process {
            identity.validate()?;
        }
        if self.revision == 0 || self.agent_epoch == 0 {
            return Err(ModelError(
                "revision and agent epoch must be positive".to_string(),
            ));
        }
        if self.completed_seq > self.run_seq {
            return Err(ModelError(
                "pane state sequence order is invalid".to_string(),
            ));
        }
        if let Some(current_run) = &self.current_run {
            current_run.validate()?;
            if current_run.run_seq != self.run_seq {
                return Err(ModelError(
                    "durable current run sequence disagrees with pane state".to_string(),
                ));
            }
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
        if !self.agent_present
            && (!self.scan_verified || (!idle && !self.lifecycle.is_usage_limited()))
        {
            return Err(ModelError(
                "absent agent must be scan-verified and idle or usage-limited".to_string(),
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
        if self.run_seq == 0 && (self.completed_seq != 0 || self.started_at.is_some()) {
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
        if let Some(response) = &self.latest_response {
            response.validate()?;
        }
        self.task_context.validate()?;
        validate_tasks(&self.tasks)?;
        validate_subagents(&self.subagents)?;
        if let Some(activity) = &self.worktree_activity {
            validate_required_text(&activity.name, "worktree name", BODY_MAX_BYTES)?;
            validate_required_text(&activity.path, "worktree path", PATH_MAX_BYTES)?;
            validate_required_text(&activity.command, "worktree command", BODY_MAX_BYTES)?;
        }
        if let Some(process) = &self.background_process {
            process.validate()?;
        }
        ProcessObservation {
            agent_process_checked: false,
            agent_process: None,
            background_process_alive: None,
            listening_ports: Some(self.listening_ports.clone()),
        }
        .validate()?;
        self.unread.validate()?;
        Ok(())
    }

    pub fn reconcile_current_run(
        &mut self,
        projection: CurrentDurableRunProjection,
        execution_active: bool,
        observed_at: i64,
        latest_unread_order: u64,
    ) -> Result<bool, ModelError> {
        projection.validate()?;
        if observed_at < 0 {
            return Err(ModelError(
                "durable run projection timestamp must be non-negative".to_string(),
            ));
        }
        if let Some(existing) = &self.current_run {
            if existing.run_seq > projection.run_seq {
                return Ok(false);
            }
            if existing.run_seq == projection.run_seq && existing.run_id != projection.run_id {
                return Err(ModelError(
                    "durable current run ID conflicts at the same run sequence".to_string(),
                ));
            }
            if existing.run_id == projection.run_id
                && existing.run_revision > projection.run_revision
            {
                return Ok(false);
            }
        }

        let before = self.clone();
        let run_advanced = projection.run_seq > self.run_seq;
        let completion_advanced = !execution_active && self.completed_seq < projection.run_seq;
        if run_advanced {
            self.run_seq = projection.run_seq;
            self.started_at = Some(observed_at);
            self.completed_at =
                (projection.run_seq > 1).then_some(self.completed_at.unwrap_or(observed_at));
            self.prompt = None;
            self.latest_response = None;
            self.tasks = TaskState::default();
            self.subagents.clear();
            self.worktree_activity = None;
        }
        if execution_active {
            self.completed_seq = projection.run_seq.saturating_sub(1);
            self.completed_at =
                (self.completed_seq > 0).then_some(self.completed_at.unwrap_or(observed_at));
            if matches!(self.lifecycle, LifecycleState::Idle) {
                self.lifecycle = LifecycleState::Running;
            }
        } else {
            self.completed_seq = projection.run_seq;
            self.lifecycle = LifecycleState::Idle;
            self.completed_at = Some(self.completed_at.unwrap_or(observed_at));
            self.subagents.clear();
            self.worktree_activity = None;
        }
        if completion_advanced {
            let seq = self
                .unread
                .occurrence_seq
                .checked_add(1)
                .ok_or_else(|| ModelError("unread occurrence sequence overflow".to_string()))?;
            let previous_order = self
                .unread
                .latest
                .as_ref()
                .map(|latest| latest.order)
                .unwrap_or_default();
            let order = latest_unread_order
                .max(previous_order)
                .checked_add(1)
                .ok_or_else(|| ModelError("unread occurrence order overflow".to_string()))?;
            self.unread.occurrence_seq = seq;
            self.unread.latest = Some(UnreadOccurrence {
                seq,
                order,
                reason: UnreadReason::Completed,
                occurred_at: self.completed_at.unwrap_or(observed_at),
            });
        }
        self.current_run = Some(projection);
        if self != &before {
            self.revision = self
                .revision
                .checked_add(1)
                .ok_or_else(|| ModelError("pane revision overflow".to_string()))?;
            self.validate()?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn clear_current_run(&mut self) -> Result<bool, ModelError> {
        if self.current_run.take().is_none() {
            return Ok(false);
        }
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| ModelError("pane revision overflow".to_string()))?;
        self.validate()?;
        Ok(true)
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
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StoredStateDescriptor {
    Canonical { version: StateVersion },
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
    UsageLimit,
    ProviderError { reason: String },
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
    SetBackgroundProcess(BackgroundProcessState),
    ClearBackgroundProcess,
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
    ActivityAndProgressObserved {
        observed_at: i64,
        operations: Vec<ProgressOperation>,
    },
    WaitRequested {
        observed_at: i64,
        reason: WaitReason,
    },
    CompleteRun {
        completed_at: i64,
    },
    ResponseAndCompleteRun {
        completed_at: i64,
        response: ResponseState,
    },
    FailRun {
        observed_at: i64,
        reason: Option<String>,
    },
    MarkPaneRead {
        through_order: u64,
    },
    MarkDone {
        expected: StateVersion,
        completed_at: i64,
    },
    TaskSummaryGenerated {
        expected_state_id: StateId,
        expected_agent_epoch: u64,
        summary: TaskSummaryState,
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
        process: Option<ProcessObservation>,
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
                | Self::ActivityAndProgressObserved { .. }
                | Self::WaitRequested { .. }
                | Self::CompleteRun { .. }
                | Self::ResponseAndCompleteRun { .. }
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
pub struct ViewVisibilityProof {
    pub pane_visible: bool,
    pub window_visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewEvent {
    pub daemon_instance_id: DaemonInstanceId,
    pub event_id: EventId,
    pub hook_kind: ViewHookKind,
    pub active_pane: Option<PaneInstance>,
    pub window_panes: Vec<PaneInstance>,
    pub visibility: ViewVisibilityProof,
}

impl ViewEvent {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.hook_kind == ViewHookKind::ClientDetached {
            if self.active_pane.is_some()
                || !self.window_panes.is_empty()
                || self.visibility.pane_visible
                || self.visibility.window_visible
            {
                return Err(ModelError(
                    "client-detached event cannot contain visibility proof".to_string(),
                ));
            }
            return Ok(());
        }
        let active_pane = self
            .active_pane
            .as_ref()
            .ok_or_else(|| ModelError("view event active pane is missing".to_string()))?;
        active_pane.validate()?;
        if self.window_panes.len() > MAX_VIEW_PANES {
            return Err(ModelError("too many panes in view event".to_string()));
        }
        let mut panes = BTreeSet::new();
        for pane in &self.window_panes {
            pane.validate()?;
            if !panes.insert(pane) {
                return Err(ModelError("duplicate pane in view event".to_string()));
            }
        }
        if !panes.contains(active_pane) {
            return Err(ModelError(
                "active pane is not in the view event window".to_string(),
            ));
        }
        if self.visibility.pane_visible && !self.visibility.window_visible {
            return Err(ModelError(
                "pane visibility requires window visibility".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VisibilitySnapshot {
    pub pane_visible_to_eligible_client: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaptureTrackerSnapshot {
    pub generation: u64,
    pub epoch: Option<(StateId, u64)>,
    pub hook_authoritative: bool,
    /// A same-session interruption received after completion awaits one terminal check.
    pub interruption_verification_pending: bool,
    /// Currently verified exact identity exposed through the daemon projection.
    pub agent_process: Option<AgentProcessIdentity>,
    /// Last exact identity, retained across temporary ambiguity for replacement detection.
    pub last_agent_process: Option<AgentProcessIdentity>,
    pub absence_count: u8,
    pub replacement_kind: Option<AgentKind>,
    pub replacement_streak: u8,
    pub fingerprint: Option<[u8; 32]>,
    pub last_change_at: Option<i64>,
    pub last_semantic_scan_at: Option<i64>,
    pub rebaseline_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureTrackerDelta {
    pub next: CaptureTrackerSnapshot,
}

#[derive(Debug, Clone)]
pub struct ReductionContext<'a> {
    pub visibility: &'a VisibilitySnapshot,
    pub tracker: &'a CaptureTrackerSnapshot,
    pub new_state_id: Option<StateId>,
    pub latest_unread_order: u64,
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
            agent_process: None,
            agent_epoch: 1,
            agent_present: true,
            scan_verified: false,
            synthetic_completion_armed: false,
            lifecycle: LifecycleState::Idle,
            run_seq: 0,
            current_run: None,
            completed_seq: 0,
            unread: UnreadState::default(),
            started_at: None,
            completed_at: None,
            prompt: None,
            latest_response: None,
            task_context: TaskContextState::default(),
            tasks: TaskState::default(),
            subagents: Vec::new(),
            worktree_activity: None,
            background_process: None,
            listening_ports: Vec::new(),
        }
    }

    #[test]
    fn task_context_keeps_bounded_recent_prompts_and_reference_response() {
        let mut context = TaskContextState::default();
        for prompt in ["origin", "one", "two", "three", "four", "five", "five"] {
            context.observe_prompt(prompt);
        }
        context.observe_prompt_with_reference("six", Some("next task is RC-01"));

        assert_eq!(context.recent_prompts, ["three", "four", "five", "six"]);
        assert_eq!(
            context.reference_response.as_deref(),
            Some("next task is RC-01")
        );
        assert_eq!(context.context_fingerprint().unwrap().len(), 64);
        context.validate().unwrap();
    }

    #[test]
    fn task_context_exposes_only_the_summary_for_its_current_fingerprint() {
        let mut context = TaskContextState::default();
        context.observe_prompt("古い依頼");
        let old_fingerprint = context.context_fingerprint().unwrap();
        context.summary = Some(TaskSummaryState {
            text: Some("古い要約".to_string()),
            context_fingerprint: old_fingerprint,
            generated_at: 1,
            outcome: TaskSummaryOutcome::Generated,
            failure_code: None,
        });
        assert_eq!(
            context
                .current_summary()
                .and_then(|summary| summary.text.as_deref()),
            Some("古い要約")
        );

        context.observe_prompt("新しい依頼");
        assert!(context.current_summary().is_none());

        context.summary = Some(TaskSummaryState {
            text: None,
            context_fingerprint: context.context_fingerprint().unwrap(),
            generated_at: 2,
            outcome: TaskSummaryOutcome::Failed,
            failure_code: Some("timeout".to_string()),
        });
        assert_eq!(
            context.current_summary().unwrap().failure_code.as_deref(),
            Some("timeout")
        );
        context.validate().unwrap();
    }

    #[test]
    fn durable_run_projection_advances_sequence_and_rejects_same_sequence_rebinding() {
        let mut state = valid_state();
        state.prompt = Some(PromptState {
            text: "old prompt".to_string(),
            source: "user".to_string(),
            digest: None,
        });
        state.latest_response = Some(ResponseState {
            text: "old response".to_string(),
            observed_at: 1,
        });
        let projection = CurrentDurableRunProjection {
            run_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            run_seq: 3,
            run_revision: 4,
        };
        assert!(
            state
                .reconcile_current_run(projection.clone(), true, 10, 0)
                .unwrap()
        );
        assert_eq!(state.run_seq, 3);
        assert_eq!(state.completed_seq, 2);
        assert_eq!(state.current_run.as_ref(), Some(&projection));
        assert!(matches!(state.lifecycle, LifecycleState::Running));
        assert!(state.prompt.is_none());
        assert!(state.latest_response.is_none());

        let rebinding = CurrentDurableRunProjection {
            run_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            run_seq: 3,
            run_revision: 1,
        };
        assert!(state.reconcile_current_run(rebinding, true, 11, 0).is_err());

        let completed = CurrentDurableRunProjection {
            run_revision: 5,
            ..projection
        };
        assert!(
            state
                .reconcile_current_run(completed, false, 12, 0)
                .unwrap()
        );
        assert_eq!(state.completed_seq, 3);
        assert!(matches!(state.lifecycle, LifecycleState::Idle));
        assert_eq!(state.unread.occurrence_seq, 1);
        assert_eq!(
            state.unread.latest.as_ref().map(|latest| latest.reason),
            Some(UnreadReason::Completed)
        );
    }

    #[test]
    fn active_durable_projection_reopens_completed_first_run_consistently() {
        let mut state = valid_state();
        let projection = CurrentDurableRunProjection {
            run_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            run_seq: 1,
            run_revision: 1,
        };
        state
            .reconcile_current_run(projection.clone(), true, 10, 0)
            .unwrap();
        state
            .reconcile_current_run(projection.clone(), false, 12, 0)
            .unwrap();
        assert_eq!(state.completed_seq, 1);
        assert_eq!(state.completed_at, Some(12));

        assert!(
            state
                .reconcile_current_run(projection, true, 13, 1)
                .unwrap()
        );
        assert_eq!(state.run_seq, 1);
        assert_eq!(state.completed_seq, 0);
        assert_eq!(state.completed_at, None);
        assert!(matches!(state.lifecycle, LifecycleState::Running));
        state.validate().unwrap();
    }

    #[test]
    fn prompt_digest_is_domain_separated_and_strictly_validated() {
        assert_eq!(
            PromptState::digest_decoded_prompt("hello\nworld"),
            "3cf479a04899c793e4faf30c5b150c0c6e0aca73f52780ec274168e795a9634b"
        );

        let mut prompt = PromptState {
            text: "hello world".to_string(),
            source: "user".to_string(),
            digest: Some(PromptState::digest_decoded_prompt("hello\nworld")),
        };
        prompt.validate().unwrap();
        prompt.digest = Some("A".repeat(64));
        assert_eq!(prompt.validate().unwrap_err().0, "prompt digest is invalid");
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
        let mut value = serde_json::to_value(valid_state()).unwrap();
        value["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PaneState>(value).is_err());

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
    fn unread_state_validates_sequences_and_lifecycle_eligibility() {
        let mut unread = UnreadState {
            occurrence_seq: 1,
            read_seq: 0,
            latest: Some(UnreadOccurrence {
                seq: 1,
                order: 7,
                reason: UnreadReason::Waiting,
                occurred_at: 10,
            }),
        };
        assert!(unread.validate().is_ok());
        assert!(unread.is_jump_eligible(&LifecycleState::Waiting {
            reason: WaitReason::PermissionPrompt,
        }));
        assert!(!unread.is_jump_eligible(&LifecycleState::Running));

        unread.latest.as_mut().unwrap().reason = UnreadReason::Completed;
        assert!(unread.is_jump_eligible(&LifecycleState::Running));
        unread.read_seq = 1;
        assert!(!unread.is_jump_eligible(&LifecycleState::Idle));
        assert!(unread.validate().is_ok());

        unread.read_seq = 2;
        assert!(unread.validate().is_err());
        unread.read_seq = 0;
        unread.latest.as_mut().unwrap().order = 0;
        assert!(unread.validate().is_err());
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
    fn view_event_rejects_duplicate_window_panes() {
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 42,
        };
        let event = ViewEvent {
            daemon_instance_id: DaemonInstanceId::parse("00112233445566778899aabbccddeeff")
                .unwrap(),
            event_id: EventId::parse("ffeeddccbbaa99887766554433221100").unwrap(),
            hook_kind: ViewHookKind::WindowPaneChanged,
            active_pane: Some(pane.clone()),
            window_panes: vec![pane.clone(), pane],
            visibility: ViewVisibilityProof {
                pane_visible: false,
                window_visible: false,
            },
        };
        assert!(event.validate().is_err());
    }
}
