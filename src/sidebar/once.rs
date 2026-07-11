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
    crate::sidebar::render::render_rows(
        &snapshot.sidebar.rows,
        &snapshot.sidebar.state,
        width as usize,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::SidebarFrame;
    use crate::hook::RollupLevel;
    use crate::sidebar::state::SidebarState;
    use crate::sidebar::tree::{BadgeCounts, SidebarRow, SidebarRowKind};

    #[test]
    fn render_snapshot_uses_canonical_sidebar_frame_without_tmux_reparse() {
        let snapshot = ResolvedSnapshot {
            snapshot_revision: 7,
            panes: Vec::new(),
            sidebar: SidebarFrame {
                state: SidebarState::default(),
                counts: BadgeCounts {
                    total: 1,
                    working: 1,
                    ..BadgeCounts::default()
                },
                rows: vec![SidebarRow {
                    id: "chat::%1".to_string(),
                    kind: SidebarRowKind::Chat,
                    depth: 0,
                    label: "codex (%1)".to_string(),
                    chat_count: 1,
                    rollup: RollupLevel::Running,
                    badge_state: None,
                    expanded: true,
                    pane_id: Some("%1".to_string()),
                    git: None,
                    active: false,
                    meta: None,
                }],
            },
            attention: Vec::new(),
            events: Vec::new(),
            diagnostics: Vec::new(),
        };

        let rendered = render_snapshot(&snapshot, &Config::default());

        assert!(rendered.contains("Codex"), "{rendered}");
    }
}
