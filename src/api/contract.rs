use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::Result;
use schemars::{JsonSchema, schema_for};
use serde::Serialize;

use crate::agent_state::{
    AgentStateUsage, ExecutionPhase, OperationRecord, RecoveryPrecondition,
    ResponseArtifactMetadata, RunRecord, SemanticOutcome,
};
use crate::daemon::protocol::v2::PROTOCOL_VERSION;
use crate::daemon::session_badge::BadgeState;

use super::common::epoch_now;

pub const API_VERSION: u16 = 4;
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
    pub(super) code: ApiErrorCode,
    pub(super) message: String,
    pub(super) stage: ApiErrorStage,
    pub(super) side_effect: ApiSideEffect,
    pub(super) retry_action: ApiRetryAction,
    pub(super) receipt: Option<OperationErrorReceipt>,
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

    pub(super) fn with_dispatch_context(
        mut self,
        stage: ApiErrorStage,
        side_effect: ApiSideEffect,
        retry_action: ApiRetryAction,
        receipt: Option<OperationErrorReceipt>,
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
    AgentLimited,
    PromptConfirmationUnavailable,
    AgentNotInputOwner,
    PromptDispatchBusy,
    DispatchRejected,
    DeliveryUnknown,
    OperationConflict,
    OperationNotFound,
    OperationStoreFull,
    OperationGenerationReplaced,
    RunNotFound,
    RunGenerationReplaced,
    RunUnresolved,
    StalePrecondition,
    RecoveryNotAllowed,
    ResolutionConflict,
    RunAlreadyResolved,
    TargetReplaced,
    UnsupportedProvider,
    ProviderEventConflict,
    StorageCapacityExceeded,
    StateUninitialized,
    ArtifactUnavailable,
    ArtifactExpired,
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
            Self::AgentLimited => "agent_limited",
            Self::PromptConfirmationUnavailable => "prompt_confirmation_unavailable",
            Self::AgentNotInputOwner => "agent_not_input_owner",
            Self::PromptDispatchBusy => "prompt_dispatch_busy",
            Self::DispatchRejected => "dispatch_rejected",
            Self::DeliveryUnknown => "delivery_unknown",
            Self::OperationConflict => "operation_conflict",
            Self::OperationNotFound => "operation_not_found",
            Self::OperationStoreFull => "operation_store_full",
            Self::OperationGenerationReplaced => "operation_generation_replaced",
            Self::RunNotFound => "run_not_found",
            Self::RunGenerationReplaced => "run_generation_replaced",
            Self::RunUnresolved => "run_unresolved",
            Self::StalePrecondition => "stale_precondition",
            Self::RecoveryNotAllowed => "recovery_not_allowed",
            Self::ResolutionConflict => "resolution_conflict",
            Self::RunAlreadyResolved => "run_already_resolved",
            Self::TargetReplaced => "target_replaced",
            Self::UnsupportedProvider => "unsupported_provider",
            Self::ProviderEventConflict => "provider_event_conflict",
            Self::StorageCapacityExceeded => "storage_capacity_exceeded",
            Self::StateUninitialized => "state_uninitialized",
            Self::ArtifactUnavailable => "artifact_unavailable",
            Self::ArtifactExpired => "artifact_expired",
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
            | Self::StaleReference
            | Self::TargetReplaced
            | Self::UnsupportedProvider => ApiErrorStage::TargetResolution,
            Self::OperationConflict | Self::ProviderEventConflict | Self::ResolutionConflict => {
                ApiErrorStage::RequestValidation
            }
            Self::AgentBusy
            | Self::AgentBlocked
            | Self::AgentLimited
            | Self::PromptConfirmationUnavailable
            | Self::AgentNotInputOwner
            | Self::PromptDispatchBusy
            | Self::OperationStoreFull => ApiErrorStage::BeforeDispatch,
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
            | Self::TargetReplaced
            | Self::DispatchRejected => ApiRetryAction::RefreshTarget,
            Self::ExactIdentityUnavailable
            | Self::DaemonNotReady
            | Self::Timeout
            | Self::IdentityVerificationFailed
            | Self::ControlUnavailable
            | Self::ResourceLimit
            | Self::AgentBusy
            | Self::AgentBlocked
            | Self::AgentLimited
            | Self::PromptConfirmationUnavailable
            | Self::AgentNotInputOwner
            | Self::PromptDispatchBusy
            | Self::OperationStoreFull
            | Self::RunUnresolved
            | Self::RecoveryNotAllowed
            | Self::StorageCapacityExceeded => ApiRetryAction::WaitThenRetry,
            Self::TmuxServerUnavailable
            | Self::DaemonUnavailable
            | Self::DaemonQueryFailed
            | Self::DaemonStreamError
            | Self::StaleDaemon
            | Self::EventHistoryLost => ApiRetryAction::RestartObservation,
            Self::DeliveryUnknown => ApiRetryAction::InspectManually,
            Self::StalePrecondition
            | Self::OperationGenerationReplaced
            | Self::RunGenerationReplaced => ApiRetryAction::RestartObservation,
            _ => ApiRetryAction::Never,
        }
    }
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
    pub receipt: Option<OperationErrorReceipt>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OperationErrorReceipt {
    pub operation_ref: String,
    #[schemars(with = "serde_json::Value")]
    pub operation: OperationRecord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunRecoveryStatus {
    Completed,
    ExactPresentStable,
    ExactAbsent,
    Replaced,
    Unstable,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RunRecoveryDiagnostic {
    pub status: RunRecoveryStatus,
    pub first_revision: u64,
    pub second_revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiSchemaContract {
    pub versions: ApiVersionContract,
    pub hard_limits: ApiHardLimitContract,
    pub providers: BTreeMap<String, ApiProviderContract>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiVersionContract {
    pub public_agent_api: u16,
    pub daemon_protocol: u16,
    pub pane_state_schema: u16,
    pub private_state_format: u16,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiHardLimitContract {
    pub pane_snapshot_max_bytes: u64,
    pub pane_projection_max_bytes: u64,
    pub historical_runs_max_per_pane: u64,
    pub run_store_max_records: u64,
    pub run_store_max_bytes: u64,
    pub run_retention_days: u64,
    pub run_record_max_bytes: u64,
    pub run_evidence_max_bytes: u64,
    pub run_event_reference_max_count: u64,
    pub operation_store_max_records: u64,
    pub operation_store_max_bytes: u64,
    pub operation_record_max_bytes: u64,
    pub prompt_body_max_bytes: u64,
    pub prompt_store_max_records: u64,
    pub prompt_store_max_bytes: u64,
    pub response_artifact_body_max_bytes: u64,
    pub artifact_store_max_files: u64,
    pub artifact_store_max_bytes: u64,
    pub concurrent_subscription_max_streams: u64,
    pub wait_timeout_min_ms: u64,
    pub wait_timeout_max_ms: u64,
    pub daemon_request_frame_max_bytes: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiProviderContract {
    pub agent_kind: String,
    pub durable_adapter_status: ApiProviderStatus,
    pub capabilities: ApiProviderCapabilities,
    pub recorded_version: String,
    pub evidence_basis: ApiProviderEvidenceBasis,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_observation_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<ApiProviderAttributionContract>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiProviderStatus {
    Enabled,
    DisabledPendingAuthenticatedP0,
    NotAvailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiProviderEvidenceBasis {
    AuthenticatedIsolatedRuntimeProbe,
    IsolatedProbeBlockedByAuthentication,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ApiProviderCapabilities {
    pub prompt_dispatch: ApiPromptDispatchCapability,
    pub steer: ApiAgentSteerCapability,
    pub prompt_confirmation: ApiPromptConfirmationCapability,
    pub response: ApiResponseCapability,
    pub interactive_keys: bool,
    pub start: ApiAgentStartCapability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiPromptDispatchCapability {
    Durable,
    GuardedTerminal,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiAgentSteerCapability {
    GuardedTerminalBestEffort,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiPromptConfirmationCapability {
    ProviderDigest,
    LifecycleCursor,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiResponseCapability {
    Artifact,
    TerminalRead,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiAgentStartCapability {
    DurableInitialPrompt,
    ProviderSession,
    InputOwnerOnly,
    Disabled,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiProviderAttributionContract {
    pub stable_event_reference: Vec<ApiProviderAttributionField>,
    pub prompt_event: ApiProviderHookEvent,
    pub completion_event: ApiProviderHookEvent,
    pub response_artifact_source: ApiResponseArtifactSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiProviderAttributionField {
    Provider,
    SessionId,
    TurnId,
    HookKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiProviderHookEvent {
    UserPromptSubmit,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiResponseArtifactSource {
    StopPayload,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiResult {
    Schema {
        schemas: BTreeMap<String, serde_json::Value>,
        contract: ApiSchemaContract,
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
    PaneSplit {
        split: PaneSplitReceipt,
    },
    AgentList {
        agents: Vec<AgentSummary>,
    },
    AgentGet {
        agent: AgentDetail,
    },
    AgentPrompt {
        operation_ref: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        run_ref: Option<String>,
        #[schemars(with = "serde_json::Value")]
        operation: OperationRecord,
        waited_ms: u64,
    },
    AgentSend {
        send: AgentTerminalSendReceipt,
    },
    AgentSteer {
        steer: AgentSteerReceipt,
    },
    AgentSendKeys {
        send: AgentKeySendReceipt,
    },
    AgentStart {
        start: AgentStartReceipt,
        agent: AgentDetail,
        waited_ms: u64,
    },
    AgentRun {
        run_ref: String,
        #[schemars(with = "serde_json::Value")]
        run: RunRecord,
        waited_ms: u64,
    },
    AgentRunCheck {
        run_ref: String,
        #[schemars(with = "serde_json::Value")]
        run: Box<RunRecord>,
        diagnostic: RunRecoveryDiagnostic,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "Option<serde_json::Value>")]
        recovery_precondition: Option<RecoveryPrecondition>,
    },
    AgentRunResolved {
        run_ref: String,
        #[schemars(with = "serde_json::Value")]
        run: RunRecord,
    },
    AgentOperation {
        operation_ref: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        run_ref: Option<String>,
        #[schemars(with = "serde_json::Value")]
        operation: OperationRecord,
        waited_ms: u64,
    },
    AgentResponse {
        run_ref: String,
        #[schemars(with = "serde_json::Value")]
        metadata: ResponseArtifactMetadata,
        encoding: String,
        body: String,
    },
    AgentStorage {
        #[schemars(with = "serde_json::Value")]
        usage: AgentStateUsage,
    },
    AgentStorageReset {
        previous_generation: String,
        generation: String,
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
    CategoryList {
        category_state_revision: u64,
        categories: Vec<ApiCategorySummary>,
    },
    CategoryGet {
        category_state_revision: u64,
        placement: ApiCategoryPlacement,
    },
    CategoryMutation {
        receipt: ApiCategoryMutationReceipt,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiCategorySource {
    Configured,
    Dynamic,
    System,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiCategorySummary {
    pub index: usize,
    pub name: String,
    pub display_name: String,
    pub source: ApiCategorySource,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiRepoSummary {
    pub key: String,
    pub rule_path: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiCategoryPlacement {
    pub repo: ApiRepoSummary,
    pub category: String,
    pub explicit: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiCategoryTarget {
    Category { category: String },
    Automatic,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiCategoryMutationReceipt {
    pub accepted_seq: u64,
    pub repo: ApiRepoSummary,
    pub requested: ApiCategoryTarget,
    pub before: ApiCategoryPlacementState,
    pub after: ApiCategoryPlacementState,
    pub changed: bool,
    pub category_state_revision: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiCategoryPlacementState {
    pub category: String,
    pub explicit: bool,
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
    Limited,
    Working,
    Done,
    Idle,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Limited => "limited",
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
    Limited,
    Working,
    Done,
    Idle,
}

impl From<BadgeState> for AgentBadge {
    fn from(value: BadgeState) -> Self {
        match value {
            BadgeState::Blocked => Self::Blocked,
            BadgeState::Limited => Self::Limited,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DurableRunStatus {
    Working,
    Blocked,
    EndedUnconfirmed,
    Done,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CurrentRunSummary {
    pub run_ref: String,
    #[schemars(with = "String")]
    pub execution_phase: ExecutionPhase,
    #[schemars(with = "String")]
    pub semantic_outcome: SemanticOutcome,
    pub status: DurableRunStatus,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_run: Option<CurrentRunSummary>,
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
    pub task_summary_status: Option<TaskSummaryStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_summary_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskSummaryStatus {
    Current,
    Failed,
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
pub enum PaneSplitDirection {
    Right,
    Down,
}

#[derive(Debug, Clone, Copy)]
pub struct PaneSplitOptions<'a> {
    pub direction: PaneSplitDirection,
    pub size_percent: Option<u8>,
    pub cwd: Option<&'a str>,
    pub focus: bool,
}

impl PaneSplitDirection {
    pub(super) fn tmux_flag(self) -> &'static str {
        match self {
            Self::Right => "-h",
            Self::Down => "-v",
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PaneSplitReceipt {
    pub target_pane_ref: String,
    pub pane_ref: String,
    pub pane_id: String,
    pub pane_pid: u32,
    pub direction: PaneSplitDirection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_percent: Option<u8>,
    pub cwd: String,
    pub focused: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentTerminalSendReceipt {
    pub target: AgentWaitTarget,
    pub prompt_digest: String,
    pub baseline_state_revision: u64,
    pub baseline_run_seq: u64,
    pub baseline_completed_seq: u64,
    pub dispatch: ApiPromptDispatchCapability,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentSteerReceipt {
    pub target: AgentWaitTarget,
    pub prompt_digest: String,
    pub baseline_state_revision: u64,
    pub baseline_run_seq: u64,
    pub baseline_completed_seq: u64,
    pub dispatch: ApiAgentSteerCapability,
    pub race_policy: ApiSteerRacePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiSteerRacePolicy {
    MayStartNextTurn,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentKeySendReceipt {
    pub target: AgentWaitTarget,
    pub keys: Vec<String>,
    pub baseline_state_revision: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentStartReceipt {
    pub target_pane_ref: String,
    pub pane_ref: String,
    pub agent_ref: String,
    pub agent: String,
    pub readiness: ApiAgentStartCapability,
    pub command_digest: String,
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
    pub(super) fn as_str(self) -> &'static str {
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

pub(super) fn default_wait_statuses() -> BTreeSet<AgentStatus> {
    [
        AgentStatus::Done,
        AgentStatus::Blocked,
        AgentStatus::Limited,
    ]
    .into_iter()
    .collect()
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunWaitUntil {
    Completed,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationWaitUntil {
    PromptConfirmed,
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
    PaneSplit {
        target: String,
        direction: PaneSplitDirection,
        #[serde(default)]
        size_percent: Option<u8>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        focus: bool,
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
        operation_id: String,
        #[serde(default = "default_prompt_confirm_timeout_ms")]
        #[schemars(range(min = 1, max = 60_000))]
        confirm_timeout_ms: u64,
    },
    AgentSend {
        target: String,
    },
    AgentSteer {
        target: String,
    },
    AgentSendKeys {
        target: String,
        keys: Vec<String>,
    },
    AgentStart {
        target: String,
        agent: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default = "default_wait_timeout_ms")]
        #[schemars(range(min = 1, max = 86_400_000))]
        timeout_ms: u64,
    },
    AgentRunGet {
        run_ref: String,
    },
    AgentRunWait {
        run_ref: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<RunWaitUntil>,
        #[serde(default = "default_wait_timeout_ms")]
        #[schemars(range(min = 1, max = 86_400_000))]
        timeout_ms: u64,
    },
    AgentRunResponse {
        run_ref: String,
    },
    AgentRunCheck {
        run_ref: String,
    },
    AgentRunResolve {
        run_ref: String,
        outcome: String,
        #[schemars(with = "serde_json::Value")]
        precondition: Box<RecoveryPrecondition>,
        resolution_id: String,
        reason: String,
    },
    AgentOperationGet {
        operation_ref: String,
    },
    AgentOperationWait {
        operation_ref: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<OperationWaitUntil>,
        #[serde(default)]
        follow_unknown: bool,
        #[serde(default = "default_wait_timeout_ms")]
        #[schemars(range(min = 1, max = 86_400_000))]
        timeout_ms: u64,
    },
    AgentStorageStatus,
    AgentWait {
        target: String,
        #[serde(default = "default_wait_statuses")]
        #[schemars(length(min = 1, max = 5))]
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
    CategoryList,
    CategoryGet {
        repo: String,
    },
    CategoryAssign {
        category: String,
        repo: String,
    },
    CategoryAutomatic {
        repo: String,
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
        result: ApiResult::Schema {
            schemas,
            contract: api_schema_contract(),
        },
    })?)
}

fn api_schema_contract() -> ApiSchemaContract {
    ApiSchemaContract {
        versions: ApiVersionContract {
            public_agent_api: API_VERSION,
            daemon_protocol: PROTOCOL_VERSION,
            pane_state_schema: crate::pane_state::PANE_STATE_SCHEMA_VERSION,
            private_state_format: crate::agent_state::PRIVATE_STATE_FORMAT_VERSION,
        },
        hard_limits: ApiHardLimitContract {
            pane_snapshot_max_bytes: crate::pane_state::MAX_RESPONSE_FRAME_BYTES as u64,
            pane_projection_max_bytes: 4 * 1024,
            historical_runs_max_per_pane: 64,
            run_store_max_records: crate::agent_state::RUN_STORE_MAX_RECORDS as u64,
            run_store_max_bytes: crate::agent_state::RUN_STORE_MAX_BYTES,
            run_retention_days: 30,
            run_record_max_bytes: crate::agent_state::RUN_RECORD_MAX_BYTES as u64,
            run_evidence_max_bytes: crate::agent_state::RUN_EVIDENCE_MAX_BYTES as u64,
            run_event_reference_max_count: crate::agent_state::RUN_EVENT_REFERENCE_MAX_COUNT as u64,
            operation_store_max_records: crate::agent_state::OPERATION_STORE_MAX_RECORDS as u64,
            operation_store_max_bytes: crate::agent_state::OPERATION_STORE_MAX_BYTES,
            operation_record_max_bytes: crate::agent_state::OPERATION_RECORD_MAX_BYTES as u64,
            prompt_body_max_bytes: crate::agent_state::PROMPT_BODY_MAX_BYTES as u64,
            prompt_store_max_records: crate::agent_state::PROMPT_STORE_MAX_RECORDS as u64,
            prompt_store_max_bytes: crate::agent_state::PROMPT_STORE_MAX_BYTES,
            response_artifact_body_max_bytes: crate::agent_state::RESPONSE_ARTIFACT_BODY_MAX_BYTES
                as u64,
            artifact_store_max_files: crate::agent_state::ARTIFACT_STORE_MAX_FILES as u64,
            artifact_store_max_bytes: crate::agent_state::ARTIFACT_STORE_MAX_BYTES,
            concurrent_subscription_max_streams: 48,
            wait_timeout_min_ms: 1,
            wait_timeout_max_ms: MAX_WAIT_TIMEOUT.as_millis() as u64,
            daemon_request_frame_max_bytes: crate::pane_state::MAX_REQUEST_FRAME_BYTES as u64,
        },
        providers: BTreeMap::from([
            (
                "codex".to_string(),
                ApiProviderContract {
                    agent_kind: "codex".to_string(),
                    durable_adapter_status: ApiProviderStatus::Enabled,
                    capabilities: ApiProviderCapabilities {
                        prompt_dispatch: ApiPromptDispatchCapability::Durable,
                        steer: ApiAgentSteerCapability::GuardedTerminalBestEffort,
                        prompt_confirmation: ApiPromptConfirmationCapability::ProviderDigest,
                        response: ApiResponseCapability::Artifact,
                        interactive_keys: true,
                        start: ApiAgentStartCapability::DurableInitialPrompt,
                    },
                    recorded_version: "0.147.0".to_string(),
                    evidence_basis: ApiProviderEvidenceBasis::AuthenticatedIsolatedRuntimeProbe,
                    observed_at: Some("2026-08-15".to_string()),
                    source_revision: Some("a4fb816a52fc4178ef3a01d285f0c6cc0191d7c0".to_string()),
                    probe_observation_count: Some(20),
                    attribution: Some(ApiProviderAttributionContract {
                        stable_event_reference: vec![
                            ApiProviderAttributionField::Provider,
                            ApiProviderAttributionField::SessionId,
                            ApiProviderAttributionField::TurnId,
                            ApiProviderAttributionField::HookKind,
                        ],
                        prompt_event: ApiProviderHookEvent::UserPromptSubmit,
                        completion_event: ApiProviderHookEvent::Stop,
                        response_artifact_source: ApiResponseArtifactSource::StopPayload,
                    }),
                },
            ),
            (
                "claude".to_string(),
                ApiProviderContract {
                    agent_kind: "claude".to_string(),
                    durable_adapter_status: ApiProviderStatus::DisabledPendingAuthenticatedP0,
                    capabilities: ApiProviderCapabilities {
                        prompt_dispatch: ApiPromptDispatchCapability::GuardedTerminal,
                        steer: ApiAgentSteerCapability::GuardedTerminalBestEffort,
                        prompt_confirmation: ApiPromptConfirmationCapability::LifecycleCursor,
                        response: ApiResponseCapability::TerminalRead,
                        interactive_keys: true,
                        start: ApiAgentStartCapability::ProviderSession,
                    },
                    recorded_version: "2.1.227".to_string(),
                    evidence_basis: ApiProviderEvidenceBasis::IsolatedProbeBlockedByAuthentication,
                    observed_at: None,
                    source_revision: None,
                    probe_observation_count: None,
                    attribution: None,
                },
            ),
            (
                "opencode".to_string(),
                ApiProviderContract {
                    agent_kind: "opencode".to_string(),
                    durable_adapter_status: ApiProviderStatus::NotAvailable,
                    capabilities: ApiProviderCapabilities {
                        prompt_dispatch: ApiPromptDispatchCapability::GuardedTerminal,
                        steer: ApiAgentSteerCapability::Disabled,
                        prompt_confirmation: ApiPromptConfirmationCapability::None,
                        response: ApiResponseCapability::TerminalRead,
                        interactive_keys: true,
                        start: ApiAgentStartCapability::InputOwnerOnly,
                    },
                    recorded_version: "unverified".to_string(),
                    evidence_basis: ApiProviderEvidenceBasis::NotApplicable,
                    observed_at: None,
                    source_revision: None,
                    probe_observation_count: None,
                    attribution: None,
                },
            ),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::agent::{
        durable::{prompt_before_dispatch_timeout, prompt_wait_timeout},
        guards::provider_capabilities,
    };

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
            51
        );
        let error_codes = value["result"]["schemas"]["error"]["$defs"]["ApiErrorCode"]["enum"]
            .as_array()
            .unwrap();
        for code in [
            "agent_limited",
            "operation_conflict",
            "operation_not_found",
            "operation_store_full",
            "operation_generation_replaced",
            "run_not_found",
            "run_generation_replaced",
            "run_unresolved",
            "run_already_resolved",
            "target_replaced",
            "unsupported_provider",
            "provider_event_conflict",
            "recovery_not_allowed",
            "stale_precondition",
            "resolution_conflict",
            "storage_capacity_exceeded",
            "state_uninitialized",
            "artifact_unavailable",
            "artifact_expired",
        ] {
            assert!(
                error_codes.contains(&serde_json::json!(code)),
                "missing {code}"
            );
        }
        let wait_schema = value["result"]["schemas"]["request"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|schema| schema["properties"]["command"]["const"] == "agent_wait")
            .unwrap();
        assert_eq!(wait_schema["properties"]["until"]["minItems"], 1);
        assert_eq!(wait_schema["properties"]["until"]["maxItems"], 5);
        assert_eq!(
            default_wait_statuses(),
            [
                AgentStatus::Blocked,
                AgentStatus::Limited,
                AgentStatus::Done,
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn generated_schema_exposes_version_limit_and_provider_contracts() {
        let value: serde_json::Value = serde_json::from_str(&schema_json(123).unwrap()).unwrap();
        let contract = &value["result"]["contract"];

        assert_eq!(contract["versions"]["public_agent_api"], API_VERSION);
        assert_eq!(contract["versions"]["daemon_protocol"], PROTOCOL_VERSION);
        assert_eq!(
            contract["versions"]["pane_state_schema"],
            crate::pane_state::PANE_STATE_SCHEMA_VERSION
        );
        assert_eq!(
            contract["versions"]["private_state_format"],
            crate::agent_state::PRIVATE_STATE_FORMAT_VERSION
        );
        assert_eq!(
            contract["hard_limits"]["operation_store_max_records"],
            crate::agent_state::OPERATION_STORE_MAX_RECORDS
        );
        assert_eq!(
            contract["hard_limits"]["response_artifact_body_max_bytes"],
            crate::agent_state::RESPONSE_ARTIFACT_BODY_MAX_BYTES
        );
        assert_eq!(
            contract["hard_limits"]["concurrent_subscription_max_streams"],
            48
        );
        assert_eq!(
            contract["providers"]["codex"]["durable_adapter_status"],
            "enabled"
        );
        assert_eq!(
            contract["providers"]["codex"]["capabilities"]["prompt_dispatch"],
            "durable"
        );
        assert_eq!(
            contract["providers"]["codex"]["capabilities"]["steer"],
            "guarded_terminal_best_effort"
        );
        assert_eq!(
            contract["providers"]["codex"]["recorded_version"],
            "0.147.0"
        );
        assert_eq!(
            contract["providers"]["codex"]["evidence_basis"],
            "authenticated_isolated_runtime_probe"
        );
        assert_eq!(
            contract["providers"]["codex"]["attribution"]["stable_event_reference"],
            serde_json::json!(["provider", "session_id", "turn_id", "hook_kind"])
        );
        assert_eq!(
            contract["providers"]["claude"]["durable_adapter_status"],
            "disabled_pending_authenticated_p0"
        );
        assert!(contract["providers"]["claude"].get("attribution").is_none());
        assert_eq!(
            contract["providers"]["claude"]["capabilities"]["prompt_dispatch"],
            "guarded_terminal"
        );
        assert_eq!(
            contract["providers"]["claude"]["capabilities"]["steer"],
            "guarded_terminal_best_effort"
        );
        assert_eq!(
            contract["providers"]["opencode"]["capabilities"]["prompt_confirmation"],
            "none"
        );
        assert_eq!(
            contract["providers"]["opencode"]["capabilities"]["steer"],
            "disabled"
        );
        for agent in ["codex", "claude", "opencode"] {
            assert_eq!(
                contract["providers"][agent]["capabilities"],
                serde_json::to_value(provider_capabilities(agent).unwrap()).unwrap(),
                "schema and runtime capabilities drifted for {agent}"
            );
        }
    }

    #[test]
    fn error_retry_actions_distinguish_recovery_strategies() {
        assert_eq!(
            ApiError::new(ApiErrorCode::ExactIdentityUnavailable, "wait").retry_action,
            ApiRetryAction::WaitThenRetry
        );
        assert_eq!(
            ApiError::new(ApiErrorCode::EventHistoryLost, "restart").retry_action,
            ApiRetryAction::RestartObservation
        );
        assert_eq!(
            ApiError::new(ApiErrorCode::StaleReference, "refresh").retry_action,
            ApiRetryAction::RefreshTarget
        );
        assert_eq!(
            ApiError::new(ApiErrorCode::DeliveryUnknown, "inspect").retry_action,
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

        let before_dispatch = prompt_before_dispatch_timeout("not written");
        assert_eq!(before_dispatch.code, ApiErrorCode::Timeout);
        assert_eq!(before_dispatch.stage, ApiErrorStage::BeforeDispatch);
        assert_eq!(before_dispatch.side_effect, ApiSideEffect::None);
        assert_eq!(
            before_dispatch.retry_action,
            ApiRetryAction::RetrySameRequest
        );

        let ambiguous = prompt_wait_timeout("written without a response");
        assert_eq!(ambiguous.code, ApiErrorCode::DeliveryUnknown);
        assert_eq!(ambiguous.stage, ApiErrorStage::AfterDispatch);
        assert_eq!(ambiguous.side_effect, ApiSideEffect::Possible);
        assert_eq!(ambiguous.retry_action, ApiRetryAction::InspectManually);
    }

    #[test]
    fn render_error_emits_the_closed_code_and_recovery_contract() {
        let error = anyhow::Error::new(ApiError::new(
            ApiErrorCode::IdentityVerificationFailed,
            "process scan unavailable",
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
}
