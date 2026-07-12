pub mod client;
pub mod control;
pub mod input;
pub mod layout;
pub mod once;
pub mod preview;
pub mod render;
pub mod state;
pub mod store;
pub mod tree;
pub mod tui;

pub(crate) fn current_degraded_message(
    snapshot: &crate::daemon::protocol::v2::ResolvedSnapshot,
) -> Option<String> {
    snapshot
        .diagnostics
        .iter()
        .rev()
        .find(|diagnostic| diagnostic.code == crate::daemon::protocol::v2::ErrorCode::HookCollision)
        .map(|diagnostic| diagnostic.message.clone())
        .or_else(|| {
            snapshot
                .panes
                .iter()
                .find_map(|pane| pane.diagnostic.clone())
                .map(|diagnostic| format!("pane state quarantined: {diagnostic:?}"))
        })
}
