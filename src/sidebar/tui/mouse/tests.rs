use super::*;
use crate::hook::RollupLevel;
use crate::sidebar::render::{
    SidebarRenderTheme, build_header_layout_with_counts, render_lines_with_indices,
};
use crate::sidebar::tree::BadgeCounts;
use crate::sidebar::tui::projection::{ViewportMove, move_projected_viewport};

#[test]
fn rendered_width_tiers_keep_clicks_bound_to_the_last_row_mapping() {
    let row = |id: &str| SidebarRow {
        id: id.to_string(),
        kind: SidebarRowKind::Chat,
        depth: 0,
        label: id.to_string(),
        chat_count: 1,
        rollup: RollupLevel::Running,
        badge_state: Some(BadgeState::Working),
        expanded: false,
        pane_id: Some(id.to_string()),
        git: None,
        active: false,
        meta: None,
    };
    let sidebar = SidebarView {
        rows: vec![row("first"), row("second")],
        ..SidebarView::default()
    };
    let theme = SidebarRenderTheme::default();

    for width in [3, 4, 24, 36] {
        let rendered = render_lines_with_indices(&sidebar.rows, &sidebar.state, width, &theme);
        let display_index = rendered
            .row_indices
            .iter()
            .position(|index| *index == Some(0))
            .expect("first row remains rendered at every width tier");
        let clicked = row_for_click_with_indices(
            &sidebar,
            3 + display_index as u16,
            3,
            0,
            &rendered.row_indices,
        )
        .expect("click resolves through the exact rendered row mapping");
        assert_eq!(clicked.row.id, "first");
        assert!(clicked.is_first_rendered_line);
    }
}

#[test]
fn mouse_coordinates_map_through_header_scroll_and_rendered_rows() {
    let row = |id: &str| SidebarRow {
        id: id.to_string(),
        kind: SidebarRowKind::Chat,
        depth: 0,
        label: id.to_string(),
        chat_count: 1,
        rollup: RollupLevel::Running,
        badge_state: Some(BadgeState::Working),
        expanded: false,
        pane_id: Some(id.to_string()),
        git: None,
        active: false,
        meta: None,
    };
    let sidebar = SidebarView {
        rows: vec![row("first"), row("second")],
        ..SidebarView::default()
    };
    let row_indices = vec![None, Some(0), Some(1)];

    assert!(row_for_click_with_indices(&sidebar, 1, 2, 0, &row_indices).is_none());
    assert_eq!(
        row_for_click_with_indices(&sidebar, 2, 2, 1, &row_indices)
            .map(|clicked| (clicked.row.id.as_str(), clicked.is_first_rendered_line)),
        Some(("first", true))
    );
    assert_eq!(
        row_for_click_with_indices(&sidebar, 3, 2, 1, &row_indices)
            .map(|clicked| (clicked.row.id.as_str(), clicked.is_first_rendered_line)),
        Some(("second", true))
    );
}

#[test]
fn agent_click_toggles_the_first_rendered_line_and_jumps_from_later_lines() {
    let chat = SidebarRow {
        id: "chat::%1::101".to_string(),
        kind: SidebarRowKind::Chat,
        depth: 0,
        label: "codex".to_string(),
        chat_count: 0,
        rollup: RollupLevel::Running,
        badge_state: Some(BadgeState::Working),
        expanded: false,
        pane_id: Some("%1".to_string()),
        git: None,
        active: false,
        meta: None,
    };
    let detail = SidebarRow {
        id: "detail::%1::101::prompt".to_string(),
        kind: SidebarRowKind::Detail,
        depth: 1,
        label: "fix bug".to_string(),
        chat_count: 0,
        rollup: RollupLevel::Running,
        badge_state: Some(BadgeState::Working),
        expanded: true,
        pane_id: Some("%1".to_string()),
        git: None,
        active: false,
        meta: None,
    };
    let pane = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 101,
    };

    assert_eq!(
        row_click_intent(&ClickedRenderedRow {
            row: &chat,
            is_first_rendered_line: true,
        }),
        Some(RowClickIntent::Toggle(chat.id.clone()))
    );
    assert_eq!(
        row_click_intent(&ClickedRenderedRow {
            row: &chat,
            is_first_rendered_line: false,
        }),
        Some(RowClickIntent::Jump(pane.clone()))
    );
    assert_eq!(
        row_click_intent(&ClickedRenderedRow {
            row: &detail,
            is_first_rendered_line: true,
        }),
        Some(RowClickIntent::Jump(pane))
    );
}

#[test]
fn mouse_scroll_keeps_cursor_while_vim_moves_follow_selection() {
    let rows = (0..8)
        .map(|index| SidebarRow {
            id: format!("chat::%{}::{}", index + 1, index + 101),
            kind: SidebarRowKind::Chat,
            depth: 0,
            label: format!("agent-{index}"),
            chat_count: 0,
            rollup: RollupLevel::Running,
            badge_state: Some(BadgeState::Working),
            expanded: false,
            pane_id: Some(format!("%{}", index + 1)),
            git: None,
            active: false,
            meta: None,
        })
        .collect::<Vec<_>>();
    let mut state = SidebarState {
        selection: Some(rows[0].id.clone()),
        ..SidebarState::default()
    };
    let sidebar = SidebarView {
        state: state.clone(),
        rows,
        counts: BadgeCounts::default(),
    };
    let theme = SidebarRenderTheme::default();
    let rendered = render_lines_with_indices(&sidebar.rows, &sidebar.state, 60, &theme);
    let mut frame = DrawnFrame {
        header: build_header_layout_with_counts(&sidebar.state, 60, &theme, sidebar.counts),
        header_rows: 2,
        rows_height: 4,
        width: 60,
        scroll: 0,
        row_indices: rendered.row_indices.clone(),
    };

    let original_selection = state.selection.clone();
    scroll_mouse_viewport(&mut state, &frame, true);
    assert_eq!(state.scroll, 3);
    assert_eq!(state.selection, original_selection);
    assert!(state.manual_scroll);

    frame.scroll = state.scroll;
    scroll_mouse_viewport(&mut state, &frame, false);
    assert_eq!(state.scroll, 0);
    assert_eq!(state.selection, original_selection);
    assert!(state.manual_scroll);

    frame.scroll = 0;

    move_projected_viewport(
        &sidebar,
        &rendered,
        &mut state,
        &frame,
        ViewportMove::PageDown,
    );
    assert_eq!(state.scroll, 4);
    assert_ne!(
        state.selection.as_deref(),
        Some(sidebar.rows[0].id.as_str())
    );
    assert!(!state.manual_scroll);

    move_projected_viewport(&sidebar, &rendered, &mut state, &frame, ViewportMove::First);
    assert_eq!(
        state.selection.as_deref(),
        Some(sidebar.rows[0].id.as_str())
    );
    assert_eq!(state.scroll, 0);

    move_projected_viewport(&sidebar, &rendered, &mut state, &frame, ViewportMove::Last);
    assert_eq!(
        state.selection.as_deref(),
        Some(sidebar.rows[7].id.as_str())
    );
    assert_eq!(
        state.scroll,
        rendered
            .lines
            .len()
            .saturating_sub(frame.rows_height as usize)
    );

    state.scroll = 0;
    state.manual_scroll = false;
    let before = state.clone();
    frame.scroll = 0;
    frame.rows_height = rendered.lines.len() as u16;
    scroll_mouse_viewport(&mut state, &frame, true);
    assert_eq!(state, before, "wheel is a no-op without overflow");
}
