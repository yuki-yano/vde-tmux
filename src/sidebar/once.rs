use std::path::Path;

use anyhow::Result;

use crate::config::{Config, SidebarWidth};
use crate::daemon::protocol::v2::ResolvedSnapshot;

pub fn render_once(socket: &Path, server_identity: &str, config: &Config) -> Result<String> {
    let snapshot = crate::sidebar::client::query_resolved_snapshot_v2(socket, server_identity)?;
    Ok(render_snapshot(&snapshot, config))
}

fn render_snapshot(snapshot: &ResolvedSnapshot, config: &Config) -> String {
    let width = match config.sidebar.width {
        SidebarWidth::Columns(width) => width,
        SidebarWidth::Percent(_) => config.sidebar.min_width,
    };
    let state = crate::sidebar::state::SidebarState::default();
    let projection = crate::sidebar::tree::project_sidebar(
        config,
        &snapshot.panes,
        &snapshot.sidebar_model,
        &state,
        crate::sidebar::tree::now_epoch_secs(),
    );
    let rendered = crate::sidebar::render::render_rows(&projection.rows, &state, width as usize);
    if !rendered.is_empty() {
        return rendered;
    }
    if let Some(message) = crate::sidebar::current_degraded_message(snapshot) {
        return format!("Degraded: {message}");
    }
    if state.filter == crate::sidebar::state::StatusFilter::All {
        "No agents detected".to_string()
    } else {
        format!("No matching agents\nFilter: {}", state.filter.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn render_snapshot_projects_sidebar_model_without_tmux_reparse() {
        let snapshot = ResolvedSnapshot {
            snapshot_revision: 7,
            panes: Vec::new(),
            sidebar_model: crate::daemon::SidebarModel::default(),
            attention: Vec::new(),
            events: Vec::new(),
            diagnostics: Vec::new(),
        };

        let rendered = render_snapshot(&snapshot, &Config::default());

        assert_eq!(rendered, "No agents detected");
    }

    #[test]
    fn historical_global_diagnostic_does_not_hide_the_empty_state() {
        let snapshot = ResolvedSnapshot {
            snapshot_revision: 7,
            panes: Vec::new(),
            sidebar_model: crate::daemon::SidebarModel::default(),
            attention: Vec::new(),
            events: Vec::new(),
            diagnostics: vec![crate::daemon::protocol::v2::DaemonDiagnostic {
                code: crate::daemon::protocol::v2::ErrorCode::PersistFailed,
                message: "historical write failure".to_string(),
                pane_instance: None,
                event_id: None,
            }],
        };

        assert_eq!(
            render_snapshot(&snapshot, &Config::default()),
            "No agents detected"
        );
    }
}
