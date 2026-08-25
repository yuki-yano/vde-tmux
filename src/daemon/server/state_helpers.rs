use std::collections::BTreeSet;

use anyhow::Result;

use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
use crate::pane_state::EventId;

use super::ProductionV2Coordinator;

pub(super) fn pane_snapshot_store(
    coordinator: &ProductionV2Coordinator,
) -> crate::pane_state::snapshot::FilePaneSnapshotStore {
    crate::pane_state::snapshot::FilePaneSnapshotStore::new(
        crate::pane_state::snapshot::snapshot_path(&coordinator.env, &coordinator.incarnation.hash),
        coordinator.incarnation.identity.clone(),
    )
}

pub(super) fn persist_pruned_sidebar_pins(
    coordinator: &ProductionV2Coordinator,
    state: &mut crate::daemon::runtime::CanonicalCoordinatorState,
) -> Result<bool, crate::pane_state::store::StoreError> {
    let present = state
        .topology
        .panes
        .iter()
        .map(|pane| pane.pane_instance.clone())
        .collect::<BTreeSet<_>>();
    let mut candidate = state.sidebar_preferences.clone();
    if !candidate.retain_panes(&present) {
        return Ok(false);
    }
    let path =
        crate::sidebar::store::state_path(&coordinator.env, &coordinator.incarnation.socket_path);
    crate::sidebar::store::save_state(&path, &candidate).map_err(|error| {
        crate::pane_state::store::StoreError::PersistFailed(format!(
            "sidebar pin cleanup persistence failed: {error:#}"
        ))
    })?;
    state.replace_sidebar_preferences(candidate)
}

pub(super) fn store_error_code(error: &crate::pane_state::store::StoreError) -> ErrorCode {
    use crate::pane_state::reducer::ReduceError;
    use crate::pane_state::store::StoreError;
    use ErrorCode;
    match error {
        StoreError::StateTooLarge => ErrorCode::StateTooLarge,
        StoreError::InvalidPaneInstance => ErrorCode::InvalidPaneInstance,
        StoreError::StaleStateIdentity => ErrorCode::StaleStateIdentity,
        StoreError::WriterLeaseHeld => ErrorCode::WriterLeaseHeld,
        StoreError::PersistFailed(_) => ErrorCode::PersistFailed,
        StoreError::FailStop(_) | StoreError::CounterOverflow(_) | StoreError::Random(_) => {
            ErrorCode::InternalError
        }
        StoreError::Reduce(reduce) => match reduce {
            ReduceError::InvalidRequest(_) | ReduceError::MissingStateId => {
                ErrorCode::InvalidRequest
            }
            ReduceError::InvalidPaneInstance => ErrorCode::InvalidPaneInstance,
            ReduceError::StaleStateIdentity => ErrorCode::StaleStateIdentity,
            ReduceError::StaleSelection => ErrorCode::StaleSelection,
            ReduceError::StaleAgentEvent => ErrorCode::StaleAgentEvent,
            ReduceError::InvalidProgressOperation(_) => ErrorCode::InvalidProgressOperation,
            ReduceError::StateInvariantViolation(_) => ErrorCode::StateInvariantViolation,
            ReduceError::CounterOverflow(_) => ErrorCode::InternalError,
        },
    }
}

pub(super) fn store_error_response(
    error: crate::pane_state::store::StoreError,
    event_id: Option<EventId>,
) -> ServerMessage {
    ServerMessage::error(store_error_code(&error), error.to_string(), event_id)
}

pub(super) fn production_store_error_response(
    coordinator: &ProductionV2Coordinator,
    error: crate::pane_state::store::StoreError,
    event_id: Option<EventId>,
) -> ServerMessage {
    if error.requires_daemon_exit() {
        coordinator.fail_stop(error.to_string());
    }
    store_error_response(error, event_id)
}
