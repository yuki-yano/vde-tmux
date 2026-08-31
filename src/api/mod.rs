mod terminal_mutation;

mod contract;

pub use contract::{
    API_VERSION, AgentBadge, AgentDetail, AgentIdentityStrength, AgentKeySendReceipt,
    AgentListFilter, AgentPromptConfirmation, AgentPromptDispatch, AgentPromptReceipt,
    AgentStartReceipt, AgentStatus, AgentSteerReceipt, AgentSummary, AgentTerminalSendReceipt,
    AgentWaitCursor, AgentWaitMatchSource, AgentWaitTarget, ApiAgentStartCapability,
    ApiAgentSteerCapability, ApiCategoryMutationReceipt, ApiCategoryPlacement,
    ApiCategoryPlacementState, ApiCategorySource, ApiCategorySummary, ApiCategoryTarget, ApiError,
    ApiErrorBody, ApiErrorCode, ApiErrorEnvelope, ApiErrorStage, ApiHardLimitContract, ApiMeta,
    ApiPromptConfirmationCapability, ApiPromptDispatchCapability, ApiProviderAttributionContract,
    ApiProviderAttributionField, ApiProviderCapabilities, ApiProviderContract,
    ApiProviderEvidenceBasis, ApiProviderHookEvent, ApiProviderStatus, ApiRepoSummary, ApiRequest,
    ApiResponseArtifactSource, ApiResponseCapability, ApiResult, ApiRetryAction, ApiSchemaContract,
    ApiSideEffect, ApiSteerRacePolicy, ApiSuccessEnvelope, ApiVersionContract, CurrentRunSummary,
    DEFAULT_PROMPT_CONFIRM_TIMEOUT, DEFAULT_READ_LINES, DEFAULT_WAIT_TIMEOUT, DiagnosticSummary,
    DurableRunStatus, LifecycleSummary, MAX_PROMPT_BYTES, MAX_PROMPT_CONFIRM_TIMEOUT,
    MAX_READ_BYTES, MAX_READ_LINES, MAX_WAIT_TIMEOUT, OperationErrorReceipt, OperationWaitUntil,
    PaneDetail, PaneSplitDirection, PaneSplitOptions, PaneSplitReceipt, PaneSummary, ReadOptions,
    ReadResult, ReadSource, RunRecoveryDiagnostic, RunRecoveryStatus, RunWaitUntil, SessionLink,
    TaskSummaryStatus, render_error, schema_json,
};

macro_rules! api_error {
    ("invalid_arguments", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::InvalidArguments, $message)
    };
    ("invalid_target", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::InvalidTarget, $message)
    };
    ("invalid_reference", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::InvalidReference, $message)
    };
    ("no_current_pane", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::NoCurrentPane, $message)
    };
    ("pane_not_found", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::PaneNotFound, $message)
    };
    ("agent_not_found", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::AgentNotFound, $message)
    };
    ("exact_identity_unavailable", $message:expr $(,)?) => {
        $crate::api::ApiError::new(
            $crate::api::ApiErrorCode::ExactIdentityUnavailable,
            $message,
        )
    };
    ("stale_reference", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::StaleReference, $message)
    };
    ("tmux_server_unavailable", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::TmuxServerUnavailable, $message)
    };
    ("daemon_unavailable", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::DaemonUnavailable, $message)
    };
    ("daemon_not_ready", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::DaemonNotReady, $message)
    };
    ("daemon_query_failed", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::DaemonQueryFailed, $message)
    };
    ("daemon_stream_error", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::DaemonStreamError, $message)
    };
    ("daemon_invalid_request", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::DaemonInvalidRequest, $message)
    };
    ("stale_daemon", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::StaleDaemon, $message)
    };
    ("timeout", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::Timeout, $message)
    };
    ("event_history_lost", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::EventHistoryLost, $message)
    };
    ("identity_verification_failed", $message:expr $(,)?) => {
        $crate::api::ApiError::new(
            $crate::api::ApiErrorCode::IdentityVerificationFailed,
            $message,
        )
    };
    ("control_unavailable", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::ControlUnavailable, $message)
    };
    ("protocol_mismatch", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::ProtocolMismatch, $message)
    };
    ("resource_limit", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::ResourceLimit, $message)
    };
    ("invalid_daemon_response", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::InvalidDaemonResponse, $message)
    };
    ("capture_failed", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::CaptureFailed, $message)
    };
    ("agent_busy", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::AgentBusy, $message)
    };
    ("agent_blocked", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::AgentBlocked, $message)
    };
    ("agent_limited", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::AgentLimited, $message)
    };
    ("prompt_confirmation_unavailable", $message:expr $(,)?) => {
        $crate::api::ApiError::new(
            $crate::api::ApiErrorCode::PromptConfirmationUnavailable,
            $message,
        )
    };
    ("agent_not_input_owner", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::AgentNotInputOwner, $message)
    };
    ("prompt_dispatch_busy", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::PromptDispatchBusy, $message)
    };
    ("dispatch_rejected", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::DispatchRejected, $message)
    };
    ("delivery_unknown", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::DeliveryUnknown, $message)
    };
    ("operation_conflict", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::OperationConflict, $message)
    };
    ("operation_not_found", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::OperationNotFound, $message)
    };
    ("operation_store_full", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::OperationStoreFull, $message)
    };
    ("operation_generation_replaced", $message:expr $(,)?) => {
        $crate::api::ApiError::new(
            $crate::api::ApiErrorCode::OperationGenerationReplaced,
            $message,
        )
    };
    ("run_not_found", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::RunNotFound, $message)
    };
    ("run_generation_replaced", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::RunGenerationReplaced, $message)
    };
    ("run_unresolved", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::RunUnresolved, $message)
    };
    ("run_already_resolved", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::RunAlreadyResolved, $message)
    };
    ("target_replaced", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::TargetReplaced, $message)
    };
    ("unsupported_provider", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::UnsupportedProvider, $message)
    };
    ("provider_event_conflict", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::ProviderEventConflict, $message)
    };
    ("recovery_not_allowed", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::RecoveryNotAllowed, $message)
    };
    ("stale_precondition", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::StalePrecondition, $message)
    };
    ("resolution_conflict", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::ResolutionConflict, $message)
    };
    ("storage_capacity_exceeded", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::StorageCapacityExceeded, $message)
    };
    ("state_uninitialized", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::StateUninitialized, $message)
    };
    ("artifact_unavailable", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::ArtifactUnavailable, $message)
    };
    ("artifact_expired", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::ArtifactExpired, $message)
    };
    ("daemon_error", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::DaemonError, $message)
    };
    ("internal_error", $message:expr $(,)?) => {
        $crate::api::ApiError::new($crate::api::ApiErrorCode::InternalError, $message)
    };
}

mod category;
mod common;
mod connection;
mod mutation;
mod pane;

pub use category::{category_assign, category_automatic, category_get, category_list};
pub use pane::{pane_current, pane_get, pane_list, pane_read, pane_split, snapshot};
mod agent;

pub(crate) use agent::run::{
    capture_visible_viewport_fingerprint, verify_recovery_foreground_owner,
};
pub use agent::{
    dispatch::{agent_prompt, agent_send, agent_send_keys, agent_start, agent_steer},
    operation::{agent_operation_get, agent_operation_wait},
    projection::{agent_get, agent_list},
    read_wait::{agent_read, agent_wait},
    run::{agent_run_check, agent_run_get, agent_run_resolve, agent_run_response, agent_run_wait},
    storage::{agent_storage_reset_result, agent_storage_status},
};
pub(crate) use agent::{
    guards::validate_prompt,
    operation::{PromptRequestIdentity, agent_prompt_resume},
};
#[cfg(test)]
mod test_support;
