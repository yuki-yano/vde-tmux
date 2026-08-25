use super::super::test_support::*;
use super::*;
use crate::hook::RollupLevel;
use crate::sidebar::tui::mouse::row_for_click_with_indices;
use ratatui::backend::TestBackend;

fn buffer_row(terminal: &Terminal<TestBackend>, row: u16) -> String {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.width)
        .filter_map(|column| buffer.cell((column, row)))
        .map(|cell| cell.symbol())
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[test]
fn test_backend_draw_characterizes_header_body_footer_and_dialog_geometry() {
    let snapshot = snapshot(10);
    let sidebar = SidebarView {
        rows: vec![structural_row("category::work", SidebarRowKind::Category)],
        ..SidebarView::default()
    };
    let theme = SidebarRenderTheme::default();
    let header = build_header_layout_with_counts(&sidebar.state, 36, &theme, sidebar.counts);
    let areas = compute_areas(Rect::new(0, 0, 36, 12), &header);
    assert_eq!(
        (areas.header_rows, areas.rows_height, areas.footer_rows),
        (3, 8, 1)
    );

    let mut terminal = Terminal::new(TestBackend::new(36, 12)).unwrap();
    draw_snapshot_with_theme_and_scroll_options(
        &mut terminal,
        &snapshot,
        &sidebar,
        DrawOptions {
            theme: &theme,
            scroll: 0,
            connection: &ConnectionState::Connected,
            toast: None,
            category_dialog: None,
            rendered: None,
        },
    )
    .unwrap();
    assert!(buffer_row(&terminal, 0).contains("SIDEBAR"));
    assert!(!buffer_row(&terminal, areas.header_rows).is_empty());
    assert!(!buffer_row(&terminal, 11).is_empty());

    let dialog = CategoryDialog::editing(CategoryEditMode::Add {
        input: "work".to_string(),
    });
    draw_snapshot_with_theme_and_scroll_options(
        &mut terminal,
        &snapshot,
        &sidebar,
        DrawOptions {
            theme: &theme,
            scroll: 0,
            connection: &ConnectionState::Connected,
            toast: None,
            category_dialog: Some(&dialog),
            rendered: None,
        },
    )
    .unwrap();
    let screen = (0..12)
        .map(|row| buffer_row(&terminal, row))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("ADD CATEGORY"), "{screen}");
    assert!(screen.contains("work"), "{screen}");
}

#[test]
fn test_backend_draw_characterizes_connection_placeholders() {
    let cases = [
        (ConnectionState::Connecting, "connecting to daemon..."),
        (
            ConnectionState::Degraded("hook collision".to_string()),
            "daemon degraded; reconnecting...",
        ),
        (
            ConnectionState::Disconnected,
            "daemon disconnected; reconnecting...",
        ),
    ];
    for (connection, expected) in cases {
        let mut terminal = Terminal::new(TestBackend::new(40, 4)).unwrap();
        draw_connection_placeholder(&mut terminal, &connection).unwrap();
        assert_eq!(buffer_row(&terminal, 0), expected);
    }
}

#[test]
fn expanded_agent_selection_keeps_all_detail_rows_in_view() {
    let chat = |id: String| SidebarRow {
        id: id.clone(),
        kind: SidebarRowKind::Chat,
        depth: 0,
        label: id,
        chat_count: 0,
        rollup: RollupLevel::Running,
        badge_state: Some(BadgeState::Working),
        expanded: false,
        pane_id: None,
        git: None,
        active: false,
        meta: None,
    };
    let detail = |suffix: &str| SidebarRow {
        id: format!("detail::%6::106::{suffix}"),
        kind: SidebarRowKind::Detail,
        depth: 1,
        label: suffix.to_string(),
        chat_count: 0,
        rollup: RollupLevel::Running,
        badge_state: Some(BadgeState::Working),
        expanded: true,
        pane_id: Some("%6".to_string()),
        git: None,
        active: false,
        meta: None,
    };
    let mut rows = (1..=5)
        .map(|index| chat(format!("chat::%{index}::{}", index + 100)))
        .collect::<Vec<_>>();
    let mut selected = chat("chat::%6::106".to_string());
    selected.expanded = true;
    rows.push(selected);
    rows.extend([detail("summary"), detail("origin"), detail("prompt")]);
    rows.push(chat("chat::%7::107".to_string()));
    let row_indices = (0..rows.len()).map(Some).collect::<Vec<_>>();

    let range = rendered_selection_range(&rows, &row_indices, 5);

    assert_eq!(range, Some((5, 8)));
    assert_eq!(resolve_scroll_range(0, range, rows.len(), 6), 3);
}

#[test]
fn oversized_expanded_agent_is_anchored_at_its_first_line() {
    let rows = (0..10)
        .map(|index| SidebarRow {
            id: if index == 1 {
                "chat::%1::101".to_string()
            } else if index > 1 && index < 9 {
                format!("detail::%1::101::{index}")
            } else {
                format!("chat::%{}::{}", index + 1, index + 101)
            },
            kind: if index > 1 && index < 9 {
                SidebarRowKind::Detail
            } else {
                SidebarRowKind::Chat
            },
            depth: usize::from(index > 1 && index < 9),
            label: format!("row-{index}"),
            chat_count: 0,
            rollup: RollupLevel::Running,
            badge_state: Some(BadgeState::Working),
            expanded: index == 1,
            pane_id: None,
            git: None,
            active: false,
            meta: None,
        })
        .collect::<Vec<_>>();
    let row_indices = (0..rows.len()).map(Some).collect::<Vec<_>>();

    let range = rendered_selection_range(&rows, &row_indices, 1);

    assert_eq!(range, Some((1, 8)));
    assert_eq!(resolve_scroll_range(5, range, rows.len(), 4), 1);
}

#[test]
fn repeated_rendered_chat_rows_preserve_their_line_offset_when_scrolled() {
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
    let sidebar = SidebarView {
        rows: vec![chat],
        ..SidebarView::default()
    };
    let row_indices = vec![Some(0), Some(0)];

    let first = row_for_click_with_indices(&sidebar, 2, 2, 0, &row_indices).unwrap();
    let second = row_for_click_with_indices(&sidebar, 2, 2, 1, &row_indices).unwrap();

    assert!(first.is_first_rendered_line);
    assert!(!second.is_first_rendered_line);
}

#[test]
fn degraded_empty_message_takes_priority_over_healthy_empty() {
    let lines = connection_empty_lines(
        &ConnectionState::Degraded("hook collision".to_string()),
        &SidebarRenderTheme::default(),
        80,
    )
    .unwrap();
    let text = lines[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("Degraded: hook collision"));
}

#[test]
fn toast_and_connection_lines_respect_width_and_semantic_colors() {
    let theme = SidebarRenderTheme::default();
    let success = contextual_footer_line(
        12,
        &theme,
        Some(Notice {
            message: "operation completed successfully with a long message",
            level: NoticeLevel::Success,
        }),
        &ConnectionState::Connected,
    );
    assert!(
        success
            .spans
            .iter()
            .map(|span| display_width(span.content.as_ref()))
            .sum::<usize>()
            <= 12
    );
    assert_eq!(
        success.spans.first().and_then(|span| span.style.fg),
        Some(theme.badge_done)
    );

    for connection in [
        ConnectionState::Disconnected,
        ConnectionState::Degraded("a very long degraded diagnostic".to_string()),
    ] {
        let footer = contextual_footer_line(10, &theme, None, &connection);
        assert!(
            footer
                .spans
                .iter()
                .map(|span| display_width(span.content.as_ref()))
                .sum::<usize>()
                <= 10
        );
        assert_eq!(
            footer.spans.first().and_then(|span| span.style.fg),
            Some(theme.badge_blocked)
        );
        let empty = connection_empty_lines(&connection, &theme, 10).unwrap();
        assert!(empty.iter().all(|line| {
            line.spans
                .iter()
                .map(|span| display_width(span.content.as_ref()))
                .sum::<usize>()
                <= 10
        }));
    }
}
