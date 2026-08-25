use crate::pane_state::{EventId, PaneInstance};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SidebarEffectCompletion {
    pub(super) original_accepted_seq: u64,
    pub(super) event_id: EventId,
    pub(super) snapshot_revision: u64,
    pub(super) witness_observation_floor: u64,
    pub(super) result: SidebarEffectResult,
    pub(super) effect: crate::daemon::runtime::CanonicalSidebarEffect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SidebarEffectResult {
    Succeeded(PaneInstance),
    ServerIncarnationMismatch,
    PaneInstanceMismatch,
    NoAvailablePane,
    SourceClientMismatch,
    Failed(String),
}
