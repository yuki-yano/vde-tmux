use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;

use super::common::enum_json_name;
use super::contract::{ApiError, ApiErrorCode};
use crate::daemon::protocol::v2::{
    CLIENT_REQUEST_TIMEOUT, ClientMessage, PROTOCOL_VERSION, ResolvedSnapshot, ServerMessage,
    V2Client,
};
use crate::tmux::TmuxRunner;

pub(super) struct ApiConnection {
    pub(super) client: V2Client,
    pub(super) socket: PathBuf,
    pub(super) incarnation: crate::daemon::lifecycle::TmuxServerIncarnation,
    pub(super) server_identity: String,
    pub(super) daemon_instance_id: String,
    wait_deadline: Option<Instant>,
    last_snapshot_revision: Option<u64>,
}

impl ApiConnection {
    pub(super) fn connect(
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

    pub(super) fn reconnect(&self) -> Result<Self> {
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

    pub(super) fn query_snapshot(&mut self) -> Result<ResolvedSnapshot> {
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

    pub(super) fn query_runtime_info(
        &mut self,
    ) -> Result<crate::daemon::protocol::v2::RuntimeInfo> {
        self.client
            .set_deadline(Instant::now() + CLIENT_REQUEST_TIMEOUT);
        match self
            .client
            .request(&ClientMessage::QueryRuntimeInfo {
                proto: PROTOCOL_VERSION,
            })
            .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?
        {
            ServerMessage::RuntimeInfoResult { info } => Ok(info),
            ServerMessage::Error { code, message, .. } => {
                Err(daemon_api_error(code, message).into())
            }
            other => Err(api_error!(
                "invalid_daemon_response",
                format!("unexpected daemon runtime response: {other:?}"),
            )
            .into()),
        }
    }

    pub(super) fn subscribe(&mut self) -> Result<ResolvedSnapshot> {
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

    pub(super) fn next_snapshot(&mut self) -> Result<ResolvedSnapshot> {
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

pub(super) fn daemon_connect_error(error: anyhow::Error) -> ApiError {
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

pub(super) fn snapshot_from_message(message: ServerMessage) -> Result<ResolvedSnapshot> {
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

pub(super) fn daemon_api_error(
    code: crate::daemon::protocol::v2::ErrorCode,
    message: String,
) -> ApiError {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::contract::{ApiErrorStage, ApiRetryAction, ApiSideEffect};

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
}
