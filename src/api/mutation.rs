use std::time::Duration;

use anyhow::Result;

use super::connection::ApiConnection;
use super::contract::{ApiError, ApiErrorCode, ApiErrorStage, ApiRetryAction, ApiSideEffect};
use super::terminal_mutation;
use crate::daemon::protocol::v2::{ClientMessage, PROTOCOL_VERSION, ServerMessage, V2Client};
use crate::pane_state::EventId;

pub(in crate::api) const MUTATION_PROJECTION_TIMEOUT: Duration = Duration::from_secs(3);

pub(in crate::api) fn apply_terminal_mutation(
    outcome: terminal_mutation::TerminalMutationOutcome,
) -> Result<()> {
    match outcome {
        terminal_mutation::TerminalMutationOutcome::Applied => Ok(()),
        terminal_mutation::TerminalMutationOutcome::Rejected(message) => {
            Err(api_error!("dispatch_rejected", message)
                .with_dispatch_context(
                    ApiErrorStage::BeforeDispatch,
                    ApiSideEffect::None,
                    ApiRetryAction::RefreshTarget,
                    None,
                )
                .into())
        }
        terminal_mutation::TerminalMutationOutcome::DeliveryUnknown(message) => {
            Err(api_error!("delivery_unknown", message)
                .with_dispatch_context(
                    ApiErrorStage::AfterDispatch,
                    ApiSideEffect::Possible,
                    ApiRetryAction::InspectManually,
                    None,
                )
                .into())
        }
    }
}

pub(in crate::api) fn refresh_canonical_topology_after_dispatch(
    connection: &ApiConnection,
    applied_effect: String,
) -> Result<u64> {
    let refresh = (|| -> Result<u64> {
        let mut client = V2Client::connect(&connection.socket, &connection.server_identity)?;
        if client.daemon_instance_id().as_str() != connection.daemon_instance_id {
            return Err(anyhow::anyhow!(
                "daemon instance changed before canonical topology refresh"
            ));
        }
        let event_id = EventId::generate()?;
        let response = client.request(&ClientMessage::RefreshTopology {
            proto: PROTOCOL_VERSION,
            daemon_instance_id: client.daemon_instance_id().clone(),
            event_id: event_id.clone(),
        })?;
        match response {
            ServerMessage::SnapshotAck {
                event_id: acknowledged,
                snapshot_revision,
                ..
            } if acknowledged == event_id => Ok(snapshot_revision),
            ServerMessage::Error { code, message, .. } => {
                Err(anyhow::anyhow!("{code:?}: {message}"))
            }
            other => Err(anyhow::anyhow!(
                "unexpected topology refresh response: {other:?}"
            )),
        }
    })();
    refresh.map_err(|error| {
        api_error!(
            "delivery_unknown",
            format!("{applied_effect} but canonical topology refresh failed: {error:#}"),
        )
        .with_dispatch_context(
            ApiErrorStage::AfterDispatch,
            ApiSideEffect::Confirmed,
            ApiRetryAction::InspectManually,
            None,
        )
        .into()
    })
}

pub(in crate::api) fn after_dispatch_error(error: anyhow::Error) -> ApiError {
    ApiError::new(
        ApiErrorCode::DeliveryUnknown,
        format!("terminal mutation was applied but target revalidation failed: {error:#}"),
    )
    .with_dispatch_context(
        ApiErrorStage::AfterDispatch,
        ApiSideEffect::Confirmed,
        ApiRetryAction::InspectManually,
        None,
    )
}
