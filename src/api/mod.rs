use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use base64::Engine as _;
use schemars::{JsonSchema, schema_for};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::daemon::protocol::v2::{
    CLIENT_REQUEST_TIMEOUT, ClientMessage, CurrentAgentRun, DaemonDiagnostic, PROTOCOL_VERSION,
    PanePresentation, ResolvedSnapshot, ServerMessage, V2Client, V2RequestFailureStage,
};
use crate::daemon::session_badge::BadgeState;
use crate::pane_state::{LifecycleState, PaneInstance, WaitReason};
use crate::tmux::TmuxRunner;
use crate::{
    agent_state::{
        AgentBinding, AgentStateUsage, DispatchState, ExecutionPhase, OperationId, OperationRecord,
        OperationRef, RecoveryPaneFence, RecoveryPrecondition, RecoveryProcessExpectation,
        RecoveryViewportFingerprint, ResolutionId, ResponseArtifactMetadata, RunRecord, RunRef,
        SemanticOutcome, Sha256Digest, VIEWPORT_FINGERPRINT_CONVENTION_VERSION,
    },
    pane_state::EventId,
};

pub const API_VERSION: u16 = 3;
pub const DEFAULT_READ_LINES: usize = 120;
pub const MAX_READ_LINES: usize = 2_000;
pub const MAX_READ_BYTES: usize = 1024 * 1024;
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;
pub const DEFAULT_PROMPT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(7);
pub const MAX_PROMPT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const WAIT_POLL_INITIAL_INTERVAL: Duration = Duration::from_millis(50);
const WAIT_POLL_MAX_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct ApiError {
    code: ApiErrorCode,
    message: String,
    stage: ApiErrorStage,
    side_effect: ApiSideEffect,
    retry_action: ApiRetryAction,
    receipt: Option<OperationErrorReceipt>,
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
    ("operation_conflict", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::OperationConflict, $message)
    };
    ("operation_not_found", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::OperationNotFound, $message)
    };
    ("operation_store_full", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::OperationStoreFull, $message)
    };
    ("operation_generation_replaced", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::OperationGenerationReplaced, $message)
    };
    ("run_not_found", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::RunNotFound, $message)
    };
    ("run_generation_replaced", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::RunGenerationReplaced, $message)
    };
    ("run_unresolved", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::RunUnresolved, $message)
    };
    ("run_already_resolved", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::RunAlreadyResolved, $message)
    };
    ("target_replaced", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::TargetReplaced, $message)
    };
    ("unsupported_provider", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::UnsupportedProvider, $message)
    };
    ("provider_event_conflict", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::ProviderEventConflict, $message)
    };
    ("recovery_not_allowed", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::RecoveryNotAllowed, $message)
    };
    ("stale_precondition", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::StalePrecondition, $message)
    };
    ("resolution_conflict", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::ResolutionConflict, $message)
    };
    ("storage_capacity_exceeded", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::StorageCapacityExceeded, $message)
    };
    ("state_uninitialized", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::StateUninitialized, $message)
    };
    ("artifact_unavailable", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::ArtifactUnavailable, $message)
    };
    ("artifact_expired", $message:expr $(,)?) => {
        ApiError::new(ApiErrorCode::ArtifactExpired, $message)
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
    pub providers: ApiProviderCompatibilityContract,
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
pub struct ApiProviderCompatibilityContract {
    pub codex: ApiProviderContract,
    pub claude_code: ApiProviderContract,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiProviderContract {
    pub status: ApiProviderStatus,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiProviderEvidenceBasis {
    AuthenticatedIsolatedRuntimeProbe,
    IsolatedProbeBlockedByAuthentication,
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
        providers: ApiProviderCompatibilityContract {
            codex: ApiProviderContract {
                status: ApiProviderStatus::Enabled,
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
            claude_code: ApiProviderContract {
                status: ApiProviderStatus::DisabledPendingAuthenticatedP0,
                recorded_version: "2.1.227".to_string(),
                evidence_basis: ApiProviderEvidenceBasis::IsolatedProbeBlockedByAuthentication,
                observed_at: None,
                source_revision: None,
                probe_observation_count: None,
                attribution: None,
            },
        },
    }
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
    let current_runs = query_current_agent_runs(&mut connection, &snapshot)?;
    let agents = snapshot
        .panes
        .iter()
        .filter_map(|pane| {
            let mut summary = agent_summary(pane, &snapshot, &connection.server_identity)?;
            summary.current_run = current_run_for_pane(pane, &current_runs);
            Some(summary)
        })
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
    let current_runs = query_current_agent_runs(&mut connection, &snapshot)?;
    let mut agent = agent_detail(pane, &snapshot, &connection.server_identity)
        .expect("resolve_agent only returns resolved agents");
    agent.summary.current_run = current_run_for_pane(pane, &current_runs);
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
    operation_id: &str,
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
    let operation_id = OperationId::parse(operation_id.to_string()).map_err(|error| {
        api_error!(
            "invalid_arguments",
            format!("invalid --operation-id: {error}")
        )
    })?;
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
    let started = Instant::now();
    let deadline = Instant::now() + confirm_timeout;
    let prompt_digest = Sha256Digest::parse(crate::pane_state::PromptState::digest_decoded_prompt(
        prompt,
    ))
    .expect("PromptState emits a valid SHA-256 digest");
    let prompt_base64 = base64::engine::general_purpose::STANDARD.encode(prompt.as_bytes());
    let observed_at = started_at;
    let mut poll_interval = WAIT_POLL_INITIAL_INTERVAL;
    let mut request_may_have_reached_daemon = false;

    let (start_connection, operation_ref, operation) = loop {
        if Instant::now() >= deadline {
            let message = "daemon did not accept or recover the idempotent prompt operation before the deadline";
            return Err(if request_may_have_reached_daemon {
                prompt_wait_timeout(message)
            } else {
                prompt_before_dispatch_timeout(message)
            }
            .into());
        }
        let mut connection = match ApiConnection::connect(runner, env, Some(deadline)) {
            Ok(connection) => connection,
            Err(_) => {
                sleep_until_next_poll(deadline, &mut poll_interval);
                continue;
            }
        };
        let event_id = EventId::generate()
            .map_err(|error| api_error!("internal_error", format!("event ID: {error}")))?;
        let request = ClientMessage::StartAgentPrompt {
            proto: PROTOCOL_VERSION,
            daemon_instance_id: connection.client.daemon_instance_id().clone(),
            event_id,
            target_agent_ref: target.to_string(),
            operation_id: operation_id.clone(),
            prompt_base64: prompt_base64.clone(),
            prompt_digest: prompt_digest.clone(),
            dispatch_option: "paste_enter".to_string(),
            observed_at,
        };
        connection
            .client
            .set_deadline(deadline.min(Instant::now() + CLIENT_REQUEST_TIMEOUT));
        match connection.client.request_with_stage(&request) {
            Ok(ServerMessage::AgentPromptResult {
                proto,
                operation_ref,
                operation,
            }) if proto == PROTOCOL_VERSION => break (connection, operation_ref, operation),
            Ok(ServerMessage::Error { code, message, .. }) => {
                return Err(daemon_api_error(code, message).into());
            }
            Ok(other) => {
                return Err(api_error!(
                    "invalid_daemon_response",
                    format!("unexpected prompt response: {other:?}"),
                )
                .into());
            }
            Err(error) => {
                request_may_have_reached_daemon |=
                    error.stage == V2RequestFailureStage::AfterFullWrite;
                // The caller-supplied operation ID makes this exact request replay-safe.
                sleep_until_next_poll(deadline, &mut poll_interval);
            }
        }
    };

    let (connection, operation) = if operation_is_terminal(&operation) {
        (start_connection, operation)
    } else {
        let (connection, returned_ref, operation) =
            wait_for_operation(runner, env, &operation_ref, deadline, false)?;
        if returned_ref != operation_ref {
            return Err(api_error!(
                "invalid_daemon_response",
                "operation query returned a different operation_ref",
            )
            .into());
        }
        (connection, operation)
    };
    if operation.dispatch_state != DispatchState::PromptConfirmed {
        return Err(operation_terminal_error(&operation_ref, operation).into());
    }
    let run_ref = linked_run_ref(&operation_ref, &operation)?;
    success_agent_json(
        &connection,
        started_at,
        ApiResult::AgentPrompt {
            operation_ref,
            run_ref,
            operation,
            waited_ms: elapsed_millis(started),
        },
    )
}

pub fn agent_run_get(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    run_ref: &str,
) -> Result<String> {
    let (connection, returned_ref, run) = query_agent_run(runner, env, run_ref, None)?;
    success_agent_json(
        &connection,
        observed_at,
        ApiResult::AgentRun {
            run_ref: returned_ref,
            run,
            waited_ms: 0,
        },
    )
}

pub fn agent_run_wait(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    run_ref: &str,
    timeout: Duration,
    until_completed: bool,
) -> Result<String> {
    validate_agent_wait_timeout(timeout)?;
    let started = Instant::now();
    let deadline = started + timeout;
    let (connection, returned_ref, run) =
        wait_for_run(runner, env, run_ref, deadline, until_completed)?;
    success_agent_json(
        &connection,
        observed_at,
        ApiResult::AgentRun {
            run_ref: returned_ref,
            run,
            waited_ms: elapsed_millis(started),
        },
    )
}

pub fn agent_run_response(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    run_ref: &str,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    connection
        .client
        .set_deadline(Instant::now() + CLIENT_REQUEST_TIMEOUT);
    let response = connection
        .client
        .request(&ClientMessage::QueryAgentResponse {
            proto: PROTOCOL_VERSION,
            run_ref: run_ref.to_string(),
        })
        .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
    let (returned_ref, metadata, body_base64) = match response {
        ServerMessage::AgentResponseResult {
            proto,
            run_ref,
            metadata,
            body_base64,
        } if proto == PROTOCOL_VERSION => (run_ref, metadata, body_base64),
        ServerMessage::Error { code, message, .. } => {
            return Err(daemon_api_error(code, message).into());
        }
        ServerMessage::AgentResponseResult { .. } => {
            return Err(api_error!(
                "invalid_daemon_response",
                "response query returned a mismatched protocol version",
            )
            .into());
        }
        other => {
            return Err(api_error!(
                "invalid_daemon_response",
                format!("unexpected agent response result: {other:?}"),
            )
            .into());
        }
    };
    if returned_ref != run_ref {
        return Err(api_error!(
            "invalid_daemon_response",
            "response query returned a different run_ref",
        )
        .into());
    }
    metadata.validate().map_err(|error| {
        api_error!(
            "invalid_daemon_response",
            format!("invalid response metadata: {error}")
        )
    })?;
    if metadata.encoding != "utf-8" {
        return Err(api_error!(
            "invalid_daemon_response",
            format!("unsupported response encoding: {}", metadata.encoding),
        )
        .into());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body_base64.as_bytes())
        .map_err(|error| {
            api_error!(
                "invalid_daemon_response",
                format!("response body is not valid base64: {error}")
            )
        })?;
    if bytes.len() as u64 != metadata.stored_byte_count
        || metadata.stored_digest.as_ref() != Some(&Sha256Digest::of(&bytes))
    {
        return Err(api_error!(
            "invalid_daemon_response",
            "response body does not match its stored byte count and digest",
        )
        .into());
    }
    let body = String::from_utf8(bytes).map_err(|error| {
        api_error!(
            "invalid_daemon_response",
            format!("response body is not valid UTF-8: {error}")
        )
    })?;
    success_agent_json(
        &connection,
        observed_at,
        ApiResult::AgentResponse {
            run_ref: returned_ref,
            metadata,
            encoding: "utf-8".to_string(),
            body,
        },
    )
}

pub fn agent_run_check(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    run_ref: &str,
) -> Result<String> {
    agent_run_check_with_interval(runner, env, observed_at, run_ref, Duration::from_secs(2))
}

fn agent_run_check_with_interval(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    run_ref: &str,
    interval: Duration,
) -> Result<String> {
    let first = observe_run_recovery_sample(runner, env, run_ref)?;
    std::thread::sleep(interval);
    let second = observe_run_recovery_sample(runner, env, run_ref)?;
    if first.run_ref != run_ref
        || second.run_ref != run_ref
        || first.run.run_id != second.run.run_id
    {
        return Err(api_error!(
            "invalid_daemon_response",
            "run check returned a mismatched stable run identity",
        )
        .into());
    }

    let (status, message, process_expectation) = classify_run_recovery(&first, &second);
    let recovery_precondition = if let Some(process_expectation) = process_expectation {
        let issued_at = epoch_now();
        Some(RecoveryPrecondition {
            run_ref: run_ref.to_string(),
            binding: second.run.binding.clone(),
            run_revision: second.run.revision,
            evidence_digest: crate::agent_state::runtime::AgentRuntime::evidence_digest(
                &second.run,
            )?,
            pane: second.pane.clone(),
            viewport_fingerprint: second.viewport_fingerprint.clone(),
            process_expectation,
            issued_at,
            expires_at: issued_at.saturating_add(60),
        })
    } else {
        None
    };
    success_agent_json(
        &second.connection,
        observed_at,
        ApiResult::AgentRunCheck {
            run_ref: second.run_ref.clone(),
            run: Box::new(second.run.clone()),
            diagnostic: RunRecoveryDiagnostic {
                status,
                first_revision: first.run.revision,
                second_revision: second.run.revision,
                message,
            },
            recovery_precondition,
        },
    )
}

struct RunRecoveryObservation {
    connection: ApiConnection,
    run_ref: String,
    run: RunRecord,
    pane: RecoveryPaneFence,
    process: Option<crate::pane_state::AgentProcessIdentity>,
    viewport_fingerprint: Option<RecoveryViewportFingerprint>,
}

fn observe_run_recovery_sample(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    run_ref: &str,
) -> Result<RunRecoveryObservation> {
    let (connection, returned_ref, run) = query_agent_run(runner, env, run_ref, None)?;
    let mut snapshot_connection = connection.reconnect()?;
    let snapshot = snapshot_connection.query_snapshot()?;
    let pane = recovery_pane_fence(&snapshot, &run)?;
    let process = observe_run_process(runner, &run)?;
    let viewport_fingerprint = if process.as_ref() == Some(&run.binding.process) {
        verify_recovery_foreground_owner(runner, &run.binding.pane_instance, &run.binding.process)
            .map_err(|error| {
                api_error!(
                    "identity_verification_failed",
                    format!("the exact bound process is not the foreground input owner: {error}"),
                )
            })?;
        Some(capture_visible_viewport_fingerprint(
            runner,
            &run.binding.pane_instance,
        )?)
    } else {
        None
    };
    Ok(RunRecoveryObservation {
        connection,
        run_ref: returned_ref,
        run,
        pane,
        process,
        viewport_fingerprint,
    })
}

pub(crate) fn verify_recovery_foreground_owner(
    runner: &dyn TmuxRunner,
    pane: &PaneInstance,
    process: &crate::pane_state::AgentProcessIdentity,
) -> std::result::Result<(), String> {
    runner
        .verify_agent_input_owner(pane.pane_pid, process.pid)
        .map_err(|error| format!("{error:#}"))
}

fn recovery_pane_fence(snapshot: &ResolvedSnapshot, run: &RunRecord) -> Result<RecoveryPaneFence> {
    let state = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_instance == run.binding.pane_instance)
        .and_then(|pane| pane.resolved.as_ref())
        .map(|resolved| &resolved.canonical)
        .ok_or_else(|| {
            api_error!(
                "stale_precondition",
                "the pane bound to the run has no canonical state",
            )
        })?;
    if state.state_id != run.binding.pane_state_id
        || state.agent_epoch != run.binding.agent_epoch
        || state.agent != run.binding.agent_kind
        || state.agent_session_id.as_ref() != Some(&run.binding.provider_session_id)
        || state.pane_instance != run.binding.pane_instance
    {
        return Err(api_error!(
            "stale_precondition",
            "the pane no longer has the complete binding recorded by the run",
        )
        .into());
    }
    let current_run = state.current_run.clone().ok_or_else(|| {
        api_error!(
            "stale_precondition",
            "the pane no longer points at the checked durable run",
        )
    })?;
    if current_run.run_id != run.run_id.as_str()
        || current_run.run_seq != run.run_seq
        || current_run.run_revision != run.revision
    {
        return Err(
            api_error!("stale_precondition", "the pane durable-run pointer changed",).into(),
        );
    }
    if matches!(state.lifecycle, LifecycleState::Waiting { .. }) {
        return Err(api_error!(
            "recovery_not_allowed",
            "a pane waiting for permission or user input cannot be operator-completed",
        )
        .into());
    }
    if !state.subagents.is_empty() {
        return Err(api_error!(
            "recovery_not_allowed",
            "a run with active subagents cannot be operator-completed",
        )
        .into());
    }
    Ok(RecoveryPaneFence {
        state_id: state.state_id.clone(),
        revision: state.revision,
        current_run,
        lifecycle: state.lifecycle.clone(),
        subagent_count: 0,
    })
}

pub(crate) fn capture_visible_viewport_fingerprint(
    runner: &dyn TmuxRunner,
    pane: &PaneInstance,
) -> Result<RecoveryViewportFingerprint> {
    let before = recovery_viewport_dimensions(runner, pane)?;
    let output = runner
        .run_bounded(
            &["capture-pane", "-pJ", "-t", &pane.pane_id],
            MAX_READ_BYTES,
        )
        .map_err(|error| {
            api_error!(
                "capture_failed",
                format!("failed to capture the visible recovery viewport: {error:#}"),
            )
        })?;
    if output.truncated {
        return Err(api_error!(
            "resource_limit",
            "visible recovery viewport exceeded the capture limit",
        )
        .into());
    }
    let after = recovery_viewport_dimensions(runner, pane)?;
    if before != after {
        return Err(api_error!(
            "stale_precondition",
            "pane identity or dimensions changed during viewport capture",
        )
        .into());
    }
    let (pane_width, pane_height) = before;
    let mut bytes = Vec::with_capacity(output.text.len() + 32);
    bytes.extend_from_slice(b"vde-tmux-recovery-viewport-v1\0");
    bytes.extend_from_slice(&pane_width.to_be_bytes());
    bytes.extend_from_slice(&pane_height.to_be_bytes());
    bytes.extend_from_slice(output.text.as_bytes());
    Ok(RecoveryViewportFingerprint {
        convention_version: VIEWPORT_FINGERPRINT_CONVENTION_VERSION,
        pane_width,
        pane_height,
        digest: Sha256Digest::of(&bytes),
    })
}

fn recovery_viewport_dimensions(
    runner: &dyn TmuxRunner,
    pane: &PaneInstance,
) -> Result<(u16, u16)> {
    let output = runner
        .run(&[
            "display-message",
            "-p",
            "-t",
            &pane.pane_id,
            "#{pane_id}\t#{pane_pid}\t#{pane_width}\t#{pane_height}",
        ])
        .map_err(|error| {
            api_error!(
                "stale_precondition",
                format!("failed to resolve the recovery pane viewport: {error:#}"),
            )
        })?;
    let fields = output.trim_end().split('\t').collect::<Vec<_>>();
    let width = fields.get(2).and_then(|value| value.parse().ok());
    let height = fields.get(3).and_then(|value| value.parse().ok());
    if fields.len() != 4
        || fields[0] != pane.pane_id
        || fields[1].parse::<u32>().ok() != Some(pane.pane_pid)
        || width.is_none_or(|value: u16| value == 0)
        || height.is_none_or(|value: u16| value == 0)
    {
        return Err(api_error!(
            "stale_precondition",
            "recovery pane identity or dimensions are invalid",
        )
        .into());
    }
    Ok((width.unwrap(), height.unwrap()))
}

fn classify_run_recovery(
    first: &RunRecoveryObservation,
    second: &RunRecoveryObservation,
) -> (
    RunRecoveryStatus,
    Option<String>,
    Option<RecoveryProcessExpectation>,
) {
    if second.run.semantic_outcome == SemanticOutcome::Completed {
        return (
            RunRecoveryStatus::Completed,
            Some("run is already completed".to_string()),
            None,
        );
    }
    if first.run != second.run
        || first.pane != second.pane
        || first.process != second.process
        || first.viewport_fingerprint != second.viewport_fingerprint
    {
        return (
            RunRecoveryStatus::Unstable,
            Some(
                "run, pane, process, foreground ownership, or viewport changed between samples"
                    .to_string(),
            ),
            None,
        );
    }
    match &second.process {
        Some(process) if process == &second.run.binding.process => (
            RunRecoveryStatus::ExactPresentStable,
            Some(
                "the exact foreground process and content-agnostic viewport were stable"
                    .to_string(),
            ),
            Some(RecoveryProcessExpectation::ExactPresentStable {
                process: process.clone(),
            }),
        ),
        None => (
            RunRecoveryStatus::ExactAbsent,
            None,
            Some(RecoveryProcessExpectation::ExactAbsent),
        ),
        Some(process) => (
            RunRecoveryStatus::Replaced,
            None,
            Some(RecoveryProcessExpectation::ReplacedBy {
                process: process.clone(),
            }),
        ),
    }
}

fn observe_run_process(
    runner: &dyn TmuxRunner,
    run: &RunRecord,
) -> Result<Option<crate::pane_state::AgentProcessIdentity>> {
    runner
        .resolve_agent_process(run.binding.pane_instance.pane_pid, &run.binding.agent_kind)
        .map_err(|error| {
            api_error!(
                "identity_verification_failed",
                format!(
                    "could not freshly resolve the process bound to pane {}: {error}",
                    run.binding.pane_instance.pane_id
                ),
            )
            .into()
        })
}

#[allow(clippy::too_many_arguments)]
pub fn agent_run_resolve(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    run_ref: &str,
    outcome: &str,
    precondition: RecoveryPrecondition,
    resolution_id: &str,
    reason: &str,
) -> Result<String> {
    if outcome != "completed" {
        return Err(api_error!(
            "invalid_arguments",
            "API v3 only supports --outcome completed",
        )
        .into());
    }
    if reason.is_empty() || reason.len() > 1024 {
        return Err(api_error!(
            "invalid_arguments",
            "operator completion reason must be 1 to 1,024 UTF-8 bytes",
        )
        .into());
    }
    let resolution_id = ResolutionId::parse(resolution_id.to_string())
        .map_err(|error| api_error!("invalid_arguments", error.to_string()))?;
    precondition
        .validate()
        .map_err(|error| api_error!("invalid_arguments", error.to_string()))?;
    let mut connection = ApiConnection::connect(runner, env, None)?;
    connection
        .client
        .set_deadline(Instant::now() + CLIENT_REQUEST_TIMEOUT);
    let event_id =
        EventId::generate().map_err(|error| api_error!("internal_error", error.to_string()))?;
    let response = connection
        .client
        .request(&ClientMessage::ResolveAgentRun {
            proto: PROTOCOL_VERSION,
            daemon_instance_id: connection.client.daemon_instance_id().clone(),
            event_id,
            run_ref: run_ref.to_string(),
            outcome: outcome.to_string(),
            precondition,
            resolution_id,
            reason: reason.to_string(),
            actor_pid: std::process::id(),
        })
        .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
    match response {
        ServerMessage::AgentRunResolved {
            proto,
            run_ref: returned_ref,
            run,
        } if proto == PROTOCOL_VERSION && returned_ref == run_ref => success_agent_json(
            &connection,
            observed_at,
            ApiResult::AgentRunResolved {
                run_ref: returned_ref,
                run,
            },
        ),
        ServerMessage::Error { code, message, .. } => Err(daemon_api_error(code, message).into()),
        other => Err(api_error!(
            "invalid_daemon_response",
            format!("unexpected agent run resolve result: {other:?}"),
        )
        .into()),
    }
}

pub fn agent_operation_get(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    operation_ref: &str,
) -> Result<String> {
    let (connection, returned_ref, operation) =
        query_agent_operation(runner, env, operation_ref, None)?;
    let run_ref = linked_run_ref(&returned_ref, &operation)?;
    success_agent_json(
        &connection,
        observed_at,
        ApiResult::AgentOperation {
            operation_ref: returned_ref,
            run_ref,
            operation,
            waited_ms: 0,
        },
    )
}

pub fn agent_operation_wait(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    operation_ref: &str,
    timeout: Duration,
    _until_prompt_confirmed: bool,
    follow_unknown: bool,
) -> Result<String> {
    validate_agent_wait_timeout(timeout)?;
    let started = Instant::now();
    let deadline = started + timeout;
    let (connection, returned_ref, operation) =
        wait_for_operation(runner, env, operation_ref, deadline, follow_unknown)?;
    if operation.dispatch_state != DispatchState::PromptConfirmed {
        return Err(operation_terminal_error(&returned_ref, operation).into());
    }
    let run_ref = linked_run_ref(&returned_ref, &operation)?;
    success_agent_json(
        &connection,
        observed_at,
        ApiResult::AgentOperation {
            operation_ref: returned_ref,
            run_ref,
            operation,
            waited_ms: elapsed_millis(started),
        },
    )
}

pub fn agent_storage_status(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    connection
        .client
        .set_deadline(Instant::now() + CLIENT_REQUEST_TIMEOUT);
    let response = connection
        .client
        .request(&ClientMessage::QueryAgentStorage {
            proto: PROTOCOL_VERSION,
        })
        .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
    let usage = match response {
        ServerMessage::AgentStorageResult { proto, usage } if proto == PROTOCOL_VERSION => usage,
        ServerMessage::Error { code, message, .. } => {
            return Err(daemon_api_error(code, message).into());
        }
        other => {
            return Err(api_error!(
                "invalid_daemon_response",
                format!("unexpected agent storage result: {other:?}"),
            )
            .into());
        }
    };
    success_agent_json(&connection, observed_at, ApiResult::AgentStorage { usage })
}

pub fn agent_storage_reset_result(
    observed_at: i64,
    server_identity: String,
    previous_generation: String,
    generation: String,
) -> Result<String> {
    Ok(serde_json::to_string(&ApiSuccessEnvelope {
        meta: ApiMeta {
            api_version: API_VERSION,
            server_identity: Some(server_identity),
            daemon_instance_id: None,
            snapshot_revision: None,
            started_at: observed_at,
            emitted_at: epoch_now(),
            diagnostic_count: 0,
        },
        result: ApiResult::AgentStorageReset {
            previous_generation,
            generation,
        },
    })?)
}

fn query_agent_run(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    requested_run_ref: &str,
    deadline: Option<Instant>,
) -> Result<(ApiConnection, String, RunRecord)> {
    let mut connection = ApiConnection::connect(runner, env, deadline)?;
    let request_deadline = deadline
        .map(|deadline| deadline.min(Instant::now() + CLIENT_REQUEST_TIMEOUT))
        .unwrap_or_else(|| Instant::now() + CLIENT_REQUEST_TIMEOUT);
    connection.client.set_deadline(request_deadline);
    let response = connection
        .client
        .request(&ClientMessage::QueryAgentRun {
            proto: PROTOCOL_VERSION,
            run_ref: requested_run_ref.to_string(),
        })
        .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
    match response {
        ServerMessage::AgentRunResult {
            proto,
            run_ref,
            run,
        } if proto == PROTOCOL_VERSION && run_ref == requested_run_ref => {
            Ok((connection, run_ref, run))
        }
        ServerMessage::AgentRunResult { .. } => Err(api_error!(
            "invalid_daemon_response",
            "run query returned a mismatched protocol version or run_ref",
        )
        .into()),
        ServerMessage::Error { code, message, .. } => Err(daemon_api_error(code, message).into()),
        other => Err(api_error!(
            "invalid_daemon_response",
            format!("unexpected agent run result: {other:?}"),
        )
        .into()),
    }
}

fn exact_binding_for_pane(
    pane: &PanePresentation,
    server_identity: &crate::daemon::topology::ServerIdentity,
) -> Option<AgentBinding> {
    let state = &pane.resolved.as_ref()?.canonical;
    if !state.agent_present {
        return None;
    }
    let process = state.agent_process.clone()?;
    if pane.agent_process.as_ref() != Some(&process) {
        return None;
    }
    let binding = AgentBinding {
        server_identity: server_identity.clone(),
        pane_instance: pane.pane_instance.clone(),
        pane_state_id: state.state_id.clone(),
        agent_epoch: state.agent_epoch,
        agent_kind: state.agent.clone(),
        provider_session_id: state.agent_session_id.clone()?,
        process,
    };
    binding.validate().ok()?;
    Some(binding)
}

fn query_current_agent_runs(
    connection: &mut ApiConnection,
    snapshot: &ResolvedSnapshot,
) -> Result<Vec<CurrentAgentRun>> {
    let bindings = snapshot
        .panes
        .iter()
        .filter_map(|pane| exact_binding_for_pane(pane, &connection.incarnation.identity))
        .collect::<Vec<_>>();
    // Runtime connections accept exactly one request after Hello. Keep the snapshot and
    // current-run observations tied to the same daemon instance, but use a fresh connection for
    // the second request instead of writing to the peer-closed snapshot connection.
    let mut current_run_connection = connection.reconnect()?;
    current_run_connection
        .client
        .set_deadline(Instant::now() + CLIENT_REQUEST_TIMEOUT);
    let response = current_run_connection
        .client
        .request(&ClientMessage::QueryCurrentAgentRuns {
            proto: PROTOCOL_VERSION,
            bindings: bindings.clone(),
        })
        .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
    let runs = match response {
        ServerMessage::CurrentAgentRunsResult { proto, runs } if proto == PROTOCOL_VERSION => runs,
        ServerMessage::Error { code, message, .. } => {
            return Err(daemon_api_error(code, message).into());
        }
        other => {
            return Err(api_error!(
                "invalid_daemon_response",
                format!("unexpected current agent runs result: {other:?}"),
            )
            .into());
        }
    };
    let mut seen = Vec::<AgentBinding>::new();
    for run in &runs {
        if !bindings.contains(&run.binding) || seen.contains(&run.binding) {
            return Err(api_error!(
                "invalid_daemon_response",
                "current run batch returned an unrequested or duplicate Agent Binding",
            )
            .into());
        }
        let reference = RunRef::decode(&run.run_ref).map_err(|error| {
            api_error!(
                "invalid_daemon_response",
                format!("current run batch returned an invalid run_ref: {error}")
            )
        })?;
        if reference.server_identity != connection.server_identity {
            return Err(api_error!(
                "invalid_daemon_response",
                "current run batch returned a run_ref for another tmux server",
            )
            .into());
        }
        seen.push(run.binding.clone());
    }
    Ok(runs)
}

fn current_run_for_pane(
    pane: &PanePresentation,
    runs: &[CurrentAgentRun],
) -> Option<CurrentRunSummary> {
    let state = &pane.resolved.as_ref()?.canonical;
    let process = state.agent_process.as_ref()?;
    let run = runs.iter().find(|run| {
        run.binding.pane_instance == pane.pane_instance
            && run.binding.pane_state_id == state.state_id
            && run.binding.agent_epoch == state.agent_epoch
            && &run.binding.process == process
    })?;
    Some(CurrentRunSummary {
        run_ref: run.run_ref.clone(),
        execution_phase: run.execution_phase,
        semantic_outcome: run.semantic_outcome,
        status: durable_run_status(run.execution_phase, run.semantic_outcome),
    })
}

fn durable_run_status(
    execution_phase: ExecutionPhase,
    semantic_outcome: SemanticOutcome,
) -> DurableRunStatus {
    if semantic_outcome == SemanticOutcome::Completed {
        return DurableRunStatus::Done;
    }
    match execution_phase {
        ExecutionPhase::Running => DurableRunStatus::Working,
        ExecutionPhase::Waiting | ExecutionPhase::Error => DurableRunStatus::Blocked,
        ExecutionPhase::Ended => DurableRunStatus::EndedUnconfirmed,
    }
}

fn query_agent_operation(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    requested_operation_ref: &str,
    deadline: Option<Instant>,
) -> Result<(ApiConnection, String, OperationRecord)> {
    let mut connection = ApiConnection::connect(runner, env, deadline)?;
    let request_deadline = deadline
        .map(|deadline| deadline.min(Instant::now() + CLIENT_REQUEST_TIMEOUT))
        .unwrap_or_else(|| Instant::now() + CLIENT_REQUEST_TIMEOUT);
    connection.client.set_deadline(request_deadline);
    let response = connection
        .client
        .request(&ClientMessage::QueryAgentOperation {
            proto: PROTOCOL_VERSION,
            operation_ref: requested_operation_ref.to_string(),
        })
        .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
    match response {
        ServerMessage::AgentOperationResult {
            proto,
            operation_ref,
            operation,
        } if proto == PROTOCOL_VERSION && operation_ref == requested_operation_ref => {
            Ok((connection, operation_ref, operation))
        }
        ServerMessage::AgentOperationResult { .. } => Err(api_error!(
            "invalid_daemon_response",
            "operation query returned a mismatched protocol version or operation_ref",
        )
        .into()),
        ServerMessage::Error { code, message, .. } => Err(daemon_api_error(code, message).into()),
        other => Err(api_error!(
            "invalid_daemon_response",
            format!("unexpected agent operation result: {other:?}"),
        )
        .into()),
    }
}

fn wait_for_run(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    run_ref: &str,
    deadline: Instant,
    until_completed: bool,
) -> Result<(ApiConnection, String, RunRecord)> {
    let mut poll_interval = WAIT_POLL_INITIAL_INTERVAL;
    loop {
        match query_agent_run(runner, env, run_ref, Some(deadline)) {
            Ok((connection, returned_ref, run)) if run_wait_matches(&run, until_completed) => {
                return Ok((connection, returned_ref, run));
            }
            Ok(_) => {}
            Err(error) if retryable_poll_error(&error) => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            let expected = if until_completed {
                "semantic_outcome=completed"
            } else {
                "completed, waiting, error, or ended_unconfirmed"
            };
            return Err(api_error!(
                "timeout",
                format!("run {run_ref} did not reach {expected} before the deadline"),
            )
            .into());
        }
        sleep_until_next_poll(deadline, &mut poll_interval);
    }
}

fn wait_for_operation(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    operation_ref: &str,
    deadline: Instant,
    follow_unknown: bool,
) -> Result<(ApiConnection, String, OperationRecord)> {
    let mut poll_interval = WAIT_POLL_INITIAL_INTERVAL;
    let mut last_operation = None;
    loop {
        match query_agent_operation(runner, env, operation_ref, Some(deadline)) {
            Ok((connection, returned_ref, operation))
                if operation_wait_matches(&operation, follow_unknown) =>
            {
                return Ok((connection, returned_ref, operation));
            }
            Ok((_, _, operation)) => last_operation = Some(operation),
            Err(error) if retryable_poll_error(&error) => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            if let Some(operation) = last_operation {
                return Err(match operation.dispatch_state {
                    DispatchState::Prepared => {
                        operation_pre_dispatch_timeout(operation_ref, operation)
                    }
                    DispatchState::DispatchStarted => {
                        operation_dispatch_started_timeout(operation_ref, operation)
                    }
                    DispatchState::DeliveryUnknown => {
                        operation_terminal_error(operation_ref, operation)
                    }
                    DispatchState::PromptConfirmed | DispatchState::Rejected => ApiError::new(
                        ApiErrorCode::InvalidDaemonResponse,
                        "operation wait ignored a terminal operation",
                    ),
                }
                .into());
            }
            let expected = if follow_unknown {
                "prompt_confirmed or rejected after delivery_unknown"
            } else {
                "a terminal dispatch state"
            };
            return Err(prompt_wait_timeout(format!(
                "operation {operation_ref} did not reach {expected} before the deadline"
            ))
            .into());
        }
        sleep_until_next_poll(deadline, &mut poll_interval);
    }
}

fn run_wait_matches(run: &RunRecord, until_completed: bool) -> bool {
    if until_completed {
        return run.semantic_outcome == SemanticOutcome::Completed;
    }
    durable_run_status(run.execution_phase, run.semantic_outcome) != DurableRunStatus::Working
}

fn operation_wait_matches(operation: &OperationRecord, follow_unknown: bool) -> bool {
    match operation.dispatch_state {
        DispatchState::PromptConfirmed | DispatchState::Rejected => true,
        DispatchState::DeliveryUnknown => !follow_unknown,
        DispatchState::Prepared | DispatchState::DispatchStarted => false,
    }
}

fn operation_is_terminal(operation: &OperationRecord) -> bool {
    matches!(
        operation.dispatch_state,
        DispatchState::PromptConfirmed | DispatchState::DeliveryUnknown | DispatchState::Rejected
    )
}

fn linked_run_ref(operation_ref: &str, operation: &OperationRecord) -> Result<Option<String>> {
    let reference = OperationRef::decode(operation_ref).map_err(|error| {
        api_error!(
            "invalid_daemon_response",
            format!("daemon returned an invalid operation_ref: {error}")
        )
    })?;
    if reference.operation_id != operation.operation_id {
        return Err(api_error!(
            "invalid_daemon_response",
            "operation_ref does not identify the returned operation record",
        )
        .into());
    }
    operation
        .run_id
        .as_ref()
        .map(|run_id| {
            RunRef {
                server_identity: reference.server_identity,
                generation: reference.generation,
                run_id: run_id.clone(),
            }
            .encode()
            .map_err(|error| {
                api_error!(
                    "invalid_daemon_response",
                    format!("failed to derive linked run_ref: {error}")
                )
                .into()
            })
        })
        .transpose()
}

fn validate_agent_wait_timeout(timeout: Duration) -> Result<()> {
    if timeout.is_zero() || timeout > MAX_WAIT_TIMEOUT {
        return Err(api_error!(
            "invalid_arguments",
            format!(
                "--timeout-ms must be between 1 and {}",
                MAX_WAIT_TIMEOUT.as_millis()
            ),
        )
        .into());
    }
    Ok(())
}

fn retryable_poll_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|source| source.downcast_ref::<ApiError>())
        .is_some_and(|error| {
            matches!(
                error.code,
                ApiErrorCode::DaemonUnavailable
                    | ApiErrorCode::DaemonNotReady
                    | ApiErrorCode::DaemonQueryFailed
                    | ApiErrorCode::StaleDaemon
            )
        })
}

fn sleep_until_next_poll(deadline: Instant, interval: &mut Duration) {
    if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        std::thread::sleep(remaining.min(*interval));
        *interval = next_wait_poll_interval(*interval);
    }
}

fn next_wait_poll_interval(interval: Duration) -> Duration {
    interval
        .checked_mul(2)
        .unwrap_or(WAIT_POLL_MAX_INTERVAL)
        .min(WAIT_POLL_MAX_INTERVAL)
}

fn elapsed_millis(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

fn prompt_wait_timeout(message: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorCode::DeliveryUnknown, message).with_dispatch_context(
        ApiErrorStage::AfterDispatch,
        ApiSideEffect::Possible,
        ApiRetryAction::InspectManually,
        None,
    )
}

fn prompt_before_dispatch_timeout(message: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorCode::Timeout, message).with_dispatch_context(
        ApiErrorStage::BeforeDispatch,
        ApiSideEffect::None,
        ApiRetryAction::RetrySameRequest,
        None,
    )
}

fn operation_pre_dispatch_timeout(operation_ref: &str, operation: OperationRecord) -> ApiError {
    ApiError::new(
        ApiErrorCode::Timeout,
        "prompt operation remained prepared before the wait deadline",
    )
    .with_dispatch_context(
        ApiErrorStage::BeforeDispatch,
        ApiSideEffect::None,
        ApiRetryAction::RetrySameRequest,
        Some(OperationErrorReceipt {
            operation_ref: operation_ref.to_string(),
            operation,
        }),
    )
}

fn operation_dispatch_started_timeout(operation_ref: &str, operation: OperationRecord) -> ApiError {
    ApiError::new(
        ApiErrorCode::DeliveryUnknown,
        "prompt dispatch started but was not confirmed before the wait deadline",
    )
    .with_dispatch_context(
        ApiErrorStage::AfterDispatch,
        ApiSideEffect::Possible,
        ApiRetryAction::InspectManually,
        Some(OperationErrorReceipt {
            operation_ref: operation_ref.to_string(),
            operation,
        }),
    )
}

fn operation_terminal_error(operation_ref: &str, operation: OperationRecord) -> ApiError {
    let receipt_code = operation
        .result_receipt
        .as_ref()
        .map(|receipt| receipt.code.as_str())
        .unwrap_or("missing_receipt")
        .to_string();
    let dispatch_state = operation.dispatch_state;
    let receipt = Some(OperationErrorReceipt {
        operation_ref: operation_ref.to_string(),
        operation,
    });
    match dispatch_state {
        DispatchState::DeliveryUnknown => ApiError::new(
            ApiErrorCode::DeliveryUnknown,
            format!("prompt delivery is ambiguous: {receipt_code}"),
        )
        .with_dispatch_context(
            ApiErrorStage::AfterDispatch,
            ApiSideEffect::Possible,
            ApiRetryAction::InspectManually,
            receipt,
        ),
        DispatchState::Rejected => ApiError::new(
            ApiErrorCode::DispatchRejected,
            format!("prompt dispatch was rejected: {receipt_code}"),
        )
        .with_dispatch_context(
            ApiErrorStage::Dispatch,
            ApiSideEffect::None,
            ApiRetryAction::RefreshTarget,
            receipt,
        ),
        _ => ApiError::new(
            ApiErrorCode::InvalidDaemonResponse,
            "operation terminal error requested for a non-error state",
        ),
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
        ErrorCode::PromptDispatchBusy => ApiErrorCode::PromptDispatchBusy,
        ErrorCode::OperationConflict => ApiErrorCode::OperationConflict,
        ErrorCode::OperationNotFound => ApiErrorCode::OperationNotFound,
        ErrorCode::OperationStoreFull => ApiErrorCode::OperationStoreFull,
        ErrorCode::OperationGenerationReplaced => ApiErrorCode::OperationGenerationReplaced,
        ErrorCode::RunNotFound => ApiErrorCode::RunNotFound,
        ErrorCode::RunGenerationReplaced => ApiErrorCode::RunGenerationReplaced,
        ErrorCode::RunUnresolved => ApiErrorCode::RunUnresolved,
        ErrorCode::TargetReplaced => ApiErrorCode::TargetReplaced,
        ErrorCode::UnsupportedProvider => ApiErrorCode::UnsupportedProvider,
        ErrorCode::ProviderEventConflict => ApiErrorCode::ProviderEventConflict,
        ErrorCode::StaleStateIdentity | ErrorCode::StaleSelection | ErrorCode::StaleAgentEvent => {
            ApiErrorCode::StaleReference
        }
        ErrorCode::StaleDaemonInstance => ApiErrorCode::StaleDaemon,
        ErrorCode::StalePrecondition => ApiErrorCode::StalePrecondition,
        ErrorCode::RecoveryNotAllowed => ApiErrorCode::RecoveryNotAllowed,
        ErrorCode::ResolutionConflict => ApiErrorCode::ResolutionConflict,
        ErrorCode::RunAlreadyResolved => ApiErrorCode::RunAlreadyResolved,
        ErrorCode::StorageCapacityExceeded => ApiErrorCode::StorageCapacityExceeded,
        ErrorCode::StateUninitialized => ApiErrorCode::StateUninitialized,
        ErrorCode::ArtifactUnavailable => ApiErrorCode::ArtifactUnavailable,
        ErrorCode::ArtifactExpired => ApiErrorCode::ArtifactExpired,
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

fn success_agent_json(
    connection: &ApiConnection,
    started_at: i64,
    result: ApiResult,
) -> Result<String> {
    Ok(serde_json::to_string(&ApiSuccessEnvelope {
        meta: ApiMeta {
            api_version: API_VERSION,
            server_identity: Some(connection.server_identity.clone()),
            daemon_instance_id: Some(connection.daemon_instance_id.clone()),
            snapshot_revision: None,
            started_at,
            emitted_at: epoch_now(),
            diagnostic_count: 0,
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
    if !state.agent_present && !state.lifecycle.is_usage_limited() {
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
        current_run: None,
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
                    current_run: None,
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
    fn operation_result_derives_the_linked_run_reference() {
        let generation = crate::agent_state::StateGeneration::generate().unwrap();
        let operation_id = OperationId::parse("operation_123456").unwrap();
        let run_id = crate::agent_state::StableRunId::generate().unwrap();
        let operation_ref = OperationRef {
            server_identity: "server_identity".to_string(),
            generation: generation.clone(),
            operation_id: operation_id.clone(),
        }
        .encode()
        .unwrap();
        let pane = test_agent_pane();
        let state = &pane.resolved.as_ref().unwrap().canonical;
        let mut operation = OperationRecord {
            state_format_version: crate::agent_state::PRIVATE_STATE_FORMAT_VERSION,
            generation,
            operation_id,
            revision: 3,
            request_fingerprint: Sha256Digest::of(b"request"),
            target_agent_ref: "vta1:test".to_string(),
            prompt_digest: Sha256Digest::of(b"prompt"),
            dispatch_option: "paste_enter".to_string(),
            binding: crate::agent_state::AgentBinding {
                server_identity: crate::daemon::topology::ServerIdentity {
                    pid: 77,
                    start_time: 88,
                },
                pane_instance: pane.pane_instance,
                pane_state_id: state.state_id.clone(),
                agent_epoch: state.agent_epoch,
                agent_kind: state.agent.clone(),
                provider_session_id: state.agent_session_id.clone().unwrap(),
                process: state.agent_process.clone().unwrap(),
            }
            .into(),
            expected_pane_version: state.version(),
            expected_current_run: state.current_run.clone(),
            expected_run_seq: 2,
            confirmation_deadline_at: 11,
            dispatch_state: DispatchState::PromptConfirmed,
            run_id: Some(run_id.clone()),
            result_receipt: Some(crate::agent_state::OperationResultReceipt {
                code: "prompt_confirmed".to_string(),
                observed_at: 3,
                confirmation_basis: Some("provider_prompt_digest".to_string()),
                source_attribution: Some("user_prompt_submit".to_string()),
            }),
            created_at: 1,
            updated_at: 3,
        };
        operation.validate().unwrap();

        let linked = linked_run_ref(&operation_ref, &operation).unwrap().unwrap();
        let linked = RunRef::decode(&linked).unwrap();
        assert_eq!(linked.server_identity, "server_identity");
        assert_eq!(linked.generation, operation.generation);
        assert_eq!(linked.run_id, run_id);

        operation.dispatch_state = DispatchState::Prepared;
        operation.run_id = None;
        operation.result_receipt = None;
        assert_eq!(linked_run_ref(&operation_ref, &operation).unwrap(), None);
    }

    #[test]
    fn durable_status_and_unknown_follow_match_the_public_wait_contract() {
        assert_eq!(
            durable_run_status(ExecutionPhase::Running, SemanticOutcome::Unresolved),
            DurableRunStatus::Working
        );
        assert_eq!(
            durable_run_status(ExecutionPhase::Waiting, SemanticOutcome::Unresolved),
            DurableRunStatus::Blocked
        );
        assert_eq!(
            durable_run_status(ExecutionPhase::Error, SemanticOutcome::Unresolved),
            DurableRunStatus::Blocked
        );
        assert_eq!(
            durable_run_status(ExecutionPhase::Ended, SemanticOutcome::Unresolved),
            DurableRunStatus::EndedUnconfirmed
        );
        assert_eq!(
            durable_run_status(ExecutionPhase::Ended, SemanticOutcome::Completed),
            DurableRunStatus::Done
        );

        let mut operation = {
            let generation = crate::agent_state::StateGeneration::generate().unwrap();
            let pane = test_agent_pane();
            let state = &pane.resolved.as_ref().unwrap().canonical;
            OperationRecord {
                state_format_version: crate::agent_state::PRIVATE_STATE_FORMAT_VERSION,
                generation,
                operation_id: OperationId::parse("operation_wait_1234").unwrap(),
                revision: 2,
                request_fingerprint: Sha256Digest::of(b"request"),
                target_agent_ref: "vta1:test".to_string(),
                prompt_digest: Sha256Digest::of(b"prompt"),
                dispatch_option: "paste_enter".to_string(),
                binding: AgentBinding {
                    server_identity: crate::daemon::topology::ServerIdentity {
                        pid: 77,
                        start_time: 88,
                    },
                    pane_instance: pane.pane_instance.clone(),
                    pane_state_id: state.state_id.clone(),
                    agent_epoch: state.agent_epoch,
                    agent_kind: state.agent.clone(),
                    provider_session_id: state.agent_session_id.clone().unwrap(),
                    process: state.agent_process.clone().unwrap(),
                }
                .into(),
                expected_pane_version: state.version(),
                expected_current_run: state.current_run.clone(),
                expected_run_seq: 2,
                confirmation_deadline_at: 11,
                dispatch_state: DispatchState::DeliveryUnknown,
                run_id: None,
                result_receipt: None,
                created_at: 1,
                updated_at: 2,
            }
        };
        assert!(operation_wait_matches(&operation, false));
        assert!(!operation_wait_matches(&operation, true));
        operation.result_receipt = Some(crate::agent_state::OperationResultReceipt {
            code: "prompt_confirmation_timeout".to_string(),
            observed_at: 2,
            confirmation_basis: None,
            source_attribution: None,
        });
        let unknown = operation_terminal_error("vto3:test", operation.clone());
        assert_eq!(unknown.code, ApiErrorCode::DeliveryUnknown);
        assert_eq!(unknown.side_effect, ApiSideEffect::Possible);
        assert_eq!(unknown.retry_action, ApiRetryAction::InspectManually);
        assert_eq!(
            unknown
                .receipt
                .as_ref()
                .map(|receipt| receipt.operation_ref.as_str()),
            Some("vto3:test")
        );
        operation.dispatch_state = DispatchState::PromptConfirmed;
        assert!(operation_wait_matches(&operation, true));
        operation.dispatch_state = DispatchState::Rejected;
        assert!(operation_wait_matches(&operation, true));
        operation.result_receipt.as_mut().unwrap().code = "pane_precondition_changed".to_string();
        let rejected = operation_terminal_error("vto3:test", operation);
        assert_eq!(rejected.code, ApiErrorCode::DispatchRejected);
        assert_eq!(rejected.side_effect, ApiSideEffect::None);
        assert_eq!(rejected.retry_action, ApiRetryAction::RefreshTarget);
    }

    #[test]
    fn durable_wait_polling_backs_off_to_one_second() {
        let mut interval = WAIT_POLL_INITIAL_INTERVAL;
        let mut observed = Vec::new();
        for _ in 0..6 {
            observed.push(interval);
            interval = next_wait_poll_interval(interval);
        }
        assert_eq!(
            observed,
            [50, 100, 200, 400, 800, 1_000].map(Duration::from_millis)
        );
        assert_eq!(next_wait_poll_interval(interval), WAIT_POLL_MAX_INTERVAL);
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
            50
        );
        let error_codes = value["result"]["schemas"]["error"]["$defs"]["ApiErrorCode"]["enum"]
            .as_array()
            .unwrap();
        for code in [
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
        assert_eq!(wait_schema["properties"]["until"]["maxItems"], 4);
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
        assert_eq!(contract["providers"]["codex"]["status"], "enabled");
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
            contract["providers"]["claude_code"]["status"],
            "disabled_pending_authenticated_p0"
        );
        assert!(
            contract["providers"]["claude_code"]
                .get("attribution")
                .is_none()
        );
    }

    #[test]
    fn daemon_typed_errors_preserve_public_recovery_contracts() {
        use crate::daemon::protocol::v2::ErrorCode;

        let cases = [
            (
                ErrorCode::PromptDispatchBusy,
                ApiErrorCode::PromptDispatchBusy,
                ApiErrorStage::BeforeDispatch,
                ApiRetryAction::WaitThenRetry,
            ),
            (
                ErrorCode::OperationConflict,
                ApiErrorCode::OperationConflict,
                ApiErrorStage::RequestValidation,
                ApiRetryAction::Never,
            ),
            (
                ErrorCode::OperationNotFound,
                ApiErrorCode::OperationNotFound,
                ApiErrorStage::Observation,
                ApiRetryAction::Never,
            ),
            (
                ErrorCode::OperationStoreFull,
                ApiErrorCode::OperationStoreFull,
                ApiErrorStage::BeforeDispatch,
                ApiRetryAction::WaitThenRetry,
            ),
            (
                ErrorCode::OperationGenerationReplaced,
                ApiErrorCode::OperationGenerationReplaced,
                ApiErrorStage::Observation,
                ApiRetryAction::RestartObservation,
            ),
            (
                ErrorCode::RunNotFound,
                ApiErrorCode::RunNotFound,
                ApiErrorStage::Observation,
                ApiRetryAction::Never,
            ),
            (
                ErrorCode::RunGenerationReplaced,
                ApiErrorCode::RunGenerationReplaced,
                ApiErrorStage::Observation,
                ApiRetryAction::RestartObservation,
            ),
            (
                ErrorCode::RunUnresolved,
                ApiErrorCode::RunUnresolved,
                ApiErrorStage::Observation,
                ApiRetryAction::WaitThenRetry,
            ),
            (
                ErrorCode::RunAlreadyResolved,
                ApiErrorCode::RunAlreadyResolved,
                ApiErrorStage::Observation,
                ApiRetryAction::Never,
            ),
            (
                ErrorCode::TargetReplaced,
                ApiErrorCode::TargetReplaced,
                ApiErrorStage::TargetResolution,
                ApiRetryAction::RefreshTarget,
            ),
            (
                ErrorCode::UnsupportedProvider,
                ApiErrorCode::UnsupportedProvider,
                ApiErrorStage::TargetResolution,
                ApiRetryAction::Never,
            ),
            (
                ErrorCode::ProviderEventConflict,
                ApiErrorCode::ProviderEventConflict,
                ApiErrorStage::RequestValidation,
                ApiRetryAction::Never,
            ),
            (
                ErrorCode::RecoveryNotAllowed,
                ApiErrorCode::RecoveryNotAllowed,
                ApiErrorStage::Observation,
                ApiRetryAction::WaitThenRetry,
            ),
            (
                ErrorCode::StalePrecondition,
                ApiErrorCode::StalePrecondition,
                ApiErrorStage::Observation,
                ApiRetryAction::RestartObservation,
            ),
            (
                ErrorCode::ResolutionConflict,
                ApiErrorCode::ResolutionConflict,
                ApiErrorStage::RequestValidation,
                ApiRetryAction::Never,
            ),
            (
                ErrorCode::StorageCapacityExceeded,
                ApiErrorCode::StorageCapacityExceeded,
                ApiErrorStage::Observation,
                ApiRetryAction::WaitThenRetry,
            ),
            (
                ErrorCode::StateUninitialized,
                ApiErrorCode::StateUninitialized,
                ApiErrorStage::Observation,
                ApiRetryAction::Never,
            ),
            (
                ErrorCode::ArtifactUnavailable,
                ApiErrorCode::ArtifactUnavailable,
                ApiErrorStage::Observation,
                ApiRetryAction::Never,
            ),
            (
                ErrorCode::ArtifactExpired,
                ApiErrorCode::ArtifactExpired,
                ApiErrorStage::Observation,
                ApiRetryAction::Never,
            ),
        ];

        for (daemon_code, public_code, stage, retry_action) in cases {
            let error = daemon_api_error(daemon_code, "detail".to_string());
            assert_eq!(error.code, public_code);
            assert_eq!(error.stage, stage);
            assert_eq!(error.side_effect, ApiSideEffect::None);
            assert_eq!(error.retry_action, retry_action);
        }
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
    fn recovery_viewport_fingerprint_is_content_agnostic_and_dimension_bound() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        runner.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "%1",
                "#{pane_id}\t#{pane_pid}\t#{pane_width}\t#{pane_height}",
            ],
            "%1\t101\t80\t24\n",
        );
        runner.stub(&["capture-pane", "-pJ", "-t", "%1"], "first screen\n");
        let first = capture_visible_viewport_fingerprint(&runner, &pane).unwrap();

        runner.stub(&["capture-pane", "-pJ", "-t", "%1"], "different screen\n");
        let second = capture_visible_viewport_fingerprint(&runner, &pane).unwrap();

        assert_eq!(
            first.convention_version,
            VIEWPORT_FINGERPRINT_CONVENTION_VERSION
        );
        assert_eq!((first.pane_width, first.pane_height), (80, 24));
        assert_ne!(first.digest, second.digest);
    }

    #[test]
    fn recovery_foreground_fence_rejects_a_non_owner() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let process = crate::pane_state::AgentProcessIdentity {
            pid: 9001,
            start_token: "process-start".to_string(),
        };
        runner.stub_agent_input_owner(pane.pane_pid, process.pid, false);

        assert!(verify_recovery_foreground_owner(&runner, &pane, &process).is_err());
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
    fn absent_usage_limited_agent_remains_queryable_as_blocked() {
        let mut pane = test_agent_pane();
        pane.agent_process = None;
        let state = &mut pane.resolved.as_mut().unwrap().canonical;
        state.agent_process = None;
        state.agent_present = false;
        state.lifecycle = LifecycleState::Waiting {
            reason: WaitReason::usage_limit(),
        };
        pane.resolved.as_mut().unwrap().badge = BadgeState::Blocked;
        let snapshot = test_snapshot(pane.clone());

        let summary = agent_summary(&pane, &snapshot, "server").unwrap();
        assert_eq!(summary.status, AgentStatus::Blocked);
        assert_eq!(summary.lifecycle.state, "waiting");
        assert_eq!(summary.lifecycle.reason.as_deref(), Some("usage_limit"));
        assert!(!summary.present);
        assert!(summary.agent_ref.is_none());
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
