use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use schemars::{JsonSchema, schema_for};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::daemon::protocol::v2::{
    CLIENT_REQUEST_TIMEOUT, ClientMessage, DaemonDiagnostic, HookHealth, PROTOCOL_VERSION,
    PanePresentation, ResolvedSnapshot, ServerMessage, V2Client,
};
use crate::daemon::session_badge::BadgeState;
use crate::pane_state::{LifecycleState, PaneInstance, WaitReason};
use crate::tmux::TmuxRunner;

pub const API_VERSION: u16 = 2;
pub const DEFAULT_READ_LINES: usize = 120;
pub const MAX_READ_LINES: usize = 2_000;
pub const MAX_READ_BYTES: usize = 1024 * 1024;
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;
pub const DEFAULT_PROMPT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(7);
pub const MAX_PROMPT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug)]
pub struct ApiError {
    code: ApiErrorCode,
    message: String,
    stage: ApiErrorStage,
    side_effect: ApiSideEffect,
    retry_action: ApiRetryAction,
    receipt: Option<AgentPromptReceipt>,
}

impl ApiError {
    pub fn new(code: ApiErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            stage: code.default_stage(),
            side_effect: ApiSideEffect::None,
            retry_action: code.default_retry_action(),
            receipt: None,
        }
    }

    pub fn code(&self) -> &'static str {
        self.code.as_str()
    }

    fn with_dispatch_context(
        mut self,
        stage: ApiErrorStage,
        side_effect: ApiSideEffect,
        retry_action: ApiRetryAction,
        receipt: Option<AgentPromptReceipt>,
    ) -> Self {
        self.stage = stage;
        self.side_effect = side_effect;
        self.retry_action = retry_action;
        self.receipt = receipt;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorStage {
    RequestValidation,
    TargetResolution,
    Observation,
    BeforeDispatch,
    Dispatch,
    AfterDispatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiSideEffect {
    None,
    Possible,
    Confirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiRetryAction {
    RetrySameRequest,
    RefreshTarget,
    WaitThenRetry,
    RestartObservation,
    InspectManually,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    InvalidArguments,
    InvalidTarget,
    InvalidReference,
    NoCurrentPane,
    PaneNotFound,
    AgentNotFound,
    ExactIdentityUnavailable,
    StaleReference,
    TmuxServerUnavailable,
    DaemonUnavailable,
    DaemonNotReady,
    DaemonQueryFailed,
    DaemonStreamError,
    DaemonInvalidRequest,
    StaleDaemon,
    Timeout,
    EventHistoryLost,
    IdentityVerificationFailed,
    ControlUnavailable,
    ProtocolMismatch,
    ResourceLimit,
    InvalidDaemonResponse,
    CaptureFailed,
    AgentBusy,
    AgentBlocked,
    PromptConfirmationUnavailable,
    AgentNotInputOwner,
    PromptDispatchBusy,
    DispatchRejected,
    DeliveryUnknown,
    DaemonError,
    InternalError,
}

impl ApiErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArguments => "invalid_arguments",
            Self::InvalidTarget => "invalid_target",
            Self::InvalidReference => "invalid_reference",
            Self::NoCurrentPane => "no_current_pane",
            Self::PaneNotFound => "pane_not_found",
            Self::AgentNotFound => "agent_not_found",
            Self::ExactIdentityUnavailable => "exact_identity_unavailable",
            Self::StaleReference => "stale_reference",
            Self::TmuxServerUnavailable => "tmux_server_unavailable",
            Self::DaemonUnavailable => "daemon_unavailable",
            Self::DaemonNotReady => "daemon_not_ready",
            Self::DaemonQueryFailed => "daemon_query_failed",
            Self::DaemonStreamError => "daemon_stream_error",
            Self::DaemonInvalidRequest => "daemon_invalid_request",
            Self::StaleDaemon => "stale_daemon",
            Self::Timeout => "timeout",
            Self::EventHistoryLost => "event_history_lost",
            Self::IdentityVerificationFailed => "identity_verification_failed",
            Self::ControlUnavailable => "control_unavailable",
            Self::ProtocolMismatch => "protocol_mismatch",
            Self::ResourceLimit => "resource_limit",
            Self::InvalidDaemonResponse => "invalid_daemon_response",
            Self::CaptureFailed => "capture_failed",
            Self::AgentBusy => "agent_busy",
            Self::AgentBlocked => "agent_blocked",
            Self::PromptConfirmationUnavailable => "prompt_confirmation_unavailable",
            Self::AgentNotInputOwner => "agent_not_input_owner",
            Self::PromptDispatchBusy => "prompt_dispatch_busy",
            Self::DispatchRejected => "dispatch_rejected",
            Self::DeliveryUnknown => "delivery_unknown",
            Self::DaemonError => "daemon_error",
            Self::InternalError => "internal_error",
        }
    }

    fn default_stage(self) -> ApiErrorStage {
        match self {
            Self::InvalidArguments | Self::InvalidTarget | Self::InvalidReference => {
                ApiErrorStage::RequestValidation
            }
            Self::NoCurrentPane
            | Self::PaneNotFound
            | Self::AgentNotFound
            | Self::ExactIdentityUnavailable
            | Self::StaleReference => ApiErrorStage::TargetResolution,
            Self::AgentBusy
            | Self::AgentBlocked
            | Self::PromptConfirmationUnavailable
            | Self::AgentNotInputOwner
            | Self::PromptDispatchBusy => ApiErrorStage::BeforeDispatch,
            Self::DispatchRejected => ApiErrorStage::Dispatch,
            Self::DeliveryUnknown => ApiErrorStage::AfterDispatch,
            _ => ApiErrorStage::Observation,
        }
    }

    fn default_retry_action(self) -> ApiRetryAction {
        match self {
            Self::PaneNotFound
            | Self::AgentNotFound
            | Self::StaleReference
            | Self::DispatchRejected => ApiRetryAction::RefreshTarget,
            Self::ExactIdentityUnavailable
            | Self::DaemonNotReady
            | Self::Timeout
            | Self::IdentityVerificationFailed
            | Self::ControlUnavailable
            | Self::ResourceLimit
            | Self::AgentBusy
            | Self::AgentBlocked
            | Self::PromptConfirmationUnavailable
            | Self::AgentNotInputOwner
            | Self::PromptDispatchBusy => ApiRetryAction::WaitThenRetry,
            Self::TmuxServerUnavailable
            | Self::DaemonUnavailable
            | Self::DaemonQueryFailed
            | Self::DaemonStreamError
            | Self::StaleDaemon
            | Self::EventHistoryLost => ApiRetryAction::RestartObservation,
            Self::DeliveryUnknown => ApiRetryAction::InspectManually,
            _ => ApiRetryAction::Never,
        }
    }
}

macro_rules! api_error {
    ("invalid_arguments", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::InvalidArguments, $message)
    };
    ("invalid_target", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::InvalidTarget, $message)
    };
    ("invalid_reference", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::InvalidReference, $message)
    };
    ("no_current_pane", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::NoCurrentPane, $message)
    };
    ("pane_not_found", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::PaneNotFound, $message)
    };
    ("agent_not_found", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::AgentNotFound, $message)
    };
    ("exact_identity_unavailable", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::ExactIdentityUnavailable, $message)
    };
    ("stale_reference", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::StaleReference, $message)
    };
    ("tmux_server_unavailable", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::TmuxServerUnavailable, $message)
    };
    ("daemon_unavailable", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::DaemonUnavailable, $message)
    };
    ("daemon_not_ready", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::DaemonNotReady, $message)
    };
    ("daemon_query_failed", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::DaemonQueryFailed, $message)
    };
    ("daemon_stream_error", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::DaemonStreamError, $message)
    };
    ("daemon_invalid_request", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::DaemonInvalidRequest, $message)
    };
    ("stale_daemon", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::StaleDaemon, $message)
    };
    ("timeout", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::Timeout, $message)
    };
    ("event_history_lost", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::EventHistoryLost, $message)
    };
    ("identity_verification_failed", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::IdentityVerificationFailed, $message)
    };
    ("control_unavailable", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::ControlUnavailable, $message)
    };
    ("protocol_mismatch", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::ProtocolMismatch, $message)
    };
    ("resource_limit", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::ResourceLimit, $message)
    };
    ("invalid_daemon_response", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::InvalidDaemonResponse, $message)
    };
    ("capture_failed", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::CaptureFailed, $message)
    };
    ("agent_busy", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::AgentBusy, $message)
    };
    ("agent_blocked", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::AgentBlocked, $message)
    };
    ("prompt_confirmation_unavailable", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::PromptConfirmationUnavailable, $message)
    };
    ("agent_not_input_owner", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::AgentNotInputOwner, $message)
    };
    ("prompt_dispatch_busy", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::PromptDispatchBusy, $message)
    };
    ("dispatch_rejected", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::DispatchRejected, $message)
    };
    ("delivery_unknown", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::DeliveryUnknown, $message)
    };
    ("daemon_error", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::DaemonError, $message)
    };
    ("internal_error", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::InternalError, $message)
    };
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiMeta {
    pub api_version: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_revision: Option<u64>,
    pub started_at: i64,
    pub emitted_at: i64,
    pub diagnostic_count: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiSuccessEnvelope {
    pub meta: ApiMeta,
    pub result: ApiResult,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiErrorEnvelope {
    pub meta: ApiMeta,
    pub error: ApiErrorBody,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiErrorBody {
    pub code: ApiErrorCode,
    pub message: String,
    pub stage: ApiErrorStage,
    pub side_effect: ApiSideEffect,
    pub retry_action: ApiRetryAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<AgentPromptReceipt>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiResult {
    Schema {
        schemas: BTreeMap<String, serde_json::Value>,
    },
    Snapshot {
        panes: Vec<PaneSummary>,
        agents: Vec<AgentSummary>,
        diagnostics: Vec<DiagnosticSummary>,
    },
    PaneList {
        panes: Vec<PaneSummary>,
    },
    PaneGet {
        pane: PaneDetail,
    },
    PaneRead {
        pane: PaneSummary,
        read: ReadResult,
    },
    AgentList {
        agents: Vec<AgentSummary>,
    },
    AgentGet {
        agent: AgentDetail,
    },
    AgentPrompt {
        receipt: AgentPromptReceipt,
        dispatch: AgentPromptDispatch,
        confirmation: AgentPromptConfirmation,
        #[serde(skip_serializing_if = "Option::is_none")]
        observed_run_seq: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        observed_state_revision: Option<u64>,
        wait_cursor: AgentWaitCursor,
    },
    AgentWait {
        target: AgentWaitTarget,
        matched_status: AgentStatus,
        match_source: AgentWaitMatchSource,
        baseline_completed_seq: u64,
        matched_completed_seq: u64,
        matched_state_revision: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        matched_at: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_agent: Option<AgentSummary>,
        waited_ms: u64,
    },
    AgentRead {
        agent: AgentSummary,
        read: ReadResult,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentWaitMatchSource {
    CurrentState,
    TransitionEvent,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionLink {
    pub session_id: String,
    pub session_name: String,
    pub window_index: i64,
    pub window_active: bool,
    pub window_last: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PaneSummary {
    pub pane_ref: String,
    pub pane_id: String,
    pub pane_pid: u32,
    pub sessions: Vec<SessionLink>,
    pub window_id: String,
    pub window_name: String,
    pub current_path: String,
    pub current_command: String,
    pub pane_width: u16,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PaneDetail {
    pub summary: PaneSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentDetail>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Blocked,
    Working,
    Done,
    Idle,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Working => "working",
            Self::Done => "done",
            Self::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentBadge {
    Blocked,
    Working,
    Done,
    Idle,
}

impl From<BadgeState> for AgentBadge {
    fn from(value: BadgeState) -> Self {
        match value {
            BadgeState::Blocked => Self::Blocked,
            BadgeState::Working => Self::Working,
            BadgeState::Done => Self::Done,
            BadgeState::Idle => Self::Idle,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LifecycleSummary {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_ref: Option<String>,
    pub identity: AgentIdentityStrength,
    pub pane_ref: String,
    pub pane_id: String,
    pub pane_pid: u32,
    pub agent: String,
    pub status: AgentStatus,
    pub badge: AgentBadge,
    pub lifecycle: LifecycleSummary,
    pub sessions: Vec<SessionLink>,
    pub window_id: String,
    pub window_name: String,
    pub current_path: String,
    pub active: bool,
    pub present: bool,
    pub unread: bool,
    pub needs_action: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentIdentityStrength {
    Exact,
    Inferred,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentDetail {
    pub summary: AgentSummary,
    pub state_id: String,
    pub state_revision: u64,
    pub agent_epoch: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    pub run_seq: u64,
    pub completed_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_digest: Option<String>,
    pub task_progress_done: u64,
    pub task_progress_total: u64,
    pub subagent_count: usize,
    pub listening_ports: Vec<u16>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentWaitTarget {
    pub agent_ref: String,
    pub pane_ref: String,
    pub pane_id: String,
    pub pane_pid: u32,
    pub agent: String,
    pub state_id: String,
    pub agent_epoch: u64,
    pub process_pid: u32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentPromptReceipt {
    pub target: AgentWaitTarget,
    pub prompt_digest: String,
    pub baseline_run_seq: u64,
    pub baseline_completed_seq: u64,
    pub expected_run_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentPromptDispatch {
    Submitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentPromptConfirmation {
    DigestMatched,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentWaitCursor {
    pub agent_ref: String,
    pub after_completed_seq: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiagnosticSummary {
    pub code: String,
    pub message: String,
    pub count: usize,
    pub pane_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReadResult {
    pub source: String,
    pub ansi: bool,
    pub lines_requested: usize,
    pub bytes_captured: usize,
    pub bytes_returned: usize,
    pub truncated: bool,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct AgentListFilter {
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub status: Option<AgentStatus>,
    #[serde(default)]
    pub cwd_prefix: Option<String>,
    #[serde(default)]
    pub unread_only: bool,
    #[serde(default)]
    pub needs_action_only: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadSource {
    Visible,
    #[default]
    Latest,
}

impl ReadSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Latest => "latest",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
pub struct ReadOptions {
    #[serde(default)]
    pub source: ReadSource,
    #[serde(default = "default_read_lines")]
    #[schemars(range(min = 1, max = 2_000))]
    pub lines: usize,
    #[serde(default)]
    pub ansi: bool,
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            source: ReadSource::Latest,
            lines: DEFAULT_READ_LINES,
            ansi: false,
        }
    }
}

fn default_read_lines() -> usize {
    DEFAULT_READ_LINES
}

fn default_wait_timeout_ms() -> u64 {
    DEFAULT_WAIT_TIMEOUT.as_millis() as u64
}

fn default_prompt_confirm_timeout_ms() -> u64 {
    DEFAULT_PROMPT_CONFIRM_TIMEOUT.as_millis() as u64
}

fn default_wait_statuses() -> BTreeSet<AgentStatus> {
    [AgentStatus::Done, AgentStatus::Blocked]
        .into_iter()
        .collect()
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ApiRequest {
    ApiSchema,
    ApiSnapshot,
    PaneList,
    PaneGet {
        target: String,
    },
    PaneCurrent,
    PaneRead {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        read: ReadOptions,
    },
    AgentList {
        #[serde(default)]
        filter: AgentListFilter,
    },
    AgentGet {
        target: String,
    },
    AgentPrompt {
        target: String,
        #[serde(default = "default_prompt_confirm_timeout_ms")]
        #[schemars(range(min = 1, max = 60_000))]
        confirm_timeout_ms: u64,
    },
    AgentWait {
        target: String,
        #[serde(default = "default_wait_statuses")]
        #[schemars(length(min = 1, max = 4))]
        until: BTreeSet<AgentStatus>,
        #[serde(default = "default_wait_timeout_ms")]
        #[schemars(range(min = 1, max = 86_400_000))]
        timeout_ms: u64,
        #[serde(default)]
        after_completed_seq: Option<u64>,
    },
    AgentRead {
        target: String,
        #[serde(default)]
        read: ReadOptions,
    },
}

pub fn render_error(error: &anyhow::Error, started_at: i64) -> String {
    let api_error = error
        .chain()
        .find_map(|source| source.downcast_ref::<ApiError>());
    let clap_error = error
        .chain()
        .any(|source| source.downcast_ref::<clap::Error>().is_some());
    let body = ApiErrorBody {
        code: api_error
            .map(|error| error.code)
            .or_else(|| clap_error.then_some(ApiErrorCode::InvalidArguments))
            .unwrap_or(ApiErrorCode::InternalError),
        message: api_error
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("{error:#}")),
        stage: api_error
            .map(|error| error.stage)
            .unwrap_or(ApiErrorStage::RequestValidation),
        side_effect: api_error
            .map(|error| error.side_effect)
            .unwrap_or(ApiSideEffect::None),
        retry_action: api_error
            .map(|error| error.retry_action)
            .unwrap_or(ApiRetryAction::Never),
        receipt: api_error.and_then(|error| error.receipt.clone()),
    };
    serde_json::to_string(&ApiErrorEnvelope {
        meta: ApiMeta {
            api_version: API_VERSION,
            server_identity: None,
            daemon_instance_id: None,
            snapshot_revision: None,
            started_at,
            emitted_at: epoch_now(),
            diagnostic_count: 0,
        },
        error: body,
    })
    .expect("API error envelope must serialize")
}

pub fn schema_json(started_at: i64) -> Result<String> {
    let schemas = BTreeMap::from([
        (
            "request".to_string(),
            serde_json::to_value(schema_for!(ApiRequest))?,
        ),
        (
            "success".to_string(),
            serde_json::to_value(schema_for!(ApiSuccessEnvelope))?,
        ),
        (
            "error".to_string(),
            serde_json::to_value(schema_for!(ApiErrorEnvelope))?,
        ),
    ]);
    Ok(serde_json::to_string_pretty(&ApiSuccessEnvelope {
        meta: ApiMeta {
            api_version: API_VERSION,
            server_identity: None,
            daemon_instance_id: None,
            snapshot_revision: None,
            started_at,
            emitted_at: epoch_now(),
            diagnostic_count: 0,
        },
        result: ApiResult::Schema { schemas },
    })?)
}

pub fn snapshot(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let snapshot = connection.query_snapshot()?;
    let panes = snapshot
        .panes
        .iter()
        .map(|pane| pane_summary(pane, &connection.server_identity))
        .collect();
    let agents = snapshot
        .panes
        .iter()
        .filter_map(|pane| agent_summary(pane, &snapshot, &connection.server_identity))
        .collect();
    success_json(
        &connection,
        &snapshot,
        observed_at,
        ApiResult::Snapshot {
            panes,
            agents,
            diagnostics: aggregate_diagnostics(&snapshot.diagnostics),
        },
    )
}

pub fn pane_list(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let snapshot = connection.query_snapshot()?;
    let panes = snapshot
        .panes
        .iter()
        .map(|pane| pane_summary(pane, &connection.server_identity))
        .collect();
    success_json(
        &connection,
        &snapshot,
        observed_at,
        ApiResult::PaneList { panes },
    )
}

pub fn pane_get(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let snapshot = connection.query_snapshot()?;
    let pane = resolve_pane(&snapshot, target, &connection.server_identity)?;
    let detail = pane_detail(pane, &snapshot, &connection.server_identity);
    success_json(
        &connection,
        &snapshot,
        observed_at,
        ApiResult::PaneGet { pane: detail },
    )
}

pub fn pane_current(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
) -> Result<String> {
    let target = env
        .get("TMUX_PANE")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| api_error!("no_current_pane", "TMUX_PANE is not set"))?;
    pane_get(runner, env, observed_at, target)
}

pub fn pane_read(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
    options: ReadOptions,
) -> Result<String> {
    validate_read_options(options)?;
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let before = connection.query_snapshot()?;
    let pane = resolve_pane(&before, target, &connection.server_identity)?;
    let expected = pane.pane_instance.clone();
    let read = capture_pane_guarded(runner, env, &connection, &expected, options)?;
    let mut after_connection = connection.reconnect()?;
    let after = after_connection.query_snapshot()?;
    let pane = require_same_pane(&after, &expected)?;
    let pane_result = pane_summary(pane, &after_connection.server_identity);
    success_json(
        &after_connection,
        &after,
        observed_at,
        ApiResult::PaneRead {
            pane: pane_result,
            read,
        },
    )
}

pub fn agent_list(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    filter: &AgentListFilter,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let snapshot = connection.query_snapshot()?;
    let agents = snapshot
        .panes
        .iter()
        .filter_map(|pane| agent_summary(pane, &snapshot, &connection.server_identity))
        .filter(|agent| matches_agent_filter(agent, filter))
        .collect();
    success_json(
        &connection,
        &snapshot,
        observed_at,
        ApiResult::AgentList { agents },
    )
}

pub fn agent_get(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let snapshot = connection.query_snapshot()?;
    let pane = resolve_agent(&snapshot, target, &connection.server_identity)?;
    let agent = agent_detail(pane, &snapshot, &connection.server_identity)
        .expect("resolve_agent only returns resolved agents");
    success_json(
        &connection,
        &snapshot,
        observed_at,
        ApiResult::AgentGet { agent },
    )
}

pub fn agent_prompt(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    target: &str,
    prompt: &str,
    confirm_timeout: Duration,
) -> Result<String> {
    validate_prompt(prompt)?;
    if !target.starts_with("vta1:") {
        return Err(api_error!(
            "invalid_arguments",
            "agent prompt requires an exact agent_ref target",
        )
        .into());
    }
    if confirm_timeout.is_zero() || confirm_timeout > MAX_PROMPT_CONFIRM_TIMEOUT {
        return Err(api_error!(
            "invalid_arguments",
            format!(
                "--confirm-timeout-ms must be between 1 and {}",
                MAX_PROMPT_CONFIRM_TIMEOUT.as_millis()
            ),
        )
        .into());
    }

    let started_at = epoch_now();
    let deadline = Instant::now() + confirm_timeout;
    let mut connection = ApiConnection::connect(runner, env, Some(deadline))?;
    if connection.client.hook_health() != HookHealth::Healthy {
        return Err(api_error!(
            "prompt_confirmation_unavailable",
            "daemon hook health is degraded; no prompt bytes were sent",
        )
        .into());
    }
    let subscribed = connection.subscribe()?;
    let (initial_pane, identity) =
        resolve_wait_resume_agent(&subscribed, target, &connection.server_identity)?;
    require_prompt_capable_agent(&identity)?;
    let baseline = PromptBaseline::from_pane(initial_pane)?;
    let prompt_digest = crate::pane_state::PromptState::digest_decoded_prompt(prompt);
    let receipt = AgentPromptReceipt {
        target: wait_target(
            initial_pane,
            &connection.server_identity,
            &identity,
            Some(target),
        ),
        prompt_digest: prompt_digest.clone(),
        baseline_run_seq: baseline.run_seq,
        baseline_completed_seq: baseline.completed_seq,
        expected_run_seq: baseline.expected_run_seq,
    };

    let _lock = crate::runtime_dir::try_acquire_pane_dispatch_lock(
        &connection.incarnation.identity,
        &identity.pane_instance.pane_id,
        identity.pane_instance.pane_pid,
    )
    .map_err(|error| {
        ApiError::new(
            ApiErrorCode::PromptDispatchBusy,
            format!("could not acquire the guarded dispatch lock: {error:#}"),
        )
    })?
    .ok_or_else(|| {
        api_error!(
            "prompt_dispatch_busy",
            format!(
                "another guarded dispatch is active for pane {}",
                identity.pane_instance.pane_id
            ),
        )
    })?;

    let mut fence_connection = connection.reconnect()?;
    let before_dispatch = fence_connection.query_snapshot()?;
    let pane = require_same_agent(&before_dispatch, &identity).map_err(before_dispatch_error)?;
    require_prompt_baseline(pane, &baseline).map_err(before_dispatch_error)?;
    verify_live_pane(runner, env, &fence_connection, &identity.pane_instance)
        .map_err(before_dispatch_error)?;
    verify_live_agent_process(runner, &identity, pane).map_err(before_dispatch_error)?;
    verify_agent_input_owner(runner, &identity).map_err(before_dispatch_error)?;

    dispatch_prompt_guarded(
        runner,
        &connection.incarnation,
        &identity.pane_instance,
        prompt.as_bytes(),
        &prompt_digest,
        &receipt,
    )?;

    let mut after_connection = match connection.reconnect() {
        Ok(connection) => connection,
        Err(error) => {
            return Err(delivery_unknown(
                format!("prompt was submitted but daemon revalidation failed: {error:#}"),
                receipt,
            )
            .into());
        }
    };
    let after = match after_connection.query_snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return Err(delivery_unknown(
                format!("prompt was submitted but the post-dispatch snapshot failed: {error:#}"),
                receipt,
            )
            .into());
        }
    };
    let after_pane = require_same_agent(&after, &identity).map_err(|error| {
        delivery_unknown(
            format!("prompt was submitted but the exact agent changed: {error:#}"),
            receipt.clone(),
        )
    })?;
    verify_live_pane(runner, env, &after_connection, &identity.pane_instance).map_err(|error| {
        delivery_unknown(
            format!("prompt was submitted but the live pane fence failed: {error:#}"),
            receipt.clone(),
        )
    })?;
    verify_live_agent_process(runner, &identity, after_pane).map_err(|error| {
        delivery_unknown(
            format!("prompt was submitted but the live agent fence failed: {error:#}"),
            receipt.clone(),
        )
    })?;

    let (confirmed_snapshot, observed_run_seq, observed_state_revision) =
        confirm_prompt_digest(&mut connection, subscribed, &identity, &receipt, deadline)
            .map_err(|message| delivery_unknown(message, receipt.clone()))?;
    success_json(
        &connection,
        &confirmed_snapshot,
        started_at,
        ApiResult::AgentPrompt {
            wait_cursor: AgentWaitCursor {
                agent_ref: receipt.target.agent_ref.clone(),
                after_completed_seq: receipt.baseline_completed_seq,
            },
            receipt,
            dispatch: AgentPromptDispatch::Submitted,
            confirmation: AgentPromptConfirmation::DigestMatched,
            observed_run_seq: Some(observed_run_seq),
            observed_state_revision: Some(observed_state_revision),
        },
    )
}

#[derive(Debug, Clone)]
struct PromptBaseline {
    state_id: String,
    agent_epoch: u64,
    run_seq: u64,
    completed_seq: u64,
    expected_run_seq: u64,
}

impl PromptBaseline {
    fn from_pane(pane: &PanePresentation) -> Result<Self> {
        let state = canonical_state(pane)
            .ok_or_else(|| api_error!("agent_not_found", "agent state is unavailable"))?;
        require_promptable_lifecycle(state)?;
        let expected_run_seq = state
            .run_seq
            .checked_add(1)
            .ok_or_else(|| api_error!("resource_limit", "run sequence overflow"))?;
        Ok(Self {
            state_id: state.state_id.as_str().to_string(),
            agent_epoch: state.agent_epoch,
            run_seq: state.run_seq,
            completed_seq: state.completed_seq,
            expected_run_seq,
        })
    }
}

fn validate_prompt(prompt: &str) -> Result<()> {
    if prompt.is_empty() || prompt.len() > MAX_PROMPT_BYTES {
        return Err(api_error!(
            "invalid_arguments",
            format!("prompt must contain between 1 and {MAX_PROMPT_BYTES} UTF-8 bytes"),
        )
        .into());
    }
    if prompt.chars().any(|character| {
        matches!(character, '\r' | '\t' | '\u{7f}')
            || (character.is_control() && character != '\n')
            || ('\u{80}'..='\u{9f}').contains(&character)
    }) {
        return Err(api_error!(
            "invalid_arguments",
            "prompt may contain LF newlines but must not contain CR, TAB, C0, DEL, or C1 controls",
        )
        .into());
    }
    Ok(())
}

fn require_prompt_capable_agent(identity: &AgentIdentity) -> Result<()> {
    if matches!(identity.agent.as_str(), "claude" | "codex") {
        Ok(())
    } else {
        Err(ApiError::new(
            ApiErrorCode::PromptConfirmationUnavailable,
            format!(
                "agent kind {} has no supported prompt-bearing hook authority",
                identity.agent
            ),
        )
        .with_dispatch_context(
            ApiErrorStage::BeforeDispatch,
            ApiSideEffect::None,
            ApiRetryAction::Never,
            None,
        )
        .into())
    }
}

fn before_dispatch_error(error: anyhow::Error) -> anyhow::Error {
    match error.downcast::<ApiError>() {
        Ok(mut error) => {
            error.stage = ApiErrorStage::BeforeDispatch;
            error.side_effect = ApiSideEffect::None;
            error.receipt = None;
            error.into()
        }
        Err(error) => error,
    }
}

fn require_promptable_lifecycle(state: AgentStateView<'_>) -> Result<()> {
    match state.lifecycle {
        LifecycleState::Running => {
            Err(api_error!("agent_busy", "agent is working; no prompt bytes were sent",).into())
        }
        LifecycleState::Waiting { .. } | LifecycleState::Error { .. } => Err(api_error!(
            "agent_blocked",
            "agent is blocked; no prompt bytes were sent",
        )
        .into()),
        LifecycleState::Idle => Ok(()),
    }
}

fn require_prompt_baseline(pane: &PanePresentation, baseline: &PromptBaseline) -> Result<()> {
    let state = canonical_state(pane)
        .ok_or_else(|| api_error!("stale_reference", "agent state disappeared"))?;
    require_promptable_lifecycle(state)?;
    if state.state_id.as_str() != baseline.state_id
        || state.agent_epoch != baseline.agent_epoch
        || state.run_seq != baseline.run_seq
        || state.completed_seq != baseline.completed_seq
    {
        return Err(api_error!(
            "stale_reference",
            "agent state changed after the prompt baseline was established",
        )
        .into());
    }
    Ok(())
}

fn verify_agent_input_owner(runner: &dyn TmuxRunner, identity: &AgentIdentity) -> Result<()> {
    runner
        .verify_agent_input_owner(identity.pane_instance.pane_pid, identity.agent_process.pid)
        .map_err(|error| {
            api_error!(
                "agent_not_input_owner",
                format!(
                    "exact agent process is not the foreground input owner for pane {}: {error}",
                    identity.pane_instance.pane_id
                ),
            )
        })?;
    Ok(())
}

fn delivery_unknown(message: impl Into<String>, receipt: AgentPromptReceipt) -> ApiError {
    ApiError::new(ApiErrorCode::DeliveryUnknown, message).with_dispatch_context(
        ApiErrorStage::AfterDispatch,
        ApiSideEffect::Possible,
        ApiRetryAction::InspectManually,
        Some(receipt),
    )
}

fn dispatch_prompt_guarded(
    runner: &dyn TmuxRunner,
    incarnation: &crate::daemon::lifecycle::TmuxServerIncarnation,
    pane: &PaneInstance,
    prompt: &[u8],
    prompt_digest: &str,
    receipt: &AgentPromptReceipt,
) -> Result<()> {
    let nonce = format!(
        "{}:{}:{}:{}:{}",
        std::process::id(),
        epoch_now(),
        pane.pane_id,
        pane.pane_pid,
        prompt_digest
    );
    let nonce = format!("{:x}", Sha256::digest(nonce.as_bytes()));
    dispatch_prompt_guarded_with_nonce(runner, incarnation, pane, prompt, receipt, &nonce)
}

struct GuardedPromptCommand {
    args: Vec<String>,
    buffer: String,
    success: String,
    server_mismatch: String,
    pane_mismatch: String,
}

fn build_guarded_prompt_command(
    incarnation: &crate::daemon::lifecycle::TmuxServerIncarnation,
    pane: &PaneInstance,
    nonce: &str,
) -> GuardedPromptCommand {
    const SUCCESS_PREFIX: &str = "__vde_agent_prompt_submitted__";
    const SERVER_MISMATCH_PREFIX: &str = "__vde_agent_prompt_server_mismatch__";
    const PANE_MISMATCH_PREFIX: &str = "__vde_agent_prompt_pane_mismatch__";

    let buffer = format!("vde-agent-prompt-{}", &nonce[..24]);
    let success = format!("{SUCCESS_PREFIX}:{nonce}");
    let server_mismatch = format!("{SERVER_MISMATCH_PREFIX}:{nonce}");
    let pane_mismatch = format!("{PANE_MISMATCH_PREFIX}:{nonce}");

    let delete_buffer = || {
        vec![
            "delete-buffer".to_string(),
            "-b".to_string(),
            buffer.clone(),
        ]
    };
    let submitted_command = crate::pane_state::store::tmux_command_string(&[
        "paste-buffer".to_string(),
        "-p".to_string(),
        "-r".to_string(),
        "-d".to_string(),
        "-b".to_string(),
        buffer.clone(),
        "-t".to_string(),
        pane.pane_id.clone(),
        ";".to_string(),
        "send-keys".to_string(),
        "-t".to_string(),
        pane.pane_id.clone(),
        "Enter".to_string(),
        ";".to_string(),
        "display-message".to_string(),
        "-p".to_string(),
        success.clone(),
    ]);
    let mut pane_mismatch_command = delete_buffer();
    pane_mismatch_command.extend([
        ";".to_string(),
        "display-message".to_string(),
        "-p".to_string(),
        pane_mismatch.clone(),
    ]);
    let pane_guarded_command = crate::pane_state::store::tmux_command_string(&[
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        pane.pane_id.clone(),
        format!("#{{==:#{{pane_pid}},{}}}", pane.pane_pid),
        submitted_command,
        crate::pane_state::store::tmux_command_string(&pane_mismatch_command),
    ]);
    let mut server_mismatch_command = delete_buffer();
    server_mismatch_command.extend([
        ";".to_string(),
        "display-message".to_string(),
        "-p".to_string(),
        server_mismatch.clone(),
    ]);
    let server_guard = format!(
        "#{{&&:#{{==:#{{pid}},{}}},#{{==:#{{start_time}},{}}}}}",
        incarnation.identity.pid, incarnation.identity.start_time
    );
    let args = vec![
        "load-buffer".to_string(),
        "-b".to_string(),
        buffer.clone(),
        "-".to_string(),
        ";".to_string(),
        "if-shell".to_string(),
        "-F".to_string(),
        server_guard,
        pane_guarded_command,
        crate::pane_state::store::tmux_command_string(&server_mismatch_command),
    ];
    GuardedPromptCommand {
        args,
        buffer,
        success,
        server_mismatch,
        pane_mismatch,
    }
}

fn dispatch_prompt_guarded_with_nonce(
    runner: &dyn TmuxRunner,
    incarnation: &crate::daemon::lifecycle::TmuxServerIncarnation,
    pane: &PaneInstance,
    prompt: &[u8],
    receipt: &AgentPromptReceipt,
    nonce: &str,
) -> Result<()> {
    let command = build_guarded_prompt_command(incarnation, pane, nonce);
    let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    let result = runner.run_with_input(&args, prompt);
    let _ = runner.run(&["delete-buffer", "-b", &command.buffer]);
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            let side_effect = match error.stage {
                crate::tmux::InputWriteStage::BeforeSpawn => ApiSideEffect::None,
                crate::tmux::InputWriteStage::AfterSpawnBeforeWrite
                | crate::tmux::InputWriteStage::AfterPartialWrite
                | crate::tmux::InputWriteStage::AfterFullWrite => ApiSideEffect::Possible,
            };
            let retry_action = match error.stage {
                crate::tmux::InputWriteStage::BeforeSpawn => ApiRetryAction::RetrySameRequest,
                crate::tmux::InputWriteStage::AfterSpawnBeforeWrite
                | crate::tmux::InputWriteStage::AfterPartialWrite
                | crate::tmux::InputWriteStage::AfterFullWrite => ApiRetryAction::InspectManually,
            };
            return Err(ApiError::new(
                if side_effect == ApiSideEffect::None {
                    ApiErrorCode::DispatchRejected
                } else {
                    ApiErrorCode::DeliveryUnknown
                },
                format!("guarded tmux dispatch failed at {:?}: {error}", error.stage),
            )
            .with_dispatch_context(
                ApiErrorStage::Dispatch,
                side_effect,
                retry_action,
                (side_effect != ApiSideEffect::None).then(|| receipt.clone()),
            )
            .into());
        }
    };
    let markers = output.lines().map(str::trim).collect::<BTreeSet<_>>();
    if markers.contains(command.success.as_str()) {
        return Ok(());
    }
    if markers.contains(command.server_mismatch.as_str())
        || markers.contains(command.pane_mismatch.as_str())
    {
        return Err(ApiError::new(
            ApiErrorCode::DispatchRejected,
            "tmux server or pane identity changed before guarded dispatch",
        )
        .with_dispatch_context(
            ApiErrorStage::Dispatch,
            ApiSideEffect::None,
            ApiRetryAction::RefreshTarget,
            None,
        )
        .into());
    }
    Err(delivery_unknown(
        "guarded dispatch returned without an unambiguous submission marker",
        receipt.clone(),
    )
    .into())
}

fn confirm_prompt_digest(
    connection: &mut ApiConnection,
    mut snapshot: ResolvedSnapshot,
    identity: &AgentIdentity,
    receipt: &AgentPromptReceipt,
    deadline: Instant,
) -> std::result::Result<(ResolvedSnapshot, u64, u64), String> {
    loop {
        if let Some((run_seq, revision)) = observe_prompt_digest(&snapshot, identity, receipt)? {
            return Ok((snapshot.clone(), run_seq, revision));
        }
        if Instant::now() >= deadline {
            return Err(
                "prompt digest was not confirmed before the deadline; do not resend automatically"
                    .to_string(),
            );
        }
        snapshot = connection
            .next_snapshot()
            .map_err(|error| format!("prompt confirmation stream failed: {error:#}"))?;
    }
}

fn observe_prompt_digest(
    snapshot: &ResolvedSnapshot,
    identity: &AgentIdentity,
    receipt: &AgentPromptReceipt,
) -> std::result::Result<Option<(u64, u64)>, String> {
    let pane = require_same_agent_state(snapshot, identity)
        .map_err(|error| format!("exact agent changed before digest confirmation: {error:#}"))?;
    let mut observed_mismatch = false;
    for event in &snapshot.events {
        let Some(version) = &event.state_version else {
            continue;
        };
        if event.pane_instance == identity.pane_instance
            && event.agent == identity.agent
            && version.state_id.as_str() == identity.state_id
            && version.agent_epoch == identity.agent_epoch
            && event.run_seq == receipt.expected_run_seq
            && event.prompt_submitted
        {
            if event.prompt_digest.as_deref() == Some(receipt.prompt_digest.as_str()) {
                return Ok(Some((event.run_seq, version.revision)));
            }
            if event.prompt_digest.is_some() {
                observed_mismatch = true;
            }
        }
    }
    if let Some(resolved) = &pane.resolved {
        let state = &resolved.canonical;
        if state.run_seq == receipt.expected_run_seq
            && let Some(prompt) = state
                .prompt
                .as_ref()
                .filter(|prompt| prompt.source == "user")
            && let Some(digest) = prompt.digest.as_deref()
        {
            if digest == receipt.prompt_digest.as_str() {
                return Ok(Some((state.run_seq, state.revision)));
            }
            observed_mismatch = true;
        }
        if state.run_seq > receipt.expected_run_seq {
            return Err(format!(
                "agent advanced to run {} before the expected user prompt digest was confirmed",
                state.run_seq
            ));
        }
    }
    if observed_mismatch {
        return Err(
            "an observed user prompt digest did not match the dispatched prompt".to_string(),
        );
    }
    Ok(None)
}

pub fn agent_read(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
    options: ReadOptions,
) -> Result<String> {
    validate_read_options(options)?;
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let before = connection.query_snapshot()?;
    let pane = resolve_agent(&before, target, &connection.server_identity)?;
    let identity = AgentIdentity::from_pane(pane)?;
    verify_live_agent_process(runner, &identity, pane)?;
    let read = capture_pane_guarded(runner, env, &connection, &identity.pane_instance, options)?;
    let mut after_connection = connection.reconnect()?;
    let after = after_connection.query_snapshot()?;
    let pane = require_same_agent(&after, &identity)?;
    verify_live_agent_process(runner, &identity, pane)?;
    let summary = agent_summary(pane, &after, &after_connection.server_identity)
        .expect("same agent has resolved state");
    success_json(
        &after_connection,
        &after,
        observed_at,
        ApiResult::AgentRead {
            agent: summary,
            read,
        },
    )
}

pub fn agent_wait(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    target: &str,
    until: &BTreeSet<AgentStatus>,
    timeout: Duration,
    after_completed_seq: Option<u64>,
) -> Result<String> {
    if until.is_empty() {
        return Err(api_error!("invalid_arguments", "--until must not be empty").into());
    }
    if timeout.is_zero() {
        return Err(api_error!("invalid_arguments", "--timeout-ms must be positive").into());
    }
    if timeout > MAX_WAIT_TIMEOUT {
        return Err(api_error!(
            "invalid_arguments",
            format!(
                "--timeout-ms must not exceed {}",
                MAX_WAIT_TIMEOUT.as_millis()
            ),
        )
        .into());
    }
    if after_completed_seq.is_some() && !target.starts_with("vta1:") {
        return Err(api_error!(
            "invalid_arguments",
            "--after-completed-seq requires an exact agent_ref target",
        )
        .into());
    }
    let started_at = epoch_now();
    let started = Instant::now();
    let deadline = started + timeout;
    let mut connection = ApiConnection::connect(runner, env, Some(deadline))?;
    let first = connection.subscribe()?;
    let (pane, identity) = if after_completed_seq.is_some() {
        resolve_wait_resume_agent(&first, target, &connection.server_identity)?
    } else {
        let pane = resolve_agent(&first, target, &connection.server_identity)?;
        (pane, AgentIdentity::from_pane(pane)?)
    };
    let baseline = WaitBaseline::from_pane(pane, after_completed_seq)?;
    let completion_already_recorded = canonical_state(pane).is_some_and(|state| {
        until.contains(&AgentStatus::Done)
            && state.completed_seq >= baseline.expected_completion_seq
    });
    if canonical_state(pane).is_some_and(|state| state.agent_present)
        && !completion_already_recorded
    {
        verify_live_pane(runner, env, &connection, &identity.pane_instance)?;
        reject_live_agent_process_replacement(runner, &identity, pane)?;
    }
    let target = wait_target(
        pane,
        &connection.server_identity,
        &identity,
        target.starts_with("vta1:").then_some(target),
    );
    let mut history_revision = baseline.state_revision;
    let mut current = first;
    let mut initial = true;
    loop {
        if !initial
            && let Some(matched) = match_wait_event(&current, &baseline, history_revision, until)
        {
            let current_agent =
                current_agent_after_event_match(runner, env, &connection, &current, &identity);
            return success_json(
                &connection,
                &current,
                started_at,
                ApiResult::AgentWait {
                    target,
                    matched_status: matched.status,
                    match_source: AgentWaitMatchSource::TransitionEvent,
                    baseline_completed_seq: baseline.completed_seq,
                    matched_completed_seq: matched.completed_seq,
                    matched_state_revision: matched.state_revision,
                    matched_at: Some(matched.at_epoch),
                    current_agent,
                    waited_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                },
            );
        }

        let pane = require_same_agent_state(&current, &identity)?;
        let state = canonical_state(pane).expect("same agent state requires a retained record");
        if let Some(status) = match_current_wait_status(
            state,
            &baseline,
            until,
            initial,
            after_completed_seq.is_some(),
        ) {
            let current_agent = if state.agent_present {
                current_agent_after_event_match(runner, env, &connection, &current, &identity)
            } else {
                None
            };
            let matched_at = match status {
                AgentStatus::Done => state.completed_at,
                AgentStatus::Working | AgentStatus::Blocked | AgentStatus::Idle => None,
            };
            return success_json(
                &connection,
                &current,
                started_at,
                ApiResult::AgentWait {
                    target,
                    matched_status: status,
                    match_source: AgentWaitMatchSource::CurrentState,
                    baseline_completed_seq: baseline.completed_seq,
                    matched_completed_seq: state.completed_seq,
                    matched_state_revision: state.revision,
                    matched_at,
                    current_agent,
                    waited_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                },
            );
        }
        verify_wait_history_coverage(&current, &baseline, history_revision, state.revision, until)?;
        if !state.agent_present {
            return Err(api_error!(
                "stale_reference",
                format!(
                    "agent in pane {} exited before reaching {}",
                    identity.pane_instance.pane_id,
                    format_statuses(until)
                ),
            )
            .into());
        }
        reject_replaced_agent_process(pane, &identity)?;
        history_revision = state.revision;
        initial = false;
        if Instant::now() >= deadline {
            return Err(api_error!(
                "timeout",
                format!(
                    "agent {} did not reach {} within {} ms",
                    identity.pane_instance.pane_id,
                    format_statuses(until),
                    timeout.as_millis()
                ),
            )
            .into());
        }
        current = match connection.next_snapshot() {
            Ok(snapshot) => snapshot,
            Err(_) if Instant::now() >= deadline => {
                return Err(api_error!(
                    "timeout",
                    format!(
                        "agent {} did not reach {} within {} ms",
                        identity.pane_instance.pane_id,
                        format_statuses(until),
                        timeout.as_millis()
                    ),
                )
                .into());
            }
            Err(error) => return Err(error),
        };
    }
}

#[derive(Debug, Clone)]
struct WaitBaseline {
    pane_instance: PaneInstance,
    state_id: String,
    agent_epoch: u64,
    agent: String,
    state_revision: u64,
    completed_seq: u64,
    expected_completion_seq: u64,
}

impl WaitBaseline {
    fn from_pane(pane: &PanePresentation, after_completed_seq: Option<u64>) -> Result<Self> {
        let state = canonical_state(pane)
            .ok_or_else(|| api_error!("agent_not_found", "agent state is unavailable"))?;
        if after_completed_seq.is_some_and(|completed| completed > state.completed_seq) {
            return Err(api_error!(
                "invalid_arguments",
                format!(
                    "--after-completed-seq exceeds the current completed sequence {}",
                    state.completed_seq
                ),
            )
            .into());
        }
        let expected_completion_seq = match after_completed_seq {
            Some(completed) => completed
                .checked_add(1)
                .ok_or_else(|| api_error!("resource_limit", "run sequence overflow"))?,
            None if state.run_seq > state.completed_seq => state.run_seq,
            None => state
                .completed_seq
                .checked_add(1)
                .ok_or_else(|| api_error!("resource_limit", "run sequence overflow"))?,
        };
        Ok(Self {
            pane_instance: pane.pane_instance.clone(),
            state_id: state.state_id.as_str().to_string(),
            agent_epoch: state.agent_epoch,
            agent: state.agent.as_str().to_string(),
            state_revision: state.revision,
            completed_seq: after_completed_seq.unwrap_or(state.completed_seq),
            expected_completion_seq,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WaitMatch {
    status: AgentStatus,
    state_revision: u64,
    completed_seq: u64,
    at_epoch: i64,
}

fn match_wait_event(
    snapshot: &ResolvedSnapshot,
    baseline: &WaitBaseline,
    history_revision: u64,
    until: &BTreeSet<AgentStatus>,
) -> Option<WaitMatch> {
    for event in &snapshot.events {
        let Some(version) = &event.state_version else {
            continue;
        };
        if event.pane_instance != baseline.pane_instance
            || event.agent != baseline.agent
            || version.state_id.as_str() != baseline.state_id
            || version.agent_epoch != baseline.agent_epoch
            || version.revision <= history_revision
        {
            continue;
        }
        let badge_status = match event.to {
            BadgeState::Blocked => AgentStatus::Blocked,
            BadgeState::Working => AgentStatus::Working,
            BadgeState::Done => AgentStatus::Done,
            BadgeState::Idle if event.completed_seq == 0 => AgentStatus::Idle,
            BadgeState::Idle => AgentStatus::Done,
        };
        let completion_is_new = event.completed_seq >= baseline.expected_completion_seq;
        let status = if until.contains(&badge_status)
            && (badge_status != AgentStatus::Done || completion_is_new)
        {
            Some(badge_status)
        } else if completion_is_new && until.contains(&AgentStatus::Done) {
            Some(AgentStatus::Done)
        } else {
            None
        };
        if let Some(status) = status {
            return Some(WaitMatch {
                status,
                state_revision: version.revision,
                completed_seq: event.completed_seq,
                at_epoch: event.at_epoch,
            });
        }
    }
    None
}

fn match_current_wait_status(
    state: AgentStateView<'_>,
    baseline: &WaitBaseline,
    until: &BTreeSet<AgentStatus>,
    initial: bool,
    explicit_completion_baseline: bool,
) -> Option<AgentStatus> {
    if until.contains(&AgentStatus::Done) && state.completed_seq >= baseline.expected_completion_seq
    {
        return Some(AgentStatus::Done);
    }
    let current = match state.lifecycle {
        LifecycleState::Waiting { .. } | LifecycleState::Error { .. } => AgentStatus::Blocked,
        LifecycleState::Running => AgentStatus::Working,
        LifecycleState::Idle if state.completed_seq > 0 => AgentStatus::Done,
        LifecycleState::Idle => AgentStatus::Idle,
    };
    if current == AgentStatus::Done && explicit_completion_baseline {
        return None;
    }
    (initial || state.revision > baseline.state_revision)
        .then_some(current)
        .filter(|status| until.contains(status))
}

fn verify_wait_history_coverage(
    snapshot: &ResolvedSnapshot,
    baseline: &WaitBaseline,
    history_revision: u64,
    current_revision: u64,
    until: &BTreeSet<AgentStatus>,
) -> Result<()> {
    let transient_requested = until.iter().any(|status| *status != AgentStatus::Done);
    if !transient_requested {
        return Ok(());
    }
    let revisions = snapshot
        .events
        .iter()
        .filter_map(|event| {
            let version = event.state_version.as_ref()?;
            (event.pane_instance == baseline.pane_instance
                && event.agent == baseline.agent
                && version.state_id.as_str() == baseline.state_id
                && version.agent_epoch == baseline.agent_epoch
                && version.revision > history_revision
                && version.revision <= current_revision)
                .then_some(version.revision)
        })
        .collect::<BTreeSet<_>>();
    let revision_delta = current_revision.saturating_sub(history_revision);
    if (revisions.len() as u64) < revision_delta {
        return Err(api_error!(
            "event_history_lost",
            format!(
                "agent transition history no longer covers revisions {}..={}",
                history_revision.saturating_add(1),
                current_revision
            ),
        )
        .into());
    }
    Ok(())
}

struct ApiConnection {
    client: V2Client,
    socket: PathBuf,
    incarnation: crate::daemon::lifecycle::TmuxServerIncarnation,
    server_identity: String,
    daemon_instance_id: String,
    wait_deadline: Option<Instant>,
    last_snapshot_revision: Option<u64>,
}

impl ApiConnection {
    fn connect(
        runner: &dyn TmuxRunner,
        env: &BTreeMap<String, String>,
        deadline: Option<Instant>,
    ) -> Result<Self> {
        let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)
            .map_err(|error| api_error!("tmux_server_unavailable", format!("{error:#}")))?;
        let socket =
            crate::daemon::daemon_socket_path_for_incarnation(env, None, &incarnation.hash);
        let handshake_deadline = deadline
            .map(|deadline| deadline.min(Instant::now() + CLIENT_REQUEST_TIMEOUT))
            .unwrap_or_else(|| Instant::now() + CLIENT_REQUEST_TIMEOUT);
        let client =
            V2Client::connect_with_deadline(&socket, &incarnation.hash, handshake_deadline)
                .map_err(daemon_connect_error)?;
        let server_identity = client.server_identity().to_string();
        let daemon_instance_id = client.daemon_instance_id().as_str().to_string();
        Ok(Self {
            client,
            socket,
            incarnation,
            server_identity,
            daemon_instance_id,
            wait_deadline: deadline,
            last_snapshot_revision: None,
        })
    }

    fn reconnect(&self) -> Result<Self> {
        let client =
            V2Client::connect(&self.socket, &self.server_identity).map_err(daemon_connect_error)?;
        if client.daemon_instance_id().as_str() != self.daemon_instance_id {
            return Err(api_error!(
                "stale_daemon",
                "daemon instance changed during the API operation",
            )
            .into());
        }
        Ok(Self {
            daemon_instance_id: self.daemon_instance_id.clone(),
            client,
            socket: self.socket.clone(),
            incarnation: self.incarnation.clone(),
            server_identity: self.server_identity.clone(),
            wait_deadline: None,
            last_snapshot_revision: None,
        })
    }

    fn query_snapshot(&mut self) -> Result<ResolvedSnapshot> {
        self.client
            .set_deadline(Instant::now() + CLIENT_REQUEST_TIMEOUT);
        let message = self
            .client
            .request(&ClientMessage::QueryResolvedSnapshot {
                proto: PROTOCOL_VERSION,
            })
            .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
        let snapshot = snapshot_from_message(message)?;
        self.last_snapshot_revision = Some(snapshot.snapshot_revision);
        Ok(snapshot)
    }

    fn subscribe(&mut self) -> Result<ResolvedSnapshot> {
        let request_deadline = self
            .wait_deadline
            .map(|deadline| deadline.min(Instant::now() + CLIENT_REQUEST_TIMEOUT))
            .unwrap_or_else(|| Instant::now() + CLIENT_REQUEST_TIMEOUT);
        self.client.set_deadline(request_deadline);
        let message = self
            .client
            .request(&ClientMessage::Subscribe {
                proto: PROTOCOL_VERSION,
            })
            .map_err(|error| api_error!("daemon_stream_error", format!("{error:#}")))?;
        let snapshot = snapshot_from_message(message)?;
        self.last_snapshot_revision = Some(snapshot.snapshot_revision);
        if let Some(deadline) = self.wait_deadline {
            self.client.set_deadline(deadline);
        }
        Ok(snapshot)
    }

    fn next_snapshot(&mut self) -> Result<ResolvedSnapshot> {
        loop {
            match self
                .client
                .receive()
                .map_err(|error| api_error!("daemon_stream_error", format!("{error:#}")))?
            {
                ServerMessage::ResolvedSnapshotResult {
                    snapshot_revision,
                    snapshot,
                } if snapshot.snapshot_revision == snapshot_revision => {
                    if self
                        .last_snapshot_revision
                        .is_some_and(|previous| snapshot_revision <= previous)
                    {
                        return Err(api_error!(
                            "invalid_daemon_response",
                            format!(
                                "snapshot revision did not increase: previous={}, received={snapshot_revision}",
                                self.last_snapshot_revision.unwrap_or_default()
                            ),
                        )
                        .into());
                    }
                    self.last_snapshot_revision = Some(snapshot_revision);
                    return Ok(snapshot);
                }
                ServerMessage::ResolvedSnapshotResult {
                    snapshot_revision,
                    snapshot,
                } => {
                    return Err(api_error!(
                        "invalid_daemon_response",
                        format!(
                            "snapshot revision mismatch: envelope={snapshot_revision}, snapshot={}",
                            snapshot.snapshot_revision
                        ),
                    )
                    .into());
                }
                ServerMessage::Heartbeat {
                    daemon_instance_id, ..
                } if daemon_instance_id.as_str() == self.daemon_instance_id => continue,
                ServerMessage::Heartbeat { .. } => {
                    return Err(api_error!(
                        "stale_daemon",
                        "daemon instance changed while waiting",
                    )
                    .into());
                }
                ServerMessage::Error { code, message, .. } => {
                    return Err(daemon_api_error(code, message).into());
                }
                other => {
                    return Err(api_error!(
                        "invalid_daemon_response",
                        format!("unexpected daemon streaming response: {other:?}"),
                    )
                    .into());
                }
            }
        }
    }
}

fn daemon_connect_error(error: anyhow::Error) -> ApiError {
    if let Some(rejection) = error.chain().find_map(|source| {
        source.downcast_ref::<crate::daemon::protocol::v2::DaemonHandshakeError>()
    }) {
        return daemon_api_error(rejection.code.clone(), rejection.message.clone());
    }
    let code = if crate::daemon::protocol::v2::is_protocol_version_mismatch(&error) {
        ApiErrorCode::ProtocolMismatch
    } else {
        ApiErrorCode::DaemonUnavailable
    };
    ApiError::new(code, format!("{error:#}"))
}

fn snapshot_from_message(message: ServerMessage) -> Result<ResolvedSnapshot> {
    match message {
        ServerMessage::ResolvedSnapshotResult {
            snapshot_revision,
            snapshot,
        } if snapshot.snapshot_revision == snapshot_revision => Ok(snapshot),
        ServerMessage::ResolvedSnapshotResult {
            snapshot_revision,
            snapshot,
        } => Err(api_error!(
            "invalid_daemon_response",
            format!(
                "snapshot revision mismatch: envelope={snapshot_revision}, snapshot={}",
                snapshot.snapshot_revision
            ),
        )
        .into()),
        ServerMessage::Error { code, message, .. } => Err(daemon_api_error(code, message).into()),
        other => Err(api_error!(
            "invalid_daemon_response",
            format!("unexpected daemon response: {other:?}"),
        )
        .into()),
    }
}

fn daemon_api_error(code: crate::daemon::protocol::v2::ErrorCode, message: String) -> ApiError {
    use crate::daemon::protocol::v2::ErrorCode;

    let public_code = match code {
        ErrorCode::UnsupportedProtocol => ApiErrorCode::ProtocolMismatch,
        ErrorCode::NotReady => ApiErrorCode::DaemonNotReady,
        ErrorCode::InvalidRequest
        | ErrorCode::InvalidPaneInstance
        | ErrorCode::InvalidProgressOperation => ApiErrorCode::DaemonInvalidRequest,
        ErrorCode::PaneNotFound => ApiErrorCode::PaneNotFound,
        ErrorCode::StaleStateIdentity | ErrorCode::StaleSelection | ErrorCode::StaleAgentEvent => {
            ApiErrorCode::StaleReference
        }
        ErrorCode::StaleDaemonInstance => ApiErrorCode::StaleDaemon,
        ErrorCode::StateTooLarge | ErrorCode::FrameTooLarge | ErrorCode::QueueFull => {
            ApiErrorCode::ResourceLimit
        }
        ErrorCode::ControlUnavailable => ApiErrorCode::ControlUnavailable,
        ErrorCode::StateInvariantViolation
        | ErrorCode::PersistFailed
        | ErrorCode::HookCollision
        | ErrorCode::WriterLeaseHeld
        | ErrorCode::InternalError => ApiErrorCode::DaemonError,
    };
    ApiError::new(public_code, format!("{}: {message}", enum_json_name(&code)))
}

fn success_json(
    connection: &ApiConnection,
    snapshot: &ResolvedSnapshot,
    started_at: i64,
    result: ApiResult,
) -> Result<String> {
    Ok(serde_json::to_string(&ApiSuccessEnvelope {
        meta: ApiMeta {
            api_version: API_VERSION,
            server_identity: Some(connection.server_identity.clone()),
            daemon_instance_id: Some(connection.daemon_instance_id.clone()),
            snapshot_revision: Some(snapshot.snapshot_revision),
            started_at,
            emitted_at: epoch_now(),
            diagnostic_count: snapshot.diagnostics.len(),
        },
        result,
    })?)
}

fn pane_summary(pane: &PanePresentation, server_identity: &str) -> PaneSummary {
    PaneSummary {
        pane_ref: pane_ref(server_identity, &pane.pane_instance),
        pane_id: pane.pane_instance.pane_id.clone(),
        pane_pid: pane.pane_instance.pane_pid,
        sessions: session_links(pane),
        window_id: pane.window_id.clone(),
        window_name: pane.window_name.clone(),
        current_path: pane.current_path.clone(),
        current_command: pane.current_command.clone(),
        pane_width: pane.pane_width,
        active: pane.active,
        agent_ref: pane
            .resolved
            .as_ref()
            .filter(|resolved| resolved.canonical.agent_present && pane.agent_process.is_some())
            .map(|_| agent_ref(server_identity, pane)),
    }
}

#[derive(Clone, Copy)]
struct AgentStateView<'a> {
    state_id: &'a crate::pane_state::StateId,
    revision: u64,
    agent: &'a crate::pane_state::AgentKind,
    agent_process: Option<&'a crate::pane_state::AgentProcessIdentity>,
    agent_epoch: u64,
    agent_present: bool,
    lifecycle: &'a LifecycleState,
    run_seq: u64,
    completed_seq: u64,
    completed_at: Option<i64>,
}

impl<'a> From<&'a crate::pane_state::PaneState> for AgentStateView<'a> {
    fn from(state: &'a crate::pane_state::PaneState) -> Self {
        Self {
            state_id: &state.state_id,
            revision: state.revision,
            agent: &state.agent,
            agent_process: state.agent_process.as_ref(),
            agent_epoch: state.agent_epoch,
            agent_present: state.agent_present,
            lifecycle: &state.lifecycle,
            run_seq: state.run_seq,
            completed_seq: state.completed_seq,
            completed_at: state.completed_at,
        }
    }
}

impl<'a> From<&'a crate::daemon::protocol::v2::RetainedAgentState> for AgentStateView<'a> {
    fn from(state: &'a crate::daemon::protocol::v2::RetainedAgentState) -> Self {
        Self {
            state_id: &state.state_id,
            revision: state.revision,
            agent: &state.agent,
            agent_process: state.agent_process.as_ref(),
            agent_epoch: state.agent_epoch,
            agent_present: state.agent_present,
            lifecycle: &state.lifecycle,
            run_seq: state.run_seq,
            completed_seq: state.completed_seq,
            completed_at: state.completed_at,
        }
    }
}

fn canonical_state(pane: &PanePresentation) -> Option<AgentStateView<'_>> {
    if let Some(resolved) = &pane.resolved {
        Some(AgentStateView::from(&resolved.canonical))
    } else {
        pane.retained_state.as_ref().map(AgentStateView::from)
    }
}

fn pane_detail(
    pane: &PanePresentation,
    snapshot: &ResolvedSnapshot,
    server_identity: &str,
) -> PaneDetail {
    PaneDetail {
        summary: pane_summary(pane, server_identity),
        agent: agent_detail(pane, snapshot, server_identity),
    }
}

fn session_links(pane: &PanePresentation) -> Vec<SessionLink> {
    pane.session_links
        .iter()
        .map(|link| SessionLink {
            session_id: link.session_id.clone(),
            session_name: link.session_name.clone(),
            window_index: link.window_index,
            window_active: link.window_active,
            window_last: link.window_last,
        })
        .collect()
}

fn lifecycle_summary(lifecycle: &LifecycleState) -> LifecycleSummary {
    match lifecycle {
        LifecycleState::Idle => LifecycleSummary {
            state: "idle".to_string(),
            reason: None,
        },
        LifecycleState::Running => LifecycleSummary {
            state: "running".to_string(),
            reason: None,
        },
        LifecycleState::Waiting { reason } => LifecycleSummary {
            state: "waiting".to_string(),
            reason: Some(match reason {
                WaitReason::PermissionPrompt => "permission_prompt".to_string(),
                WaitReason::Other(reason) => reason.clone(),
            }),
        },
        LifecycleState::Error { reason } => LifecycleSummary {
            state: "error".to_string(),
            reason: reason.clone(),
        },
    }
}

fn agent_summary(
    pane: &PanePresentation,
    snapshot: &ResolvedSnapshot,
    server_identity: &str,
) -> Option<AgentSummary> {
    let resolved = pane.resolved.as_ref()?;
    let state = &resolved.canonical;
    if !state.agent_present {
        return None;
    }
    let exact_identity = pane.agent_process.is_some();
    Some(AgentSummary {
        agent_ref: exact_identity.then(|| agent_ref(server_identity, pane)),
        identity: if exact_identity {
            AgentIdentityStrength::Exact
        } else {
            AgentIdentityStrength::Inferred
        },
        pane_ref: pane_ref(server_identity, &pane.pane_instance),
        pane_id: pane.pane_instance.pane_id.clone(),
        pane_pid: pane.pane_instance.pane_pid,
        agent: state.agent.as_str().to_string(),
        status: agent_status(state),
        badge: AgentBadge::from(resolved.badge),
        lifecycle: lifecycle_summary(&state.lifecycle),
        sessions: session_links(pane),
        window_id: pane.window_id.clone(),
        window_name: pane.window_name.clone(),
        current_path: pane.current_path.clone(),
        active: pane.active,
        present: state.agent_present,
        unread: state.unread.is_unread(),
        needs_action: snapshot
            .sidebar_model
            .needs_action
            .contains(&pane.pane_instance),
        task_summary: state
            .task_context
            .summary
            .as_ref()
            .and_then(|summary| summary.text.clone()),
        latest_response: state
            .latest_response
            .as_ref()
            .map(|response| response.text.clone()),
        completed_at: state.completed_at,
    })
}

fn agent_status(state: &crate::pane_state::PaneState) -> AgentStatus {
    match state.lifecycle {
        LifecycleState::Waiting { .. } | LifecycleState::Error { .. } => AgentStatus::Blocked,
        LifecycleState::Running => AgentStatus::Working,
        LifecycleState::Idle if state.completed_seq > 0 => AgentStatus::Done,
        LifecycleState::Idle => AgentStatus::Idle,
    }
}

fn agent_detail(
    pane: &PanePresentation,
    snapshot: &ResolvedSnapshot,
    server_identity: &str,
) -> Option<AgentDetail> {
    let resolved = pane.resolved.as_ref()?;
    let state = &resolved.canonical;
    Some(AgentDetail {
        summary: agent_summary(pane, snapshot, server_identity)?,
        state_id: state.state_id.as_str().to_string(),
        state_revision: state.revision,
        agent_epoch: state.agent_epoch,
        agent_session_id: state
            .agent_session_id
            .as_ref()
            .map(|session| session.as_str().to_string()),
        run_seq: state.run_seq,
        completed_seq: state.completed_seq,
        started_at: state.started_at,
        prompt: state.prompt.as_ref().map(|prompt| prompt.text.clone()),
        prompt_digest: state
            .prompt
            .as_ref()
            .and_then(|prompt| prompt.digest.clone()),
        task_progress_done: state.tasks.progress.done,
        task_progress_total: state.tasks.progress.total,
        subagent_count: state.subagents.len(),
        listening_ports: state.listening_ports.clone(),
    })
}

fn resolve_pane<'a>(
    snapshot: &'a ResolvedSnapshot,
    target: &str,
    server_identity: &str,
) -> Result<&'a PanePresentation> {
    if target.starts_with("vtp1:") {
        let expected = parse_pane_ref(target, server_identity)?;
        return require_same_pane(snapshot, &expected);
    }
    validate_pane_id(target)?;
    snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_instance.pane_id == target)
        .ok_or_else(|| api_error!("pane_not_found", format!("pane {target} was not found")).into())
}

fn resolve_agent<'a>(
    snapshot: &'a ResolvedSnapshot,
    target: &str,
    server_identity: &str,
) -> Result<&'a PanePresentation> {
    if target.starts_with("vta1:") {
        let identity = parse_agent_ref(target, server_identity)?;
        return require_same_agent(snapshot, &identity);
    }
    let pane = resolve_pane(snapshot, target, server_identity)?;
    if pane
        .resolved
        .as_ref()
        .is_none_or(|resolved| !resolved.canonical.agent_present)
    {
        return Err(api_error!(
            "agent_not_found",
            format!("pane {} has no present agent", pane.pane_instance.pane_id),
        )
        .into());
    }
    Ok(pane)
}

fn resolve_wait_resume_agent<'a>(
    snapshot: &'a ResolvedSnapshot,
    target: &str,
    server_identity: &str,
) -> Result<(&'a PanePresentation, AgentIdentity)> {
    let mut identity = parse_agent_ref(target, server_identity)?;
    let pane = require_same_pane(snapshot, &identity.pane_instance)?;
    let state = canonical_state(pane).ok_or_else(|| {
        api_error!(
            "stale_reference",
            format!(
                "agent state in pane {} is no longer retained",
                identity.pane_instance.pane_id
            ),
        )
    })?;
    if state.state_id.as_str() != identity.state_id || state.agent_epoch != identity.agent_epoch {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent in pane {} was replaced",
                identity.pane_instance.pane_id
            ),
        )
        .into());
    }
    let persisted_process = state.agent_process.map(agent_process_ref);
    if persisted_process.as_ref() != Some(&identity.agent_process) {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent process baseline in pane {} does not match the reference",
                identity.pane_instance.pane_id
            ),
        )
        .into());
    }
    identity.agent = state.agent.as_str().to_string();
    Ok((pane, identity))
}

fn require_same_pane<'a>(
    snapshot: &'a ResolvedSnapshot,
    expected: &PaneInstance,
) -> Result<&'a PanePresentation> {
    let Some(pane) = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_instance.pane_id == expected.pane_id)
    else {
        return Err(api_error!(
            "stale_reference",
            format!("pane {} no longer exists", expected.pane_id),
        )
        .into());
    };
    if pane.pane_instance != *expected {
        return Err(api_error!(
            "stale_reference",
            format!("pane {} was replaced by another process", expected.pane_id),
        )
        .into());
    }
    Ok(pane)
}

#[derive(Debug, Clone)]
struct AgentIdentity {
    pane_instance: PaneInstance,
    state_id: String,
    agent_epoch: u64,
    agent: String,
    agent_process: AgentProcessRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentProcessRef {
    pid: u32,
    start_token_hash: String,
}

impl AgentIdentity {
    fn from_pane(pane: &PanePresentation) -> Result<Self> {
        let resolved = pane.resolved.as_ref().ok_or_else(|| {
            api_error!(
                "agent_not_found",
                format!("pane {} has no resolved agent", pane.pane_instance.pane_id),
            )
        })?;
        if !resolved.canonical.agent_present {
            return Err(api_error!(
                "stale_reference",
                format!(
                    "agent in pane {} is no longer present",
                    pane.pane_instance.pane_id
                ),
            )
            .into());
        }
        let agent_process = pane
            .agent_process
            .as_ref()
            .map(agent_process_ref)
            .ok_or_else(|| {
                api_error!(
                    "exact_identity_unavailable",
                    format!(
                        "agent in pane {} has no unique live process identity; exact read/wait is unavailable",
                        pane.pane_instance.pane_id
                    ),
                )
            })?;
        Ok(Self {
            pane_instance: pane.pane_instance.clone(),
            state_id: resolved.canonical.state_id.as_str().to_string(),
            agent_epoch: resolved.canonical.agent_epoch,
            agent: resolved.canonical.agent.as_str().to_string(),
            agent_process,
        })
    }
}

fn wait_target(
    pane: &PanePresentation,
    server_identity: &str,
    identity: &AgentIdentity,
    agent_ref_override: Option<&str>,
) -> AgentWaitTarget {
    AgentWaitTarget {
        agent_ref: agent_ref_override
            .map(str::to_string)
            .unwrap_or_else(|| agent_ref(server_identity, pane)),
        pane_ref: pane_ref(server_identity, &identity.pane_instance),
        pane_id: identity.pane_instance.pane_id.clone(),
        pane_pid: identity.pane_instance.pane_pid,
        agent: identity.agent.clone(),
        state_id: identity.state_id.clone(),
        agent_epoch: identity.agent_epoch,
        process_pid: identity.agent_process.pid,
    }
}

fn require_same_agent<'a>(
    snapshot: &'a ResolvedSnapshot,
    expected: &AgentIdentity,
) -> Result<&'a PanePresentation> {
    let pane = require_same_pane(snapshot, &expected.pane_instance)?;
    let Some(resolved) = pane.resolved.as_ref() else {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent in pane {} is no longer resolved",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    };
    if resolved.canonical.state_id.as_str() != expected.state_id
        || resolved.canonical.agent_epoch != expected.agent_epoch
        || !resolved.canonical.agent_present
    {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    }
    match pane.agent_process.as_ref().map(agent_process_ref) {
        None => Err(api_error!(
            "exact_identity_unavailable",
            format!(
                "agent process in pane {} is not currently uniquely verifiable",
                expected.pane_instance.pane_id
            ),
        )
        .into()),
        Some(actual) if actual != expected.agent_process => Err(api_error!(
            "stale_reference",
            format!(
                "agent process in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into()),
        Some(_) => Ok(pane),
    }
}

fn require_same_agent_state<'a>(
    snapshot: &'a ResolvedSnapshot,
    expected: &AgentIdentity,
) -> Result<&'a PanePresentation> {
    let pane = require_same_pane(snapshot, &expected.pane_instance)?;
    let Some(state) = canonical_state(pane) else {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent in pane {} is no longer resolved",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    };
    if state.state_id.as_str() != expected.state_id
        || state.agent_epoch != expected.agent_epoch
        || state.agent.as_str() != expected.agent
    {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    }
    Ok(pane)
}

fn require_same_agent_process(pane: &PanePresentation, expected: &AgentIdentity) -> Result<()> {
    let actual = pane.agent_process.as_ref().map(agent_process_ref);
    match actual.as_ref() {
        Some(actual) if actual == &expected.agent_process => Ok(()),
        None => Err(api_error!(
            "identity_verification_failed",
            format!(
                "agent process in pane {} is no longer uniquely verifiable",
                expected.pane_instance.pane_id
            ),
        )
        .into()),
        Some(_) => Err(api_error!(
            "stale_reference",
            format!(
                "agent process in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into()),
    }
}

fn reject_replaced_agent_process(pane: &PanePresentation, expected: &AgentIdentity) -> Result<()> {
    let Some(actual) = pane.agent_process.as_ref().map(agent_process_ref) else {
        return Ok(());
    };
    if actual != expected.agent_process {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent process in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    }
    Ok(())
}

fn current_agent_after_event_match(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    connection: &ApiConnection,
    snapshot: &ResolvedSnapshot,
    expected: &AgentIdentity,
) -> Option<AgentSummary> {
    let pane = require_same_agent_state(snapshot, expected).ok()?;
    let state = canonical_state(pane)?;
    if !state.agent_present {
        return None;
    }
    require_same_agent_process(pane, expected).ok()?;
    verify_live_pane(runner, env, connection, &expected.pane_instance).ok()?;
    verify_live_agent_process(runner, expected, pane).ok()?;
    agent_summary(pane, snapshot, &connection.server_identity)
}

fn verify_live_agent_process(
    runner: &dyn TmuxRunner,
    expected: &AgentIdentity,
    pane: &PanePresentation,
) -> Result<()> {
    let actual = resolve_live_agent_process(runner, expected, pane)?;
    if actual.as_ref() != Some(&expected.agent_process) {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent process in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    }
    Ok(())
}

fn reject_live_agent_process_replacement(
    runner: &dyn TmuxRunner,
    expected: &AgentIdentity,
    pane: &PanePresentation,
) -> Result<()> {
    if resolve_live_agent_process(runner, expected, pane)?
        .is_some_and(|actual| actual != expected.agent_process)
    {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent process in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    }
    Ok(())
}

fn resolve_live_agent_process(
    runner: &dyn TmuxRunner,
    expected: &AgentIdentity,
    pane: &PanePresentation,
) -> Result<Option<AgentProcessRef>> {
    let state = &pane
        .resolved
        .as_ref()
        .ok_or_else(|| api_error!("stale_reference", "agent state disappeared"))?
        .canonical;
    let actual = runner
        .resolve_agent_process(expected.pane_instance.pane_pid, &state.agent)
        .map_err(|error| {
            api_error!(
                "identity_verification_failed",
                format!(
                    "could not verify the live agent process in pane {}: {error}",
                    expected.pane_instance.pane_id
                ),
            )
        })?
        .as_ref()
        .map(agent_process_ref);
    Ok(actual)
}

fn pane_ref(server_identity: &str, pane: &PaneInstance) -> String {
    format!(
        "vtp1:{server_identity}:{}:{}",
        pane.pane_id.trim_start_matches('%'),
        pane.pane_pid
    )
}

fn agent_ref(server_identity: &str, pane: &PanePresentation) -> String {
    let state = &pane
        .resolved
        .as_ref()
        .expect("agent_ref requires resolved agent")
        .canonical;
    let process = pane
        .agent_process
        .as_ref()
        .map(agent_process_ref)
        .expect("agent_ref requires exact agent process identity");
    format!(
        "vta1:{server_identity}:{}:{}:{}:{}:{}:{}",
        pane.pane_instance.pane_id.trim_start_matches('%'),
        pane.pane_instance.pane_pid,
        state.state_id.as_str(),
        state.agent_epoch,
        process.pid,
        process.start_token_hash,
    )
}

fn agent_process_ref(identity: &crate::pane_state::AgentProcessIdentity) -> AgentProcessRef {
    AgentProcessRef {
        pid: identity.pid,
        start_token_hash: format!("{:x}", Sha256::digest(identity.start_token.as_bytes())),
    }
}

fn parse_pane_ref(value: &str, server_identity: &str) -> Result<PaneInstance> {
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() != 4 || parts[0] != "vtp1" {
        return Err(api_error!("invalid_reference", "invalid pane_ref").into());
    }
    if parts[1] != server_identity {
        return Err(
            api_error!("stale_reference", "pane_ref belongs to another tmux server").into(),
        );
    }
    let pane = PaneInstance {
        pane_id: format!("%{}", parts[2]),
        pane_pid: parts[3]
            .parse()
            .map_err(|_| api_error!("invalid_reference", "invalid pane_ref pane PID"))?,
    };
    pane.validate()
        .map_err(|error| api_error!("invalid_reference", error.to_string()))?;
    Ok(pane)
}

fn parse_agent_ref(value: &str, server_identity: &str) -> Result<AgentIdentity> {
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() != 8 || parts[0] != "vta1" {
        return Err(api_error!("invalid_reference", "invalid agent_ref").into());
    }
    if parts[1] != server_identity {
        return Err(api_error!(
            "stale_reference",
            "agent_ref belongs to another tmux server",
        )
        .into());
    }
    let pane_instance = PaneInstance {
        pane_id: format!("%{}", parts[2]),
        pane_pid: parts[3]
            .parse()
            .map_err(|_| api_error!("invalid_reference", "invalid agent_ref pane PID"))?,
    };
    pane_instance
        .validate()
        .map_err(|error| api_error!("invalid_reference", error.to_string()))?;
    crate::pane_state::StateId::parse(parts[4])
        .map_err(|error| api_error!("invalid_reference", error.to_string()))?;
    let agent_epoch = parts[5]
        .parse::<u64>()
        .ok()
        .filter(|epoch| *epoch > 0)
        .ok_or_else(|| api_error!("invalid_reference", "invalid agent_ref epoch"))?;
    let agent_pid = parts[6]
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| api_error!("invalid_reference", "invalid agent_ref process PID"))?;
    if parts[7].len() != 64
        || !parts[7]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(
            api_error!("invalid_reference", "invalid agent_ref process start token").into(),
        );
    }
    Ok(AgentIdentity {
        pane_instance,
        state_id: parts[4].to_string(),
        agent_epoch,
        agent: String::new(),
        agent_process: AgentProcessRef {
            pid: agent_pid,
            start_token_hash: parts[7].to_string(),
        },
    })
}

fn validate_pane_id(value: &str) -> Result<()> {
    let valid = value.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    if valid {
        Ok(())
    } else {
        Err(api_error!(
            "invalid_target",
            "target must be a %pane_id, pane_ref, or agent_ref",
        )
        .into())
    }
}

fn matches_agent_filter(agent: &AgentSummary, filter: &AgentListFilter) -> bool {
    filter.session.as_ref().is_none_or(|session| {
        agent
            .sessions
            .iter()
            .any(|link| link.session_id == *session || link.session_name == *session)
    }) && filter
        .agent
        .as_ref()
        .is_none_or(|kind| agent.agent == *kind)
        && filter.status.is_none_or(|status| agent.status == status)
        && filter.cwd_prefix.as_ref().is_none_or(|prefix| {
            std::path::Path::new(&agent.current_path).starts_with(std::path::Path::new(prefix))
        })
        && (!filter.unread_only || agent.unread)
        && (!filter.needs_action_only || agent.needs_action)
}

fn validate_read_options(options: ReadOptions) -> Result<()> {
    if options.lines == 0 || options.lines > MAX_READ_LINES {
        return Err(api_error!(
            "invalid_arguments",
            format!("--lines must be between 1 and {MAX_READ_LINES}"),
        )
        .into());
    }
    Ok(())
}

fn capture_pane(
    runner: &dyn TmuxRunner,
    pane_id: &str,
    options: ReadOptions,
) -> Result<ReadResult> {
    let flag = if options.ansi { "-epJ" } else { "-pJ" };
    let start = format!("-{}", options.lines);
    let mut owned = vec![
        "capture-pane".to_string(),
        flag.to_string(),
        "-t".to_string(),
        pane_id.to_string(),
    ];
    if matches!(options.source, ReadSource::Latest) {
        owned.push("-S".to_string());
        owned.push(start);
    }
    let args = owned.iter().map(String::as_str).collect::<Vec<_>>();
    let output = runner
        .run_tail_bounded(&args, MAX_READ_BYTES)
        .map_err(|error| api_error!("capture_failed", format!("{error:#}")))?;
    let text = tail_lines(&output.text, options.lines);
    Ok(ReadResult {
        source: options.source.as_str().to_string(),
        ansi: options.ansi,
        lines_requested: options.lines,
        bytes_captured: output.total_bytes,
        bytes_returned: text.len(),
        truncated: output.truncated,
        text,
    })
}

fn capture_pane_guarded(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    connection: &ApiConnection,
    expected: &PaneInstance,
    options: ReadOptions,
) -> Result<ReadResult> {
    verify_live_pane(runner, env, connection, expected)?;
    let read = capture_pane(runner, &expected.pane_id, options)?;
    verify_live_pane(runner, env, connection, expected)?;
    Ok(read)
}

fn verify_live_pane(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    connection: &ApiConnection,
    expected: &PaneInstance,
) -> Result<()> {
    connection
        .incarnation
        .verify(runner, env)
        .map_err(|error| api_error!("stale_reference", format!("{error:#}")))?;
    require_live_pane_instance(runner, expected)
}

fn require_live_pane_instance(runner: &dyn TmuxRunner, expected: &PaneInstance) -> Result<()> {
    let output = runner
        .run(&[
            "display-message",
            "-p",
            "-t",
            &expected.pane_id,
            "#{pane_id}\t#{pane_pid}",
        ])
        .map_err(|error| {
            api_error!(
                "stale_reference",
                format!(
                    "failed to resolve live pane {}: {error:#}",
                    expected.pane_id
                ),
            )
        })?;
    let mut fields = output.trim_end().split('\t');
    let actual = PaneInstance {
        pane_id: fields.next().unwrap_or_default().to_string(),
        pane_pid: fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or_default(),
    };
    if fields.next().is_some() || actual.validate().is_err() || actual != *expected {
        return Err(api_error!(
            "stale_reference",
            format!(
                "pane {} was replaced before or during capture",
                expected.pane_id
            ),
        )
        .into());
    }
    Ok(())
}

fn tail_lines(text: &str, lines: usize) -> String {
    let mut end = text.len();
    if text.as_bytes().last() == Some(&b'\n') {
        end = end.saturating_sub(1);
    }
    let Some(start) = text.as_bytes()[..end]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .and_then(|last| {
            let mut position = last;
            for _ in 1..lines {
                let previous = text.as_bytes()[..position]
                    .iter()
                    .rposition(|byte| *byte == b'\n')?;
                position = previous;
            }
            Some(position + 1)
        })
    else {
        return text.to_string();
    };
    text[start..].to_string()
}

fn aggregate_diagnostics(diagnostics: &[DaemonDiagnostic]) -> Vec<DiagnosticSummary> {
    let mut grouped = BTreeMap::<(String, String), (usize, BTreeSet<String>)>::new();
    for diagnostic in diagnostics {
        let entry = grouped
            .entry((enum_json_name(&diagnostic.code), diagnostic.message.clone()))
            .or_default();
        entry.0 += 1;
        if let Some(pane) = &diagnostic.pane_instance {
            entry.1.insert(pane.pane_id.clone());
        }
    }
    grouped
        .into_iter()
        .map(|((code, message), (count, pane_ids))| DiagnosticSummary {
            code,
            message,
            count,
            pane_ids: pane_ids.into_iter().collect(),
        })
        .collect()
}

fn enum_json_name(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_statuses(statuses: &BTreeSet<AgentStatus>) -> String {
    statuses
        .iter()
        .map(|status| status.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn epoch_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent_pane() -> PanePresentation {
        let pane_instance = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        PanePresentation {
            pane_instance: pane_instance.clone(),
            session_links: Vec::new(),
            window_id: "@1".to_string(),
            window_name: "main".to_string(),
            current_path: "/tmp".to_string(),
            current_command: "codex".to_string(),
            pane_width: 80,
            active: true,
            agent_process: Some(crate::pane_state::AgentProcessIdentity {
                pid: 9001,
                start_token: "test-process-start".to_string(),
            }),
            stored: None,
            resolved: Some(crate::pane_state::ResolvedPaneState {
                canonical: crate::pane_state::PaneState {
                    schema_version: crate::pane_state::PANE_STATE_SCHEMA_VERSION,
                    state_id: crate::pane_state::StateId::parse("00112233445566778899aabbccddeeff")
                        .unwrap(),
                    revision: 1,
                    pane_instance,
                    agent: crate::pane_state::AgentKind::parse("codex").unwrap(),
                    agent_session_id: Some(
                        crate::pane_state::AgentSessionId::parse("session-1").unwrap(),
                    ),
                    agent_process: Some(crate::pane_state::AgentProcessIdentity {
                        pid: 9001,
                        start_token: "test-process-start".to_string(),
                    }),
                    agent_epoch: 1,
                    agent_present: true,
                    scan_verified: true,
                    synthetic_completion_armed: false,
                    lifecycle: LifecycleState::Running,
                    run_seq: 1,
                    completed_seq: 0,
                    unread: crate::pane_state::UnreadState::default(),
                    started_at: Some(1),
                    completed_at: None,
                    prompt: None,
                    latest_response: None,
                    task_context: crate::pane_state::TaskContextState::default(),
                    tasks: crate::pane_state::TaskState::default(),
                    subagents: Vec::new(),
                    worktree_activity: None,
                    background_process: None,
                    listening_ports: Vec::new(),
                },
                window_id: "@1".to_string(),
                pane_id: "%1".to_string(),
                current_path: "/tmp".to_string(),
                badge: BadgeState::Working,
            }),
            retained_state: None,
        }
    }

    fn test_snapshot(pane: PanePresentation) -> ResolvedSnapshot {
        ResolvedSnapshot {
            snapshot_revision: 1,
            panes: vec![pane],
            sidebar_model: crate::daemon::SidebarModel::default(),
            attention: Vec::new(),
            events: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn test_incarnation() -> crate::daemon::lifecycle::TmuxServerIncarnation {
        crate::daemon::lifecycle::TmuxServerIncarnation {
            socket_path: PathBuf::from("/tmp/test-tmux.sock"),
            identity: crate::daemon::topology::ServerIdentity {
                pid: 77,
                start_time: 88,
            },
            hash: "server-hash".to_string(),
        }
    }

    fn test_prompt_receipt() -> AgentPromptReceipt {
        AgentPromptReceipt {
            target: AgentWaitTarget {
                agent_ref: "vta1:test".to_string(),
                pane_ref: "vtp1:test".to_string(),
                pane_id: "%1".to_string(),
                pane_pid: 101,
                agent: "codex".to_string(),
                state_id: "00112233445566778899aabbccddeeff".to_string(),
                agent_epoch: 1,
                process_pid: 9001,
            },
            prompt_digest: crate::pane_state::PromptState::digest_decoded_prompt("review\nthis"),
            baseline_run_seq: 1,
            baseline_completed_seq: 1,
            expected_run_seq: 2,
        }
    }

    #[test]
    fn prompt_input_contract_accepts_lf_and_rejects_unsafe_controls() {
        validate_prompt("review\nthis").unwrap();
        validate_prompt(&"x".repeat(MAX_PROMPT_BYTES)).unwrap();

        for invalid in [
            "",
            "tab\there",
            "cr\rhere",
            "nul\0here",
            "esc\u{1b}here",
            "c1\u{85}here",
        ] {
            assert!(validate_prompt(invalid).is_err(), "{invalid:?}");
        }
        assert!(validate_prompt(&"x".repeat(MAX_PROMPT_BYTES + 1)).is_err());
    }

    #[test]
    fn guarded_prompt_command_has_server_and_pane_fences_and_one_submit_queue() {
        let nonce = "a".repeat(64);
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let command = build_guarded_prompt_command(&test_incarnation(), &pane, &nonce);
        assert_eq!(
            &command.args[..5],
            ["load-buffer", "-b", &command.buffer, "-", ";"]
        );
        let serialized = command.args.join(" ");
        assert!(serialized.contains("#{==:#{pid},77}"));
        assert!(serialized.contains("#{==:#{start_time},88}"));
        assert!(serialized.contains("#{==:#{pane_pid},101}"));
        let paste = serialized.find("paste-buffer").unwrap();
        let raw = serialized[paste..].find("-r").unwrap() + paste;
        let enter = serialized.find("send-keys").unwrap();
        let marker = serialized.find(&command.success).unwrap();
        assert!(paste < raw && raw < enter && enter < marker);
    }

    #[test]
    fn guarded_prompt_transport_keeps_prompt_out_of_argv() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let nonce = "b".repeat(64);
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let command = build_guarded_prompt_command(&test_incarnation(), &pane, &nonce);
        let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
        runner.stub(&args, &format!("{}\n", command.success));
        runner.stub(&["delete-buffer", "-b", &command.buffer], "");
        let receipt = test_prompt_receipt();

        dispatch_prompt_guarded_with_nonce(
            &runner,
            &test_incarnation(),
            &pane,
            b"review\nthis",
            &receipt,
            &nonce,
        )
        .unwrap();

        let calls = runner.input_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, b"review\nthis");
        assert!(!calls[0].0.join(" ").contains("review"));
    }

    #[test]
    fn guarded_prompt_transport_classifies_input_write_ambiguity() {
        for (stage, side_effect, retry_action, has_receipt) in [
            (
                crate::tmux::InputWriteStage::BeforeSpawn,
                ApiSideEffect::None,
                ApiRetryAction::RetrySameRequest,
                false,
            ),
            (
                crate::tmux::InputWriteStage::AfterSpawnBeforeWrite,
                ApiSideEffect::Possible,
                ApiRetryAction::InspectManually,
                true,
            ),
            (
                crate::tmux::InputWriteStage::AfterPartialWrite,
                ApiSideEffect::Possible,
                ApiRetryAction::InspectManually,
                true,
            ),
            (
                crate::tmux::InputWriteStage::AfterFullWrite,
                ApiSideEffect::Possible,
                ApiRetryAction::InspectManually,
                true,
            ),
        ] {
            let runner = crate::tmux::mock::MockTmuxRunner::new();
            let nonce = "c".repeat(64);
            let pane = PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 101,
            };
            let command = build_guarded_prompt_command(&test_incarnation(), &pane, &nonce);
            let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
            runner.stub_input_error(&args, stage, "private transport failure");
            runner.stub(&["delete-buffer", "-b", &command.buffer], "");

            let error = dispatch_prompt_guarded_with_nonce(
                &runner,
                &test_incarnation(),
                &pane,
                b"review\nthis",
                &test_prompt_receipt(),
                &nonce,
            )
            .unwrap_err();
            let api = error.downcast_ref::<ApiError>().unwrap();
            assert_eq!(api.side_effect, side_effect);
            assert_eq!(api.retry_action, retry_action);
            assert_eq!(api.receipt.is_some(), has_receipt);
            assert!(!format!("{error:#}").contains("review"));
        }
    }

    #[test]
    fn guarded_prompt_identity_mismatch_is_known_not_dispatched() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let nonce = "d".repeat(64);
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let command = build_guarded_prompt_command(&test_incarnation(), &pane, &nonce);
        let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
        runner.stub(&args, &format!("{}\n", command.pane_mismatch));
        runner.stub(&["delete-buffer", "-b", &command.buffer], "");

        let error = dispatch_prompt_guarded_with_nonce(
            &runner,
            &test_incarnation(),
            &pane,
            b"review\nthis",
            &test_prompt_receipt(),
            &nonce,
        )
        .unwrap_err();
        let api = error.downcast_ref::<ApiError>().unwrap();
        assert_eq!(api.code, ApiErrorCode::DispatchRejected);
        assert_eq!(api.side_effect, ApiSideEffect::None);
        assert_eq!(api.retry_action, ApiRetryAction::RefreshTarget);
        assert!(api.receipt.is_none());
    }

    #[test]
    fn prompt_confirmation_accepts_only_expected_run_and_digest() {
        let mut pane = test_agent_pane();
        let identity = AgentIdentity::from_pane(&pane).unwrap();
        let receipt = test_prompt_receipt();
        let state = &mut pane.resolved.as_mut().unwrap().canonical;
        state.revision = 2;
        state.run_seq = 2;
        state.prompt = Some(crate::pane_state::PromptState {
            text: "review this".to_string(),
            source: "user".to_string(),
            digest: Some(receipt.prompt_digest.clone()),
        });
        assert_eq!(
            observe_prompt_digest(&test_snapshot(pane.clone()), &identity, &receipt).unwrap(),
            Some((2, 2))
        );

        pane.resolved.as_mut().unwrap().canonical.prompt = None;
        let mut snapshot = test_snapshot(pane);
        snapshot.events.push(crate::daemon::TransitionEvent {
            pane_instance: identity.pane_instance.clone(),
            agent: identity.agent.clone(),
            state_version: Some(crate::pane_state::StateVersion {
                state_id: crate::pane_state::StateId::parse(&identity.state_id).unwrap(),
                revision: 3,
                agent_epoch: identity.agent_epoch,
            }),
            run_seq: 2,
            completed_seq: 1,
            prompt_digest: Some(receipt.prompt_digest.clone()),
            prompt_submitted: true,
            from: Some(BadgeState::Idle),
            to: BadgeState::Working,
            at_epoch: 10,
        });
        assert_eq!(
            observe_prompt_digest(&snapshot, &identity, &receipt).unwrap(),
            Some((2, 3))
        );

        snapshot.events[0].prompt_submitted = false;
        assert_eq!(
            observe_prompt_digest(&snapshot, &identity, &receipt).unwrap(),
            None
        );

        snapshot.events[0].prompt_submitted = true;
        let current = &mut snapshot.panes[0].resolved.as_mut().unwrap().canonical;
        current.prompt = Some(crate::pane_state::PromptState {
            text: "goal replaced the preview".to_string(),
            source: "goal".to_string(),
            digest: Some(crate::pane_state::PromptState::digest_decoded_prompt(
                "goal replaced the preview",
            )),
        });
        assert_eq!(
            observe_prompt_digest(&snapshot, &identity, &receipt).unwrap(),
            Some((2, 3))
        );
    }

    #[test]
    fn generated_schema_contains_success_and_error_envelopes() {
        let value: serde_json::Value = serde_json::from_str(&schema_json(123).unwrap()).unwrap();
        assert_eq!(value["meta"]["api_version"], API_VERSION);
        assert_eq!(value["meta"]["started_at"], 123);
        assert_eq!(value["result"]["type"], "schema");
        assert!(value["result"]["schemas"]["request"]["$defs"].is_object());
        assert!(value["result"]["schemas"]["success"]["$defs"].is_object());
        assert!(value["result"]["schemas"]["error"]["properties"].is_object());
        assert_eq!(
            value["result"]["schemas"]["error"]["$defs"]["ApiErrorCode"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            32
        );
        let wait_schema = value["result"]["schemas"]["request"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|schema| schema["properties"]["command"]["const"] == "agent_wait")
            .unwrap();
        assert_eq!(wait_schema["properties"]["until"]["minItems"], 1);
        assert_eq!(wait_schema["properties"]["until"]["maxItems"], 4);
    }

    #[test]
    fn error_retry_actions_distinguish_recovery_strategies() {
        assert_eq!(
            api_error!("exact_identity_unavailable", "wait").retry_action,
            ApiRetryAction::WaitThenRetry
        );
        assert_eq!(
            api_error!("event_history_lost", "restart").retry_action,
            ApiRetryAction::RestartObservation
        );
        assert_eq!(
            api_error!("stale_reference", "refresh").retry_action,
            ApiRetryAction::RefreshTarget
        );
        assert_eq!(
            api_error!("delivery_unknown", "inspect").retry_action,
            ApiRetryAction::InspectManually
        );
        for code in [
            ApiErrorCode::AgentBusy,
            ApiErrorCode::AgentBlocked,
            ApiErrorCode::PromptConfirmationUnavailable,
            ApiErrorCode::AgentNotInputOwner,
        ] {
            assert_eq!(
                ApiError::new(code, "transient pre-dispatch condition").retry_action,
                ApiRetryAction::WaitThenRetry
            );
        }
    }

    #[test]
    fn pre_dispatch_recontext_preserves_recovery_but_reports_the_actual_stage() {
        let error = before_dispatch_error(anyhow::Error::new(api_error!(
            "stale_reference",
            "changed during fence"
        )));
        let error = error.downcast_ref::<ApiError>().unwrap();

        assert_eq!(error.stage, ApiErrorStage::BeforeDispatch);
        assert_eq!(error.side_effect, ApiSideEffect::None);
        assert_eq!(error.retry_action, ApiRetryAction::RefreshTarget);
    }

    #[test]
    fn render_error_emits_the_closed_code_and_recovery_contract() {
        let error = anyhow::Error::new(api_error!(
            "identity_verification_failed",
            "process scan unavailable"
        ));
        let value: serde_json::Value = serde_json::from_str(&render_error(&error, 123)).unwrap();

        assert_eq!(value["meta"]["api_version"], API_VERSION);
        assert_eq!(value["meta"]["started_at"], 123);
        assert_eq!(value["error"]["code"], "identity_verification_failed");
        assert_eq!(value["error"]["message"], "process scan unavailable");
        assert_eq!(value["error"]["stage"], "observation");
        assert_eq!(value["error"]["side_effect"], "none");
        assert_eq!(value["error"]["retry_action"], "wait_then_retry");
        assert!(value["error"].get("retryable").is_none());
    }

    #[test]
    fn daemon_handshake_protocol_mismatch_has_a_distinct_public_code() {
        let error = anyhow::Error::new(crate::daemon::protocol::v2::ProtocolVersionMismatch {
            requested: PROTOCOL_VERSION,
            received: Some(PROTOCOL_VERSION - 1),
            detail: "old daemon".to_string(),
        });

        assert_eq!(daemon_connect_error(error).code(), "protocol_mismatch");
    }

    #[test]
    fn daemon_handshake_overload_preserves_resource_limit_recovery() {
        let error = anyhow::Error::new(crate::daemon::protocol::v2::DaemonHandshakeError {
            code: crate::daemon::protocol::v2::ErrorCode::QueueFull,
            message: "connection capacity is full".to_string(),
        });
        let error = daemon_connect_error(error);
        assert_eq!(error.code(), "resource_limit");
        assert_eq!(error.retry_action, ApiRetryAction::WaitThenRetry);
    }

    #[test]
    fn references_pin_server_pane_process_and_agent_epoch() {
        let pane = PaneInstance {
            pane_id: "%456".to_string(),
            pane_pid: 1234,
        };
        let encoded = pane_ref("server", &pane);
        assert_eq!(encoded, "vtp1:server:456:1234");
        assert_eq!(parse_pane_ref(&encoded, "server").unwrap(), pane);
        assert_eq!(
            parse_pane_ref(&encoded, "other").unwrap_err().to_string(),
            "pane_ref belongs to another tmux server"
        );

        let agent_pane = test_agent_pane();
        let encoded = agent_ref("server", &agent_pane);
        assert!(!encoded.contains("test-process-start"));
        let identity = parse_agent_ref(&encoded, "server").unwrap();
        assert_eq!(identity.pane_instance, agent_pane.pane_instance);
        assert_eq!(identity.state_id, "00112233445566778899aabbccddeeff");
        assert_eq!(identity.agent_epoch, 1);
        assert_eq!(identity.agent_process.pid, 9001);
        assert_eq!(identity.agent_process.start_token_hash.len(), 64);
    }

    #[test]
    fn read_line_bounds_are_enforced_before_capture() {
        assert!(
            validate_read_options(ReadOptions {
                source: ReadSource::Latest,
                lines: 0,
                ansi: false,
            })
            .is_err()
        );
        assert!(
            validate_read_options(ReadOptions {
                source: ReadSource::Latest,
                lines: MAX_READ_LINES + 1,
                ansi: false,
            })
            .is_err()
        );
    }

    #[test]
    fn latest_capture_includes_visible_rows_and_returns_the_requested_tail() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub(
            &["capture-pane", "-pJ", "-t", "%1", "-S", "-2"],
            "old\none\ntwo\n",
        );

        let read = capture_pane(
            &runner,
            "%1",
            ReadOptions {
                source: ReadSource::Latest,
                lines: 2,
                ansi: false,
            },
        )
        .unwrap();

        assert_eq!(read.text, "one\ntwo\n");
        assert!(!read.truncated);
    }

    #[test]
    fn visible_capture_honors_the_requested_line_count() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub(&["capture-pane", "-pJ", "-t", "%1"], "one\ntwo\nthree\n");

        let read = capture_pane(
            &runner,
            "%1",
            ReadOptions {
                source: ReadSource::Visible,
                lines: 1,
                ansi: false,
            },
        )
        .unwrap();

        assert_eq!(read.text, "three\n");
        assert_eq!(read.lines_requested, 1);
        assert_eq!(read.bytes_returned, 6);
    }

    #[test]
    fn live_pane_guard_rejects_a_replaced_process() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let expected = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        runner.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "%1",
                "#{pane_id}\t#{pane_pid}",
            ],
            "%1\t202\n",
        );

        let error = require_live_pane_instance(&runner, &expected).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "stale_reference"
        );
    }

    #[test]
    fn acknowledged_completion_is_done_but_never_matches_idle() {
        let mut pane = test_agent_pane();
        let resolved = pane.resolved.as_mut().unwrap();
        resolved.canonical.lifecycle = LifecycleState::Idle;
        resolved.canonical.completed_seq = 1;
        resolved.canonical.completed_at = Some(2);
        resolved.badge = BadgeState::Idle;
        let baseline = WaitBaseline::from_pane(&pane, None).unwrap();
        let idle = [AgentStatus::Idle].into_iter().collect();
        let done = [AgentStatus::Done].into_iter().collect();
        let state = &pane.resolved.as_ref().unwrap().canonical;

        assert_eq!(agent_status(state), AgentStatus::Done);
        assert_eq!(
            match_current_wait_status(AgentStateView::from(state), &baseline, &idle, true, false,),
            None
        );
        assert_eq!(
            match_current_wait_status(AgentStateView::from(state), &baseline, &done, true, false,),
            Some(AgentStatus::Done)
        );
    }

    #[test]
    fn explicit_completion_baseline_can_match_an_existing_newer_completion() {
        let mut pane = test_agent_pane();
        let state = &mut pane.resolved.as_mut().unwrap().canonical;
        state.lifecycle = LifecycleState::Idle;
        state.completed_seq = 1;
        state.completed_at = Some(2);
        let baseline = WaitBaseline::from_pane(&pane, Some(0)).unwrap();
        let until = [AgentStatus::Done].into_iter().collect();
        let state = &pane.resolved.as_ref().unwrap().canonical;

        assert_eq!(
            match_current_wait_status(AgentStateView::from(state), &baseline, &until, true, true,),
            Some(AgentStatus::Done)
        );
    }

    #[test]
    fn wait_recovers_a_completion_coalesced_into_the_next_working_snapshot() {
        let first = test_agent_pane();
        let baseline = WaitBaseline::from_pane(&first, None).unwrap();
        let mut current = first.clone();
        let state = &mut current.resolved.as_mut().unwrap().canonical;
        state.revision = 3;
        state.run_seq = 2;
        state.completed_seq = 1;
        state.completed_at = Some(2);
        let mut completion_version = state.version();
        completion_version.revision = 2;
        let mut snapshot = test_snapshot(current.clone());
        snapshot.snapshot_revision = 3;
        snapshot.events.push(crate::daemon::TransitionEvent {
            pane_instance: current.pane_instance.clone(),
            agent: "codex".to_string(),
            state_version: Some(completion_version),
            run_seq: 1,
            completed_seq: 1,
            prompt_digest: None,
            prompt_submitted: false,
            from: Some(BadgeState::Working),
            to: BadgeState::Idle,
            at_epoch: 2,
        });
        snapshot.panes.clear();
        let until = [AgentStatus::Done].into_iter().collect();

        assert_eq!(
            match_wait_event(&snapshot, &baseline, baseline.state_revision, &until),
            Some(WaitMatch {
                status: AgentStatus::Done,
                state_revision: 2,
                completed_seq: 1,
                at_epoch: 2,
            })
        );
    }

    #[test]
    fn wait_recovers_a_transient_blocked_transition() {
        let first = test_agent_pane();
        let baseline = WaitBaseline::from_pane(&first, None).unwrap();
        let mut current = first.clone();
        let state = &mut current.resolved.as_mut().unwrap().canonical;
        state.revision = 3;
        let mut blocked_version = state.version();
        blocked_version.revision = 2;
        let mut snapshot = test_snapshot(current.clone());
        snapshot.snapshot_revision = 3;
        snapshot.events.push(crate::daemon::TransitionEvent {
            pane_instance: current.pane_instance.clone(),
            agent: "codex".to_string(),
            state_version: Some(blocked_version),
            run_seq: 1,
            completed_seq: 0,
            prompt_digest: None,
            prompt_submitted: false,
            from: Some(BadgeState::Working),
            to: BadgeState::Blocked,
            at_epoch: 2,
        });
        let until = [AgentStatus::Blocked].into_iter().collect();

        assert_eq!(
            match_wait_event(&snapshot, &baseline, baseline.state_revision, &until),
            Some(WaitMatch {
                status: AgentStatus::Blocked,
                state_revision: 2,
                completed_seq: 0,
                at_epoch: 2,
            })
        );
    }

    #[test]
    fn a_prior_completion_does_not_hide_a_later_blocked_transition() {
        let first = test_agent_pane();
        let baseline = WaitBaseline::from_pane(&first, None).unwrap();
        let mut current = first.clone();
        let state = &mut current.resolved.as_mut().unwrap().canonical;
        state.revision = 3;
        state.run_seq = 2;
        state.completed_seq = 1;
        let mut blocked_version = state.version();
        blocked_version.revision = 3;
        let mut snapshot = test_snapshot(current.clone());
        snapshot.events.push(crate::daemon::TransitionEvent {
            pane_instance: current.pane_instance.clone(),
            agent: "codex".to_string(),
            state_version: Some(blocked_version),
            run_seq: 2,
            completed_seq: 1,
            prompt_digest: None,
            prompt_submitted: false,
            from: Some(BadgeState::Working),
            to: BadgeState::Blocked,
            at_epoch: 3,
        });
        let until = [AgentStatus::Blocked].into_iter().collect();

        assert_eq!(
            match_wait_event(&snapshot, &baseline, baseline.state_revision, &until),
            Some(WaitMatch {
                status: AgentStatus::Blocked,
                state_revision: 3,
                completed_seq: 1,
                at_epoch: 3,
            })
        );
    }

    #[test]
    fn wait_history_coverage_advances_with_each_verified_snapshot() {
        let pane = test_agent_pane();
        let baseline = WaitBaseline::from_pane(&pane, None).unwrap();
        let until = [AgentStatus::Blocked].into_iter().collect();
        let snapshot_with_revisions = |start: u64, end: u64| {
            let mut snapshot = test_snapshot(pane.clone());
            snapshot.events = (start..=end)
                .map(|revision| crate::daemon::TransitionEvent {
                    pane_instance: pane.pane_instance.clone(),
                    agent: "codex".to_string(),
                    state_version: Some(crate::pane_state::StateVersion {
                        state_id: crate::pane_state::StateId::parse(
                            "00112233445566778899aabbccddeeff",
                        )
                        .unwrap(),
                        agent_epoch: 1,
                        revision,
                    }),
                    run_seq: 1,
                    completed_seq: 0,
                    prompt_digest: None,
                    prompt_submitted: false,
                    from: Some(BadgeState::Working),
                    to: BadgeState::Working,
                    at_epoch: revision as i64,
                })
                .collect();
            snapshot
        };

        let first = snapshot_with_revisions(2, 201);
        verify_wait_history_coverage(&first, &baseline, 1, 201, &until).unwrap();
        let second = snapshot_with_revisions(202, 401);
        verify_wait_history_coverage(&second, &baseline, 201, 401, &until).unwrap();
    }

    #[test]
    fn exact_agent_operations_reject_missing_process_identity() {
        let mut pane = test_agent_pane();
        pane.agent_process = None;

        let error = AgentIdentity::from_pane(&pane).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "exact_identity_unavailable"
        );
    }

    #[test]
    fn ongoing_wait_tolerates_unverifiable_identity_but_rejects_replacement() {
        let mut pane = test_agent_pane();
        let identity = AgentIdentity::from_pane(&pane).unwrap();
        pane.agent_process = None;
        reject_replaced_agent_process(&pane, &identity).unwrap();
        let snapshot = test_snapshot(pane.clone());
        let error = require_same_agent(&snapshot, &identity).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "exact_identity_unavailable"
        );

        pane.agent_process = Some(crate::pane_state::AgentProcessIdentity {
            pid: 9002,
            start_token: "replacement-process-start".to_string(),
        });
        let error = reject_replaced_agent_process(&pane, &identity).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "stale_reference"
        );
    }

    #[test]
    fn wait_start_allows_process_exit_but_rejects_a_live_replacement() {
        let pane = test_agent_pane();
        let identity = AgentIdentity::from_pane(&pane).unwrap();
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub_agent_process(101, "codex", None);
        reject_live_agent_process_replacement(&runner, &identity, &pane).unwrap();

        runner.stub_agent_process(
            101,
            "codex",
            Some(crate::pane_state::AgentProcessIdentity {
                pid: 9002,
                start_token: "replacement-process-start".to_string(),
            }),
        );
        let error = reject_live_agent_process_replacement(&runner, &identity, &pane).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "stale_reference"
        );
    }

    #[test]
    fn completion_cursor_resumes_an_exact_agent_that_exited_before_wait_started() {
        let mut pane = test_agent_pane();
        let reference = agent_ref("server", &pane);
        pane.agent_process = None;
        let mut state = pane.resolved.take().unwrap().canonical;
        state.agent_present = false;
        state.lifecycle = LifecycleState::Idle;
        state.completed_seq = 1;
        state.completed_at = Some(2);
        pane.retained_state = Some(crate::daemon::protocol::v2::RetainedAgentState::from(
            &state,
        ));
        let snapshot = test_snapshot(pane);

        let (pane, identity) = resolve_wait_resume_agent(&snapshot, &reference, "server").unwrap();
        assert_eq!(identity.agent, "codex");
        let baseline = WaitBaseline::from_pane(pane, Some(0)).unwrap();
        assert_eq!(baseline.completed_seq, 0);
        assert_eq!(baseline.expected_completion_seq, 1);
        let until = [AgentStatus::Done].into_iter().collect();
        assert_eq!(
            match_current_wait_status(
                canonical_state(pane).unwrap(),
                &baseline,
                &until,
                true,
                true,
            ),
            Some(AgentStatus::Done)
        );
    }

    #[test]
    fn absent_agent_records_are_not_exposed_as_current_occupants() {
        let mut pane = test_agent_pane();
        let state = &mut pane.resolved.as_mut().unwrap().canonical;
        state.agent_present = false;
        state.lifecycle = LifecycleState::Idle;
        state.completed_seq = state.run_seq;
        state.completed_at = Some(2);
        let snapshot = test_snapshot(pane.clone());

        assert!(agent_summary(&pane, &snapshot, "server").is_none());
        assert!(pane_summary(&pane, "server").agent_ref.is_none());
    }

    #[test]
    fn wait_timeout_is_bounded_before_resolving_tmux() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let until = [AgentStatus::Done].into_iter().collect();

        let error = agent_wait(
            &runner,
            &BTreeMap::new(),
            "%1",
            &until,
            MAX_WAIT_TIMEOUT + Duration::from_millis(1),
            None,
        )
        .unwrap_err();

        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "invalid_arguments"
        );
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn completion_cursor_requires_an_exact_agent_reference_before_resolving_tmux() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let until = [AgentStatus::Done].into_iter().collect();

        let error = agent_wait(
            &runner,
            &BTreeMap::new(),
            "%1",
            &until,
            Duration::from_secs(1),
            Some(0),
        )
        .unwrap_err();

        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "invalid_arguments"
        );
        assert!(runner.calls().is_empty());
    }
}
